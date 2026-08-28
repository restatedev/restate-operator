use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::StreamExt;
use k8s_openapi::api::core::v1::ObjectReference;
use kube::Resource;
use kube::client::Client;
use kube::runtime::WatchStreamExt;
use kube::runtime::events::{Event, EventType, Recorder};
use kube::runtime::{reflector, watcher};
use serde::Serialize;
use tokio::sync::RwLock;
use tokio::time::sleep;
use tracing::{debug, info, warn};
use url::Url;

use crate::Metrics;
use crate::resources::ReconciliationState;

pub mod restatecloudenvironment;
pub mod restatecluster;
pub mod restatedeployment;

/// How often each controller re-checks for its missing CRD, and how often the aggregate
/// `WaitingForCRD` event is re-published while any are missing.
const CRD_POLL_INTERVAL: Duration = Duration::from_secs(10);

/// Break-glass annotation. Set to `disabled` on a resource and every controller leaves it,
/// and everything it owns, completely alone until the annotation is removed.
pub const RECONCILE_ANNOTATION: &str = "restate.dev/reconcile";

/// The one annotation value that suspends reconciliation.
pub const RECONCILE_DISABLED: &str = "disabled";

/// The condition we write while a resource is suspended. It says in words what
/// [`ReconciliationState`] says as a value, and the normal status writes replace the whole
/// condition list, so it clears itself on resume.
pub const RECONCILING_CONDITION: &str = "Reconciling";

/// Whether reconciliation of this resource is suspended by [`RECONCILE_ANNOTATION`].
///
/// Deletion is deliberately not covered: the annotation suspends management of the resource,
/// not its teardown, so a resource with a deletion timestamp reconciles as normal.
pub fn reconciliation_suspended<K: kube::ResourceExt>(obj: &K) -> bool {
    obj.meta().deletion_timestamp.is_none()
        && obj
            .annotations()
            .get(RECONCILE_ANNOTATION)
            .is_some_and(|value| value == RECONCILE_DISABLED)
}

/// The `Reconciling` condition message for a suspended resource.
pub fn suspended_message() -> String {
    format!(
        "Reconciliation is suspended by the {RECONCILE_ANNOTATION}={RECONCILE_DISABLED} \
         annotation."
    )
}

/// Log a skipped reconcile: once at info when the suspension starts, at debug for as long as
/// it lasts. A resource can sit suspended for days, and every watch resync comes back here.
pub fn log_suspended(kind: &str, name: &str, previous: Option<ReconciliationState>) {
    if previous == Some(ReconciliationState::Disabled) {
        debug!("Skipping {kind} {name}: {}", suspended_message());
    } else {
        info!("Skipping {kind} {name}: {}", suspended_message());
    }
}

/// The state to report after a reconcile that actually ran.
pub fn state_after_reconcile(
    previous: Option<ReconciliationState>,
    ready_status: &str,
) -> ReconciliationState {
    match previous {
        Some(ReconciliationState::Disabled | ReconciliationState::ResumingReconciliation)
            if ready_status != "True" =>
        {
            ReconciliationState::ResumingReconciliation
        }
        _ => ReconciliationState::Reconciling,
    }
}

/// Diagnostics to be exposed by the web server
#[derive(Clone, Serialize)]
pub struct Diagnostics {
    #[serde(deserialize_with = "from_ts")]
    pub last_event: DateTime<Utc>,
}

impl Default for Diagnostics {
    fn default() -> Self {
        Self {
            last_event: Utc::now(),
        }
    }
}

/// The controllers whose startup gates the operator's readiness.
///
/// Until a controller's CRD exists it cannot reconcile anything, so the operator is not
/// meaningfully ready; see [`Readiness`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadinessGate {
    RestateCluster,
    RestateCloudEnvironment,
    RestateDeployment,
}

impl ReadinessGate {
    /// Every gate. The order is the order flags are stored in [`Readiness`], so a gate's
    /// position here must match its discriminant; there is a test for that.
    pub const ALL: [Self; 3] = [
        Self::RestateCluster,
        Self::RestateCloudEnvironment,
        Self::RestateDeployment,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::RestateCluster => "RestateCluster",
            Self::RestateCloudEnvironment => "RestateCloudEnvironment",
            Self::RestateDeployment => "RestateDeployment",
        }
    }
}

