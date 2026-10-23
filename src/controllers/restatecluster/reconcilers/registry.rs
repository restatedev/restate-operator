//! The workflow registry, where the relay and the ingress mux look the tenants' workflows up,
//! in one of three modes:
//!
//! - `included`: the relay runs it (`59-registry-replication.md`): shards in a bucket, a replica
//!   per relay pod, the lookups in process. The operator only renders `[registry]` into the
//!   relay's config and exposes the UI's read API;
//! - `external`: a URL the user gives (`02-contract.md` §4); nothing deployed;
//! - `operator`: the operator serves it itself, from `RestateDeploymentEndpoint` objects;
//!   nothing deployed in the cluster's namespace, but the UI's read API is exposed by a Service
//!   in the operator's namespace.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::Service;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::Api;

use crate::Error;
use crate::controllers::restatecluster::controller::Context;
use crate::resources::restateclusters::{Multitenancy, ReadApi, RegistryMode, RegistryStatus};

use super::multitenancy::{
    Advertised, Apis, ComponentState, Dns, apply, component_selector, delete_if_exists,
    exposed_service,
};
use super::relay::{REGISTRY_API_PORT, REGISTRY_READ_PORT, RELAY, RelayRegistry};

/// The UI's read API in `included` mode, in front of the relay pods' :8091.
const REGISTRY_READ: &str = "registry-read";
/// The Service in front of the operator's own registry (the Helm chart's, O2).
const OPERATOR_REGISTRY: &str = "restate-operator-registry";

/// The operator's own lookup port (the Helm chart's Service).
const LOOKUP_PORT: u16 = 8090;

/// The read API Service in the operator's namespace, one per cluster.
fn operator_read_api_name(cluster: &str) -> String {
    format!("registry-read-{cluster}")
}

/// Where the relay looks the workflows up, by mode, and the registry's URL for the status (in
/// `included` mode: the registry API on the `relay` Service).
pub(super) fn relay_registry(
    mt: &Multitenancy,
    dns: Dns<'_>,
    cluster: &str,
    operator_namespace: &str,
) -> Result<(RelayRegistry, String), Error> {
    let registry = &mt.workflow_registry;
    match registry.mode {
        RegistryMode::Included => {
            let included = registry
                .included
                .as_ref()
                .ok_or_else(|| missing_block("included"))?;
            Ok((
                RelayRegistry::InRelay {
                    bucket_url: included.bucket_url.clone(),
                    shards: included.shards,
                    read_api: included.read_api.is_some(),
                },
                dns.url(RELAY, REGISTRY_API_PORT),
            ))
        }
        RegistryMode::External => {
            let url = registry
                .external
                .as_ref()
                .map(|e| e.url.clone())
                .ok_or_else(|| missing_block("external"))?;
            Ok((RelayRegistry::Url(url.clone()), url))
        }
        RegistryMode::Operator => {
            let url = format!(
                "http://{OPERATOR_REGISTRY}.{operator_namespace}.svc.{}:{LOOKUP_PORT}/clusters/{cluster}",
                dns.cluster_dns
            );
            Ok((RelayRegistry::Url(url.clone()), url))
        }
    }
}

fn missing_block(mode: &str) -> Error {
    Error::InvalidRestateConfig(format!(
        "spec.multitenancy.workflowRegistry.mode is {mode} but workflowRegistry.{mode} is not set"
    ))
}

/// What the registry's reconcile found.
pub(super) struct RegistryOutcome {
    pub status: RegistryStatus,
    pub state: ComponentState,
    /// What the relay's config says about the registry.
    pub relay: RelayRegistry,
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
    let (relay, url) = relay_registry(mt, dns, name, &ctx.operator_namespace)?;

    let included_read_api = registry
        .included
        .as_ref()
        .and_then(|i| i.read_api.as_ref())
        .filter(|_| mode == RegistryMode::Included);
    if included_read_api.is_none() {
        delete_if_exists(&apis.services, REGISTRY_READ).await?;
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
            // Exposed before the relay renders: Restate advertises its address. The relay's own
            // readiness covers the registry (RelayReady).
            let advertised = match included_read_api {
                Some(read_api) => {
                    let svc = apply(
                        &apis.services,
                        &read_api_service(
                            base_metadata,
                            REGISTRY_READ,
                            read_api,
                            component_selector(base_metadata, RELAY),
                        ),
                    )
                    .await?;
                    advertised_read_api(read_api, &svc)
                }
                None => Advertised::Unexposed,
            };
            (read_api_state(&advertised, REGISTRY_READ), advertised)
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
        relay,
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
        REGISTRY_READ_PORT.into(),
    )
}

fn advertised_read_api(read_api: &ReadApi, svc: &Service) -> Advertised {
    Advertised::of(
        read_api.advertised_url.as_deref(),
        read_api.service.as_ref(),
        svc,
        REGISTRY_READ_PORT,
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

pub(super) async fn cleanup_registry(ctx: &Context, name: &str) -> Result<(), Error> {
    delete_if_exists(&Apis::new(ctx, name).services, REGISTRY_READ).await?;
    let operator_services: Api<Service> =
        Api::namespaced(ctx.client.clone(), &ctx.operator_namespace);
    delete_if_exists(&operator_services, &operator_read_api_name(name)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::restateclusters::{
        ExternalRegistry, IncludedRegistry, OperatorRegistry, RelaySpec, WorkflowRegistry,
    };

    const DNS: Dns<'static> = Dns {
        namespace: "restate",
        cluster_dns: "cluster.local",
    };

    fn multitenancy(registry: WorkflowRegistry) -> Multitenancy {
        Multitenancy {
            relay: RelaySpec {
                image: "relay".into(),
                replicas: None,
                image_pull_policy: None,
                resources: None,
                env: None,
                storage: None,
                service_account_annotations: None,
                ingress_mux: None,
                tunnel: None,
                extra_config: None,
            },
            workflow_registry: registry,
        }
    }

    #[test]
    fn relay_registry_by_mode() {
        let included = multitenancy(WorkflowRegistry {
            mode: RegistryMode::Included,
            included: Some(IncludedRegistry {
                bucket_url: "gs://bucket/registry".into(),
                shards: Some(100),
                read_api: Some(ReadApi::default()),
            }),
            external: None,
            operator: None,
        });
        assert_eq!(
            relay_registry(&included, DNS, "restate", "restate-operator").unwrap(),
            (
                RelayRegistry::InRelay {
                    bucket_url: "gs://bucket/registry".into(),
                    shards: Some(100),
                    read_api: true,
                },
                "http://relay.restate.svc.cluster.local:8090".into()
            )
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
            relay_registry(&external, DNS, "restate", "restate-operator").unwrap(),
            (
                RelayRegistry::Url("https://apps.example/restate-registry/".into()),
                "https://apps.example/restate-registry/".into()
            )
        );

        let operator = multitenancy(WorkflowRegistry {
            mode: RegistryMode::Operator,
            included: None,
            external: None,
            operator: Some(OperatorRegistry::default()),
        });
        let url = "http://restate-operator-registry.restate-operator.svc.cluster.local:8090/clusters/restate";
        assert_eq!(
            relay_registry(&operator, DNS, "restate", "restate-operator").unwrap(),
            (RelayRegistry::Url(url.into()), url.into())
        );

        for mode in [RegistryMode::External, RegistryMode::Included] {
            let broken = multitenancy(WorkflowRegistry {
                mode,
                included: None,
                external: None,
                operator: None,
            });
            assert!(relay_registry(&broken, DNS, "restate", "restate-operator").is_err());
        }
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
