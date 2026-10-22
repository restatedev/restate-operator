//! The RestateDeploymentEndpoint controller: resolves each endpoint's tenant, discovers it,
//! assigns its revisions and writes what the registry serves into its status.
//!
//! Unlike the other controllers this one keeps no reflector store of the objects it manages:
//! at scale that copy would be the biggest thing in the operator's memory, and the index
//! already holds what lookups need (`56-operator-registry.md`, "The index"). The watch feeds
//! the index and a work queue of keys; a worker fetches the object when it pops its key, so
//! the only per-object state outside the index is the namespace → names map (to re-check a
//! namespace's endpoints when its tenant label changes) and the backoff of failing ones.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use futures::StreamExt;
use k8s_openapi::api::core::v1::Namespace;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use kube::api::{Patch, PatchParams};
use kube::runtime::reflector::{self, ObjectRef, Store, reflector};
use kube::runtime::{WatchStreamExt, metadata_watcher, watcher};
use kube::{Api, Client, Resource, ResourceExt};
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::controllers::{CrdWait, ReadinessGate, ShutdownSignals, State, wait_for_crd};
use crate::registry::Registry;
use crate::registry::manifest::{self, Projection};
use crate::resources::restateclusters::{RegistryMode, RestateCluster};
use crate::resources::restatedeploymentendpoints::{
    CLUSTER_LABEL, DiscoveredDeployment, EndpointPhase, ProjectedService,
    RestateDeploymentEndpoint, RestateDeploymentEndpointStatus, TENANT_LABEL,
    deployment_id_from_uid, is_valid_scope,
};
use crate::{Error, Metrics, Result};

use super::discovery::DiscoveryClient;
use super::gc;

/// The field manager of the endpoint statuses (and the labels stamped on the objects).
const FIELD_MANAGER: &str = "restate-operator";
/// Discoveries in flight at once.
const CONCURRENCY: usize = 32;
/// The largest projection written to a status. Hundreds of handlers fit; beyond this the
/// object would be a burden on etcd and on every watcher.
const MAX_PROJECTION_BYTES: usize = 64 * 1024;
/// How often an endpoint whose cluster isn't (yet) an operator-mode cluster is re-checked.
const CLUSTER_RECHECK: Duration = Duration::from_secs(60);
const BACKOFF_INITIAL: Duration = Duration::from_secs(5);
const BACKOFF_MAX: Duration = Duration::from_secs(300);
/// How long after a new revision the tenant's GC runs (the index has it by then), and how long
/// before a GC that found pinned candidates looks again.
const GC_DELAY: Duration = Duration::from_secs(10);
const GC_RETRY: Duration = Duration::from_secs(600);

type Key = ObjectRef<RestateDeploymentEndpoint>;

struct Context {
    client: Client,
    registry: Registry,
    discovery: DiscoveryClient,
    clusters: Store<RestateCluster>,
    /// Namespace → its `restate.dev/tenant` label, for every namespace.
    tenants: Mutex<HashMap<String, Option<String>>>,
    /// Namespace → the names of its endpoints, so a label change re-checks them.
    endpoints_by_namespace: Mutex<HashMap<String, HashSet<String>>>,
    /// Consecutive failures per endpoint, for the backoff.
    failures: Mutex<HashMap<Key, u32>>,
    queue: Queue,
    /// The (cluster, tenant) GC runs scheduled, so a burst of deploys runs one.
    gc_scheduled: Mutex<HashSet<(String, String)>>,
    http: reqwest::Client,
    cluster_dns: String,
}

/// A deduplicating work queue: a key is queued at most once, and never reconciled twice at
/// the same time (a key popped while it's running goes back to the queue after it).
struct Queue {
    tx: mpsc::UnboundedSender<Key>,
    queued: Mutex<HashSet<Key>>,
    running: Mutex<HashSet<Key>>,
}

impl Queue {
    fn push(&self, key: Key) {
        if self.queued.lock().expect("queue lock").insert(key.clone()) {
            // the receiver lives as long as the controller
            let _ = self.tx.send(key);
        }
    }
}

enum Next {
    Done,
    RequeueAfter(Duration),
    /// A new revision is served: the tenant's GC may have something to collect.
    Served {
        cluster: String,
        tenant: String,
    },
}

