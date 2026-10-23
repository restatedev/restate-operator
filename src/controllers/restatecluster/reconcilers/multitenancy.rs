//! The multitenant package (`spec.multitenancy`): what the operator runs next to the Restate
//! nodes, in the cluster's namespace. The relay (Restate's single endpoint in preflight mode,
//! the tenants' ingress mux, optionally the tunnel receiver), the cron, and the workflow
//! registry in one of three modes; and the env that points the nodes at them.
//!
//! Each part is reconciled by its own module (`relay`, `cron`, `registry`); this one holds what
//! they share and the order they run in. Like cluster-setup, the exposed Services come first:
//! their addresses are advertised (by Restate to the web UI, by the relay to tunnel clients),
//! so the nodes and the relay are rendered once they're known.

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::time::Duration;

use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{ConfigMap, EnvVar, Service, ServicePort, ServiceSpec};
use k8s_openapi::api::policy::v1::PodDisruptionBudget;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::{DeleteParams, Patch, PatchParams};
use kube::runtime::reflector::ObjectRef;
use kube::{Api, Resource};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tracing::debug;

use crate::Error;
use crate::controllers::restatecluster::controller::Context;
use crate::resources::restateclusters::{
    AdvertisedAddresses, ExposedService, Multitenancy, MultitenancyStatus, RestateCluster,
};

use super::{cron, object_meta, registry, relay};

/// The conditions the package adds to the cluster's status, one per part. `Ready` requires all
/// of them.
pub const RELAY_READY_CONDITION: &str = "RelayReady";
pub const CRON_REGISTERED_CONDITION: &str = "CronRegistered";
pub const REGISTRY_READY_CONDITION: &str = "RegistryReady";
pub const MULTITENANCY_CONDITIONS: [&str; 3] = [
    RELAY_READY_CONDITION,
    CRON_REGISTERED_CONDITION,
    REGISTRY_READY_CONDITION,
];

/// How long to wait before looking again at something that isn't there yet (a load balancer
/// address, a rollout). The watches usually trigger a reconcile sooner.
const REQUEUE: Duration = Duration::from_secs(10);

/// In-cluster DNS names in the cluster's namespace.
#[derive(Clone, Copy)]
pub(super) struct Dns<'a> {
    pub namespace: &'a str,
    pub cluster_dns: &'a str,
}

impl Dns<'_> {
    pub fn host(&self, service: &str) -> String {
        format!("{service}.{}.svc.{}", self.namespace, self.cluster_dns)
    }

    pub fn url(&self, service: &str, port: u16) -> String {
        format!("http://{}:{port}", self.host(service))
    }
}

/// Labels of a component's pods. Deliberately not the cluster's `mandatory_labels`: those
/// select the Restate pods (Services, PDB, network policies) and the Restate PVCs (storage
/// resizing), and none of that must catch the relay's, the cron's or the registry's.
pub(super) fn component_labels(
    base_metadata: &ObjectMeta,
    component: &str,
) -> BTreeMap<String, String> {
    let mut labels = component_selector(base_metadata, component);
    labels.insert("app.kubernetes.io/part-of".into(), "restate".into());
    labels
}

pub(super) fn component_selector(
    base_metadata: &ObjectMeta,
    component: &str,
) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("app.kubernetes.io/name".into(), component.into()),
        (
            "app.kubernetes.io/instance".into(),
            base_metadata.name.clone().unwrap(),
        ),
    ])
}

pub(super) fn component_label_selector(
    base_metadata: &ObjectMeta,
    component: &str,
) -> LabelSelector {
    LabelSelector {
        match_labels: Some(component_selector(base_metadata, component)),
        match_expressions: None,
    }
}

