use std::collections::{BTreeMap, HashSet};
use std::convert::Into;
use std::path::PathBuf;

use k8s_openapi::api::apps::v1::{StatefulSet, StatefulSetSpec, StatefulSetStatus};
use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::{
    ConfigMap, ConfigMapVolumeSource, Container, ContainerPort, EnvVar, EnvVarSource,
    HTTPGetAction, ObjectFieldSelector, PersistentVolumeClaim, PersistentVolumeClaimSpec, Pod,
    PodSecurityContext, PodSpec, PodTemplateSpec, Probe, SeccompProfile, SecurityContext, Service,
    ServiceAccount, ServicePort, ServiceSpec, Toleration, Volume, VolumeMount,
    VolumeResourceRequirements,
};
use k8s_openapi::api::policy::v1::{PodDisruptionBudget, PodDisruptionBudgetSpec};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::{DeleteParams, ListParams, Preconditions, PropagationPolicy};
use kube::core::PartialObjectMeta;
use kube::runtime::reflector::{ObjectRef, Store};
use kube::{
    api::{Patch, PatchParams},
    Api, ResourceExt,
};
use sha2::Digest;
use tracing::{debug, error, warn};

use crate::controllers::restatecluster::controller::Context;
use crate::resources::podidentityassociations::{
    PodIdentityAssociation, PodIdentityAssociationSpec,
};
use crate::resources::restateclusters::{RestateClusterSpec, RestateClusterStorage};
use crate::resources::securitygrouppolicies::{
    SecurityGroupPolicy, SecurityGroupPolicySecurityGroups, SecurityGroupPolicySpec,
};
use crate::Error;

use super::quantity_parser::QuantityParser;
use super::{label_selector, mandatory_labels, object_meta};

fn restate_service_account(
    base_metadata: &ObjectMeta,
    annotations: Option<&BTreeMap<String, String>>,
) -> ServiceAccount {
    let mut metadata = object_meta(base_metadata, "restate");
    if let Some(annotations) = annotations {
        match &mut metadata.annotations {
            Some(existing_annotations) => {
                existing_annotations.extend(annotations.iter().map(|(k, v)| (k.clone(), v.clone())))
            }
            None => metadata.annotations = Some(annotations.clone()),
        }
    }

    ServiceAccount {
        metadata,
        ..Default::default()
    }
}

fn restate_configmap(base_metadata: &ObjectMeta, config: Option<&str>) -> ConfigMap {
    let config: String = config.unwrap_or_default().into();

    let mut hasher = sha2::Sha256::new();
    hasher.update(config.as_bytes());
    let result = u32::from_le_bytes(hasher.finalize()[..4].try_into().unwrap());

    let metadata = object_meta(base_metadata, format!("restate-config-{result:x}"));

    ConfigMap {
        metadata,
        data: Some(BTreeMap::from([("config.toml".into(), config)])),
        ..Default::default()
    }
}

fn restate_pod_identity_association(
    ns: &str,
    base_metadata: &ObjectMeta,
    pod_identity_association_cluster: &str,
    pod_identity_association_role_arn: &str,
) -> PodIdentityAssociation {
    PodIdentityAssociation {
        metadata: object_meta(base_metadata, "restate"),
        spec: PodIdentityAssociationSpec {
            cluster_name: Some(pod_identity_association_cluster.into()),
            namespace: ns.into(),
            service_account: "restate".into(),
            role_arn: Some(pod_identity_association_role_arn.into()),
            client_request_token: None,
            cluster_ref: None,
            role_ref: None,
            tags: None,
        },
        status: None,
    }
}

fn restate_security_group_policy(
    base_metadata: &ObjectMeta,
    aws_security_groups: &[String],
) -> SecurityGroupPolicy {
    SecurityGroupPolicy {
        metadata: object_meta(base_metadata, "restate"),
        spec: SecurityGroupPolicySpec {
            security_groups: Some(SecurityGroupPolicySecurityGroups {
                group_ids: Some(aws_security_groups.into()),
            }),
            pod_selector: Some(label_selector(base_metadata)),
            service_account_selector: None,
        },
    }
}