/// Run the RestateDeploymentEndpoint controller, until the process is asked to stop.
pub async fn run(client: Client, metrics: Metrics, state: State) {
    match wait_for_crd::<RestateDeploymentEndpoint>(
        ReadinessGate::RestateDeploymentEndpoint,
        &client,
        &metrics,
        &state,
    )
    .await
    {
        Ok(CrdWait::Available) => {}
        Ok(CrdWait::ShuttingDown) => return,
        Err(e) => {
            error!(
                "Could not determine whether the RestateDeploymentEndpoint CRD is installed; {e:?}"
            );
            std::process::exit(1);
        }
    }

    let mut signals = ShutdownSignals::new();
    tokio::select! {
        _ = run_controller(client, state.workflow_registry().clone(), state.cluster_dns.clone()) => {}
        _ = signals.recv() => info!("Shutting down the RestateDeploymentEndpoint controller"),
    }
}

async fn run_controller(client: Client, registry: Registry, cluster_dns: String) {
    let (clusters, cluster_writer) = reflector::store();
    let cluster_stream = reflector(
        cluster_writer,
        watcher(
            Api::<RestateCluster>::all(client.clone()),
            Default::default(),
        ),
    )
    .default_backoff()
    .for_each(|_| futures::future::ready(()));
    tokio::spawn(cluster_stream);
    if clusters.wait_until_ready().await.is_err() {
        error!("The RestateCluster store failed to sync");
        return;
    }

    let (tx, rx) = mpsc::unbounded_channel();
    let ctx = Arc::new(Context {
        client: client.clone(),
        registry,
        discovery: DiscoveryClient::new(),
        clusters,
        tenants: Default::default(),
        endpoints_by_namespace: Default::default(),
        failures: Default::default(),
        queue: Queue {
            tx,
            queued: Default::default(),
            running: Default::default(),
        },
        gc_scheduled: Default::default(),
        http: reqwest::Client::new(),
        cluster_dns,
    });

    // Tenants first: an endpoint can't be reconciled before its namespace's label is known.
    let (namespaces_synced_tx, namespaces_synced) = tokio::sync::oneshot::channel();
    tokio::spawn(watch_namespaces(ctx.clone(), namespaces_synced_tx));
    if namespaces_synced.await.is_err() {
        error!("The Namespace watch ended before it synced");
        return;
    }

    tokio::spawn(watch_endpoints(ctx.clone()));

    tokio_stream_from(rx)
        .for_each_concurrent(CONCURRENCY, |key| {
            let ctx = ctx.clone();
            async move { work(ctx, key).await }
        })
        .await;
}

fn tokio_stream_from(mut rx: mpsc::UnboundedReceiver<Key>) -> impl futures::Stream<Item = Key> {
    futures::stream::poll_fn(move |cx| rx.poll_recv(cx))
}

async fn watch_namespaces(ctx: Arc<Context>, synced: tokio::sync::oneshot::Sender<()>) {
    let mut synced = Some(synced);
    let mut stream = metadata_watcher(
        Api::<Namespace>::all(ctx.client.clone()),
        Default::default(),
    )
    .default_backoff()
    .boxed();
    while let Some(event) = stream.next().await {
        let event = match event {
            Ok(event) => event,
            Err(e) => {
                warn!("Namespace watch error: {e}");
                continue;
            }
        };
        match event {
            watcher::Event::Apply(ns) | watcher::Event::InitApply(ns) => {
                let name = ns.name_any();
                let tenant = ns.labels().get(TENANT_LABEL).cloned();
                let changed = {
                    let mut tenants = ctx.tenants.lock().expect("tenants lock");
                    tenants.insert(name.clone(), tenant.clone()) != Some(tenant)
                };
                if changed && synced.is_none() {
                    requeue_namespace(&ctx, &name);
                }
            }
            watcher::Event::Delete(ns) => {
                ctx.tenants
                    .lock()
                    .expect("tenants lock")
                    .remove(&ns.name_any());
            }
            watcher::Event::Init => {}
            watcher::Event::InitDone => {
                if let Some(synced) = synced.take() {
                    let _ = synced.send(());
                }
            }
        }
    }
}

