use std::collections::{BTreeMap, HashSet};
use std::convert::Into;

use k8s_openapi::api::apps::v1::{StatefulSet, StatefulSetSpec};
use k8s_openapi::api::core::v1::{
    Container, ContainerPort, EnvVar, PersistentVolumeClaim, PersistentVolumeClaimSpec,
    PodSecurityContext, PodSpec, PodTemplateSpec, SeccompProfile, SecurityContext, Service,
    ServiceAccount, ServicePort, ServiceSpec, Volume, VolumeMount, VolumeResourceRequirements,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use kube::api::{DeleteParams, Preconditions, PropagationPolicy};
use kube::core::PartialObjectMeta;
use kube::runtime::reflector::{ObjectRef, Store};
use kube::{
    api::{Patch, PatchParams},
    Api, ResourceExt,
};
use tracing::{debug, warn};

use crate::podidentityassociations::{PodIdentityAssociation, PodIdentityAssociationSpec};
use crate::reconcilers::{label_selector, object_meta, resource_labels};
use crate::{Context, Error, RestateClusterCompute, RestateClusterSpec, RestateClusterStorage};

fn restate_service_account(
    oref: &OwnerReference,
    annotations: Option<&BTreeMap<String, String>>,
) -> ServiceAccount {
    let mut metadata = object_meta(oref, "restate");
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
    oref: &OwnerReference,
    pod_identity_association_cluster: &str,
    pod_identity_association_role_arn: &str,
) -> PodIdentityAssociation {
    PodIdentityAssociation {
        metadata: object_meta(oref, "restate"),
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

fn restate_service(
    oref: &OwnerReference,
    annotations: Option<&BTreeMap<String, String>>,
) -> Service {
    let mut metadata = object_meta(oref, "restate");
    if let Some(annotations) = annotations {
        match &mut metadata.annotations {
            Some(existing_annotations) => {
                existing_annotations.extend(annotations.iter().map(|(k, v)| (k.clone(), v.clone())))
            }
            None => metadata.annotations = Some(annotations.clone()),
        }
    }

    Service {
        metadata: object_meta(oref, "restate"),
        spec: Some(ServiceSpec {
            selector: label_selector(&oref.name).match_labels,
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

fn env(custom: Option<&[EnvVar]>) -> Option<Vec<EnvVar>> {
    let defaults = [
        ("RESTATE_OBSERVABILITY__LOG__FORMAT", "Json"),
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
        Some(defaults.chain(custom.iter().cloned()).collect())
    } else {
        Some(defaults.collect())
    }
}

const RESTATE_STATEFULSET_NAME: &str = "restate";

fn restate_statefulset(
    oref: &OwnerReference,
    compute: &RestateClusterCompute,
    storage: &RestateClusterStorage,
    pod_annotations: Option<BTreeMap<String, String>>,
) -> StatefulSet {
    StatefulSet {
        metadata: object_meta(oref, RESTATE_STATEFULSET_NAME),
        spec: Some(StatefulSetSpec {
            replicas: compute.replicas,
            selector: label_selector(&oref.name),
            service_name: "restate".into(),
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(resource_labels(&oref.name)),
                    annotations: pod_annotations,
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    automount_service_account_token: Some(false),
                    containers: vec![Container {
                        name: "restate".into(),
                        image: Some(compute.image.clone()),
                        image_pull_policy: compute.image_pull_policy.clone(),
                        env: env(compute.env.as_deref()),
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
                        resources: compute.resources.clone(),
                        security_context: Some(SecurityContext {
                            read_only_root_filesystem: Some(true),
                            allow_privilege_escalation: Some(false),
                            ..Default::default()
                        }),
                        volume_mounts: Some(vec![
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
                        ]),
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
                    volumes: Some(vec![Volume {
                        name: "tmp".into(),
                        empty_dir: Some(Default::default()),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
            },
            volume_claim_templates: Some(vec![PersistentVolumeClaim {
                metadata: ObjectMeta {
                    name: Some("storage".into()),
                    labels: Some(resource_labels(&oref.name)),
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
    oref: &OwnerReference,
    spec: &RestateClusterSpec,
) -> Result<(), Error> {
    let ss_api: Api<StatefulSet> = Api::namespaced(ctx.client.clone(), namespace);
    let pvc_api: Api<PersistentVolumeClaim> = Api::namespaced(ctx.client.clone(), namespace);
    let svc_api: Api<Service> = Api::namespaced(ctx.client.clone(), namespace);
    let svcacc_api: Api<ServiceAccount> = Api::namespaced(ctx.client.clone(), namespace);
    let pia_api: Api<PodIdentityAssociation> = Api::namespaced(ctx.client.clone(), namespace);

    apply_service_account(
        namespace,
        &svcacc_api,
        restate_service_account(
            oref,
            spec.security
                .as_ref()
                .and_then(|s| s.service_account_annotations.as_ref()),
        ),
    )
    .await?;

    // Pods MUST roll when these change, so we will apply these parameters as annotations to the pod meta
    let pod_annotations: Option<BTreeMap<String, String>> = match (
        ctx.aws_pod_identity_association_cluster.as_ref(),
        spec.security
            .as_ref()
            .and_then(|s| s.aws_pod_identity_association_role_arn.as_ref()),
    ) {
        (
            Some(aws_pod_identity_association_cluster),
            Some(aws_pod_identity_association_role_arn),
        ) => {
            apply_pod_identity_association(
                namespace,
                &pia_api,
                restate_pod_identity_association(
                    namespace,
                    oref,
                    aws_pod_identity_association_cluster,
                    aws_pod_identity_association_role_arn,
                ),
            )
            .await?;
            Some(BTreeMap::from([
                (
                    "restate.dev/aws-pod-identity-association-cluster".into(),
                    aws_pod_identity_association_cluster.clone(),
                ),
                (
                    "restate.dev/aws-pod-identity-association-role-arn".into(),
                    aws_pod_identity_association_role_arn.clone(),
                ),
            ]))
        }
        (Some(_), None) => {
            delete_pod_identity_association(namespace, &pia_api, "restate").await?;
            None
        }
        (None, Some(aws_pod_identity_association_role_arn)) => {
            warn!("Ignoring AWS pod identity association role ARN {aws_pod_identity_association_role_arn} as the operator is not configured with --aws-pod-identity-association-cluster");
            None
        }
        (None, None) => None,
    };

    apply_service(
        namespace,
        &svc_api,
        restate_service(
            oref,
            spec.security
                .as_ref()
                .and_then(|s| s.service_annotations.as_ref()),
        ),
    )
    .await?;

    resize_statefulset_storage(
        namespace,
        oref,
        &ss_api,
        &ctx.ss_store,
        &pvc_api,
        &ctx.pvc_meta_store,
        &spec.storage,
    )
    .await?;

    apply_stateful_set(
        namespace,
        &ss_api,
        restate_statefulset(oref, &spec.compute, &spec.storage, pod_annotations),
    )
    .await?;

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
) -> Result<(), Error> {
    let name = pia.metadata.name.as_ref().unwrap();
    let params: PatchParams = PatchParams::apply("restate-operator").force();
    debug!(
        "Applying PodIdentityAssociation {} in namespace {}",
        name, namespace
    );
    pia_api.patch(name, &params, &Patch::Apply(&pia)).await?;
    Ok(())
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

async fn resize_statefulset_storage(
    namespace: &str,
    oref: &OwnerReference,
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
    let labels = resource_labels(&oref.name);
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

        pvc_api
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
) -> Result<(), Error> {
    let name = ss.metadata.name.as_ref().unwrap();
    let params: PatchParams = PatchParams::apply("restate-operator").force();
    debug!("Applying Stateful Set {} in namespace {}", name, namespace);
    ss_api.patch(name, &params, &Patch::Apply(&ss)).await?;
    Ok(())
}