fn restate_service(
    base_metadata: &ObjectMeta,
    annotations: Option<&BTreeMap<String, String>>,
) -> Service {
    let mut metadata = object_meta(base_metadata, "restate");
    if let Some(annotations) = annotations {
        match &mut metadata.annotations {
            Some(existing_annotations) => {
                existing_annotations.extend(annotations.iter().map(|(k, v)| (k.clone(), v.clone())))
            }
            None => metadata.annotations = Some(annotations.clone()),
        }
    }

    Service {
        metadata,
        spec: Some(ServiceSpec {
            selector: label_selector(base_metadata).match_labels,
            ports: Some(vec![
                ServicePort {
                    app_protocol: Some("kubernetes.io/h2c".into()),
                    port: 8080,
                    name: Some("ingress".into()),
                    ..Default::default()
                },
                ServicePort {
                    app_protocol: Some("kubernetes.io/h2c".into()),
                    port: 9070,
                    name: Some("admin".into()),
                    ..Default::default()
                },
                ServicePort {
                    app_protocol: Some("kubernetes.io/h2c".into()),
                    port: 5122,
                    name: Some("metrics".into()),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        }),
        status: None,
    }
}

fn restate_cluster_service(base_metadata: &ObjectMeta) -> Service {
    Service {
        metadata: object_meta(base_metadata, "restate-cluster"),
        spec: Some(ServiceSpec {
            selector: label_selector(base_metadata).match_labels,
            ports: Some(vec![ServicePort {
                app_protocol: Some("kubernetes.io/h2c".into()),
                port: 5122,
                name: Some("node".into()),
                ..Default::default()
            }]),
            // We want all pods in the StatefulSet to have their addresses published for
            // the sake of the other Restate pods even before they're ready, since they
            // have to be able to talk to each other in order to become ready.
            publish_not_ready_addresses: Some(true),
            cluster_ip: Some("None".into()), // headless service
            ..Default::default()
        }),
        status: None,
    }
}

fn restate_pod_disruption_budget(base_metadata: &ObjectMeta) -> PodDisruptionBudget {
    PodDisruptionBudget {
        metadata: object_meta(base_metadata, "restate"),
        spec: Some(PodDisruptionBudgetSpec {
            // 1 is a sane default for clusters of all sizes:
            // cluster size one it will allow downtime, but this is unavoidable when draining nodes
            // cluster size of 3 with r=2, it will prevent rollouts leading to unavailability
            // cluster size of 5 with r=3, it is conservative but not unreasonable
            max_unavailable: Some(IntOrString::Int(1)),
            min_available: None,
            selector: Some(label_selector(base_metadata)),
            unhealthy_pod_eviction_policy: None,
        }),
        status: None,
    }
}

fn env(cluster_name: &str, custom: Option<&[EnvVar]>) -> Vec<EnvVar> {
    let defaults = [
        ("RESTATE_LOG_FORMAT", "json"),
        ("RESTATE_CLUSTER_NAME", cluster_name),
        ("RESTATE_BASE_DIR", "/restate-data"),
        ("RUST_BACKTRACE", "1"),
        ("RUST_LIB_BACKTRACE", "0"),
        ("RESTATE_CONFIG", "/config/config.toml"),
        (
            "RESTATE_ADVERTISED_ADDRESS",
            // POD_NAME comes from the downward api, below
            "http://$(POD_NAME).restate-cluster:5122",
        ),
        (
            "RESTATE_NODE_NAME",
            // POD_NAME comes from the downward api, below
            "$(POD_NAME)",
        ),
    ];

    // allow crd to override our defaults
    let custom_names: HashSet<&str> = custom
        .map(|custom| custom.iter().map(|e| e.name.as_ref()).collect())
        .unwrap_or_default();

    let defaults = defaults
        .into_iter()
        .filter(|(k, _)| !custom_names.contains(k))
        .map(|(k, v)| EnvVar {
            name: k.into(),
            value: Some(v.into()),
            value_from: None,
        });

    let downward_api_vars = [
        EnvVar {
            name: "POD_NAME".into(),
            value: None,
            value_from: Some(EnvVarSource {
                config_map_key_ref: None,
                field_ref: Some(ObjectFieldSelector {
                    api_version: None,
                    field_path: "metadata.name".into(),
                }),
                resource_field_ref: None,
                secret_key_ref: None,
            }),
        },
        EnvVar {
            name: "POD_ZONE".into(),
            value: None,
            value_from: Some(EnvVarSource {
                config_map_key_ref: None,
                field_ref: Some(ObjectFieldSelector {
                    api_version: None,
                    field_path: "metadata.labels['topology.kubernetes.io/zone']".into(),
                }),
                resource_field_ref: None,
                secret_key_ref: None,
            }),
        },
        EnvVar {
            name: "POD_REGION".into(),
            value: None,
            value_from: Some(EnvVarSource {
                config_map_key_ref: None,
                field_ref: Some(ObjectFieldSelector {
                    api_version: None,
                    field_path: "metadata.labels['topology.kubernetes.io/region']".into(),
                }),
                resource_field_ref: None,
                secret_key_ref: None,
            }),
        },
    ];

    let defaults = downward_api_vars.into_iter().chain(defaults);

    if let Some(custom) = custom {
        defaults.chain(custom.iter().cloned()).collect()
    } else {
        defaults.collect()
    }
}

const RESTATE_STATEFULSET_NAME: &str = "restate";

fn restate_statefulset(
    base_metadata: &ObjectMeta,
    spec: &RestateClusterSpec,
    pod_annotations: Option<BTreeMap<String, String>>,
    signing_key: Option<(Volume, PathBuf)>,
    cm_name: String,
) -> StatefulSet {
    let metadata = object_meta(base_metadata, RESTATE_STATEFULSET_NAME);
    let labels = metadata.labels.clone();
    let pod_annotations = match (pod_annotations, metadata.annotations.clone()) {
        (Some(pod_annotations), Some(mut base_annotations)) => {
            base_annotations.extend(pod_annotations);
            Some(base_annotations)
        }
        (Some(annotations), None) | (None, Some(annotations)) => Some(annotations),
        (None, None) => None,
    };

    let mut volume_mounts = vec![
        VolumeMount {
            name: "storage".into(),
            mount_path: "/restate-data".into(),
            ..Default::default()
        },
        VolumeMount {
            name: "tmp".into(),
            mount_path: "/tmp".into(),
            ..Default::default()
        },
        VolumeMount {
            name: "config".into(),
            mount_path: "/config".into(),
            read_only: Some(true),
            ..Default::default()
        },
    ];

    let mut volumes = vec![
        Volume {
            name: "tmp".into(),
            empty_dir: Some(Default::default()),
            ..Default::default()
        },
        Volume {
            name: "config".into(),
            config_map: Some(ConfigMapVolumeSource {
                name: cm_name,
                ..Default::default()
            }),
            ..Default::default()
        },
    ];

    let mut env = env(
        spec.cluster_name
            .as_ref()
            .or(base_metadata.name.as_ref())
            .unwrap(),
        spec.compute.env.as_deref(),
    );

    if let Some((volume, relative_path)) = signing_key {
        let mut absolute_path = PathBuf::from("/signing-key");

        volume_mounts.push(VolumeMount {
            mount_path: absolute_path.to_str().unwrap().into(),
            name: volume.name.clone(),
            read_only: Some(true),
            ..Default::default()
        });
        volumes.push(volume);
        absolute_path.push(relative_path);
        env.push(EnvVar {
            name: "RESTATE_REQUEST_IDENTITY_PRIVATE_KEY_PEM_FILE".into(),
            value: Some(absolute_path.to_str().unwrap().into()),
            value_from: None,
        })
    }

    StatefulSet {
        metadata,
        spec: Some(StatefulSetSpec {
            replicas: spec.compute.replicas,
            selector: label_selector(base_metadata),
            service_name: "restate-cluster".into(),
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels,
                    annotations: pod_annotations,
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    affinity: spec.compute.affinity.clone(),
                    automount_service_account_token: Some(false),
                    dns_policy: spec.compute.dns_policy.clone(),
                    dns_config: spec.compute.dns_config.clone(),
                    image_pull_secrets: spec.compute.image_pull_secrets.clone(),
                    containers: vec![Container {
                        name: "restate".into(),
                        image: Some(spec.compute.image.clone()),
                        image_pull_policy: spec.compute.image_pull_policy.clone(),
                        command: spec.compute.command.clone(),
                        args: spec.compute.args.clone(),
                        env: Some(env),
                        ports: Some(vec![
                            ContainerPort {
                                name: Some("ingress".into()),
                                container_port: 8080,
                                ..Default::default()
                            },
                            ContainerPort {
                                name: Some("admin".into()),
                                container_port: 9070,
                                ..Default::default()
                            },
                            ContainerPort {
                                name: Some("metrics".into()),
                                container_port: 5122,
                                ..Default::default()
                            },
                        ]),
                        readiness_probe: Some(Probe {
                            http_get: Some(HTTPGetAction {
                                port: IntOrString::Int(9070),
                                path: Some("/health".into()),
                                ..Default::default()
                            }),
                            initial_delay_seconds: Some(30),
                            ..Default::default()
                        }),
                        resources: spec.compute.resources.clone(),
                        security_context: Some(SecurityContext {
                            read_only_root_filesystem: Some(true),
                            allow_privilege_escalation: Some(false),
                            ..Default::default()
                        }),
                        volume_mounts: Some(volume_mounts),
                        ..Default::default()
                    }],
                    security_context: Some(PodSecurityContext {
                        run_as_user: Some(1000),
                        run_as_group: Some(3000),
                        fs_group: Some(2000),
                        fs_group_change_policy: Some("OnRootMismatch".into()),
                        seccomp_profile: Some(SeccompProfile {
                            type_: "RuntimeDefault".into(),
                            localhost_profile: None,
                        }),
                        ..Default::default()
                    }),
                    service_account_name: Some("restate".into()),
                    termination_grace_period_seconds: Some(60),
                    volumes: Some(volumes),
                    tolerations: spec.compute.tolerations.clone(),
                    node_selector: spec.compute.node_selector.clone(),
                    topology_spread_constraints: spec.compute.topology_spread_constraints.clone(),
                    ..Default::default()
                }),
            },
            // It's important to start multiple pods at the same time in case multiple pods died.
            // Otherwise, we risk unavailability of an already configured metadata cluster
            pod_management_policy: Some("Parallel".to_owned()),
            volume_claim_templates: Some(vec![PersistentVolumeClaim {
                metadata: ObjectMeta {
                    name: Some("storage".into()),
                    labels: Some(mandatory_labels(base_metadata)), // caution needed; these cannot be changed
                    ..Default::default()
                },
                spec: Some(PersistentVolumeClaimSpec {
                    storage_class_name: spec.storage.storage_class_name.clone(),
                    volume_attributes_class_name: spec.storage.volume_attributes_class_name.clone(),
                    access_modes: Some(vec!["ReadWriteOnce".into()]),
                    resources: Some(restate_pvc_resources(&spec.storage)),
                    ..Default::default()
                }),
                status: None,
            }]),
            ..Default::default()
        }),
        status: None,
    }
}