fn requeue_namespace(ctx: &Context, namespace: &str) {
    let names: Vec<String> = ctx
        .endpoints_by_namespace
        .lock()
        .expect("endpoints lock")
        .get(namespace)
        .map(|names| names.iter().cloned().collect())
        .unwrap_or_default();
    for name in names {
        ctx.queue.push(ObjectRef::new(&name).within(namespace));
    }
}

async fn watch_endpoints(ctx: Arc<Context>) {
    let mut stream = watcher(
        Api::<RestateDeploymentEndpoint>::all(ctx.client.clone()),
        watcher::Config::default(),
    )
    .default_backoff()
    .boxed();
    while let Some(event) = stream.next().await {
        let event = match event {
            Ok(event) => event,
            Err(e) => {
                warn!("RestateDeploymentEndpoint watch error: {e}");
                continue;
            }
        };
        ctx.registry.apply(&event);
        match &event {
            watcher::Event::InitApply(endpoint) => {
                track(&ctx, endpoint);
                // after a restart, also retry the ones that failed before
                if needs_work(&ctx, endpoint, true) {
                    ctx.queue.push(ObjectRef::from_obj(endpoint));
                }
            }
            watcher::Event::Apply(endpoint) => {
                track(&ctx, endpoint);
                if needs_work(&ctx, endpoint, false) {
                    ctx.queue.push(ObjectRef::from_obj(endpoint));
                }
            }
            watcher::Event::Delete(endpoint) => {
                let namespace = endpoint.namespace().unwrap_or_default();
                if let Some(names) = ctx
                    .endpoints_by_namespace
                    .lock()
                    .expect("endpoints lock")
                    .get_mut(&namespace)
                {
                    names.remove(&endpoint.name_any());
                }
                ctx.failures
                    .lock()
                    .expect("failures lock")
                    .remove(&ObjectRef::from_obj(endpoint));
                if let Some(id) = endpoint.uid().as_deref().and_then(deployment_id_from_uid) {
                    ctx.registry
                        .release_reservation(&endpoint.spec.cluster, &id);
                }
            }
            watcher::Event::Init | watcher::Event::InitDone => {}
        }
    }
}

fn track(ctx: &Context, endpoint: &RestateDeploymentEndpoint) {
    ctx.endpoints_by_namespace
        .lock()
        .expect("endpoints lock")
        .entry(endpoint.namespace().unwrap_or_default())
        .or_default()
        .insert(endpoint.name_any());
}

/// Whether an endpoint, as the watch delivered it, needs a reconcile. A Failed endpoint is
/// retried by its own backoff, not by the event its failure's status write causes (that would
/// spin), except on the initial list after a restart.
fn needs_work(ctx: &Context, endpoint: &RestateDeploymentEndpoint, initial: bool) -> bool {
    if endpoint.meta().deletion_timestamp.is_some() {
        return false;
    }
    let Some(status) = &endpoint.status else {
        return true;
    };
    match status.phase {
        None | Some(EndpointPhase::Pending) => true,
        Some(EndpointPhase::Failed) => initial,
        Some(EndpointPhase::Ready) => {
            let tenant = ctx
                .tenants
                .lock()
                .expect("tenants lock")
                .get(&endpoint.namespace().unwrap_or_default())
                .cloned()
                .flatten();
            status.tenant != tenant || !labels_stamped(endpoint, status.tenant.as_deref())
        }
    }
}

fn labels_stamped(endpoint: &RestateDeploymentEndpoint, tenant: Option<&str>) -> bool {
    let labels = endpoint.labels();
    labels.get(CLUSTER_LABEL) == Some(&endpoint.spec.cluster)
        && labels.get(TENANT_LABEL).map(String::as_str) == tenant
}

async fn work(ctx: Arc<Context>, key: Key) {
    ctx.queue.queued.lock().expect("queue lock").remove(&key);
    if !ctx
        .queue
        .running
        .lock()
        .expect("queue lock")
        .insert(key.clone())
    {
        // already being reconciled: look again once that's done
        requeue_after(&ctx, key, Duration::from_secs(1));
        return;
    }

    let next = match reconcile(&ctx, &key).await {
        Ok(next) => next,
        Err(e) => {
            warn!("Reconciling RestateDeploymentEndpoint {key} failed: {e}");
            Next::RequeueAfter(backoff(&ctx, &key))
        }
    };

    ctx.queue.running.lock().expect("queue lock").remove(&key);
    match next {
        Next::Done => {}
        Next::RequeueAfter(delay) => requeue_after(&ctx, key, delay),
        Next::Served { cluster, tenant } => schedule_gc(&ctx, cluster, tenant, GC_DELAY),
    }
}