/// How far a controller has got towards reconciling.
#[derive(Debug, Clone, PartialEq, Eq)]
enum GateState {
    /// The controller has not reported anything yet.
    Starting,
    /// The controller cannot reconcile until this CRD (`<plural>.<group>`) appears.
    WaitingForCrd(String),
    /// The controller has its CRD and has started reconciling.
    Ready,
}

/// Tracks how far each controller has got towards reconciling.
///
/// This is the single source of truth for two things that would otherwise drift apart: what
/// `/ready` reports, and which CRDs the aggregate `WaitingForCRD` event names. The operator
/// serves `/health` from the moment the web server binds, so on its own that endpoint cannot
/// distinguish "running and reconciling" from "running, but waiting forever for a CRD that is
/// never going to arrive".
#[derive(Debug)]
pub struct Readiness {
    /// One entry per gate, positionally matching [`ReadinessGate::ALL`]. The set of gates is
    /// fixed up front so that `/ready` reports the truth from the first scrape, before any
    /// controller has had a chance to run.
    gates: RwLock<[GateState; ReadinessGate::ALL.len()]>,
}

impl Readiness {
    fn new() -> Self {
        Self {
            gates: RwLock::new(std::array::from_fn(|_| GateState::Starting)),
        }
    }

    /// Records that a controller cannot reconcile until `crd_name` shows up.
    pub async fn mark_waiting_for_crd(&self, gate: ReadinessGate, crd_name: &str) {
        self.gates.write().await[gate as usize] = GateState::WaitingForCrd(crd_name.to_owned());
    }

    /// Records that a controller has its CRD and is about to start reconciling.
    pub async fn mark_ready(&self, gate: ReadinessGate) {
        let mut gates = self.gates.write().await;
        if gates[gate as usize] != GateState::Ready {
            gates[gate as usize] = GateState::Ready;
            info!("{} controller is ready", gate.name());
        }
    }

    /// The CRDs controllers are currently waiting for, in gate order.
    ///
    /// Each controller waits for a different CRD, so these are naturally distinct.
    pub async fn pending_crds(&self) -> Vec<String> {
        self.gates
            .read()
            .await
            .iter()
            .filter_map(|state| match state {
                GateState::WaitingForCrd(crd_name) => Some(crd_name.clone()),
                GateState::Starting | GateState::Ready => None,
            })
            .collect()
    }

    /// A snapshot of readiness, as served by `/ready`.
    pub async fn report(&self) -> ReadinessReport {
        let gates = self.gates.read().await;

        let pending_controllers: Vec<&'static str> = ReadinessGate::ALL
            .into_iter()
            .filter(|gate| gates[*gate as usize] != GateState::Ready)
            .map(ReadinessGate::name)
            .collect();

        ReadinessReport {
            ready: pending_controllers.is_empty(),
            pending_controllers,
        }
    }
}

/// The body served by the `/ready` endpoint.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadinessReport {
    /// True once every controller has its CRD and has started reconciling.
    pub ready: bool,
    /// The controllers still waiting for their CRD, so that a failing readiness probe says
    /// which CRD to go and install.
    pub pending_controllers: Vec<&'static str>,
}

/// State shared between the controller and the web server
#[derive(Clone)]
pub struct State {
    /// Diagnostics populated by the reconciler
    pub diagnostics: Arc<RwLock<Diagnostics>>,
    /// Which controllers have started reconciling, served by the web server's `/ready` endpoint
    pub readiness: Arc<Readiness>,
    /// Metrics registry
    pub registry: prometheus::Registry,
    /// If set, watch AWS PodIdentityAssociation resources, and if requested create them against this cluster
    aws_pod_identity_association_cluster: Option<String>,
    /// If true, manage GCP Workload Identity via Config Connector IAMPolicyMember
    gcp_workload_identity: bool,

    /// Our namespace, needed for network policies and reading secrets
    operator_namespace: String,
    /// The name of a label that can select the operator, needed to support the case where restate clusters need to be reached by the operator
    operator_label_name: Option<String>,
    /// The value of the label named operator_label_name that will select the operator, needed to support the case where restate clusters need to be reached by the operator
    operator_label_value: Option<String>,

    /// The default image to use for tunnel client pods
    tunnel_client_default_image: String,