fn restate_pvc_resources(storage: &RestateClusterStorage) -> VolumeResourceRequirements {
    VolumeResourceRequirements {
        requests: Some(BTreeMap::from([(
            "storage".to_string(),
            Quantity(format!("{}", storage.storage_request_bytes)),
        )])),
        limits: None,
    }
}

pub async fn reconcile_compute(
    ctx: &Context,
    namespace: &str,
    base_metadata: &ObjectMeta,
    spec: &RestateClusterSpec,
    signing_key: Option<(Volume, PathBuf)>,
) -> Result<(), Error> {
    let ss_api: Api<StatefulSet> = Api::namespaced(ctx.client.clone(), namespace);
    let cm_api: Api<ConfigMap> = Api::namespaced(ctx.client.clone(), namespace);
    let pvc_api: Api<PersistentVolumeClaim> = Api::namespaced(ctx.client.clone(), namespace);
    let svc_api: Api<Service> = Api::namespaced(ctx.client.clone(), namespace);
    let svcacc_api: Api<ServiceAccount> = Api::namespaced(ctx.client.clone(), namespace);
    let pia_api: Api<PodIdentityAssociation> = Api::namespaced(ctx.client.clone(), namespace);
    let job_api: Api<Job> = Api::namespaced(ctx.client.clone(), namespace);
    let pod_api: Api<Pod> = Api::namespaced(ctx.client.clone(), namespace);
    let sgp_api: Api<SecurityGroupPolicy> = Api::namespaced(ctx.client.clone(), namespace);
    let pdb_api: Api<PodDisruptionBudget> = Api::namespaced(ctx.client.clone(), namespace);

    apply_service_account(
        namespace,
        &svcacc_api,
        restate_service_account(
            base_metadata,
            spec.security
                .as_ref()
                .and_then(|s| s.service_account_annotations.as_ref()),
        ),
    )
    .await?;

    let cm = restate_configmap(base_metadata, spec.config.as_deref());
    let cm_name: String = cm.metadata.name.as_ref().unwrap().into();
    apply_configmap(namespace, &cm_api, cm).await?;

    let mut pod_annotations: Option<BTreeMap<String, String>> = None;

    match (
        ctx.aws_pod_identity_association_cluster.as_ref(),
        spec.security
            .as_ref()
            .and_then(|s| s.aws_pod_identity_association_role_arn.as_ref()),
    ) {
        (
            Some(aws_pod_identity_association_cluster),
            Some(aws_pod_identity_association_role_arn),
        ) => {
            let pia = apply_pod_identity_association(
                namespace,
                &pia_api,
                restate_pod_identity_association(
                    namespace,
                    base_metadata,
                    aws_pod_identity_association_cluster,
                    aws_pod_identity_association_role_arn,
                ),
            )
            .await?;

            if !is_pod_identity_association_synced(pia) {
                return Err(Error::NotReady { reason: "PodIdentityAssociationNotSynced".into(), message: "Waiting for the AWS ACK controller to provision the Pod Identity Association with IAM".into(), requeue_after: None });
            }

            check_pia(
                namespace,
                base_metadata,
                spec.compute.tolerations.as_ref(),
                &job_api,
                &pod_api,
            )
            .await?;

            // Pods MUST roll when these change, so we will apply these parameters as annotations to the pod meta
            let pod_annotations = pod_annotations.get_or_insert_with(Default::default);
            pod_annotations.insert(
                "restate.dev/aws-pod-identity-association-cluster".into(),
                aws_pod_identity_association_cluster.to_owned(),
            );
            pod_annotations.insert(
                "restate.dev/aws-pod-identity-association-role-arn".into(),
                aws_pod_identity_association_role_arn.to_owned(),
            );
        }
        (Some(_), None) => {
            delete_pod_identity_association(namespace, &pia_api, "restate").await?;
            delete_job(namespace, &job_api, "restate-pia-canary").await?;
        }
        (None, Some(aws_pod_identity_association_role_arn)) => {
            warn!("Ignoring AWS pod identity association role ARN {aws_pod_identity_association_role_arn} as the operator is not configured with --aws-pod-identity-association-cluster");
        }
        (None, None) => {}
    };

    match spec
        .security
        .as_ref()
        .and_then(|s| s.aws_pod_security_groups.as_deref())
    {
        Some(aws_pod_security_groups)
            if ctx.security_group_policy_installed && !aws_pod_security_groups.is_empty() =>
        {
            apply_security_group_policy(
                namespace,
                &sgp_api,
                restate_security_group_policy(base_metadata, aws_pod_security_groups),
            )
            .await?;

            let pod_annotations = pod_annotations.get_or_insert_with(Default::default);
            // Pods MUST roll when these change, so we will apply the groups as annotations to the pod meta
            pod_annotations.insert(
                "restate.dev/aws-security-groups".into(),
                aws_pod_security_groups.join(","),
            );
        }
        None | Some(_) if ctx.security_group_policy_installed => {
            delete_security_group_policy(namespace, &sgp_api, "restate").await?;
        }
        Some(aws_pod_security_groups) if !aws_pod_security_groups.is_empty() => {
            warn!("Ignoring AWS pod security groups {} as the SecurityGroupPolicy CRD is not installed", aws_pod_security_groups.join(","));
        }
        None | Some(_) => {}
    }

    let restate_service = restate_service(
        base_metadata,
        spec.security
            .as_ref()
            .and_then(|s| s.service_annotations.as_ref()),
    );
    apply_service(namespace, &svc_api, restate_service).await?;

    let restate_cluster_service = restate_cluster_service(base_metadata);
    apply_service(namespace, &svc_api, restate_cluster_service).await?;

    apply_pod_disruption_budget(
        namespace,
        &pdb_api,
        restate_pod_disruption_budget(base_metadata),
    )
    .await?;

    change_statefulset_storage(
        namespace,
        base_metadata,
        &ss_api,
        &ctx.ss_store,
        &pvc_api,
        &ctx.pvc_meta_store,
        &spec.storage,
    )
    .await?;

    let ss = apply_stateful_set(
        namespace,
        &ss_api,
        restate_statefulset(base_metadata, spec, pod_annotations, signing_key, cm_name),
    )
    .await?;

    validate_stateful_set_status(ss.status, spec.compute.replicas.unwrap_or(1))?;

    Ok(())
}

