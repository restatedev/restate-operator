use std::collections::{BTreeMap, HashSet};
use std::convert::Into;
use std::path::PathBuf;
use std::time::Duration;

use k8s_openapi::api::apps::v1::{StatefulSet, StatefulSetSpec, StatefulSetStatus};
use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::{
    ConfigMap, ConfigMapVolumeSource, Container, ContainerPort, EmptyDirVolumeSource, EnvVar,
    EnvVarSource, HTTPGetAction, KeyToPath, ObjectFieldSelector, PersistentVolumeClaim,
    PersistentVolumeClaimSpec, Pod, PodSecurityContext, PodSpec, PodTemplateSpec, Probe,
    SeccompProfile, SecretVolumeSource, SecurityContext, Service, ServiceAccount, ServicePort,
    ServiceSpec, Toleration, Volume, VolumeMount, VolumeResourceRequirements,
};
use k8s_openapi::api::policy::v1::{PodDisruptionBudget, PodDisruptionBudgetSpec};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::{DeleteParams, ListParams, Preconditions, PropagationPolicy};
use kube::core::PartialObjectMeta;
use kube::runtime::reflector::{ObjectRef, Store};
use kube::{
    Api, ResourceExt,
    api::{Patch, PatchParams},
};
use sha2::Digest;
use tracing::{debug, error, trace, warn};

use crate::Error;
use crate::controllers::restatecluster::controller::Context;
use crate::resources::iampolicymembers::{
    IAMPolicyMember, IAMPolicyMemberResourceRef, IAMPolicyMemberSpec,
};
use crate::resources::podidentityassociations::{
    PodIdentityAssociation, PodIdentityAssociationSpec,
};
use crate::resources::restateclusters::{
    RestateClusterSpec, RestateClusterStatus, RestateClusterStorage, TrustedCACert,
};
use crate::resources::securitygrouppolicies::{
    SecurityGroupPolicy, SecurityGroupPolicySecurityGroups, SecurityGroupPolicySpec,
};

use super::provisioning;
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
                file_key_ref: None,
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
                file_key_ref: None,
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
                file_key_ref: None,
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

// Debian/Alpine system CA bundle path, read from the canary image's filesystem.
// The default canary image (alpine:3.21) ships this path. If a custom canary image uses
// a different distro (e.g. RHEL uses /etc/pki/tls/certs/ca-bundle.crt), this must be updated.
const SYSTEM_CA_BUNDLE: &str = "/etc/ssl/certs/ca-certificates.crt";
const COMBINED_CA_VOLUME: &str = "combined-ca-certs";
const COMBINED_CA_MOUNT: &str = "/combined-certs";
const COMBINED_CA_BUNDLE: &str = "/combined-certs/ca-certificates.crt";

/// Build volumes and an init container that concatenates system CA certs with custom trusted CAs.
/// Returns (additional_volumes, init_container).
fn trusted_ca_init_container(
    certs: &[TrustedCACert],
    canary_image: &str,
) -> (Vec<Volume>, Container) {
    let mut ca_volumes = Vec::with_capacity(certs.len() + 1);
    let mut init_volume_mounts = Vec::with_capacity(certs.len() + 1);
    let mut cat_sources = vec![SYSTEM_CA_BUNDLE.to_string()];

    for (i, cert) in certs.iter().enumerate() {
        let vol_name = format!("trusted-ca-{i}");
        let mount_path = format!("/trusted-ca-{i}");

        ca_volumes.push(Volume {
            name: vol_name.clone(),
            secret: Some(SecretVolumeSource {
                secret_name: Some(cert.secret_name.clone()),
                items: Some(vec![KeyToPath {
                    key: cert.key.clone(),
                    path: "ca.pem".into(),
                    mode: Some(0o444),
                }]),
                ..Default::default()
            }),
            ..Default::default()
        });

        init_volume_mounts.push(VolumeMount {
            name: vol_name,
            mount_path: mount_path.clone(),
            read_only: Some(true),
            ..Default::default()
        });

        cat_sources.push(format!("{mount_path}/ca.pem"));
    }

    // emptyDir for the combined bundle
    ca_volumes.push(Volume {
        name: COMBINED_CA_VOLUME.into(),
        empty_dir: Some(EmptyDirVolumeSource::default()),
        ..Default::default()
    });

    init_volume_mounts.push(VolumeMount {
        name: COMBINED_CA_VOLUME.into(),
        mount_path: COMBINED_CA_MOUNT.into(),
        ..Default::default()
    });

    let cat_command = format!("cat {} > {COMBINED_CA_BUNDLE}", cat_sources.join(" "));

    let init_container = Container {
        name: "combine-ca-certs".into(),
        image: Some(canary_image.into()),
        command: Some(vec!["sh".into(), "-c".into(), cat_command]),
        volume_mounts: Some(init_volume_mounts),
        security_context: Some(SecurityContext {
            allow_privilege_escalation: Some(false),
            ..Default::default()
        }),
        ..Default::default()
    };

    (ca_volumes, init_container)
}

/// Compute a hash of trusted CA cert references for use as a pod annotation to trigger rollouts.
fn hash_trusted_ca_cert_refs(certs: &[TrustedCACert]) -> String {
    let mut hasher = sha2::Sha256::new();
    for cert in certs {
        hasher.update(cert.secret_name.as_bytes());
        hasher.update(b":");
        hasher.update(cert.key.as_bytes());
        hasher.update(b",");
    }
    format!("{:x}", hasher.finalize())
}