    /// The cluster DNS suffix (e.g. "cluster.local")
    pub cluster_dns: String,

    /// The container image to use for canary jobs
    pub canary_image: String,

    /// A reference to the operator's own pod, used as the target for events that are about
    /// the operator itself rather than about a resource it manages. `None` when the pod name
    /// was not supplied, eg when running outside the cluster.
    operator_object_ref: Option<ObjectReference>,
}

/// State wrapper around the controller outputs for the web server
#[allow(clippy::too_many_arguments)]
impl State {
    pub fn new(
        aws_pod_identity_association_cluster: Option<String>,
        gcp_workload_identity: bool,
        operator_namespace: String,
        operator_label_name: Option<String>,
        operator_label_value: Option<String>,
        tunnel_client_default_image: String,
        cluster_dns: String,
        canary_image: String,
        operator_pod_name: Option<String>,
        operator_pod_uid: Option<String>,
    ) -> Self {
        let operator_object_ref = operator_pod_name.map(|name| ObjectReference {
            api_version: Some("v1".into()),
            kind: Some("Pod".into()),
            name: Some(name),
            namespace: Some(operator_namespace.clone()),
            uid: operator_pod_uid,
            ..Default::default()
        });

        Self {
            diagnostics: Arc::new(RwLock::new(Diagnostics::default())),
            readiness: Arc::new(Readiness::new()),
            registry: prometheus::Registry::default(),
            aws_pod_identity_association_cluster,
            gcp_workload_identity,
            operator_namespace,
            operator_label_name,
            operator_label_value,
            tunnel_client_default_image,
            cluster_dns,
            canary_image,
            operator_object_ref,
        }
    }

    /// Metrics getter
    pub fn metrics(&self) -> Vec<prometheus::proto::MetricFamily> {
        self.registry.gather()
    }

    /// State getter
    pub async fn diagnostics(&self) -> Diagnostics {
        self.diagnostics.read().await.clone()
    }
}

pub fn service_url(
    service_name: &str,
    service_namespace: &str,
    port: i32,
    path: Option<&str>,
    cluster_dns: &str,
) -> Result<Url, url::ParseError> {
    let mut url = Url::parse(&format!(
        "http://{service_name}.{service_namespace}.svc.{cluster_dns}:{port}",
    ))?;

    if let Some(path) = path {
        url.set_path(path)
    }

    Ok(url)
}

/// Creates a pre-warmed reflector stream that can be passed to controller methods.
///
/// This function:
/// 1. Creates a reflector from the given writer and watcher
/// 2. Polls the reflector until the store is ready (pre-warming)
/// 3. Returns the reflector stream ready to be passed to owns_stream() or watches_stream()
///
/// `store` may come from either `store()` or `store_shared()`; readiness is signalled by the
/// writer, which both share. Use `store_shared()` only if something else needs to subscribe.
///
/// Events consumed while pre-warming are not delivered to the controller. That is fine for
/// the streams used here: the RestateDeployment reflector driving the controller emits every
/// object on its initial list, so each one is reconciled once at startup regardless.
pub async fn prewarmed_reflector<K>(
    store: reflector::Store<K>,
    writer: reflector::store::Writer<K>,
    watch_stream: impl futures::Stream<Item = Result<watcher::Event<K>, watcher::Error>>
    + Send
    + 'static,
) -> impl futures::Stream<Item = Result<K, watcher::Error>>
where
    K: Clone + std::fmt::Debug + Send + Sync + 'static,
    K: Resource<DynamicType = ()>,
    K::DynamicType: Eq + std::hash::Hash + Clone + Default,
{
    let kind = K::kind(&()).to_string();

    debug!("Waiting for {} store to sync...", kind);

    let mut stream = reflector(writer, watch_stream)
        .touched_objects()
        .default_backoff()
        .boxed();

    let mut store_ready = std::pin::pin!(store.wait_until_ready());

    loop {
        tokio::select! {
            _ = stream.next() => {},
            ready = &mut store_ready => {
                ready.unwrap_or_else(|_| panic!("{} store failed to sync unexpectedly", kind));
                break
            }
        }
    }

    debug!("{} store ready", kind);

    stream
}