async fn apply_service(namespace: &str, ss_api: &Api<Service>, ss: Service) -> Result<(), Error> {
    let name = ss.metadata.name.as_ref().unwrap();
    let params: PatchParams = PatchParams::apply("restate-operator").force();
    debug!("Applying Service {} in namespace {}", name, namespace);
    ss_api.patch(name, &params, &Patch::Apply(&ss)).await?;
    Ok(())
}

async fn apply_service_account(
    namespace: &str,
    svcacc_api: &Api<ServiceAccount>,
    svcacc: ServiceAccount,
) -> Result<(), Error> {
    let name = svcacc.metadata.name.as_ref().unwrap();
    let params: PatchParams = PatchParams::apply("restate-operator").force();
    debug!(
        "Applying ServiceAccount {} in namespace {}",
        name, namespace
    );
    svcacc_api
        .patch(name, &params, &Patch::Apply(&svcacc))
        .await?;
    Ok(())
}

async fn apply_configmap(
    namespace: &str,
    cm_api: &Api<ConfigMap>,
    cm: ConfigMap,
) -> Result<(), Error> {
    let name = cm.metadata.name.as_ref().unwrap();
    let params: PatchParams = PatchParams::apply("restate-operator").force();
    debug!("Applying ConfigMap {} in namespace {}", name, namespace);
    cm_api.patch(name, &params, &Patch::Apply(&cm)).await?;
    Ok(())
}