fn requeue_after(ctx: &Arc<Context>, key: Key, delay: Duration) {
    let ctx = ctx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        ctx.queue.push(key);
    });
}

fn backoff(ctx: &Context, key: &Key) -> Duration {
    let mut failures = ctx.failures.lock().expect("failures lock");
    let n = failures.entry(key.clone()).or_default();
    *n = n.saturating_add(1);
    backoff_delay(*n)
}

/// The delay after the `n`-th consecutive failure (from 1): doubling, capped.
fn backoff_delay(n: u32) -> Duration {
    BACKOFF_INITIAL
        .saturating_mul(2u32.saturating_pow(n.saturating_sub(1)))
        .min(BACKOFF_MAX)
}

async fn reconcile(ctx: &Context, key: &Key) -> Result<Next> {
    let namespace = key.namespace.clone().unwrap_or_default();
    let api: Api<RestateDeploymentEndpoint> = Api::namespaced(ctx.client.clone(), &namespace);
    let Some(endpoint) = api.get_opt(&key.name).await? else {
        return Ok(Next::Done);
    };
    if endpoint.meta().deletion_timestamp.is_some() {
        return Ok(Next::Done);
    }
    let cluster = endpoint.spec.cluster.clone();
    let Some(deployment_id) = endpoint.uid().as_deref().and_then(deployment_id_from_uid) else {
        return Err(Error::InvalidRestateConfig(format!(
            "RestateDeploymentEndpoint {key} has no uid to derive a deployment id from"
        )));
    };
    let status = endpoint.status.clone().unwrap_or_default();

    let mode = ctx
        .clusters
        .get(&ObjectRef::new(&cluster))
        .and_then(|c| c.registry_mode());
    if mode != Some(RegistryMode::Operator) {
        write_status(
            &api,
            &endpoint,
            not_served(
                &status,
                &deployment_id,
                None,
                "RegistryModeMismatch",
                format!(
                    "RestateCluster {cluster} does not exist or its workflowRegistry.mode is not \
                     operator"
                ),
            ),
        )
        .await?;
        return Ok(Next::RequeueAfter(CLUSTER_RECHECK));
    }

    let tenant = ctx
        .tenants
        .lock()
        .expect("tenants lock")
        .get(&namespace)
        .cloned()
        .flatten();
    let Some(tenant) = tenant else {
        write_status(
            &api,
            &endpoint,
            not_served(
                &status,
                &deployment_id,
                None,
                "NoTenant",
                format!("Namespace {namespace} has no {TENANT_LABEL} label: it is not a tenant"),
            ),
        )
        .await?;
        // the namespace watch re-checks it when the label appears
        return Ok(Next::Done);
    };
    if !is_valid_scope(&tenant) {
        write_status(
            &api,
            &endpoint,
            not_served(
                &status,
                &deployment_id,
                None,
                "InvalidTenant",
                format!(
                    "The {TENANT_LABEL} label of namespace {namespace} ('{tenant}') is not a valid \
                     scope: 1 to 36 characters in [a-zA-Z0-9_.-]"
                ),
            ),
        )
        .await?;
        return Ok(Next::Done);
    }

    let up_to_date = status.phase == Some(EndpointPhase::Ready)
        && status.tenant.as_deref() == Some(tenant.as_str())
        && status.deployment_id.as_deref() == Some(deployment_id.as_str())
        && status.discovered.is_some();
    if up_to_date {
        stamp_labels(&api, &endpoint, &tenant).await?;
        return Ok(Next::Done);
    }

    // The revisions are per tenant: wait for the index before handing any out.
    if !ctx.registry.is_synced() {
        return Ok(Next::RequeueAfter(Duration::from_secs(1)));
    }

    let discovered = match ctx
        .discovery
        .discover(
            &endpoint.spec.address,
            &endpoint.spec.additional_headers,
            endpoint.spec.use_http11.unwrap_or(false),
        )
        .await
    {
        Ok(discovered) => discovered,
        Err(e) => {
            let delay = backoff(ctx, key);
            write_status(
                &api,
                &endpoint,
                failed(&status, &deployment_id, "DiscoveryFailed", e.to_string()),
            )
            .await?;
            return Ok(Next::RequeueAfter(delay));
        }
    };

    let projection = match manifest::project(
        &discovered.manifest,
        &endpoint.spec.address,
        &endpoint.spec.additional_headers,
        endpoint.spec.use_http11.unwrap_or(false),
    ) {
        Ok(projection) => projection,
        Err(e) => {
            let delay = backoff(ctx, key);
            write_status(
                &api,
                &endpoint,
                failed(&status, &deployment_id, "InvalidManifest", e.to_string()),
            )
            .await?;
            return Ok(Next::RequeueAfter(delay));
        }
    };

    let names: Vec<String> = projection.services.iter().map(|s| s.name.clone()).collect();
    let revisions = ctx
        .registry
        .reserve_revisions(&cluster, &tenant, &deployment_id, &names);
    let discovered = discovered_deployment(projection, discovered.sdk_version, &revisions);

    let size = serde_json::to_vec(&discovered)
        .map(|v| v.len())
        .unwrap_or(0);
    if size > MAX_PROJECTION_BYTES {
        ctx.registry.release_reservation(&cluster, &deployment_id);
        write_status(
            &api,
            &endpoint,
            failed(
                &status,
                &deployment_id,
                "ProjectionTooLarge",
                format!(
                    "The discovered services take {size} bytes, over the {MAX_PROJECTION_BYTES} a \
                     RestateDeploymentEndpoint may hold"
                ),
            ),
        )
        .await?;
        return Ok(Next::Done);
    }

    let ready = RestateDeploymentEndpointStatus {
        phase: Some(EndpointPhase::Ready),
        tenant: Some(tenant.clone()),
        deployment_id: Some(deployment_id.clone()),
        summary: Some(summary(&discovered.services)),
        discovered: Some(discovered),
        observed_generation: endpoint.meta().generation,
        conditions: Some(vec![condition(
            &status,
            "Ready",
            "True",
            "Discovered",
            format!("Served by the workflow registry of {cluster} for tenant {tenant}"),
        )]),
    };
    if let Err(e) = write_status(&api, &endpoint, ready).await {
        ctx.registry.release_reservation(&cluster, &deployment_id);
        return Err(e);
    }
    stamp_labels(&api, &endpoint, &tenant).await?;
    ctx.failures.lock().expect("failures lock").remove(key);
    info!("RestateDeploymentEndpoint {key} is Ready as {deployment_id} for tenant {tenant}");
    Ok(Next::Served { cluster, tenant })
}

