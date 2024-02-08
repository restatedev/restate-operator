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
use kube::api::{DeleteParams, ListParams, PropagationPolicy};
use kube::runtime::events::{Event, EventType, Recorder};
use kube::{
    api::{Patch, PatchParams},
    Api, Client, ResourceExt,
};
use tracing::debug;

use crate::reconcilers::{label_selector, label_selector_string, object_meta, resource_labels};
use crate::{Error, RestateClusterCompute, RestateClusterSpec, RestateClusterStorage};

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

fn restate_service(oref: &OwnerReference) -> Service {
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
            cluster_ip: None, // headless service
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
) -> StatefulSet {
    StatefulSet {
        metadata: object_meta(oref, RESTATE_STATEFULSET_NAME),
        spec: Some(StatefulSetSpec {
            replicas: compute.replicas,
            selector: label_selector(&oref.name),
            service_name: "restate".into(),
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: resource_labels(&oref.name),
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
                    labels: resource_labels(&oref.name),
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
    client: Client,
    recorder: &Recorder,
    namespace: &str,
    oref: &OwnerReference,
    spec: &RestateClusterSpec,
) -> Result<(), Error> {
    let ss_api: Api<StatefulSet> = Api::namespaced(client.clone(), namespace);
    let pvc_api: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), namespace);
    let svc_api: Api<Service> = Api::namespaced(client.clone(), namespace);
    let svcacc_api: Api<ServiceAccount> = Api::namespaced(client, namespace);

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

    apply_service(namespace, &svc_api, restate_service(oref)).await?;

    resize_statefulset_storage(recorder, namespace, oref, &ss_api, &pvc_api, &spec.storage).await?;

    apply_stateful_set(
        namespace,
        &ss_api,
        restate_statefulset(oref, &spec.compute, &spec.storage),
    )
    .await?;

    Ok(())
}

async fn apply_service(namespace: &str, ss_api: &Api<Service>, ss: Service) -> Result<(), Error> {
    let name = ss.metadata.name.as_ref().unwrap();
    let params: PatchParams = PatchParams::apply("restate-cloud").force();
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
    let params: PatchParams = PatchParams::apply("restate-cloud").force();
    debug!(
        "Applying ServiceAccount {} in namespace {}",
        name, namespace
    );
    svcacc_api
        .patch(name, &params, &Patch::Apply(&svcacc))
        .await?;
    Ok(())
}

async fn resize_statefulset_storage(
    recorder: &Recorder,
    namespace: &str,
    oref: &OwnerReference,
    ss_api: &Api<StatefulSet>,
    pvc_api: &Api<PersistentVolumeClaim>,
    storage: &RestateClusterStorage,
) -> Result<(), Error> {
    // ensure all existing pvcs have the right size set
    let pvcs = pvc_api
        .list_metadata(&ListParams {
            label_selector: Some(label_selector_string(label_selector(&oref.name))),
            ..Default::default()
        })
        .await?;

    let params: PatchParams = PatchParams::apply("restate-cloud");
    for pvc in pvcs {
        let name = pvc.name_any();
        debug!(
            "Applying PersistentVolumeCLaim {} in namespace {}",
            name, namespace
        );

        pvc_api
            .patch(
                &name,
                &params,
                &Patch::Apply(PersistentVolumeClaim {
                    metadata: pvc.metadata,
                    spec: Some(PersistentVolumeClaimSpec {
                        resources: Some(restate_pvc_resources(storage)),
                        ..Default::default()
                    }),
                    status: None,
                }),
            )
            .await?;
    }

    let existing = match ss_api.get_opt(RESTATE_STATEFULSET_NAME).await? {
        Some(existing) => existing,
        None => return Ok(()), // no existing statefulset; maybe first run or we deleted it on a previous reconciliation
    };

    let existing_request = match existing
        .spec
        .as_ref()
        .and_then(|spec| spec.volume_claim_templates.as_ref())
        .and_then(|templates| templates.first())
        .and_then(|storage| storage.spec.as_ref())
        .and_then(|spec| spec.resources.as_ref())
        .and_then(|resources| resources.requests.as_ref())
        .and_then(|requests| requests.get("storage"))
        .and_then(|storage| storage.0.parse::<i64>().ok()) // we always set this field as a number (bytes), and its immutable, so its fair to say its still a number
    {
        Some(existing_request) => existing_request,
        None => {
            // the statefulset must have a storage request
            recorder
                .publish(Event {
                    type_: EventType::Warning,
                    reason: "FailedReconcile".into(),
                    note: Some(Error::MalformedStorageRequest.to_string()),
                    action: "ResizeStatefulSet".into(),
                    secondary: None,
                })
                .await?;
            return Err(Error::MalformedStorageRequest)
        },
    };

    if existing_request == storage.storage_request_bytes {
        return Ok(()); // nothing to do
    }

    // expansion case - we would have failed when updating the pvcs if this was a contraction
    // we have already updated the pvcs, we just need to delete and recreate the statefulset
    // we *must* delete with an orphan propagation policy; this means the deletion will *not* cascade down
    // to the pods that this statefulset owns
    ss_api
        .delete(
            RESTATE_STATEFULSET_NAME,
            &DeleteParams {
                propagation_policy: Some(PropagationPolicy::Orphan),
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
    let params: PatchParams = PatchParams::apply("restate-cloud").force();
    debug!("Applying Stateful Set {} in namespace {}", name, namespace);
    ss_api.patch(name, &params, &Patch::Apply(&ss)).await?;
    Ok(())
}