/// Watches for the termination signals (SIGTERM, SIGINT) that mean the process should stop.
///
/// The controller runtimes install their own handlers via `shutdown_on_signal`, but those
/// are only registered once the controller is built; this lets the startup path bail out
/// too, instead of keeping the process alive until the kubelet SIGKILLs it.
///
/// The handlers are registered once, when this is constructed, rather than around each
/// individual wait: recreating the signal streams would leave a brief window with no
/// handler installed, during which a signal would terminate the process outright.
struct ShutdownSignals {
    #[cfg(unix)]
    sigterm: tokio::signal::unix::Signal,
    #[cfg(unix)]
    sigint: tokio::signal::unix::Signal,
}

impl ShutdownSignals {
    fn new() -> Self {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Self {
                sigterm: signal(SignalKind::terminate())
                    .expect("failed to register SIGTERM handler"),
                sigint: signal(SignalKind::interrupt()).expect("failed to register SIGINT handler"),
            }
        }
        #[cfg(not(unix))]
        Self {}
    }

    /// Resolves once a termination signal has been received.
    async fn recv(&mut self) {
        #[cfg(unix)]
        {
            let Self { sigterm, sigint } = self;
            tokio::select! {
                _ = sigterm.recv() => {}
                _ = sigint.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

/// Whether a failure to read the apiserver's discovery information is one that waiting might
/// resolve.
///
/// True for `429` and `5xx` (the apiserver is there but cannot answer right now), for a `404`
/// (the group or version is not served yet), and for any transport failure (it could not be
/// reached at all). Every one of those routinely happens while a cluster is coming up, and
/// exiting fixes none of them — it just turns a wait into a crashloop.
///
/// False for anything that needs someone to change something: a `401` or `403`, a bad
/// kubeconfig, an unparseable response.
///
/// The corollary is that a permanently unreachable apiserver — a wrong CA, say — is waited on
/// rather than crashlooped. That is deliberate now that waiting is not silent: it surfaces as a
/// `NotReady` pod, a `restate_operator_crd_missing` gauge stuck at 1, and a repeated event.
fn is_retryable_discovery_error(error: &kube::Error) -> bool {
    match error {
        kube::Error::Api(resp) => resp.code == 404 || resp.code == 429 || resp.code >= 500,
        kube::Error::Service(_) | kube::Error::HyperError(_) => true,
        _ => false,
    }
}

/// Reads the apiserver's list of API groups, waiting through the failures that waiting can
/// resolve instead of exiting and crashlooping.
///
/// This is how the RestateCluster controller detects which *optional* CRDs are installed, so it
/// runs before [`wait_for_crd`] and has to survive the same cluster-still-coming-up conditions.
///
/// Returns `None` if the process was asked to shut down while waiting.
pub async fn wait_for_api_groups(
    client: &Client,
) -> Result<Option<k8s_openapi::apimachinery::pkg::apis::meta::v1::APIGroupList>, kube::Error> {
    let mut signals = ShutdownSignals::new();

    loop {
        match client.list_api_groups().await {
            Ok(list) => return Ok(Some(list)),
            Err(ref e) if is_retryable_discovery_error(e) => {
                warn!(
                    "Could not list api groups ({e}). Is the apiserver reachable? \
                     Retrying in {CRD_POLL_INTERVAL:?}..."
                );
            }
            Err(e) => return Err(e),
        }

        tokio::select! {
            _ = sleep(CRD_POLL_INTERVAL) => {}
            _ = signals.recv() => {
                info!("Shutting down while waiting to list api groups");
                return Ok(None);
            }
        }
    }
}

/// The outcome of waiting for a CRD to become available.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrdWait {
    /// The CRD is registered and served; the caller may start its controller.
    Available,
    /// A termination signal arrived while waiting; the caller must not start its controller.
    ShuttingDown,
}

/// Polls the apiserver's discovery endpoint until the CRD for the given resource type is
/// registered and served.
///
/// This replaces the previous behaviour of exiting the operator when a required CRD was
/// not yet present, so that the operator can start before the CRDs are applied and pick
/// them up once they appear. Discovery (rather than a `list` of the resource, or a read
/// of the CRD object itself) is used because it is readable without any additional RBAC,
/// and because a resource only appears there once its CRD is established and the version
/// is served.
///
/// These are all treated as "not ready yet, keep waiting": a `404` for the whole group/version,
/// a missing resource within a served group/version, the responses that mean the apiserver
/// cannot answer right now (`429` and `5xx`), and a failure to reach the apiserver at all. Each
/// of those routinely happens while a cluster is coming up, and exiting fixes none of them.
///
/// Anything else — a `401` or `403`, a bad kubeconfig — needs someone to change something, so it
/// is returned as an error for the caller to act on.
///
/// The corollary is that a permanently unreachable apiserver (a wrong CA, say) is waited on
/// rather than crashlooped. That is deliberate: the wait is no longer silent, so it surfaces as
/// a `NotReady` pod, a `restate_operator_crd_missing` gauge stuck at 1, and a repeated event.
///
/// Because a CRD that never arrives would otherwise leave the operator silently idle while
/// still reporting healthy, every failed poll bumps the `restate_operator_crd_missing` gauge
/// and records the wait against `gate`, which is what `/ready` reports and what
/// [`report_pending_crds`] turns into a Kubernetes event.
pub async fn wait_for_crd<K>(
    gate: ReadinessGate,
    client: &Client,
    metrics: &Metrics,
    state: &State,
) -> Result<CrdWait, kube::Error>
where
    K: Resource<DynamicType = ()>,
{
    let api_version = K::api_version(&());
    let plural = K::plural(&());
    let crd_name = format!("{}.{}", plural, K::group(&()));

    let missing = metrics.crd_missing.with_label_values(&[crd_name.as_str()]);
    missing.set(0);

    let mut signals = ShutdownSignals::new();
    let mut waited = false;

    loop {
        let note = match client.list_api_group_resources(&api_version).await {
            Ok(list) if list.resources.iter().any(|r| r.name == plural) => {
                missing.set(0);
                if waited {
                    info!("CRD {crd_name} is now available");
                } else {
                    debug!("CRD {crd_name} is available");
                }
                state.readiness.mark_ready(gate).await;
                return Ok(CrdWait::Available);
            }
            // the group/version is served, but not (yet) this resource
            Ok(_) => format!("CRD {crd_name} is not yet available. Is it installed?"),
            // the whole group/version is missing, so the CRD cannot be there either
            Err(kube::Error::Api(resp)) if resp.code == 404 => format!(
                "API version {api_version} is not yet available, so neither is CRD {crd_name}. Is it installed?"
            ),
            // too busy, unhealthy, or unreachable: all worth another go in a moment
            Err(ref e) if is_retryable_discovery_error(e) => format!(
                "Could not confirm CRD {crd_name} from the apiserver ({e}). Is it reachable?"
            ),
            // anything else needs someone to change something, so let the caller decide what to
            // do rather than waiting for it to fix itself
            Err(e) => return Err(e),
        };

        waited = true;
        missing.set(1);
        warn!("{note} Retrying in {CRD_POLL_INTERVAL:?}...");
        state.readiness.mark_waiting_for_crd(gate, &crd_name).await;

        tokio::select! {
            _ = sleep(CRD_POLL_INTERVAL) => {}
            _ = signals.recv() => {
                info!("Shutting down while waiting for CRD {crd_name}");
                return Ok(CrdWait::ShuttingDown);
            }
        }
    }
}

/// Publishes one aggregate Kubernetes event naming every CRD the controllers are waiting for,
/// until they all have theirs or the process is asked to shut down.
///
/// The controllers wait independently, so emitting from inside [`wait_for_crd`] would produce a
/// separate event series per CRD. Reporting centrally instead means `kubectl describe pod` shows
/// a single line listing everything that is missing.
///
/// The first check happens one interval in, so a CRD that lands shortly after the operator
/// starts — the usual GitOps race, and the whole reason for waiting rather than exiting — does
/// not produce an event at all.
pub async fn report_pending_crds(client: Client, state: State) {
    let recorder = Recorder::new(client, "restate-operator".into());
    let mut signals = ShutdownSignals::new();
    let mut reported_waiting = false;

    loop {
        tokio::select! {
            _ = sleep(CRD_POLL_INTERVAL) => {}
            _ = signals.recv() => return,
        }

        let Some(note) = pending_crds_note(&state.readiness.pending_crds().await) else {
            if state.readiness.report().await.ready {
                // Nothing is waiting and every controller has started, so there is nothing left
                // to report; say so if we ever complained, then stop polling.
                if reported_waiting {
                    publish_crd_wait_event(
                        &recorder,
                        &state,
                        EventType::Normal,
                        "CRDsAvailable",
                        "All CRDs are now available; every controller is reconciling.".to_owned(),
                    )
                    .await;
                }
                return;
            }
            // no controller has reported a missing CRD yet, but they have not all started either
            continue;
        };

        reported_waiting = true;
        publish_crd_wait_event(&recorder, &state, EventType::Warning, "WaitingForCRD", note).await;
    }
}

/// The note for the aggregate `WaitingForCRD` event, or `None` if nothing is being waited for.
fn pending_crds_note(pending: &[String]) -> Option<String> {
    match pending {
        [] => None,
        [crd_name] => Some(format!(
            "Waiting for CRD {crd_name} to be installed before reconciling it."
        )),
        crd_names => Some(format!(
            "Waiting for {} CRDs to be installed before reconciling them: {}.",
            crd_names.len(),
            crd_names.join(", ")
        )),
    }
}

/// Publishes an event about the operator itself, against the operator's own pod.
///
/// This is best effort: if we don't know which pod we are, or the apiserver rejects the
/// event, the log line is all the caller gets. Failing to record an event is never a
/// reason to stop what we were doing.
async fn publish_crd_wait_event(
    recorder: &Recorder,
    state: &State,
    type_: EventType,
    reason: &str,
    note: String,
) {
    let Some(reference) = state.operator_object_ref.as_ref() else {
        return;
    };

    let event = Event {
        type_,
        reason: reason.into(),
        note: Some(note),
        action: "WaitForCRD".into(),
        secondary: None,
    };

    if let Err(e) = recorder.publish(&event, reference).await {
        // Worth a warning rather than a debug: if this is failing, the events an operator
        // would alert on are not being recorded at all.
        warn!("Could not publish {reason} event; {e:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::{
        RECONCILE_ANNOTATION, Readiness, ReadinessGate, pending_crds_note,
        reconciliation_suspended, state_after_reconcile, suspended_message,
    };
    use crate::resources::ReconciliationState;
    use crate::resources::restateclusters::{RestateCluster, RestateClusterSpec};
    use chrono::Utc;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
    use kube::ResourceExt;

    fn annotated(annotations: &[(&str, &str)]) -> RestateCluster {
        let mut rc = RestateCluster::new("cluster", RestateClusterSpec::default());
        rc.annotations_mut().extend(
            annotations
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string())),
        );
        rc
    }

    /// Only `disabled` suspends, so a typo in the annotation can't quietly stop the operator.
    #[test]
    fn only_disabled_suspends_reconciliation() {
        assert!(!reconciliation_suspended(&annotated(&[])));
        assert!(reconciliation_suspended(&annotated(&[(
            RECONCILE_ANNOTATION,
            "disabled"
        )])));

        for value in ["enabled", "", "Disabled", "true", "suspended"] {
            assert!(
                !reconciliation_suspended(&annotated(&[(RECONCILE_ANNOTATION, value)])),
                "{RECONCILE_ANNOTATION}={value} must not suspend reconciliation"
            );
        }
    }

    /// The suspension has to name the annotation, or there's no way to tell what stopped it.
    #[test]
    fn the_suspension_message_names_the_annotation() {
        let message = suspended_message();
        assert!(message.contains(RECONCILE_ANNOTATION), "{message}");
    }

    /// The annotation suspends management, not teardown: a delete goes through regardless.
    #[test]
    fn deletion_is_not_suspended() {
        let mut rc = annotated(&[(RECONCILE_ANNOTATION, "disabled")]);
        assert!(reconciliation_suspended(&rc));

        rc.metadata.deletion_timestamp = Some(Time(Utc::now()));
        assert!(!reconciliation_suspended(&rc));
    }

    /// Resuming lasts until the resource is ready again, not just until the annotation goes.
    #[test]
    fn resuming_lasts_until_the_resource_is_ready_again() {
        use ReconciliationState::*;

        // still converging after the annotation was removed
        assert_eq!(
            state_after_reconcile(Some(Disabled), "False"),
            ResumingReconciliation
        );
        assert_eq!(
            state_after_reconcile(Some(ResumingReconciliation), "Unknown"),
            ResumingReconciliation
        );

        // ready again, so the resume is over
        assert_eq!(
            state_after_reconcile(Some(ResumingReconciliation), "True"),
            Reconciling
        );
        assert_eq!(state_after_reconcile(Some(Disabled), "True"), Reconciling);

        // never suspended, so never resuming, however unready it is
        assert_eq!(state_after_reconcile(None, "False"), Reconciling);
        assert_eq!(
            state_after_reconcile(Some(Reconciling), "False"),
            Reconciling
        );
    }

    /// One event has to describe every controller's wait, so the note is the only place the
    /// missing CRDs get named.
    #[test]
    fn pending_crds_note_names_every_waited_for_crd() {
        assert_eq!(pending_crds_note(&[]), None);

        assert_eq!(
            pending_crds_note(&["restateclusters.restate.dev".to_owned()]).unwrap(),
            "Waiting for CRD restateclusters.restate.dev to be installed before reconciling it."
        );

        assert_eq!(
            pending_crds_note(&[
                "restateclusters.restate.dev".to_owned(),
                "restatedeployments.restate.dev".to_owned(),
            ])
            .unwrap(),
            "Waiting for 2 CRDs to be installed before reconciling them: \
             restateclusters.restate.dev, restatedeployments.restate.dev."
        );
    }

    /// `Readiness` indexes its gates by `gate as usize`, which is only the right slot if each
    /// gate sits at its own discriminant in `ALL`. Reordering one without the other would
    /// silently mark the wrong controller ready.
    #[test]
    fn readiness_gate_indices_match_declaration_order() {
        for (index, gate) in ReadinessGate::ALL.into_iter().enumerate() {
            assert_eq!(
                index, gate as usize,
                "{gate:?} is not at its own index in ALL"
            );
        }
    }

    #[tokio::test]
    async fn readiness_reports_pending_controllers_until_all_gates_are_marked() {
        let readiness = Readiness::new();

        let report = readiness.report().await;
        assert!(!report.ready);
        assert_eq!(
            report.pending_controllers,
            vec![
                "RestateCluster",
                "RestateCloudEnvironment",
                "RestateDeployment"
            ]
        );

        readiness.mark_ready(ReadinessGate::RestateCluster).await;
        // marking twice must not affect the other gates
        readiness.mark_ready(ReadinessGate::RestateCluster).await;
        readiness.mark_ready(ReadinessGate::RestateDeployment).await;

        let report = readiness.report().await;
        assert!(!report.ready);
        assert_eq!(report.pending_controllers, vec!["RestateCloudEnvironment"]);

        readiness
            .mark_ready(ReadinessGate::RestateCloudEnvironment)
            .await;

        let report = readiness.report().await;
        assert!(report.ready);
        assert!(report.pending_controllers.is_empty());
    }

    /// The aggregate event names the CRDs recorded here, so this is what decides whether one
    /// event can describe every controller's wait.
    #[tokio::test]
    async fn pending_crds_lists_only_the_crds_still_being_waited_for() {
        let readiness = Readiness::new();

        // nothing is waiting until a controller says so, even though nothing is ready either
        assert!(readiness.pending_crds().await.is_empty());
        assert!(!readiness.report().await.ready);

        readiness
            .mark_waiting_for_crd(
                ReadinessGate::RestateDeployment,
                "restatedeployments.restate.dev",
            )
            .await;
        readiness
            .mark_waiting_for_crd(ReadinessGate::RestateCluster, "restateclusters.restate.dev")
            .await;

        // in gate order, not the order they were reported
        assert_eq!(
            readiness.pending_crds().await,
            vec![
                "restateclusters.restate.dev",
                "restatedeployments.restate.dev"
            ]
        );

        // re-reporting the same wait must not duplicate it
        readiness
            .mark_waiting_for_crd(ReadinessGate::RestateCluster, "restateclusters.restate.dev")
            .await;
        assert_eq!(readiness.pending_crds().await.len(), 2);

        // becoming ready clears the wait
        readiness.mark_ready(ReadinessGate::RestateCluster).await;
        assert_eq!(
            readiness.pending_crds().await,
            vec!["restatedeployments.restate.dev"]
        );

        readiness.mark_ready(ReadinessGate::RestateDeployment).await;
        assert!(readiness.pending_crds().await.is_empty());
    }
}