/// Run the tenant's revision GC soon (once per burst of deploys).
fn schedule_gc(ctx: &Arc<Context>, cluster: String, tenant: String, delay: Duration) {
    if !ctx
        .gc_scheduled
        .lock()
        .expect("gc lock")
        .insert((cluster.clone(), tenant.clone()))
    {
        return;
    }
    let ctx = ctx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        ctx.gc_scheduled
            .lock()
            .expect("gc lock")
            .remove(&(cluster.clone(), tenant.clone()));
        match gc::collect(
            &ctx.client,
            &ctx.http,
            &ctx.registry,
            &cluster,
            &tenant,
            &ctx.cluster_dns,
        )
        .await
        {
            Ok(gc::Collected::All) => {}
            Ok(gc::Collected::SomePinned) => schedule_gc(&ctx, cluster, tenant, GC_RETRY),
            Err(e) => {
                warn!("The revision GC of tenant {tenant} in {cluster} failed: {e}");
                schedule_gc(&ctx, cluster, tenant, GC_RETRY);
            }
        }
    });
}

fn discovered_deployment(
    projection: Projection,
    sdk_version: Option<String>,
    revisions: &[u32],
) -> DiscoveredDeployment {
    let Projection {
        protocol_type,
        http_version,
        min_protocol_version,
        max_protocol_version,
        services,
    } = projection;
    DiscoveredDeployment {
        discovered_at: Time(Utc::now()),
        protocol_type,
        http_version,
        min_protocol_version,
        max_protocol_version,
        sdk_version,
        services: services
            .into_iter()
            .zip(revisions)
            .map(|(service, revision)| ProjectedService {
                revision: *revision,
                ..service
            })
            .collect(),
    }
}

