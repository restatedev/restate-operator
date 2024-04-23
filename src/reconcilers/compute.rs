use std::collections::{BTreeMap, HashSet};
use std::convert::Into;
use std::path::PathBuf;
use std::time::Duration;

use k8s_openapi::api::apps::v1::{StatefulSet, StatefulSetSpec, StatefulSetStatus};
use k8s_openapi::api::core::v1::{
    Container, ContainerPort, EnvVar, HTTPGetAction, PersistentVolumeClaim,
    PersistentVolumeClaimSpec, Pod, PodSecurityContext, PodSpec, PodTemplateSpec, Probe,
    SeccompProfile, SecurityContext, Service, ServiceAccount, ServicePort, ServiceSpec, Volume,
    VolumeMount, VolumeResourceRequirements,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::{DeleteParams, Preconditions, PropagationPolicy};
use kube::core::PartialObjectMeta;
use kube::runtime::reflector::{ObjectRef, Store};
use kube::{
    api::{Patch, PatchParams},
    Api, ResourceExt,
};
use tracing::{debug, warn};

use crate::podidentityassociations::{PodIdentityAssociation, PodIdentityAssociationSpec};
use crate::reconcilers::{label_selector, mandatory_labels, object_meta};
use crate::securitygrouppolicies::{
    SecurityGroupPolicy, SecurityGroupPolicySecurityGroups, SecurityGroupPolicySpec,
};
use crate::{Context, Error, RestateClusterCompute, RestateClusterSpec, RestateClusterStorage};

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
        metadata: object_meta(base_metadata, "restate"),
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
            cluster_ip: Some("None".into()), // headless service
            ..Default::default()
        }),
        status: None,
    }
}