async fn apply_pod_identity_association(
    namespace: &str,
    pia_api: &Api<PodIdentityAssociation>,
    pia: PodIdentityAssociation,
) -> Result<PodIdentityAssociation, Error> {
    let name = pia.metadata.name.as_ref().unwrap();
    let params: PatchParams = PatchParams::apply("restate-operator").force();
    debug!(
        "Applying PodIdentityAssociation {} in namespace {}",
        name, namespace
    );
    Ok(pia_api.patch(name, &params, &Patch::Apply(&pia)).await?)
}

async fn check_pia(
    namespace: &str,
    base_metadata: &ObjectMeta,
    tolerations: Option<&Vec<Toleration>>,
    job_api: &Api<Job>,
    pod_api: &Api<Pod>,
) -> Result<(), Error> {
    let name = "restate-pia-canary";
    let params: PatchParams = PatchParams::apply("restate-operator").force();

    let mut metadata = object_meta(base_metadata, name);
    let labels = metadata.labels.get_or_insert(Default::default());
    if let Some(existing) = labels.get_mut("app.kubernetes.io/name") {
        *existing = name.into()
    } else {
        labels.insert("app.kubernetes.io/name".into(), name.into());
    }

    debug!(
        "Applying PodIdentityAssociation canary Job in namespace {}",
        namespace
    );

    let created = job_api
        .patch(
            name,
            &params,
            &Patch::Apply(&Job {
                metadata,
                spec: Some(JobSpec {
                    // single-use job that we delete on failuire; don't want to wait 10 seconds for retries
                    backoff_limit: Some(1),
                    template: PodTemplateSpec {
                        metadata: None,
                        spec: Some(PodSpec {
                            service_account_name: Some("restate".into()),
                            containers: vec![Container {
                                name: "canary".into(),
                                image: Some("busybox:uclibc".into()),
                                command: Some(vec![
                                    "grep".into(),
                                    "-q".into(),
                                    "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE".into(),
                                    "/proc/self/environ".into(),
                                ]),
                                ..Default::default()
                            }],
                            tolerations: tolerations.cloned(),
                            restart_policy: Some("Never".into()),
                            ..Default::default()
                        }),
                    },
                    ..Default::default()
                }),
                status: None,
            }),
        )
        .await?;

    if let Some(conditions) = created.status.and_then(|s| s.conditions) {
        for condition in conditions {
            if condition.status != "True" {
                continue;
            }
            match condition.type_.as_str() {
                "Complete" => {
                    debug!(
                        "PodIdentityAssociation canary check succeeded in namespace {}",
                        namespace
                    );
                    return Ok(());
                }
                "Failed" => {
                    error!(
                        "PodIdentityAssociation canary check failed in namespace {}, deleting Job",
                        namespace
                    );

                    delete_job(namespace, job_api, name).await?;

                    return Err(Error::NotReady {
                        reason: "PodIdentityAssociationCanaryFailed".into(),
                        message: "Canary pod did not receive Pod Identity credentials; PIA webhook may need to catch up".into(),
                        // job watch will cover this
                        requeue_after: None,
                    });
                }
                _ => {}
            }
        }
    }

    // if we are here then the job hasn't succeeded or failed yet; lets try and figure things out a bit quicker
    // because it takes times for pods to schedule etc

    let pods = pod_api
        .list(&ListParams::default().labels(&format!(
            "batch.kubernetes.io/job-name={name},batch.kubernetes.io/controller-uid={}",
            created.metadata.uid.unwrap()
        )))
        .await?;

    if let Some(pod) = pods.items.first() {
        if pod
            .spec
            .as_ref()
            .and_then(|s| s.volumes.as_ref())
            .map(|vs| vs.iter().any(|v| v.name == "eks-pod-identity-token"))
            .unwrap_or(false)
        {
            debug!(
                "PodIdentityAssociation canary check succeeded via pod lookup in namespace {}",
                namespace
            );
            return Ok(());
        }

        debug!(
            "PodIdentityAssociation canary check failed via pod lookup in namespace {}, deleting Job",
            namespace
        );
        delete_job(namespace, job_api, name).await?;

        return Err(Error::NotReady {
            reason: "PodIdentityAssociationCanaryFailed".into(),
            message: "Canary pod did not receive Pod Identity credentials; PIA webhook may need to catch up".into(),
            // job watch will cover this
            requeue_after: None,
        });
    }

    // no pods; we generally expect this immediately after creating the job
    debug!(
        "PodIdentityAssociation canary Job not yet succeeded in namespace {}",
        namespace
    );

    Err(Error::NotReady {
        reason: "PodIdentityAssociationCanaryPending".into(),
        message: "Canary Job has not yet succeeded; PIA webhook may need to catch up".into(),
        // job watch will cover this
        requeue_after: None,
    })
}