fn summary(services: &[ProjectedService]) -> String {
    services
        .iter()
        .map(|s| format!("{}@{}", s.name, s.revision))
        .collect::<Vec<_>>()
        .join(",")
}

/// The status of an endpoint the registry doesn't serve (yet): Pending, without discovery
/// results, so it's out of the index.
fn not_served(
    previous: &RestateDeploymentEndpointStatus,
    deployment_id: &str,
    tenant: Option<String>,
    reason: &str,
    message: String,
) -> RestateDeploymentEndpointStatus {
    RestateDeploymentEndpointStatus {
        phase: Some(EndpointPhase::Pending),
        tenant,
        deployment_id: Some(deployment_id.to_owned()),
        summary: None,
        discovered: None,
        observed_generation: previous.observed_generation,
        conditions: Some(vec![condition(previous, "Ready", "False", reason, message)]),
    }
}

fn failed(
    previous: &RestateDeploymentEndpointStatus,
    deployment_id: &str,
    reason: &str,
    message: String,
) -> RestateDeploymentEndpointStatus {
    RestateDeploymentEndpointStatus {
        phase: Some(EndpointPhase::Failed),
        ..not_served(previous, deployment_id, None, reason, message)
    }
}

/// A condition, keeping the previous transition time if its status didn't change.
fn condition(
    previous: &RestateDeploymentEndpointStatus,
    type_: &str,
    status: &str,
    reason: &str,
    message: String,
) -> Condition {
    let last_transition_time = previous
        .conditions
        .iter()
        .flatten()
        .find(|c| c.type_ == type_ && c.status == status)
        .map(|c| c.last_transition_time.clone())
        .unwrap_or_else(|| Time(Utc::now()));
    Condition {
        last_transition_time,
        message,
        observed_generation: None,
        reason: reason.to_owned(),
        status: status.to_owned(),
        type_: type_.to_owned(),
    }
}

async fn write_status(
    api: &Api<RestateDeploymentEndpoint>,
    endpoint: &RestateDeploymentEndpoint,
    status: RestateDeploymentEndpointStatus,
) -> Result<()> {
    if endpoint.status.as_ref().map(status_without_times) == Some(status_without_times(&status)) {
        return Ok(());
    }
    debug!(
        "Writing the status of RestateDeploymentEndpoint {}/{}",
        endpoint.namespace().unwrap_or_default(),
        endpoint.name_any()
    );
    api.patch_status(
        &endpoint.name_any(),
        &PatchParams::apply(FIELD_MANAGER).force(),
        &Patch::Apply(json!({
            "apiVersion": RestateDeploymentEndpoint::api_version(&()),
            "kind": RestateDeploymentEndpoint::kind(&()),
            "status": status,
        })),
    )
    .await?;
    Ok(())
}

/// A status with its timestamps dropped, to compare what a write would change.
fn status_without_times(status: &RestateDeploymentEndpointStatus) -> serde_json::Value {
    let mut status = status.clone();
    if let Some(discovered) = &mut status.discovered {
        discovered.discovered_at = Time(chrono::DateTime::<Utc>::MIN_UTC);
    }
    for c in status.conditions.iter_mut().flatten() {
        c.last_transition_time = Time(chrono::DateTime::<Utc>::MIN_UTC);
    }
    serde_json::to_value(status).unwrap_or_default()
}

/// The `restate.dev/cluster` and `restate.dev/tenant` labels, for `kubectl get -l`. Never read
/// by the registry (a deployer could edit them; the status is the operator's).
async fn stamp_labels(
    api: &Api<RestateDeploymentEndpoint>,
    endpoint: &RestateDeploymentEndpoint,
    tenant: &str,
) -> Result<()> {
    if labels_stamped(endpoint, Some(tenant)) {
        return Ok(());
    }
    let labels = BTreeMap::from([
        (CLUSTER_LABEL.to_owned(), endpoint.spec.cluster.clone()),
        (TENANT_LABEL.to_owned(), tenant.to_owned()),
    ]);
    api.patch(
        &endpoint.name_any(),
        &PatchParams::apply("restate-operator/labels").force(),
        &Patch::Apply(json!({
            "apiVersion": RestateDeploymentEndpoint::api_version(&()),
            "kind": RestateDeploymentEndpoint::kind(&()),
            "metadata": { "labels": labels },
        })),
    )
    .await?;
    Ok(())
}