const RESTATE_STATEFULSET_NAME: &str = "restate";

fn restate_statefulset(
    base_metadata: &ObjectMeta,
    spec: &RestateClusterSpec,
    pod_annotations: Option<BTreeMap<String, String>>,
    signing_key: Option<(Volume, PathBuf)>,
    cm_name: String,
    canary_image: &str,
) -> StatefulSet {
    let metadata = object_meta(base_metadata, RESTATE_STATEFULSET_NAME);

    // Merge pod labels: start with user-specified, then apply standard labels on top
    // (standard labels take precedence in case of conflict)
    let labels = {
        let mut merged = spec.compute.labels.clone().unwrap_or_default();
        if let Some(standard) = metadata.labels.clone() {
            merged.extend(standard);
        }
        Some(merged)
    };

    // Merge pod annotations: start with user-specified, then base metadata, then internal
    // (internal annotations like WI/trusted-CA hashes take precedence)
    let pod_annotations = {
        let mut merged = spec.compute.annotations.clone().unwrap_or_default();
        if let Some(base) = metadata.annotations.clone() {
            merged.extend(base);
        }
        if let Some(internal) = pod_annotations {
            merged.extend(internal);
        }
        if merged.is_empty() {
            None
        } else {
            Some(merged)
        }
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

    let trusted_ca_certs = spec
        .security
        .as_ref()
        .and_then(|s| s.trusted_ca_certs.as_deref())
        .unwrap_or_default();

    let init_containers = if !trusted_ca_certs.is_empty() {
        let (ca_volumes, init_container) =
            trusted_ca_init_container(trusted_ca_certs, canary_image);
        volumes.extend(ca_volumes);

        // Mount combined bundle in main container
        volume_mounts.push(VolumeMount {
            name: COMBINED_CA_VOLUME.into(),
            mount_path: COMBINED_CA_MOUNT.into(),
            read_only: Some(true),
            ..Default::default()
        });

        env.push(EnvVar {
            name: "SSL_CERT_FILE".into(),
            value: Some(COMBINED_CA_BUNDLE.into()),
            value_from: None,
        });

        Some(vec![init_container])
    } else {
        None
    };

    StatefulSet {
        metadata,
        spec: Some(StatefulSetSpec {
            replicas: spec.compute.replicas,
            selector: label_selector(base_metadata),
            service_name: Some("restate-cluster".into()),
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
                    init_containers,
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
                    priority_class_name: spec.compute.priority_class_name.clone(),
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
    // name of the RestateCluster is also the namespace
    name: &str,
    base_metadata: &ObjectMeta,
    spec: &RestateClusterSpec,
    status: Option<&RestateClusterStatus>,
    signing_key: Option<(Volume, PathBuf)>,
) -> Result<(), Error> {
    let ss_api: Api<StatefulSet> = Api::namespaced(ctx.client.clone(), name);
    let cm_api: Api<ConfigMap> = Api::namespaced(ctx.client.clone(), name);
    let pvc_api: Api<PersistentVolumeClaim> = Api::namespaced(ctx.client.clone(), name);
    let svc_api: Api<Service> = Api::namespaced(ctx.client.clone(), name);
    let svcacc_api: Api<ServiceAccount> = Api::namespaced(ctx.client.clone(), name);
    let pia_api: Api<PodIdentityAssociation> = Api::namespaced(ctx.client.clone(), name);
    let job_api: Api<Job> = Api::namespaced(ctx.client.clone(), name);
    let pod_api: Api<Pod> = Api::namespaced(ctx.client.clone(), name);
    let sgp_api: Api<SecurityGroupPolicy> = Api::namespaced(ctx.client.clone(), name);
    let ipm_api: Api<IAMPolicyMember> = Api::namespaced(ctx.client.clone(), name);
    let pdb_api: Api<PodDisruptionBudget> = Api::namespaced(ctx.client.clone(), name);

    apply_service_account(
        name,
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
    apply_configmap(name, &cm_api, cm).await?;

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
                name,
                &pia_api,
                restate_pod_identity_association(
                    name,
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
                name,
                base_metadata,
                spec.compute.tolerations.as_ref(),
                &job_api,
                &pod_api,
                &ctx.canary_image,
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
            delete_pod_identity_association(name, &pia_api, "restate").await?;
            delete_job(name, &job_api, "restate-pia-canary").await?;
        }
        (None, Some(aws_pod_identity_association_role_arn)) => {
            warn!(
                "Ignoring AWS pod identity association role ARN {aws_pod_identity_association_role_arn} as the operator is not configured with --aws-pod-identity-association-cluster"
            );
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
                name,
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
            delete_security_group_policy(name, &sgp_api, "restate").await?;
        }
        Some(aws_pod_security_groups) if !aws_pod_security_groups.is_empty() => {
            warn!(
                "Ignoring AWS pod security groups {} as the SecurityGroupPolicy CRD is not installed",
                aws_pod_security_groups.join(",")
            );
        }
        None | Some(_) => {}
    }

    // GCP Workload Identity via Config Connector IAMPolicyMember
    let gcp_sa_email = spec
        .security
        .as_ref()
        .and_then(|s| s.service_account_annotations.as_ref())
        .and_then(|a| a.get("iam.gke.io/gcp-service-account"));

    match (ctx.gcp_workload_identity, gcp_sa_email) {
        (true, Some(gcp_sa_email)) => {
            let gcp_project = match parse_gcp_project_from_sa_email(gcp_sa_email) {
                Some(project) => project,
                None => {
                    return Err(Error::InvalidRestateConfig(format!(
                        "Cannot parse GCP project from service account email '{gcp_sa_email}'; expected format: name@PROJECT.iam.gserviceaccount.com"
                    )));
                }
            };

            let ipm = match apply_iam_policy_member(
                name,
                &ipm_api,
                restate_iam_policy_member(name, base_metadata, gcp_project, gcp_sa_email),
            )
            .await
            {
                Ok(ipm) => ipm,
                Err(Error::KubeError(kube::Error::Api(kube::error::ErrorResponse {
                    code: 404,
                    ..
                }))) => {
                    return Err(Error::NotReady {
                        reason: "IAMPolicyMemberCRDNotFound".into(),
                        message: "IAMPolicyMember CRD not found - is Config Connector installed?"
                            .into(),
                        requeue_after: Some(Duration::from_secs(60)),
                    });
                }
                Err(err) => return Err(err),
            };

            if !is_iam_policy_member_ready(&ipm) {
                return Err(Error::NotReady {
                    reason: "IAMPolicyMemberNotReady".into(),
                    message:
                        "Waiting for Config Connector to provision the IAM policy member binding"
                            .into(),
                    requeue_after: None,
                });
            }

            check_workload_identity(
                name,
                base_metadata,
                spec.compute.tolerations.as_ref(),
                &job_api,
                &ctx.canary_image,
            )
            .await?;

            let pod_annotations = pod_annotations.get_or_insert_with(Default::default);
            pod_annotations.insert(
                "restate.dev/gcp-service-account".into(),
                gcp_sa_email.to_owned(),
            );
        }
        (true, None) => {
            delete_iam_policy_member(name, &ipm_api, "restate-workload-identity").await?;
            delete_job(name, &job_api, "restate-wi-canary").await?;
        }
        (false, Some(gcp_sa_email)) => {
            warn!(
                "Ignoring iam.gke.io/gcp-service-account annotation {gcp_sa_email} as the operator is not configured with --gcp-workload-identity"
            );
        }
        (false, None) => {}
    }

    // Add pod annotation for trusted CA certs to trigger rollout on change
    if let Some(certs) = spec
        .security
        .as_ref()
        .and_then(|s| s.trusted_ca_certs.as_deref())
        .filter(|c| !c.is_empty())
    {
        let pod_annotations = pod_annotations.get_or_insert_with(Default::default);
        pod_annotations.insert(
            "restate.dev/trusted-ca-certs".into(),
            hash_trusted_ca_cert_refs(certs),
        );
    }

    let restate_service = restate_service(
        base_metadata,
        spec.security
            .as_ref()
            .and_then(|s| s.service_annotations.as_ref()),
    );
    apply_service(name, &svc_api, restate_service).await?;

    let restate_cluster_service = restate_cluster_service(base_metadata);
    apply_service(name, &svc_api, restate_cluster_service).await?;

    apply_pod_disruption_budget(name, &pdb_api, restate_pod_disruption_budget(base_metadata))
        .await?;

    change_statefulset_storage(
        name,
        base_metadata,
        &ss_api,
        &ctx.ss_store,
        &pvc_api,
        &ctx.pvc_meta_store,
        &spec.storage,
    )
    .await?;

    let ss = apply_stateful_set(
        name,
        &ss_api,
        restate_statefulset(
            base_metadata,
            spec,
            pod_annotations,
            signing_key,
            cm_name,
            &ctx.canary_image,
        ),
    )
    .await?;

    // Handle cluster provisioning if enabled
    // This must happen BEFORE validate_stateful_set_status because pods only become Ready
    // after the cluster is provisioned
    let provisioning_enabled = spec
        .cluster
        .as_ref()
        .map(|c| c.auto_provision)
        .unwrap_or(false);

    if provisioning_enabled {
        // Check if we've already cached that provisioning succeeded
        let already_provisioned = status.and_then(|s| s.provisioned).unwrap_or(false);

        if already_provisioned {
            trace!("Cluster already provisioned, skipping provisioning");
        } else {
            // Validate that config or env var has auto-provision = false
            provisioning::validate_config_for_provisioning(
                spec.config.as_deref(),
                spec.compute.env.as_deref(),
            )?;

            // Check if restate-0 pod is Running (not Ready - it becomes Ready after provisioning)
            if !provisioning::is_pod_running(&pod_api, "restate-0").await? {
                return Err(Error::NotReady {
                    reason: "ProvisioningPodNotRunning".into(),
                    message: "Waiting for restate-0 pod to be Running before provisioning".into(),
                    requeue_after: Some(Duration::from_secs(5)),
                });
            }

            // Run gRPC provisioning - returns Ok for both new provisioning AND AlreadyExists
            provisioning::run_provisioning(name, &ctx.cluster_dns).await?;

            // Return Provisioned error to signal that status.provisioned should be set to true
            // and reconciliation should be requeued immediately
            return Err(Error::Provisioned);
        }
    }

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

/// Configuration for a canary job that validates cloud credential injection
struct CanaryConfig {
    /// Job name, e.g. "restate-pia-canary"
    name: &'static str,
    /// Container image to use for the canary pod
    image: String,
    /// Command to run in the canary container
    command: Vec<String>,
    /// Reason prefix for NotReady conditions, e.g. "PodIdentityAssociation"
    reason_prefix: &'static str,
    /// Human-readable description of what failed, for error messages
    failure_message: &'static str,
    /// Human-readable description for pending state
    pending_message: &'static str,
}

/// Build a canary Job spec for cloud credential validation.
fn canary_job_spec(
    base_metadata: &ObjectMeta,
    tolerations: Option<&Vec<Toleration>>,
    config: &CanaryConfig,
) -> Job {
    let mut metadata = object_meta(base_metadata, config.name);
    let labels = metadata.labels.get_or_insert(Default::default());
    if let Some(existing) = labels.get_mut("app.kubernetes.io/name") {
        *existing = config.name.into()
    } else {
        labels.insert("app.kubernetes.io/name".into(), config.name.into());
    }

    Job {
        metadata,
        spec: Some(JobSpec {
            backoff_limit: Some(1),
            template: PodTemplateSpec {
                metadata: None,
                spec: Some(PodSpec {
                    service_account_name: Some("restate".into()),
                    containers: vec![Container {
                        name: "canary".into(),
                        image: Some(config.image.clone()),
                        command: Some(config.command.clone()),
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
    }
}

/// Result of checking canary job conditions.
enum CanaryResult {
    /// Job completed successfully
    Complete,
    /// Job failed
    Failed,
    /// Job hasn't completed yet (no terminal condition)
    Pending,
}

/// Check the conditions on a canary Job to determine its state.
fn check_canary_conditions(job: &Job) -> CanaryResult {
    if let Some(conditions) = job.status.as_ref().and_then(|s| s.conditions.as_ref()) {
        for condition in conditions {
            if condition.status != "True" {
                continue;
            }
            match condition.type_.as_str() {
                "Complete" => return CanaryResult::Complete,
                "Failed" => return CanaryResult::Failed,
                _ => {}
            }
        }
    }
    CanaryResult::Pending
}

/// Shared canary job logic for cloud credential validation.
/// Creates a one-shot Job and checks whether credentials are available.
/// Returns the CanaryResult so callers can distinguish Complete from Pending.
async fn run_canary_job(
    namespace: &str,
    base_metadata: &ObjectMeta,
    tolerations: Option<&Vec<Toleration>>,
    job_api: &Api<Job>,
    config: &CanaryConfig,
) -> Result<CanaryResult, Error> {
    let name = config.name;
    let params: PatchParams = PatchParams::apply("restate-operator").force();
    let job = canary_job_spec(base_metadata, tolerations, config);

    debug!("Applying canary Job {name} in namespace {namespace}");

    let created = match job_api.patch(name, &params, &Patch::Apply(&job)).await {
        Ok(created) => created,
        Err(kube::Error::Api(kube::error::ErrorResponse {
            code: 422,
            reason,
            message,
            ..
        })) if reason == "Invalid" && message.contains("field is immutable") => {
            delete_job(namespace, job_api, name).await?;

            return Err(Error::NotReady {
                reason: format!("{}CanaryPending", config.reason_prefix),
                message: "Canary Job has not yet succeeded; recreated Job after tolerations change"
                    .into(),
                requeue_after: None,
            });
        }
        Err(err) => return Err(err.into()),
    };

    let result = check_canary_conditions(&created);
    match &result {
        CanaryResult::Complete => {
            debug!("Canary {name} succeeded in namespace {namespace}");
        }
        CanaryResult::Failed => {
            error!("Canary {name} failed in namespace {namespace}, deleting Job");
            delete_job(namespace, job_api, name).await?;

            return Err(Error::NotReady {
                reason: format!("{}CanaryFailed", config.reason_prefix),
                message: config.failure_message.into(),
                requeue_after: None,
            });
        }
        CanaryResult::Pending => {
            debug!("Canary {name} not yet succeeded in namespace {namespace}");
        }
    }
    Ok(result)
}

async fn check_pia(
    namespace: &str,
    base_metadata: &ObjectMeta,
    tolerations: Option<&Vec<Toleration>>,
    job_api: &Api<Job>,
    pod_api: &Api<Pod>,
    canary_image: &str,
) -> Result<(), Error> {
    let config = CanaryConfig {
        name: "restate-pia-canary",
        image: canary_image.into(),
        command: vec![
            "grep".into(),
            "-q".into(),
            "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE".into(),
            "/proc/self/environ".into(),
        ],
        reason_prefix: "PodIdentityAssociation",
        failure_message: "Canary pod did not receive Pod Identity credentials; PIA webhook may need to catch up",
        pending_message: "Canary Job has not yet succeeded; PIA webhook may need to catch up",
    };

    match run_canary_job(namespace, base_metadata, tolerations, job_api, &config).await? {
        CanaryResult::Complete => return Ok(()),
        CanaryResult::Failed => unreachable!("run_canary_job returns Err for Failed"),
        CanaryResult::Pending => {}
    }

    // job hasn't completed yet - try the pod volume shortcut before declaring pending.
    // the eks-pod-identity-token volume is visible immediately at pod creation.
    // no need for a controller-uid filter since the job name is unique within the namespace.
    let name = config.name;
    if let Ok(pods) = pod_api
        .list(&ListParams::default().labels(&format!("batch.kubernetes.io/job-name={name}")))
        .await
        && let Some(pod) = pods.items.first()
    {
        if pod
            .spec
            .as_ref()
            .and_then(|s| s.volumes.as_ref())
            .map(|vs| vs.iter().any(|v| v.name == "eks-pod-identity-token"))
            .unwrap_or(false)
        {
            debug!(
                "PodIdentityAssociation canary check succeeded via pod lookup in namespace {namespace}"
            );
            return Ok(());
        }

        debug!(
            "PodIdentityAssociation canary check failed via pod lookup in namespace {namespace}, deleting Job"
        );
        delete_job(namespace, job_api, name).await?;

        return Err(Error::NotReady {
            reason: "PodIdentityAssociationCanaryFailed".into(),
            message: config.failure_message.into(),
            requeue_after: None,
        });
    }

    Err(Error::NotReady {
        reason: "PodIdentityAssociationCanaryPending".into(),
        message: config.pending_message.into(),
        requeue_after: None,
    })
}

fn is_pod_identity_association_synced(pia: PodIdentityAssociation) -> bool {
    if let Some(status) = pia.status
        && let Some(conditions) = status.conditions
        && let Some(synced) = conditions
            .iter()
            .find(|cond| cond.r#type == "ACK.ResourceSynced")
        && synced.status == "True"
    {
        true
    } else {
        false
    }
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

/// Parse the GCP project ID from a service account email.
/// Expected format: `name@PROJECT.iam.gserviceaccount.com`
fn parse_gcp_project_from_sa_email(email: &str) -> Option<&str> {
    email
        .split('@')
        .nth(1)?
        .strip_suffix(".iam.gserviceaccount.com")
}

fn restate_iam_policy_member(
    ns: &str,
    base_metadata: &ObjectMeta,
    gcp_project: &str,
    gcp_service_account_email: &str,
) -> IAMPolicyMember {
    let mut metadata = object_meta(base_metadata, "restate-workload-identity");
    let annotations = metadata.annotations.get_or_insert_with(Default::default);
    annotations.insert(
        "cnrm.cloud.google.com/project-id".into(),
        gcp_project.into(),
    );

    IAMPolicyMember {
        metadata,
        spec: IAMPolicyMemberSpec {
            member: format!("serviceAccount:{gcp_project}.svc.id.goog[{ns}/restate]"),
            role: "roles/iam.workloadIdentityUser".into(),
            resource_ref: IAMPolicyMemberResourceRef {
                kind: "IAMServiceAccount".into(),
                external: Some(format!(
                    "projects/{gcp_project}/serviceAccounts/{gcp_service_account_email}"
                )),
                name: None,
                namespace: None,
            },
        },
        status: None,
    }
}

fn is_iam_policy_member_ready(ipm: &IAMPolicyMember) -> bool {
    ipm.status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .and_then(|cs| cs.iter().find(|c| c.r#type == "Ready"))
        .is_some_and(|c| c.status == "True")
}

async fn apply_iam_policy_member(
    namespace: &str,
    ipm_api: &Api<IAMPolicyMember>,
    ipm: IAMPolicyMember,
) -> Result<IAMPolicyMember, Error> {
    let name = ipm.metadata.name.as_ref().unwrap();
    let params: PatchParams = PatchParams::apply("restate-operator").force();
    debug!(
        "Applying IAMPolicyMember {} in namespace {}",
        name, namespace
    );
    Ok(ipm_api.patch(name, &params, &Patch::Apply(&ipm)).await?)
}

async fn delete_iam_policy_member(
    namespace: &str,
    ipm_api: &Api<IAMPolicyMember>,
    name: &str,
) -> Result<(), Error> {
    debug!(
        "Ensuring IAMPolicyMember {} in namespace {} does not exist",
        name, namespace
    );
    match ipm_api.delete(name, &DeleteParams::default()).await {
        Err(kube::Error::Api(kube::error::ErrorResponse { code: 404, .. })) => Ok(()),
        Err(err) => Err(err.into()),
        Ok(_) => Ok(()),
    }
}

async fn check_workload_identity(
    namespace: &str,
    base_metadata: &ObjectMeta,
    tolerations: Option<&Vec<Toleration>>,
    job_api: &Api<Job>,
    canary_image: &str,
) -> Result<(), Error> {
    let config = CanaryConfig {
        name: "restate-wi-canary",
        image: canary_image.into(),
        command: vec![
            "wget".into(),
            "--header".into(),
            "Metadata-Flavor: Google".into(),
            "-q".into(),
            "-O".into(),
            "/dev/null".into(),
            "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token".into(),
        ],
        reason_prefix: "WorkloadIdentity",
        failure_message: "Canary pod could not obtain a Workload Identity token from the GKE metadata server; check that Workload Identity is enabled and the IAM binding is correct",
        pending_message: "Canary Job has not yet succeeded; Workload Identity may need to propagate",
    };

    match run_canary_job(namespace, base_metadata, tolerations, job_api, &config).await? {
        CanaryResult::Complete => Ok(()),
        CanaryResult::Failed => unreachable!("run_canary_job returns Err for Failed"),
        CanaryResult::Pending => Err(Error::NotReady {
            reason: "WorkloadIdentityCanaryPending".into(),
            message: config.pending_message.into(),
            requeue_after: None,
        }),
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
            return Ok(());
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
        return Err(Error::NotReady {
            reason: "StatefulSetScaling".into(),
            message: format!(
                "StatefulSet has {replicas} replicas instead of the expected {expected_replicas}; it may be scaling up or down"
            ),
            requeue_after: None,
        });
    };

    let ready_replicas = ready_replicas.unwrap_or(0);

    if ready_replicas != expected_replicas {
        return Err(Error::NotReady {
            reason: "StatefulSetPodNotReady".into(),
            message: format!(
                "StatefulSet has {ready_replicas} ready replicas instead of the expected {expected_replicas}; a pod may not be ready"
            ),
            requeue_after: None,
        });
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::iampolicymembers::{IAMPolicyMemberCondition, IAMPolicyMemberStatus};
    use crate::resources::restateclusters::RestateClusterCompute;
    use k8s_openapi::api::batch::v1::{JobCondition, JobStatus};

    #[test]
    fn test_parse_gcp_project_from_sa_email() {
        assert_eq!(
            parse_gcp_project_from_sa_email("restate@my-project.iam.gserviceaccount.com"),
            Some("my-project")
        );
    }

    #[test]
    fn test_parse_gcp_project_from_sa_email_complex_project() {
        assert_eq!(
            parse_gcp_project_from_sa_email("sa@my-org-prod-123.iam.gserviceaccount.com"),
            Some("my-org-prod-123")
        );
    }

    #[test]
    fn test_parse_gcp_project_from_sa_email_no_at() {
        assert_eq!(parse_gcp_project_from_sa_email("not-an-email"), None);
    }

    #[test]
    fn test_parse_gcp_project_from_sa_email_wrong_suffix() {
        assert_eq!(
            parse_gcp_project_from_sa_email("sa@project.example.com"),
            None
        );
    }

    #[test]
    fn test_parse_gcp_project_from_sa_email_empty() {
        assert_eq!(parse_gcp_project_from_sa_email(""), None);
    }

    fn make_ipm_with_conditions(
        conditions: Option<Vec<IAMPolicyMemberCondition>>,
    ) -> IAMPolicyMember {
        IAMPolicyMember {
            metadata: Default::default(),
            spec: IAMPolicyMemberSpec {
                member: "test".into(),
                role: "test".into(),
                resource_ref: IAMPolicyMemberResourceRef {
                    kind: "IAMServiceAccount".into(),
                    external: None,
                    name: None,
                    namespace: None,
                },
            },
            status: Some(IAMPolicyMemberStatus { conditions }),
        }
    }

    #[test]
    fn test_is_iam_policy_member_ready_true() {
        let ipm = make_ipm_with_conditions(Some(vec![IAMPolicyMemberCondition {
            r#type: "Ready".into(),
            status: "True".into(),
            message: None,
            reason: None,
            last_transition_time: None,
        }]));
        assert!(is_iam_policy_member_ready(&ipm));
    }

    #[test]
    fn test_is_iam_policy_member_ready_false() {
        let ipm = make_ipm_with_conditions(Some(vec![IAMPolicyMemberCondition {
            r#type: "Ready".into(),
            status: "False".into(),
            message: Some("error".into()),
            reason: None,
            last_transition_time: None,
        }]));
        assert!(!is_iam_policy_member_ready(&ipm));
    }

    #[test]
    fn test_is_iam_policy_member_ready_no_conditions() {
        let ipm = make_ipm_with_conditions(None);
        assert!(!is_iam_policy_member_ready(&ipm));
    }

    #[test]
    fn test_is_iam_policy_member_ready_no_status() {
        let ipm = IAMPolicyMember {
            metadata: Default::default(),
            spec: IAMPolicyMemberSpec {
                member: "test".into(),
                role: "test".into(),
                resource_ref: IAMPolicyMemberResourceRef {
                    kind: "IAMServiceAccount".into(),
                    external: None,
                    name: None,
                    namespace: None,
                },
            },
            status: None,
        };
        assert!(!is_iam_policy_member_ready(&ipm));
    }

    #[test]
    fn test_is_iam_policy_member_ready_wrong_condition_type() {
        let ipm = make_ipm_with_conditions(Some(vec![IAMPolicyMemberCondition {
            r#type: "Reconciling".into(),
            status: "True".into(),
            message: None,
            reason: None,
            last_transition_time: None,
        }]));
        assert!(!is_iam_policy_member_ready(&ipm));
    }

    // restate_iam_policy_member tests

    fn test_base_metadata() -> ObjectMeta {
        ObjectMeta {
            name: Some("test-cluster".into()),
            namespace: Some("test-ns".into()),
            uid: Some("test-uid".into()),
            ..Default::default()
        }
    }

    #[test]
    fn test_restate_iam_policy_member_member_format() {
        let ipm = restate_iam_policy_member(
            "my-namespace",
            &test_base_metadata(),
            "my-project",
            "restate@my-project.iam.gserviceaccount.com",
        );
        assert_eq!(
            ipm.spec.member,
            "serviceAccount:my-project.svc.id.goog[my-namespace/restate]"
        );
    }

    #[test]
    fn test_restate_iam_policy_member_role() {
        let ipm = restate_iam_policy_member(
            "ns",
            &test_base_metadata(),
            "proj",
            "sa@proj.iam.gserviceaccount.com",
        );
        assert_eq!(ipm.spec.role, "roles/iam.workloadIdentityUser");
    }

    #[test]
    fn test_restate_iam_policy_member_resource_ref() {
        let ipm = restate_iam_policy_member(
            "ns",
            &test_base_metadata(),
            "proj",
            "sa@proj.iam.gserviceaccount.com",
        );
        assert_eq!(ipm.spec.resource_ref.kind, "IAMServiceAccount");
        assert_eq!(
            ipm.spec.resource_ref.external.as_deref(),
            Some("projects/proj/serviceAccounts/sa@proj.iam.gserviceaccount.com")
        );
    }

    #[test]
    fn test_restate_iam_policy_member_project_annotation() {
        let ipm = restate_iam_policy_member(
            "ns",
            &test_base_metadata(),
            "my-project",
            "sa@my-project.iam.gserviceaccount.com",
        );
        assert_eq!(
            ipm.metadata
                .annotations
                .as_ref()
                .and_then(|a| a.get("cnrm.cloud.google.com/project-id"))
                .map(|s| s.as_str()),
            Some("my-project")
        );
    }

    // check_canary_conditions tests

    fn make_job_with_conditions(conditions: Option<Vec<JobCondition>>) -> Job {
        Job {
            metadata: Default::default(),
            spec: None,
            status: conditions.map(|c| JobStatus {
                conditions: Some(c),
                ..Default::default()
            }),
        }
    }

    #[test]
    fn test_check_canary_conditions_complete() {
        let job = make_job_with_conditions(Some(vec![JobCondition {
            type_: "Complete".into(),
            status: "True".into(),
            ..Default::default()
        }]));
        assert!(matches!(
            check_canary_conditions(&job),
            CanaryResult::Complete
        ));
    }

    #[test]
    fn test_check_canary_conditions_failed() {
        let job = make_job_with_conditions(Some(vec![JobCondition {
            type_: "Failed".into(),
            status: "True".into(),
            ..Default::default()
        }]));
        assert!(matches!(
            check_canary_conditions(&job),
            CanaryResult::Failed
        ));
    }

    #[test]
    fn test_check_canary_conditions_pending_no_conditions() {
        let job = make_job_with_conditions(None);
        assert!(matches!(
            check_canary_conditions(&job),
            CanaryResult::Pending
        ));
    }

    #[test]
    fn test_check_canary_conditions_pending_not_true() {
        let job = make_job_with_conditions(Some(vec![JobCondition {
            type_: "Complete".into(),
            status: "False".into(),
            ..Default::default()
        }]));
        assert!(matches!(
            check_canary_conditions(&job),
            CanaryResult::Pending
        ));
    }

    #[test]
    fn test_check_canary_conditions_no_status() {
        let job = Job {
            metadata: Default::default(),
            spec: None,
            status: None,
        };
        assert!(matches!(
            check_canary_conditions(&job),
            CanaryResult::Pending
        ));
    }

    // canary_job_spec tests

    #[test]
    fn test_canary_job_spec_structure() {
        let config = CanaryConfig {
            name: "test-canary",
            image: "my-registry/busybox:latest".into(),
            command: vec!["echo".into(), "hello".into()],
            reason_prefix: "Test",
            failure_message: "test failed",
            pending_message: "test pending",
        };
        let job = canary_job_spec(&test_base_metadata(), None, &config);

        let spec = job.spec.unwrap();
        assert_eq!(spec.backoff_limit, Some(1));

        let pod_spec = spec.template.spec.unwrap();
        assert_eq!(pod_spec.service_account_name.as_deref(), Some("restate"));
        assert_eq!(pod_spec.restart_policy.as_deref(), Some("Never"));
        assert_eq!(pod_spec.containers.len(), 1);

        let container = &pod_spec.containers[0];
        assert_eq!(container.name, "canary");
        assert_eq!(
            container.image.as_deref(),
            Some("my-registry/busybox:latest")
        );
        assert_eq!(
            container.command.as_ref().unwrap(),
            &vec!["echo".to_string(), "hello".to_string()]
        );
    }

    #[test]
    fn test_canary_job_spec_label() {
        let config = CanaryConfig {
            name: "my-canary",
            image: "busybox:uclibc".into(),
            command: vec!["true".into()],
            reason_prefix: "Test",
            failure_message: "",
            pending_message: "",
        };
        let job = canary_job_spec(&test_base_metadata(), None, &config);

        assert_eq!(
            job.metadata
                .labels
                .as_ref()
                .and_then(|l| l.get("app.kubernetes.io/name"))
                .map(|s| s.as_str()),
            Some("my-canary")
        );
    }

    #[test]
    fn test_canary_job_spec_tolerations() {
        let tolerations = vec![Toleration {
            key: Some("node-role".into()),
            operator: Some("Exists".into()),
            ..Default::default()
        }];
        let config = CanaryConfig {
            name: "test-canary",
            image: "busybox:uclibc".into(),
            command: vec!["true".into()],
            reason_prefix: "Test",
            failure_message: "",
            pending_message: "",
        };
        let job = canary_job_spec(&test_base_metadata(), Some(&tolerations), &config);

        let pod_spec = job.spec.unwrap().template.spec.unwrap();
        assert_eq!(pod_spec.tolerations.unwrap().len(), 1);
    }

    #[test]
    fn test_trusted_ca_init_container_single_cert() {
        let certs = vec![TrustedCACert {
            secret_name: "my-ca".into(),
            key: "ca.pem".into(),
        }];
        let (volumes, init) = trusted_ca_init_container(&certs, "busybox:uclibc");

        // 1 secret volume + 1 emptyDir
        assert_eq!(volumes.len(), 2);
        assert_eq!(volumes[0].name, "trusted-ca-0");
        assert!(volumes[0].secret.is_some());
        assert_eq!(volumes[1].name, COMBINED_CA_VOLUME);
        assert!(volumes[1].empty_dir.is_some());

        // init container uses canary image, not restate image
        assert_eq!(init.image.as_deref(), Some("busybox:uclibc"));

        // init container cat command includes system certs and custom cert
        let cmd = &init.command.as_ref().unwrap()[2];
        assert!(cmd.starts_with(&format!("cat {SYSTEM_CA_BUNDLE} /trusted-ca-0/ca.pem >")));

        // volume mount names match volume names
        let mounts = init.volume_mounts.as_ref().unwrap();
        let mount_names: Vec<&str> = mounts.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(mount_names, vec!["trusted-ca-0", COMBINED_CA_VOLUME]);
    }

    #[test]
    fn test_trusted_ca_init_container_multiple_certs() {
        let certs = vec![
            TrustedCACert {
                secret_name: "ca-one".into(),
                key: "cert.pem".into(),
            },
            TrustedCACert {
                secret_name: "ca-two".into(),
                key: "root.pem".into(),
            },
        ];
        let (volumes, init) = trusted_ca_init_container(&certs, "busybox:uclibc");

        // 2 secret volumes + 1 emptyDir
        assert_eq!(volumes.len(), 3);

        let cmd = &init.command.as_ref().unwrap()[2];
        assert!(cmd.contains("/trusted-ca-0/ca.pem"));
        assert!(cmd.contains("/trusted-ca-1/ca.pem"));

        // secret volume references correct secret names and keys
        let secret0 = volumes[0].secret.as_ref().unwrap();
        assert_eq!(secret0.secret_name.as_deref(), Some("ca-one"));
        assert_eq!(secret0.items.as_ref().unwrap()[0].key, "cert.pem");

        let secret1 = volumes[1].secret.as_ref().unwrap();
        assert_eq!(secret1.secret_name.as_deref(), Some("ca-two"));
        assert_eq!(secret1.items.as_ref().unwrap()[0].key, "root.pem");
    }

    fn minimal_spec(
        annotations: Option<BTreeMap<String, String>>,
        labels: Option<BTreeMap<String, String>>,
    ) -> RestateClusterSpec {
        RestateClusterSpec {
            compute: RestateClusterCompute {
                image: "restate:test".into(),
                annotations,
                labels,
                ..Default::default()
            },
            storage: RestateClusterStorage {
                storage_request_bytes: 1_000_000_000,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn test_statefulset_user_annotations_merged_with_internal() {
        let user_annotations = BTreeMap::from([
            (
                "cloud.google.com/compute-class".into(),
                "restate-workload".into(),
            ),
            (
                "restate.dev/trusted-ca-certs".into(),
                "user-should-lose".into(),
            ),
        ]);
        let internal_annotations = BTreeMap::from([(
            "restate.dev/trusted-ca-certs".into(),
            "internal-wins".into(),
        )]);
        let spec = minimal_spec(Some(user_annotations), None);

        let ss = restate_statefulset(
            &test_base_metadata(),
            &spec,
            Some(internal_annotations),
            None,
            "config-abc".into(),
            "busybox:uclibc",
        );
        let pod_annotations = ss
            .spec
            .unwrap()
            .template
            .metadata
            .unwrap()
            .annotations
            .unwrap();

        // User annotation preserved
        assert_eq!(
            pod_annotations
                .get("cloud.google.com/compute-class")
                .unwrap(),
            "restate-workload"
        );
        // Internal annotation wins on conflict
        assert_eq!(
            pod_annotations.get("restate.dev/trusted-ca-certs").unwrap(),
            "internal-wins"
        );
    }

    #[test]
    fn test_statefulset_user_labels_merged_with_standard() {
        let user_labels = BTreeMap::from([
            ("team".into(), "platform".into()),
            ("app.kubernetes.io/name".into(), "user-should-lose".into()),
        ]);
        let spec = minimal_spec(None, Some(user_labels));

        let ss = restate_statefulset(
            &test_base_metadata(),
            &spec,
            None,
            None,
            "config-abc".into(),
            "busybox:uclibc",
        );
        let pod_labels = ss.spec.unwrap().template.metadata.unwrap().labels.unwrap();

        // User label preserved
        assert_eq!(pod_labels.get("team").unwrap(), "platform");
        // Standard label wins on conflict
        assert_eq!(pod_labels.get("app.kubernetes.io/name").unwrap(), "restate");
    }

    #[test]
    fn test_statefulset_no_user_annotations_or_labels() {
        let spec = minimal_spec(None, None);

        let ss = restate_statefulset(
            &test_base_metadata(),
            &spec,
            None,
            None,
            "config-abc".into(),
            "busybox:uclibc",
        );
        let tmpl = ss.spec.unwrap().template.metadata.unwrap();

        // Labels should still have standard labels
        assert!(tmpl.labels.unwrap().contains_key("app.kubernetes.io/name"));
        // Annotations should be None (no user, no base, no internal)
        assert!(tmpl.annotations.is_none());
    }
}