/// A Service exposing one port of a component, as `exposed` says (type, annotations, source
/// ranges; ClusterIP when unset).
pub(super) fn exposed_service(
    base_metadata: &ObjectMeta,
    name: &str,
    exposed: Option<&ExposedService>,
    selector: BTreeMap<String, String>,
    port_name: &str,
    port: i32,
) -> Service {
    let mut metadata = object_meta(base_metadata, name);
    if let Some(annotations) = exposed.and_then(|e| e.annotations.as_ref()) {
        metadata
            .annotations
            .get_or_insert_with(Default::default)
            .extend(annotations.clone());
    }

    Service {
        metadata,
        spec: Some(ServiceSpec {
            type_: exposed.and_then(|e| e.r#type.clone()),
            load_balancer_source_ranges: exposed
                .and_then(|e| e.load_balancer_source_ranges.clone()),
            selector: Some(selector),
            ports: Some(vec![ServicePort {
                name: Some(port_name.into()),
                port,
                target_port: Some(IntOrString::Int(port)),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        status: None,
    }
}

pub(super) fn is_load_balancer(exposed: Option<&ExposedService>) -> bool {
    exposed.and_then(|e| e.r#type.as_deref()) == Some("LoadBalancer")
}

/// The first address the cloud gave a LoadBalancer Service, if any yet.
pub(super) fn load_balancer_address(svc: &Service) -> Option<String> {
    svc.status
        .as_ref()?
        .load_balancer
        .as_ref()?
        .ingress
        .as_ref()?
        .iter()
        .find_map(|ingress| ingress.ip.clone().or_else(|| ingress.hostname.clone()))
}

/// What a component's public address is, for advertising it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Advertised {
    Known(String),
    /// A LoadBalancer without an address yet.
    Pending,
    /// Not exposed outside the cluster, and no URL given: nothing to advertise.
    Unexposed,
}

impl Advertised {
    /// `url` if given, else `http://<the load balancer address>:<port>` for a LoadBalancer.
    pub fn of(
        url: Option<&str>,
        exposed: Option<&ExposedService>,
        svc: &Service,
        port: u16,
    ) -> Self {
        if let Some(url) = url {
            Advertised::Known(url.into())
        } else if is_load_balancer(exposed) {
            match load_balancer_address(svc) {
                Some(address) => Advertised::Known(format!("http://{address}:{port}")),
                None => Advertised::Pending,
            }
        } else {
            Advertised::Unexposed
        }
    }

    pub fn known(&self) -> Option<String> {
        match self {
            Advertised::Known(url) => Some(url.clone()),
            _ => None,
        }
    }
}

/// How far along one part of the package is: what its condition says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentState {
    pub ready: bool,
    pub reason: String,
    pub message: String,
}

impl ComponentState {
    pub(super) fn ready(reason: &str, message: impl Into<String>) -> Self {
        Self {
            ready: true,
            reason: reason.into(),
            message: message.into(),
        }
    }

    pub(super) fn waiting(reason: &str, message: impl Into<String>) -> Self {
        Self {
            ready: false,
            reason: reason.into(),
            message: message.into(),
        }
    }
}

/// What a reconcile of the package found: the status to write, and a condition per part.
#[derive(Debug, Clone)]
pub struct MultitenancyReport {
    pub status: MultitenancyStatus,
    pub relay: ComponentState,
    pub cron: ComponentState,
    pub registry: ComponentState,
    /// The relay revision the cron can be registered at once Restate is Ready: set when the
    /// relay, which serves it, is rolled out (so the registration discovers what it now serves).
    relay_revision: Option<String>,
}

impl MultitenancyReport {
    /// The package's conditions, by type.
    pub fn conditions(&self) -> [(&'static str, &ComponentState); 3] {
        [
            (RELAY_READY_CONDITION, &self.relay),
            (CRON_REGISTERED_CONDITION, &self.cron),
            (REGISTRY_READY_CONDITION, &self.registry),
        ]
    }

    /// `NotReady` naming the parts that aren't ready, if any.
    fn not_ready(&self) -> Option<Error> {
        let waiting: Vec<String> = self
            .conditions()
            .into_iter()
            .filter(|(_, state)| !state.ready)
            .map(|(condition, state)| format!("{condition}: {}", state.message))
            .collect();
        if waiting.is_empty() {
            return None;
        }
        Some(Error::NotReady {
            reason: "MultitenancyNotReady".into(),
            message: format!(
                "Waiting for the multitenancy package: {}",
                waiting.join("; ")
            ),
            requeue_after: Some(REQUEUE),
        })
    }
}

/// v1 runs without network policies: the operator's (deny-all, egress to public IPs, its own
/// 5122, labelled pods) would block Restate -> relay and relay -> the tenants' pods.
pub fn validate_multitenancy(rc: &RestateCluster) -> Result<(), Error> {
    if rc.spec.multitenancy.is_none() {
        return Ok(());
    }
    let disabled = rc
        .spec
        .security
        .as_ref()
        .and_then(|s| s.disable_network_policies)
        .unwrap_or(false);
    if disabled {
        Ok(())
    } else {
        Err(Error::NotReady {
            reason: "NetworkPoliciesUnsupported".into(),
            message: "spec.multitenancy requires spec.security.disableNetworkPolicies: true: \
                      the operator's network policies would block Restate -> relay and \
                      relay -> the tenants' deployments"
                .into(),
            requeue_after: Some(Duration::from_secs(5 * 60)),
        })
    }
}

/// Reconcile everything of the package that doesn't need Restate up: the Services (exposed
/// ones first), the registry, the relay. Returns the report so far and the env
/// for the Restate nodes.
pub async fn reconcile_multitenancy(
    ctx: &Context,
    // name of the RestateCluster is also the namespace
    name: &str,
    base_metadata: &ObjectMeta,
    rc: &RestateCluster,
    mt: &Multitenancy,
) -> Result<(MultitenancyReport, Vec<EnvVar>), Error> {
    let dns = Dns {
        namespace: name,
        cluster_dns: &ctx.cluster_dns,
    };
    let previous = rc.status.as_ref().and_then(|s| s.multitenancy.as_ref());

    let registry = registry::reconcile_registry(ctx, name, base_metadata, mt, dns).await?;
    let relay =
        relay::reconcile_relay(ctx, name, base_metadata, &mt.relay, &registry.relay, dns).await?;
    // The registration carries over until it's checked again, once Restate is Ready.
    let cron_status = previous.and_then(|p| p.cron.clone()).unwrap_or_default();
    let cron_state = cron::cron_state(&cron_status, relay.rolled_out_revision.as_deref());

    let advertised = AdvertisedAddresses {
        ingress_mux: relay.ingress_mux.known(),
        schema_registry: registry.advertised.known(),
        tunnel: relay.tunnel_url.clone(),
    };
    let env = restate_env(dns, &advertised);

    let report = MultitenancyReport {
        status: MultitenancyStatus {
            relay: Some(relay.status),
            cron: Some(cron_status),
            registry: Some(registry.status),
            advertised: Some(advertised),
        },
        relay: relay.state,
        cron: cron_state,
        registry: registry.state,
        relay_revision: relay.rolled_out_revision,
    };

    Ok((report, env))
}

/// What needs Restate up (the cron's registration), then whether the whole package is ready.
pub async fn reconcile_multitenancy_after_compute(
    ctx: &Context,
    name: &str,
    report: &mut MultitenancyReport,
) -> Result<(), Error> {
    if let Some(revision) = report.relay_revision.clone() {
        let dns = Dns {
            namespace: name,
            cluster_dns: &ctx.cluster_dns,
        };
        let cron_status = report.status.cron.get_or_insert_with(Default::default);
        report.cron = cron::register_cron(ctx, dns, &revision, cron_status).await?;
    }

    match report.not_ready() {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// The env of the Restate nodes: preflight mode at the relay, the generic invoke endpoint, and
/// the public addresses the admin API advertises to the web UI (once known).
pub(super) fn restate_env(dns: Dns<'_>, advertised: &AdvertisedAddresses) -> Vec<EnvVar> {
    let var = |name: &str, value: String| EnvVar {
        name: name.into(),
        value: Some(value),
        value_from: None,
    };

    let mut env = vec![
        var(
            "RESTATE_WORKER__INVOKER__SINGLE_ENDPOINT_MODE",
            "preflight".into(),
        ),
        var(
            "RESTATE_WORKER__INVOKER__SINGLE_ENDPOINT_ADDRESS",
            format!("{}/_/preflight/", dns.url(relay::RELAY, relay::SENDER_PORT)),
        ),
        var(
            "RESTATE_INGRESS__EXPERIMENTAL_GENERIC_INVOKE",
            "true".into(),
        ),
    ];
    if let Some(mux) = &advertised.ingress_mux {
        env.push(var(
            "RESTATE_ADMIN__EXPERIMENTAL_ADVERTISED_INGRESS_MUX_ENDPOINT",
            mux.clone(),
        ));
    }
    if let Some(registry) = &advertised.schema_registry {
        env.push(var(
            "RESTATE_ADMIN__EXPERIMENTAL_ADVERTISED_SCHEMA_REGISTRY_ENDPOINT",
            registry.clone(),
        ));
    }
    env
}

/// `spec.multitenancy` was removed: delete what it created. Only when there's something to
/// delete (the relay StatefulSet is in the cache), so that clusters without
/// the package don't pay a dozen deletes per reconcile. The relay StatefulSet goes last, so a
/// cleanup that fails halfway is tried again.
pub async fn cleanup_multitenancy(ctx: &Context, name: &str) -> Result<(), Error> {
    let in_cache = |sts: &str| {
        ctx.ss_store
            .get(&ObjectRef::new(sts).within(name))
            .is_some()
    };
    if !in_cache(relay::RELAY) {
        return Ok(());
    }

    debug!("Removing the multitenancy package from namespace {name}");
    registry::cleanup_registry(ctx, name).await?;
    relay::cleanup_relay(ctx, name).await?;
    Ok(())
}

pub(super) async fn apply<K>(api: &Api<K>, obj: &K) -> Result<K, Error>
where
    K: Resource<DynamicType = ()> + Clone + DeserializeOwned + Serialize + Debug,
{
    let name = obj.meta().name.as_deref().unwrap();
    debug!(
        "Applying {} {} in namespace {}",
        K::kind(&()),
        name,
        obj.meta().namespace.as_deref().unwrap_or_default()
    );
    let params = PatchParams::apply("restate-operator").force();
    Ok(api.patch(name, &params, &Patch::Apply(obj)).await?)
}

pub(super) async fn delete_if_exists<K>(api: &Api<K>, name: &str) -> Result<(), Error>
where
    K: Resource<DynamicType = ()> + Clone + DeserializeOwned + Debug,
{
    debug!("Ensuring {} {} does not exist", K::kind(&()), name);
    match api.delete(name, &DeleteParams::background()).await {
        Err(kube::Error::Api(kube::error::ErrorResponse { code: 404, .. })) => Ok(()),
        Err(err) => Err(err.into()),
        Ok(_) => Ok(()),
    }
}

/// Ready pods of a StatefulSet, and whether it's fully rolled out to its current spec.
pub(super) fn statefulset_rollout(ss: &StatefulSet) -> (i32, bool) {
    let replicas = ss.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1);
    let Some(status) = ss.status.as_ref() else {
        return (0, false);
    };
    let ready = status.ready_replicas.unwrap_or(0);
    let rolled_out = status.observed_generation >= ss.metadata.generation
        && status.replicas == replicas
        && status.updated_replicas == Some(replicas)
        && ready == replicas
        && status.current_revision == status.update_revision;
    (ready, rolled_out)
}

/// The APIs the package's objects live in, in the cluster's namespace.
pub(super) struct Apis {
    pub sts: Api<StatefulSet>,
    pub services: Api<Service>,
    pub configmaps: Api<ConfigMap>,
    pub pdbs: Api<PodDisruptionBudget>,
}

impl Apis {
    pub fn new(ctx: &Context, namespace: &str) -> Self {
        Self {
            sts: Api::namespaced(ctx.client.clone(), namespace),
            services: Api::namespaced(ctx.client.clone(), namespace),
            configmaps: Api::namespaced(ctx.client.clone(), namespace),
            pdbs: Api::namespaced(ctx.client.clone(), namespace),
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use k8s_openapi::api::apps::v1::{StatefulSetSpec, StatefulSetStatus};
    use k8s_openapi::api::core::v1::{LoadBalancerIngress, LoadBalancerStatus, ServiceStatus};

    pub(crate) fn base_metadata() -> ObjectMeta {
        ObjectMeta {
            name: Some("restate".into()),
            labels: Some(BTreeMap::from([("team".into(), "platform".into())])),
            ..Default::default()
        }
    }

    fn env_value<'a>(env: &'a [EnvVar], name: &str) -> Option<&'a str> {
        env.iter()
            .find(|e| e.name == name)
            .and_then(|e| e.value.as_deref())
    }

    #[test]
    fn restate_env_points_at_the_relay() {
        let dns = Dns {
            namespace: "restate",
            cluster_dns: "cluster.local",
        };
        let env = restate_env(dns, &AdvertisedAddresses::default());
        assert_eq!(
            env_value(&env, "RESTATE_WORKER__INVOKER__SINGLE_ENDPOINT_MODE"),
            Some("preflight")
        );
        assert_eq!(
            env_value(&env, "RESTATE_WORKER__INVOKER__SINGLE_ENDPOINT_ADDRESS"),
            Some("http://relay.restate.svc.cluster.local:9080/_/preflight/")
        );
        assert_eq!(
            env_value(&env, "RESTATE_INGRESS__EXPERIMENTAL_GENERIC_INVOKE"),
            Some("true")
        );
        // not known yet: not set
        assert_eq!(
            env_value(
                &env,
                "RESTATE_ADMIN__EXPERIMENTAL_ADVERTISED_INGRESS_MUX_ENDPOINT"
            ),
            None
        );
        assert_eq!(
            env_value(
                &env,
                "RESTATE_ADMIN__EXPERIMENTAL_ADVERTISED_SCHEMA_REGISTRY_ENDPOINT"
            ),
            None
        );

        let env = restate_env(
            dns,
            &AdvertisedAddresses {
                ingress_mux: Some("http://34.1.1.1:8080".into()),
                schema_registry: Some("http://34.2.2.2:8091".into()),
                tunnel: None,
            },
        );
        assert_eq!(
            env_value(
                &env,
                "RESTATE_ADMIN__EXPERIMENTAL_ADVERTISED_INGRESS_MUX_ENDPOINT"
            ),
            Some("http://34.1.1.1:8080")
        );
        assert_eq!(
            env_value(
                &env,
                "RESTATE_ADMIN__EXPERIMENTAL_ADVERTISED_SCHEMA_REGISTRY_ENDPOINT"
            ),
            Some("http://34.2.2.2:8091")
        );
    }

    #[test]
    fn component_labels_do_not_select_restate_pods() {
        let labels = component_labels(&base_metadata(), "relay");
        assert_eq!(
            labels.get("app.kubernetes.io/name").map(String::as_str),
            Some("relay")
        );
        assert_eq!(
            labels.get("app.kubernetes.io/instance").map(String::as_str),
            Some("restate")
        );
        // the cluster's own labels stay on the objects, not on the pods
        assert!(!labels.contains_key("team"));
    }

    fn lb_service(ingress: Option<LoadBalancerIngress>) -> Service {
        Service {
            status: Some(ServiceStatus {
                load_balancer: Some(LoadBalancerStatus {
                    ingress: ingress.map(|i| vec![i]),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn advertised_addresses() {
        let lb = ExposedService {
            r#type: Some("LoadBalancer".into()),
            ..Default::default()
        };
        let with_ip = lb_service(Some(LoadBalancerIngress {
            ip: Some("34.1.2.3".into()),
            ..Default::default()
        }));
        let with_hostname = lb_service(Some(LoadBalancerIngress {
            hostname: Some("lb.example.com".into()),
            ..Default::default()
        }));
        let pending = lb_service(None);

        assert_eq!(
            Advertised::of(None, Some(&lb), &with_ip, 8080),
            Advertised::Known("http://34.1.2.3:8080".into())
        );
        assert_eq!(
            Advertised::of(None, Some(&lb), &with_hostname, 8091),
            Advertised::Known("http://lb.example.com:8091".into())
        );
        assert_eq!(
            Advertised::of(None, Some(&lb), &pending, 8080),
            Advertised::Pending
        );
        assert_eq!(
            Advertised::of(Some("https://mux.example.com"), Some(&lb), &pending, 8080),
            Advertised::Known("https://mux.example.com".into())
        );
        assert_eq!(
            Advertised::of(None, None, &with_ip, 8080),
            Advertised::Unexposed
        );
    }

    #[test]
    fn exposed_service_carries_type_ranges_and_annotations() {
        let exposed = ExposedService {
            r#type: Some("LoadBalancer".into()),
            annotations: Some(BTreeMap::from([(
                "networking.gke.io/load-balancer-type".into(),
                "Internal".into(),
            )])),
            load_balancer_source_ranges: Some(vec!["0.0.0.0/0".into()]),
        };
        let svc = exposed_service(
            &base_metadata(),
            "ingress-mux",
            Some(&exposed),
            component_selector(&base_metadata(), "relay"),
            "mux",
            8080,
        );
        let spec = svc.spec.unwrap();
        assert_eq!(spec.type_.as_deref(), Some("LoadBalancer"));
        assert_eq!(
            spec.load_balancer_source_ranges,
            Some(vec!["0.0.0.0/0".into()])
        );
        assert_eq!(spec.ports.unwrap()[0].port, 8080);
        let labels = svc.metadata.labels.unwrap();
        assert_eq!(labels.get("app.kubernetes.io/name").unwrap(), "restate");
        assert_eq!(
            svc.metadata
                .annotations
                .unwrap()
                .get("networking.gke.io/load-balancer-type")
                .unwrap(),
            "Internal"
        );
    }

    #[test]
    fn statefulset_rollout_requires_the_current_revision_everywhere() {
        let ss = |status: StatefulSetStatus| StatefulSet {
            metadata: ObjectMeta {
                generation: Some(2),
                ..Default::default()
            },
            spec: Some(StatefulSetSpec {
                replicas: Some(3),
                ..Default::default()
            }),
            status: Some(status),
        };
        let done = StatefulSetStatus {
            observed_generation: Some(2),
            replicas: 3,
            ready_replicas: Some(3),
            updated_replicas: Some(3),
            current_revision: Some("relay-b".into()),
            update_revision: Some("relay-b".into()),
            ..Default::default()
        };
        assert_eq!(statefulset_rollout(&ss(done.clone())), (3, true));
        assert_eq!(
            statefulset_rollout(&ss(StatefulSetStatus {
                observed_generation: Some(1),
                ..done.clone()
            })),
            (3, false)
        );
        assert_eq!(
            statefulset_rollout(&ss(StatefulSetStatus {
                updated_replicas: Some(1),
                current_revision: Some("relay-a".into()),
                ..done
            })),
            (3, false)
        );
    }
}
