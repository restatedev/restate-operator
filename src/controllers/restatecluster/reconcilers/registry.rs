//! The workflow registry, where the relay and the ingress mux look the tenants' workflows up
//! (`02-contract.md` §4 at `registry_url`), in one of three modes:
//!
//! - `included`: the operator runs the schema registry (sqlite on a PVC), what cluster-setup's
//!   `k8s/15-schema-registry.yaml` and the registry half of `36-public-lbs.yaml` do by hand;
//! - `external`: a URL the user gives; nothing deployed;
//! - `operator`: the operator serves it itself, from `RestateDeploymentEndpoint` objects;
//!   nothing deployed in the cluster's namespace, but the UI's read API is exposed by a Service
//!   in the operator's namespace.

use std::collections::BTreeMap;

use k8s_openapi::api::apps::v1::{StatefulSet, StatefulSetSpec};
use k8s_openapi::api::core::v1::{
    Container, ContainerPort, EnvVar, PersistentVolumeClaim, PersistentVolumeClaimSpec,
    PodSecurityContext, PodSpec, PodTemplateSpec, Probe, Service, TCPSocketAction, VolumeMount,
    VolumeResourceRequirements,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::Api;

use crate::Error;
use crate::controllers::restatecluster::controller::Context;
use crate::resources::restateclusters::{
    IncludedRegistry, Multitenancy, ReadApi, RegistryMode, RegistryStatus,
};

use super::multitenancy::{
    Advertised, Apis, ComponentState, Dns, apply, component_label_selector, component_labels,
    component_selector, delete_if_exists, exposed_service, statefulset_rollout,
};
use super::object_meta;

pub(super) const SCHEMA_REGISTRY: &str = "schema-registry";
const SCHEMA_REGISTRY_READ: &str = "schema-registry-read";
/// The Service in front of the operator's own registry (the Helm chart's, O2).
const OPERATOR_REGISTRY: &str = "restate-operator-registry";

const LOOKUP_PORT: u16 = 8090;
const READ_API_PORT: u16 = 8091;
const DATA_MOUNT: &str = "/data";

/// The read API Service in the operator's namespace, one per cluster.
fn operator_read_api_name(cluster: &str) -> String {
    format!("registry-read-{cluster}")
}

/// Where the relay looks the workflows up, by mode.
pub(super) fn registry_url(
    mt: &Multitenancy,
    dns: Dns<'_>,
    cluster: &str,
    operator_namespace: &str,
) -> Result<String, Error> {
    let registry = &mt.workflow_registry;
    match registry.mode {
        RegistryMode::Included => Ok(dns.url(SCHEMA_REGISTRY, LOOKUP_PORT)),
        RegistryMode::External => registry
            .external
            .as_ref()
            .map(|e| e.url.clone())
            .ok_or_else(|| missing_block("external")),
        RegistryMode::Operator => Ok(format!(
            "http://{OPERATOR_REGISTRY}.{operator_namespace}.svc.{}:{LOOKUP_PORT}/clusters/{cluster}",
            dns.cluster_dns
        )),
    }
}

fn missing_block(mode: &str) -> Error {
    Error::InvalidRestateConfig(format!(
        "spec.multitenancy.workflowRegistry.mode is {mode} but workflowRegistry.{mode} is not set"
    ))
}

/// The registry: one replica on its sqlite PVC (a StatefulSet's claim template).
fn registry_statefulset(base_metadata: &ObjectMeta, registry: &IncludedRegistry) -> StatefulSet {
    let labels = component_labels(base_metadata, SCHEMA_REGISTRY);

    // :8090 the lookups and the operator API (cluster-internal, no auth); :8091 only the
    // read-only per-scope API (the web UI's). The user's args come after (--readers,
    // --cache-mib…); the registry refuses a flag given twice.
    let mut args: Vec<String> = [
        "--bind",
        &format!("0.0.0.0:{LOOKUP_PORT}"),
        "--public-bind",
        &format!("0.0.0.0:{READ_API_PORT}"),
        "--db",
        &format!("{DATA_MOUNT}/schema-registry.sqlite"),
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    args.extend(registry.args.iter().flatten().cloned());

    StatefulSet {
        metadata: object_meta(base_metadata, SCHEMA_REGISTRY),
        spec: Some(StatefulSetSpec {
            service_name: Some(SCHEMA_REGISTRY.into()),
            replicas: Some(1),
            selector: component_label_selector(base_metadata, SCHEMA_REGISTRY),
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels.clone()),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    security_context: Some(PodSecurityContext {
                        // The image runs as uid 10001; make the PVC writable for it.
                        fs_group: Some(10001),
                        ..Default::default()
                    }),
                    containers: vec![Container {
                        name: SCHEMA_REGISTRY.into(),
                        image: Some(registry.image.clone()),
                        image_pull_policy: registry.image_pull_policy.clone(),
                        args: Some(args),
                        // Not debug: that logs every lookup, a cost under load.
                        env: Some(vec![EnvVar {
                            name: "RUST_LOG".into(),
                            value: Some("info".into()),
                            value_from: None,
                        }]),
                        ports: Some(vec![
                            ContainerPort {
                                name: Some("http".into()),
                                container_port: LOOKUP_PORT.into(),
                                ..Default::default()
                            },
                            ContainerPort {
                                name: Some("public".into()),
                                container_port: READ_API_PORT.into(),
                                ..Default::default()
                            },
                        ]),
                        readiness_probe: Some(Probe {
                            tcp_socket: Some(TCPSocketAction {
                                port: IntOrString::Int(LOOKUP_PORT.into()),
                                ..Default::default()
                            }),
                            period_seconds: Some(3),
                            ..Default::default()
                        }),
                        resources: registry.resources.clone(),
                        volume_mounts: Some(vec![VolumeMount {
                            name: "data".into(),
                            mount_path: DATA_MOUNT.into(),
                            ..Default::default()
                        }]),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
            },
            volume_claim_templates: Some(vec![PersistentVolumeClaim {
                metadata: ObjectMeta {
                    name: Some("data".into()),
                    // not the cluster's mandatory labels: those PVCs get Restate's size
                    labels: Some(labels),
                    ..Default::default()
                },
                spec: Some(PersistentVolumeClaimSpec {
                    storage_class_name: registry.storage.storage_class_name.clone(),
                    access_modes: Some(vec!["ReadWriteOnce".into()]),
                    resources: Some(VolumeResourceRequirements {
                        requests: Some(BTreeMap::from([(
                            "storage".to_owned(),
                            Quantity(registry.storage.storage_request_bytes.to_string()),
                        )])),
                        limits: None,
                    }),
                    ..Default::default()
                }),
                status: None,
            }]),
            ..Default::default()
        }),
        status: None,
    }
}

/// What the registry's reconcile found.
pub(super) struct RegistryOutcome {
    pub status: RegistryStatus,
    pub state: ComponentState,
    /// The read API's public address, for the web UI.
    pub advertised: Advertised,
}

pub(super) async fn reconcile_registry(
    ctx: &Context,
    name: &str,
    base_metadata: &ObjectMeta,
    mt: &Multitenancy,
    dns: Dns<'_>,
) -> Result<RegistryOutcome, Error> {
    let apis = Apis::new(ctx, name);
    let operator_services: Api<Service> =
        Api::namespaced(ctx.client.clone(), &ctx.operator_namespace);
    let registry = &mt.workflow_registry;
    let mode = registry.mode;
    let url = registry_url(mt, dns, name, &ctx.operator_namespace)?;

    if mode != RegistryMode::Included {
        delete_included(&apis).await?;
    }
    if mode != RegistryMode::Operator
        || registry
            .operator
            .as_ref()
            .and_then(|o| o.read_api.as_ref())
            .is_none()
    {
        delete_if_exists(&operator_services, &operator_read_api_name(name)).await?;
    }

    let (state, advertised) = match mode {
        RegistryMode::Included => {
            let included = registry
                .included
                .as_ref()
                .ok_or_else(|| missing_block("included"))?;

            // The exposed read API first: Restate advertises its address.
            let advertised = match included.read_api.as_ref() {
                Some(read_api) => {
                    let svc = apply(
                        &apis.services,
                        &read_api_service(
                            base_metadata,
                            SCHEMA_REGISTRY_READ,
                            read_api,
                            component_selector(base_metadata, SCHEMA_REGISTRY),
                        ),
                    )
                    .await?;
                    advertised_read_api(read_api, &svc)
                }
                None => {
                    delete_if_exists(&apis.services, SCHEMA_REGISTRY_READ).await?;
                    Advertised::Unexposed
                }
            };

            apply(
                &apis.services,
                &exposed_service(
                    base_metadata,
                    SCHEMA_REGISTRY,
                    None,
                    component_selector(base_metadata, SCHEMA_REGISTRY),
                    "http",
                    LOOKUP_PORT.into(),
                ),
            )
            .await?;
            let ss = apply(&apis.sts, &registry_statefulset(base_metadata, included)).await?;
            let (ready, rolled_out) = statefulset_rollout(&ss);

            let state = if !rolled_out {
                ComponentState::waiting(
                    "RegistryRollingOut",
                    format!("{ready}/1 schema registry pods ready on the current spec"),
                )
            } else {
                read_api_state(&advertised, SCHEMA_REGISTRY_READ)
            };
            (state, advertised)
        }
        RegistryMode::External => {
            let advertised = match registry
                .external
                .as_ref()
                .and_then(|e| e.advertised_url.as_ref())
            {
                Some(url) => Advertised::Known(url.clone()),
                None => Advertised::Unexposed,
            };
            (
                ComponentState::ready("ExternalRegistry", format!("the registry at {url}")),
                advertised,
            )
        }
        RegistryMode::Operator => {
            match registry.operator.as_ref().and_then(|o| o.read_api.as_ref()) {
                Some(read_api) => match (&ctx.operator_label_name, &ctx.operator_label_value) {
                    (Some(label_name), Some(label_value)) => {
                        let mut svc = read_api_service(
                            base_metadata,
                            &operator_read_api_name(name),
                            read_api,
                            BTreeMap::from([(label_name.clone(), label_value.clone())]),
                        );
                        svc.metadata.namespace = Some(ctx.operator_namespace.clone());
                        let svc = apply(&operator_services, &svc).await?;
                        let advertised = operator_read_api_base(
                            advertised_read_api(read_api, &svc),
                            read_api,
                            name,
                        );
                        (
                            read_api_state(&advertised, &operator_read_api_name(name)),
                            advertised,
                        )
                    }
                    _ => (
                        ComponentState::waiting(
                            "OperatorLabelUnknown",
                            "the operator's read API needs a Service selecting the \
                                 operator's pods: run the operator with \
                                 OPERATOR_LABEL_NAME/OPERATOR_LABEL_VALUE (the Helm chart sets them)",
                        ),
                        Advertised::Unexposed,
                    ),
                },
                None => (
                    ComponentState::ready("OperatorRegistry", "served by the operator"),
                    Advertised::Unexposed,
                ),
            }
        }
    };

    Ok(RegistryOutcome {
        status: RegistryStatus {
            mode,
            url,
            ready: state.ready,
        },
        state,
        advertised,
    })
}

fn read_api_service(
    base_metadata: &ObjectMeta,
    name: &str,
    read_api: &ReadApi,
    selector: BTreeMap<String, String>,
) -> Service {
    exposed_service(
        base_metadata,
        name,
        read_api.service.as_ref(),
        selector,
        "http",
        READ_API_PORT.into(),
    )
}

fn advertised_read_api(read_api: &ReadApi, svc: &Service) -> Advertised {
    Advertised::of(
        read_api.advertised_url.as_deref(),
        read_api.service.as_ref(),
        svc,
        READ_API_PORT,
    )
}

/// The operator serves every operator-mode cluster's read API on one port, each under
/// `/clusters/<cluster>`: the UI appends `/v1/scopes/{scope}` to what's advertised, so a derived
/// load balancer URL gets the cluster's prefix. An explicit `advertisedUrl` is taken as the full
/// base.
fn operator_read_api_base(advertised: Advertised, read_api: &ReadApi, cluster: &str) -> Advertised {
    match advertised {
        Advertised::Known(url) if read_api.advertised_url.is_none() => {
            Advertised::Known(format!("{}/clusters/{cluster}", url.trim_end_matches('/')))
        }
        other => other,
    }
}

fn read_api_state(advertised: &Advertised, service: &str) -> ComponentState {
    match advertised {
        Advertised::Pending => ComponentState::waiting(
            "ReadApiAddressPending",
            format!("waiting for the {service} LoadBalancer address"),
        ),
        _ => ComponentState::ready("RegistryReady", "the registry is ready"),
    }
}

async fn delete_included(apis: &Apis) -> Result<(), Error> {
    delete_if_exists(&apis.services, SCHEMA_REGISTRY_READ).await?;
    delete_if_exists(&apis.services, SCHEMA_REGISTRY).await?;
    // The PVC (data-schema-registry-0) stays, as StatefulSet PVCs do: going back to `included`
    // finds the catalog again.
    delete_if_exists(&apis.sts, SCHEMA_REGISTRY).await
}

pub(super) async fn cleanup_registry(ctx: &Context, name: &str) -> Result<(), Error> {
    delete_included(&Apis::new(ctx, name)).await?;
    let operator_services: Api<Service> =
        Api::namespaced(ctx.client.clone(), &ctx.operator_namespace);
    delete_if_exists(&operator_services, &operator_read_api_name(name)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controllers::restatecluster::reconcilers::multitenancy::tests::base_metadata;
    use crate::resources::restateclusters::{
        CronSpec, ExternalRegistry, OperatorRegistry, RegistryStorage, RelaySpec, WorkflowRegistry,
    };

    const DNS: Dns<'static> = Dns {
        namespace: "restate",
        cluster_dns: "cluster.local",
    };

    fn included() -> IncludedRegistry {
        IncludedRegistry {
            image: "schema-registry:demo".into(),
            image_pull_policy: None,
            resources: None,
            storage: RegistryStorage {
                storage_class_name: Some("premium-rwo".into()),
                storage_request_bytes: 10737418240,
            },
            args: Some(vec![
                "--readers".into(),
                "4".into(),
                "--cache-mib".into(),
                "512".into(),
            ]),
            read_api: None,
        }
    }

    fn multitenancy(registry: WorkflowRegistry) -> Multitenancy {
        Multitenancy {
            relay: RelaySpec {
                image: "relay".into(),
                replicas: None,
                image_pull_policy: None,
                resources: None,
                env: None,
                ingress_mux: None,
                tunnel: None,
                extra_config: None,
            },
            cron: CronSpec {
                image: "cron".into(),
                replicas: None,
                image_pull_policy: None,
                resources: None,
                env: None,
            },
            workflow_registry: registry,
        }
    }

    #[test]
    fn registry_url_by_mode() {
        let included = multitenancy(WorkflowRegistry {
            mode: RegistryMode::Included,
            included: Some(included()),
            external: None,
            operator: None,
        });
        assert_eq!(
            registry_url(&included, DNS, "restate", "restate-operator").unwrap(),
            "http://schema-registry.restate.svc.cluster.local:8090"
        );

        let external = multitenancy(WorkflowRegistry {
            mode: RegistryMode::External,
            included: None,
            external: Some(ExternalRegistry {
                url: "https://apps.example/restate-registry/".into(),
                advertised_url: None,
            }),
            operator: None,
        });
        assert_eq!(
            registry_url(&external, DNS, "restate", "restate-operator").unwrap(),
            "https://apps.example/restate-registry/"
        );

        let operator = multitenancy(WorkflowRegistry {
            mode: RegistryMode::Operator,
            included: None,
            external: None,
            operator: Some(OperatorRegistry::default()),
        });
        assert_eq!(
            registry_url(&operator, DNS, "restate", "restate-operator").unwrap(),
            "http://restate-operator-registry.restate-operator.svc.cluster.local:8090/clusters/restate"
        );

        let broken = multitenancy(WorkflowRegistry {
            mode: RegistryMode::External,
            included: None,
            external: None,
            operator: None,
        });
        assert!(registry_url(&broken, DNS, "restate", "restate-operator").is_err());
    }

    #[test]
    fn statefulset_matches_cluster_setups_registry() {
        let ss = registry_statefulset(&base_metadata(), &included());
        let spec = ss.spec.unwrap();
        assert_eq!(spec.replicas, Some(1));
        let pod = spec.template.spec.unwrap();
        assert_eq!(pod.security_context.unwrap().fs_group, Some(10001));
        assert_eq!(
            pod.containers[0].args.as_ref().unwrap(),
            &[
                "--bind",
                "0.0.0.0:8090",
                "--public-bind",
                "0.0.0.0:8091",
                "--db",
                "/data/schema-registry.sqlite",
                "--readers",
                "4",
                "--cache-mib",
                "512"
            ]
        );
        let pvc = &spec.volume_claim_templates.unwrap()[0];
        let pvc_spec = pvc.spec.as_ref().unwrap();
        assert_eq!(pvc_spec.storage_class_name.as_deref(), Some("premium-rwo"));
        assert_eq!(
            pvc_spec
                .resources
                .as_ref()
                .unwrap()
                .requests
                .as_ref()
                .unwrap()["storage"],
            Quantity("10737418240".into())
        );
        assert_eq!(
            pvc.metadata.labels.as_ref().unwrap()["app.kubernetes.io/name"],
            "schema-registry"
        );
    }

    #[test]
    fn read_api_state_waits_for_the_load_balancer() {
        assert!(!read_api_state(&Advertised::Pending, "x").ready);
        assert!(read_api_state(&Advertised::Unexposed, "x").ready);
        assert!(read_api_state(&Advertised::Known("http://1.2.3.4:8091".into()), "x").ready);
    }

    #[test]
    fn the_operator_read_api_is_advertised_under_the_cluster_prefix() {
        let derived = ReadApi::default();
        assert_eq!(
            operator_read_api_base(
                Advertised::Known("http://1.2.3.4:8091".into()),
                &derived,
                "restate-op"
            ),
            Advertised::Known("http://1.2.3.4:8091/clusters/restate-op".into())
        );
        let explicit = ReadApi {
            advertised_url: Some("https://ui.example.com/registry".into()),
            ..Default::default()
        };
        assert_eq!(
            operator_read_api_base(
                Advertised::Known("https://ui.example.com/registry".into()),
                &explicit,
                "restate-op"
            ),
            Advertised::Known("https://ui.example.com/registry".into())
        );
    }
}