/// Make a Ready endpoint the latest for all its services again (a rollback onto it): new
/// revisions, past every other endpoint's, under the same deployment id. Returns the
/// endpoint's services.
pub async fn promote(
    client: &Client,
    registry: &Registry,
    endpoint: &RestateDeploymentEndpoint,
) -> Result<Vec<String>> {
    let namespace = endpoint.namespace().unwrap_or_default();
    let api: Api<RestateDeploymentEndpoint> = Api::namespaced(client.clone(), &namespace);
    let status = endpoint.status.clone().unwrap_or_default();
    let (Some(tenant), Some(deployment_id), Some(mut discovered)) = (
        status.tenant.clone(),
        status.deployment_id.clone(),
        status.discovered.clone(),
    ) else {
        return Err(Error::InvalidRestateConfig(format!(
            "RestateDeploymentEndpoint {namespace}/{} is not Ready, it can't be promoted",
            endpoint.name_any()
        )));
    };
    let names: Vec<String> = discovered.services.iter().map(|s| s.name.clone()).collect();
    let revisions =
        registry.reserve_revisions(&endpoint.spec.cluster, &tenant, &deployment_id, &names);
    for (service, revision) in discovered.services.iter_mut().zip(&revisions) {
        service.revision = *revision;
    }
    let promoted = RestateDeploymentEndpointStatus {
        summary: Some(summary(&discovered.services)),
        discovered: Some(discovered),
        ..status
    };
    if let Err(e) = write_status(&api, endpoint, promoted).await {
        registry.release_reservation(&endpoint.spec.cluster, &deployment_id);
        return Err(e);
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::restatedeploymentendpoints::{ServiceType, TargetType};

    fn service(name: &str) -> ProjectedService {
        ProjectedService {
            name: name.into(),
            ty: ServiceType::Service,
            revision: 0,
            public: true,
            options: Default::default(),
            handlers: vec![
                crate::resources::restatedeploymentendpoints::ProjectedHandler {
                    name: "run".into(),
                    target_ty: TargetType::Service,
                    public: None,
                    workflow_completion_retention: None,
                    options: Default::default(),
                    input: None,
                    output: None,
                },
            ],
        }
    }

    #[test]
    fn the_summary_lists_services_with_their_revisions() {
        let mut a = service("checkout");
        a.revision = 3;
        let mut b = service("cron");
        b.revision = 1;
        assert_eq!(summary(&[a, b]), "checkout@3,cron@1");
    }

    #[test]
    fn a_condition_keeps_its_transition_time_while_its_status_holds() {
        let then = Time(Utc::now() - chrono::Duration::hours(1));
        let previous = RestateDeploymentEndpointStatus {
            conditions: Some(vec![Condition {
                last_transition_time: then.clone(),
                message: "old".into(),
                observed_generation: None,
                reason: "Discovered".into(),
                status: "True".into(),
                type_: "Ready".into(),
            }]),
            ..Default::default()
        };
        let same = condition(&previous, "Ready", "True", "Discovered", "new".into());
        assert_eq!(same.last_transition_time, then);
        let flipped = condition(&previous, "Ready", "False", "NoTenant", "x".into());
        assert_ne!(flipped.last_transition_time, then);
    }

    #[test]
    fn statuses_that_differ_only_in_time_are_the_same_write() {
        let a = RestateDeploymentEndpointStatus {
            phase: Some(EndpointPhase::Pending),
            conditions: Some(vec![condition(
                &Default::default(),
                "Ready",
                "False",
                "NoTenant",
                "m".into(),
            )]),
            ..Default::default()
        };
        let mut b = a.clone();
        b.conditions.as_mut().unwrap()[0].last_transition_time =
            Time(Utc::now() + chrono::Duration::minutes(5));
        assert_eq!(status_without_times(&a), status_without_times(&b));
        b.phase = Some(EndpointPhase::Failed);
        assert_ne!(status_without_times(&a), status_without_times(&b));
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff_delay(1), Duration::from_secs(5));
        assert_eq!(backoff_delay(2), Duration::from_secs(10));
        assert_eq!(backoff_delay(7), BACKOFF_MAX);
        assert_eq!(backoff_delay(u32::MAX), BACKOFF_MAX);
    }
}