fn env(cluster_name: &str, custom: Option<&[EnvVar]>) -> Vec<EnvVar> {
    let defaults = [
        ("RESTATE_OBSERVABILITY__LOG__FORMAT", "Json"), // todo: old env var can be removed soon
        ("RESTATE_LOG_FORMAT", "json"),
        ("RESTATE_CLUSTER_NAME", cluster_name),
        ("RESTATE_BASE_DIR", "/target"),
        ("RUST_LOG", "info,restate=debug"),
        ("RUST_BACKTRACE", "1"),
        ("RUST_LIB_BACKTRACE", "0"),
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

    if let Some(custom) = custom {
        defaults.chain(custom.iter().cloned()).collect()
    } else {
        defaults.collect()
    }
}

const RESTATE_STATEFULSET_NAME: &str = "restate";

fn restate_statefulset(
    base_metadata: &ObjectMeta,
    compute: &RestateClusterCompute,
    storage: &RestateClusterStorage,
    pod_annotations: Option<BTreeMap<String, String>>,
    signing_key: Option<(Volume, PathBuf)>,
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
            mount_path: "/target".into(),
            ..Default::default()
        },
        VolumeMount {
            name: "tmp".into(),
            mount_path: "/tmp".into(),
            ..Default::default()
        },
    ];

    let mut volumes = vec![Volume {
        name: "tmp".into(),
        empty_dir: Some(Default::default()),
        ..Default::default()
    }];

    let mut env = env(
        base_metadata.name.as_ref().unwrap().as_str(),
        compute.env.as_deref(),
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
            name: "RESTATE_REQUEST_SIGNING_PRIVATE_KEY_PEM_FILE".into(),
            value: Some(absolute_path.to_str().unwrap().into()),
            value_from: None,
        })
    }

    StatefulSet {
        metadata,
        spec: Some(StatefulSetSpec {
            replicas: compute.replicas,
            selector: label_selector(base_metadata),
            service_name: "restate".into(),
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels,
                    annotations: pod_annotations,
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    automount_service_account_token: Some(false),
                    dns_policy: compute.dns_policy.clone(),
                    dns_config: compute.dns_config.clone(),
                    containers: vec![Container {
                        name: "restate".into(),
                        image: Some(compute.image.clone()),
                        image_pull_policy: compute.image_pull_policy.clone(),
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
                            ..Default::default()
                        }),
                        resources: compute.resources.clone(),
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
                    ..Default::default()
                }),
            },
            volume_claim_templates: Some(vec![PersistentVolumeClaim {
                metadata: ObjectMeta {
                    name: Some("storage".into()),
                    labels: Some(mandatory_labels(base_metadata)), // caution needed; these cannot be changed
                    ..Default::default()
                },
                spec: Some(PersistentVolumeClaimSpec {
                    storage_class_name: storage.storage_class_name.clone(),
                    access_modes: Some(vec!["ReadWriteOnce".into()]),
                    resources: Some(restate_pvc_resources(storage)),
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
    let pvc_api: Api<PersistentVolumeClaim> = Api::namespaced(ctx.client.clone(), namespace);
    let svc_api: Api<Service> = Api::namespaced(ctx.client.clone(), namespace);
    let svcacc_api: Api<ServiceAccount> = Api::namespaced(ctx.client.clone(), namespace);
    let pia_api: Api<PodIdentityAssociation> = Api::namespaced(ctx.client.clone(), namespace);
    let pod_api: Api<Pod> = Api::namespaced(ctx.client.clone(), namespace);
    let sgp_api: Api<SecurityGroupPolicy> = Api::namespaced(ctx.client.clone(), namespace);

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

            if !check_pia(namespace, base_metadata, &pod_api).await? {
                return Err(Error::NotReady { reason: "PodIdentityAssociationCanaryFailed".into(), message: "Canary pod did not receive Pod Identity credentials; PIA webhook may need to catch up".into(), requeue_after: Some(Duration::from_secs(2)) });
            }

            // Pods MUST roll when these change, so we will apply these parameters as annotations to the pod meta
            let pod_annotations = pod_annotations.get_or_insert_with(Default::default);
            pod_annotations.insert(
                "restate.dev/aws-pod-identity-association-cluster".into(),
                aws_pod_identity_association_cluster.clone(),
            );
            pod_annotations.insert(
                "restate.dev/aws-pod-identity-association-role-arn".into(),
                aws_pod_identity_association_role_arn.clone(),
            );
        }
        (Some(_), None) => {
            delete_pod_identity_association(namespace, &pia_api, "restate").await?;
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

    apply_service(
        namespace,
        &svc_api,
        restate_service(
            base_metadata,
            spec.security
                .as_ref()
                .and_then(|s| s.service_annotations.as_ref()),
        ),
    )
    .await?;

    resize_statefulset_storage(
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
        restate_statefulset(
            base_metadata,
            &spec.compute,
            &spec.storage,
            pod_annotations,
            signing_key,
        ),
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
    pod_api: &Api<Pod>,
) -> Result<bool, Error> {
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
        "Applying PodIdentityAssociation canary Pod in namespace {}",
        namespace
    );

    let created = pod_api
        .patch(
            name,
            &params,
            &Patch::Apply(&Pod {
                metadata: object_meta(base_metadata, name),
                spec: Some(PodSpec {
                    service_account_name: Some("restate".into()),
                    containers: vec![Container {
                        name: "canary".into(),
                        image: Some("hello-world:linux".into()),
                        ..Default::default()
                    }],
                    restart_policy: Some("Never".into()),
                    ..Default::default()
                }),
                status: None,
            }),
        )
        .await?;

    if let Some(spec) = created.spec {
        if let Some(volumes) = spec.volumes {
            if volumes.iter().any(|v| v.name == "eks-pod-identity-token") {
                debug!(
                    "PodIdentityAssociation canary check succeeded in namespace {}",
                    namespace
                );
                // leave pod in place as a signal that we passed the check
                return Ok(true);
            }
        }
    }

    debug!(
        "PodIdentityAssociation canary check failed in namespace {}, deleting canary Pod",
        namespace
    );

    // delete pod to try again next time
    pod_api.delete(name, &Default::default()).await?;

    Ok(false)
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

async fn resize_statefulset_storage(
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

    // ensure all existing pvcs have the right size set
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

        let pvc = pvc_api
            .patch(
                &name,
                &params,
                &Patch::Apply(PersistentVolumeClaim {
                    metadata: ObjectMeta {
                        name: Some(name.clone()),
                        ..Default::default()
                    },
                    spec: Some(PersistentVolumeClaimSpec {
                        resources: resources.clone(),
                        ..Default::default()
                    }),
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

    let existing_resources = existing
        .spec
        .as_ref()
        .and_then(|spec| spec.volume_claim_templates.as_ref())
        .and_then(|templates| templates.first())
        .and_then(|storage| storage.spec.as_ref())
        .and_then(|spec| spec.resources.as_ref());

    if existing_resources == resources.as_ref() {
        return Ok(()); // nothing to do
    }

    // expansion case - we would have failed when updating the pvcs if this was a contraction
    // we have already updated the pvcs, we just need to delete and recreate the statefulset
    // we *must* delete with an orphan propagation policy; this means the deletion will *not* cascade down
    // to the pods that this statefulset owns.
    // recreation will happen later in the reconcile loop
    ss_api
        .delete(
            RESTATE_STATEFULSET_NAME,
            &DeleteParams {
                propagation_policy: Some(PropagationPolicy::Orphan),
                preconditions: Some(Preconditions {
                    // resources are immutable; but if someone deleted and recreated it with different resources,
                    // we don't want to delete it, hence the uid precondition
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