fn is_pod_identity_association_synced(pia: PodIdentityAssociation) -> bool {
    if let Some(status) = pia.status {
        if let Some(conditions) = status.conditions {
            if let Some(synced) = conditions
                .iter()
                .find(|cond| cond.r#type == "ACK.ResourceSynced")
            {
                if synced.status == "True" {
                    return true;
                }
            }
        }
    }
    false
}

async fn delete_pod_identity_association(
    namespace: &str,
    pia_api: &Api<PodIdentityAssociation>,
    name: &str,
) -> Result<(), Error> {
    debug!(
        "Ensuring PodIdentityAssociation {} in namespace {} does not exist",
        name, namespace
    );
    match pia_api.delete(name, &DeleteParams::default()).await {
        Err(kube::Error::Api(kube::error::ErrorResponse { code: 404, .. })) => Ok(()),
        Err(err) => Err(err.into()),
        Ok(_) => Ok(()),
    }
}

async fn delete_job(namespace: &str, job_api: &Api<Job>, name: &str) -> Result<(), Error> {
    debug!(
        "Ensuring Job {} in namespace {} does not exist",
        name, namespace
    );
    match job_api.delete(name, &DeleteParams::default()).await {
        Err(kube::Error::Api(kube::error::ErrorResponse { code: 404, .. })) => Ok(()),
        Err(err) => Err(err.into()),
        Ok(_) => Ok(()),
    }
}

async fn apply_security_group_policy(
    namespace: &str,
    pia_api: &Api<SecurityGroupPolicy>,
    pia: SecurityGroupPolicy,
) -> Result<(), Error> {
    let name = pia.metadata.name.as_ref().unwrap();
    let params: PatchParams = PatchParams::apply("restate-operator").force();
    debug!(
        "Applying SecurityGroupPolicy {} in namespace {}",
        name, namespace
    );
    pia_api.patch(name, &params, &Patch::Apply(&pia)).await?;
    Ok(())
}

async fn delete_security_group_policy(
    namespace: &str,
    sgp_api: &Api<SecurityGroupPolicy>,
    name: &str,
) -> Result<(), Error> {
    debug!(
        "Ensuring SecurityGroupPolicy {} in namespace {} does not exist",
        name, namespace
    );
    match sgp_api.delete(name, &DeleteParams::default()).await {
        Err(kube::Error::Api(kube::error::ErrorResponse { code: 404, .. })) => Ok(()),
        Err(err) => Err(err.into()),
        Ok(_) => Ok(()),
    }
}

async fn change_statefulset_storage(
    namespace: &str,
    base_metadata: &ObjectMeta,
    ss_api: &Api<StatefulSet>,
    ss_store: &Store<StatefulSet>,
    pvc_api: &Api<PersistentVolumeClaim>,
    pvc_meta_store: &Store<PartialObjectMeta<PersistentVolumeClaim>>,
    storage: &RestateClusterStorage,
) -> Result<(), Error> {
    let params: PatchParams = PatchParams::apply("restate-operator").force();
    let resources = Some(restate_pvc_resources(storage));

    // ensure all existing pvcs have the right size and VAC set
    // first, filter the pvc meta store for our label selector
    let labels = mandatory_labels(base_metadata);
    let pvcs = pvc_meta_store.state().into_iter().filter(|pvc_meta| {
        for (k, v) in &labels {
            if pvc_meta.labels().get(k) != Some(v) {
                return false;
            }
        }
        true
    });

    for pvc in pvcs {
        let name = pvc.name_any();
        debug!(
            "Applying PersistentVolumeClaim {} in namespace {}",
            name, namespace
        );

        let mut patched_spec = PersistentVolumeClaimSpec {
            resources: resources.clone(),
            ..Default::default()
        };

        // if we don't have a particular vac to set, don't update an existing one as it can be defaulted by the CSI driver
        if let Some(volume_attributes_class_name) = &storage.volume_attributes_class_name {
            patched_spec.volume_attributes_class_name = Some(volume_attributes_class_name.clone());
        }

        let pvc = pvc_api
            .patch(
                &name,
                &params,
                &Patch::Apply(PersistentVolumeClaim {
                    metadata: ObjectMeta {
                        name: Some(name.clone()),
                        ..Default::default()
                    },
                    spec: Some(patched_spec),
                    status: None,
                }),
            )
            .await?;

        if pvc.status.and_then(|s| s.phase).as_deref() != Some("Bound") {
            return Err(Error::NotReady {
                reason: "PersistentVolumeClaimNotBound".into(),
                message: format!(
                    "PersistentVolumeClaim {} is not yet bound to a volume",
                    name
                ),
                requeue_after: None,
            });
        }
    }

    let existing = match ss_store.get(&ObjectRef::new(RESTATE_STATEFULSET_NAME).within(namespace)) {
        Some(existing) => existing,
        // no statefulset in cache; possibilities:
        // 1. first run and it hasn't ever been created => do nothing
        // 3. we deleted it in a previous reconciliation, and the cache reflects this => do nothing
        // 2. it has just been created, but cache doesn't have it yet. we'll reconcile again when it enters cache => do nothing
        None => return Ok(()),
    };

    let existing_storage_spec = existing
        .spec
        .as_ref()
        .and_then(|spec| spec.volume_claim_templates.as_ref())
        .and_then(|templates| templates.first())
        .and_then(|storage| storage.spec.as_ref());

    let existing_storage_request = existing_storage_spec
        .and_then(|spec| spec.resources.as_ref())
        .and_then(|resources| resources.requests.as_ref())
        .and_then(|requests| requests.get("storage").map(|storage| storage.to_bytes()));

    let existing_vac =
        existing_storage_spec.and_then(|spec| spec.volume_attributes_class_name.as_deref());

    match existing_storage_request {
        // check if we can interpret the statefulset as having the same storage request and the same vac
        Some(Ok(Some(bytes)))
            if bytes == storage.storage_request_bytes
                && existing_vac == storage.volume_attributes_class_name.as_deref() =>
        {
            return Ok(())
        }
        _ => {}
    }

    debug!(
        "Deleting (with orphan propagation policy) StatefulSet {} in namespace {}",
        RESTATE_STATEFULSET_NAME, namespace
    );

    // expansion case or vac change case - we would have failed when updating the pvcs if this was a contraction
    // we have already updated the pvcs, we just need to delete and recreate the statefulset
    // we *must* delete with an orphan propagation policy; this means the deletion will *not* cascade down
    // to the pods that this statefulset owns.
    // recreation will happen later in the reconcile loop
    ss_api
        .delete(
            RESTATE_STATEFULSET_NAME,
            &DeleteParams {
                propagation_policy: Some(PropagationPolicy::Orphan),
                grace_period_seconds: Some(0),
                preconditions: Some(Preconditions {
                    // ensure that the ss hasn't changed since we made the above checks
                    uid: existing.uid(),
                    resource_version: None,
                }),
                ..Default::default()
            },
        )
        .await?;

    Ok(())
}

async fn apply_stateful_set(
    namespace: &str,
    ss_api: &Api<StatefulSet>,
    ss: StatefulSet,
) -> Result<StatefulSet, Error> {
    let name = ss.metadata.name.as_ref().unwrap();
    let params: PatchParams = PatchParams::apply("restate-operator").force();
    debug!("Applying Stateful Set {} in namespace {}", name, namespace);
    Ok(ss_api.patch(name, &params, &Patch::Apply(&ss)).await?)
}

fn validate_stateful_set_status(
    status: Option<StatefulSetStatus>,
    expected_replicas: i32,
) -> Result<(), Error> {
    let status = if let Some(status) = status {
        status
    } else {
        return Err(Error::NotReady {
            message: "StatefulSetNoStatus".into(),
            reason: "StatefulSet has no status set; it may have just been created".into(),
            requeue_after: None,
        });
    };

    let StatefulSetStatus {
        replicas,
        ready_replicas,
        ..
    } = status;
    if replicas != expected_replicas {
        return Err(Error::NotReady { reason: "StatefulSetScaling".into(), message: format!("StatefulSet has {replicas} replicas instead of the expected {expected_replicas}; it may be scaling up or down"), requeue_after: None });
    };

    let ready_replicas = ready_replicas.unwrap_or(0);

    if ready_replicas != expected_replicas {
        return Err(Error::NotReady { reason: "StatefulSetPodNotReady".into(), message: format!("StatefulSet has {ready_replicas} ready replicas instead of the expected {expected_replicas}; a pod may not be ready"), requeue_after: None });
    }

    Ok(())
}

async fn apply_pod_disruption_budget(
    namespace: &str,
    pdb_api: &Api<PodDisruptionBudget>,
    pdb: PodDisruptionBudget,
) -> Result<PodDisruptionBudget, Error> {
    let name = pdb.metadata.name.as_ref().unwrap();
    let params: PatchParams = PatchParams::apply("restate-operator").force();
    debug!(
        "Applying Pod Disruption Budget {} in namespace {}",
        name, namespace
    );
    Ok(pdb_api.patch(name, &params, &Patch::Apply(&pdb)).await?)
}
