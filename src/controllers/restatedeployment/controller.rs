use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use futures::StreamExt;

use k8s_openapi::api::apps::v1::{ReplicaSet, ReplicaSetStatus};
use k8s_openapi::api::autoscaling::v2::HorizontalPodAutoscaler;
use k8s_openapi::api::core::v1::{Secret, Service};

use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::api::{Api, ObjectMeta, PartialObjectMetaExt, Patch, PatchParams, ResourceExt};
use kube::client::Client;
use kube::core::Selector;
use kube::core::subresource::Scale;
use kube::runtime::controller::Action;
use kube::runtime::events::{Event, EventType, Recorder};
use kube::runtime::finalizer::{Event as Finalizer, finalizer};
use kube::runtime::reflector::{ObjectRef, Store};
use kube::runtime::watcher::Config;
use kube::runtime::{
    Predicate, WatchStreamExt, controller, metadata_watcher, predicates, reflector, watcher,
};

use kube::Resource;
use reqwest::Method;
use serde_json::json;
use tokio::sync::RwLock;
use tracing::*;

use crate::controllers::{
    CrdWait, Diagnostics, RECONCILING_CONDITION, ReadinessGate, State, log_suspended,
    prewarmed_reflector, reconciliation_suspended, state_after_reconcile, suspended_message,
    wait_for_crd,
};
use crate::metrics::Metrics;
use crate::resources::ReconciliationState;
use crate::resources::knative::{Configuration, Revision, Route};
use crate::resources::restatecloudenvironments::{InProcessTunnelParams, RestateCloudEnvironment};
use crate::resources::restateclusters::RestateCluster;
use crate::resources::restatedeployments::{
    DeletionPhase, DeletionStatus, PendingInvocations, RESTATE_DEPLOYMENT_FINALIZER,
    RestateAdminEndpoint, RestateDeployment, RestateDeploymentCondition, RestateDeploymentStatus,
};
use crate::telemetry;
use crate::{Error, Result};

// Import our reconcilers
use crate::controllers::restatedeployment::cleanup::{
    BlockingVersion, CleanupMode, CleanupOutcome, DeploymentUsage, DeploymentUsageMap,
    DeploymentUsageRows, RESTATE_REMOVE_VERSION_AT_ANNOTATION, blocked_deletion_requeue,
    deployment_usage_query, describe_abandoned_versions, describe_blocking_versions,
    drain_deadline, unschedule_version_removal,
};
use crate::controllers::restatedeployment::reconcilers;
use crate::controllers::restatedeployment::registration::{self, RegistrationAction};
use crate::controllers::restatedeployment::retry::ExpensiveOperationRetries;

use super::reconcilers::replicaset::{
    POD_TEMPLATE_HASH_LABEL, RESTATE_POD_TEMPLATE_ANNOTATION, RESTATE_TUNNEL_NAME_ANNOTATION,
};

pub(super) const RESTATE_DEPLOYMENT_ID_ANNOTATION: &str = "restate.dev/deployment-id";
pub(super) const OWNED_BY_LABEL: &str = "restate.dev/owned-by";
pub(super) const APP_MANAGED_BY_LABEL: &str = "app.kubernetes.io/managed-by";
const APPLYSET_LABEL_PREFIX: &str = "applyset.kubernetes.io/";

/// Copy user labels to an operator-owned child without copying ApplySet
/// bookkeeping. A child that inherits `applyset.kubernetes.io/part-of` becomes
/// an accidental member of the parent's kubectl ApplySet and can be pruned even
/// though it was created and is still needed by this operator.
pub(super) fn propagated_labels(labels: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut propagated = labels.clone();
    propagated.retain(|key, _| !key.starts_with(APPLYSET_LABEL_PREFIX));
    propagated
}

pub(super) struct Context {
    /// Kubernetes client
    pub client: Client,
    /// Kubernetes event recorder
    pub recorder: Recorder,
    /// Store for replica sets
    pub replicasets_store: Store<ReplicaSet>,
    /// Store for restate cloud environments
    pub rce_store: Store<RestateCloudEnvironment>,
    /// Store for secrets in the same namespace as the operator
    pub secret_store: Store<Secret>,
    /// Store for Knative Revisions
    pub revision_store: Store<Revision>,
    /// Store for Knative Configurations
    pub configuration_store: Store<Configuration>,
    /// Store for operator-managed per-version HorizontalPodAutoscalers
    pub hpa_store: Store<HorizontalPodAutoscaler>,
    /// The namespace in which this operator runs
    pub operator_namespace: String,
    /// The cluster DNS suffix (e.g. "cluster.local")
    pub cluster_dns: String,
    /// Diagnostics read by the web server
    pub diagnostics: Arc<RwLock<Diagnostics>>,
    /// Prometheus metrics
    pub metrics: Metrics,
    /// HTTP client
    pub http_client: reqwest::Client,
    /// Process-local, endpoint-scoped retry coordination for expensive admin work.
    pub expensive_operation_retries: ExpensiveOperationRetries,
}

impl Context {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client: Client,
        replicasets_store: Store<ReplicaSet>,
        rce_store: Store<RestateCloudEnvironment>,
        secret_store: Store<Secret>,
        revision_store: Store<Revision>,
        configuration_store: Store<Configuration>,
        hpa_store: Store<HorizontalPodAutoscaler>,
        metrics: Metrics,
        state: State,
    ) -> Arc<Context> {
        Arc::new(Context {
            client: client.clone(),
            recorder: Recorder::new(client, "restate-operator".into()),
            replicasets_store,
            rce_store,
            secret_store,
            revision_store,
            configuration_store,
            hpa_store,
            operator_namespace: state.operator_namespace,
            cluster_dns: state.cluster_dns,
            metrics,
            diagnostics: state.diagnostics.clone(),
            http_client: reqwest::Client::new(),
            expensive_operation_retries: ExpensiveOperationRetries::new(),
        })
    }

    pub fn request(
        &self,
        method: reqwest::Method,
        admin_endpoint: &RestateAdminEndpoint,
        path: &str,
    ) -> Result<reqwest::RequestBuilder> {
        let bearer_token = admin_endpoint.bearer_token(
            &self.rce_store,
            &self.secret_store,
            &self.operator_namespace,
        )?;
        let admin_endpoint = admin_endpoint.admin_url(&self.rce_store, &self.cluster_dns)?;

        let mut request_builder = self.http_client.request(method, admin_endpoint.join(path)?);

        if let Some(bearer_token) = bearer_token {
            request_builder = request_builder.bearer_auth(bearer_token);
        }

        Ok(request_builder)
    }

    /// A stable cache key for capabilities of the downstream Restate server. Authentication is
    /// intentionally excluded: token rotation does not change which system tables exist.
    pub fn admin_endpoint_key(&self, admin_endpoint: &RestateAdminEndpoint) -> Result<String> {
        Ok(admin_endpoint
            .admin_url(&self.rce_store, &self.cluster_dns)?
            .to_string())
    }

    fn expensive_operation_key(&self, rsd: &RestateDeployment) -> (String, String) {
        let endpoint = self
            .admin_endpoint_key(&rsd.spec.restate.register)
            // Invalid endpoint configuration cannot be globally coordinated. Preserve the
            // floor rather than hiding the configuration error behind a retry storm.
            .unwrap_or_else(|_| "<unresolved-admin-endpoint>".into());
        let resource = rsd.uid().unwrap_or_else(|| {
            format!("{}/{}", rsd.namespace().unwrap_or_default(), rsd.name_any())
        });
        (endpoint, resource)
    }

    fn admit_expensive_operation(&self, rsd: &RestateDeployment) -> Result<()> {
        let (endpoint, resource) = self.expensive_operation_key(rsd);
        self.expensive_operation_retries
            .admit(endpoint, resource)
            .map_err(|requeue_after| Error::ExpensiveOperationDeferred { requeue_after })
    }

    fn expensive_retry_after(&self, rsd: &RestateDeployment) -> Duration {
        let (endpoint, resource) = self.expensive_operation_key(rsd);
        self.expensive_operation_retries.failure(endpoint, resource)
    }

    fn finish_expensive_operation(&self, rsd: &RestateDeployment) {
        let (endpoint, _) = self.expensive_operation_key(rsd);
        self.expensive_operation_retries.finish(&endpoint);
    }

    fn reset_expensive_retries(&self, rsd: &RestateDeployment) {
        let (endpoint, resource) = self.expensive_operation_key(rsd);
        self.expensive_operation_retries
            .reset_resource(&endpoint, &resource);
    }
}

impl RestateDeployment {
    /// Spread otherwise-healthy periodic reconciliations over a minute, using stable resource
    /// identity so an operator restart does not align every deployment again. This is a poll,
    /// not an error retry, so a small positive jitter is preferable to an exact global cadence.
    fn healthy_requeue_after(&self) -> Duration {
        const BASE: Duration = Duration::from_secs(5 * 60);
        const JITTER: u64 = 60;

        let mut hasher = fnv::FnvHasher::default();
        self.uid()
            .unwrap_or_else(|| self.name_any())
            .hash(&mut hasher);
        BASE + Duration::from_secs(hasher.finish() % (JITTER + 1))
    }
}

/// Check an admin API response, returning the response if successful or an error
/// that includes the response body if not. This replaces the `error_for_status()`
/// pattern which discards the response body.
pub(crate) async fn check_admin_response(resp: reqwest::Response) -> Result<reqwest::Response> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status();
    let url = resp.url().to_string();
    let body = resp.text().await.unwrap_or_default();
    Err(Error::AdminCallRejected { status, url, body })
}

#[instrument(skip(ctx, rs), fields(trace_id))]
async fn reconcile(rs: Arc<RestateDeployment>, ctx: Arc<Context>) -> Result<Action> {
    if let Some(trace_id) = telemetry::get_trace_id() {
        Span::current().record("trace_id", field::display(&trace_id));
    }
    let _timer = ctx.metrics.count_and_measure::<RestateDeployment>();
    ctx.diagnostics.write().await.last_event = Utc::now();

    let namespace = match rs.metadata.namespace.as_deref() {
        Some("") | None => "default",
        Some(ns) => ns,
    };

    let services_api: Api<RestateDeployment> = Api::namespaced(ctx.client.clone(), namespace);

    // Check this before the finalizer helper, which adds our finalizer to resources that
    // don't have one yet. That's a write, and we've been asked not to write to this one.
    if reconciliation_suspended(rs.as_ref()) {
        return rs.reconcile_suspended(&services_api).await;
    }

    info!(
        "Reconciling RestateDeployment {} in namespace {namespace}",
        rs.name_any(),
    );
    match finalizer(
        &services_api,
        RESTATE_DEPLOYMENT_FINALIZER,
        rs.clone(),
        |event| async {
            match event {
                Finalizer::Apply(rs) => rs.reconcile_status(ctx.clone(), namespace).await,
                Finalizer::Cleanup(rs) => rs.cleanup(ctx.clone(), namespace).await,
            }
        },
    )
    .await
    {
        Ok(action) => Ok(action),
        Err(err) => {
            warn!("reconcile failed: {:?}", err);

            ctx.recorder
                .publish(
                    &Event {
                        type_: EventType::Warning,
                        reason: "FailedReconcile".into(),
                        note: Some(err.to_string()),
                        action: "Reconcile".into(),
                        secondary: None,
                    },
                    &rs.object_ref(&()),
                )
                .await?;

            let err = Error::FinalizerError(Box::new(err));
            // Label the failure with the variant the reconciler actually raised. The
            // wrapper above would otherwise collapse every one of them into
            // `FinalizerError`, which is what `error_policy` unwraps for too.
            ctx.metrics.reconcile_failure(rs.as_ref(), root_cause(&err));
            Err(err)
        }
    }
}

/// Look through the `finalizer` wrapper `reconcile` puts on every error.
///
/// Errors raised inside the finalizer closure reach `error_policy` as
/// `FinalizerError(CleanupFailed(..))`, so matching on the reconciler's own variants only
/// works after unwrapping — otherwise every arm below the wrapper is dead.
fn root_cause(err: &Error) -> &Error {
    match err {
        Error::FinalizerError(inner) => match inner.as_ref() {
            kube::runtime::finalizer::Error::ApplyFailed(err)
            | kube::runtime::finalizer::Error::CleanupFailed(err) => root_cause(err),
            _ => err,
        },
        err => err,
    }
}

#[cfg(test)]
fn error_policy<K, C>(_rs: Arc<K>, err: &Error, _ctx: C) -> Action {
    match root_cause(err) {
        Error::ExpensiveOperationDeferred { requeue_after } => Action::requeue(*requeue_after),
        // A drain knows its own deadline; the blanket interval would make a short
        // drainDelaySeconds cost up to 30s per version anyway. A deletion blocked on
        // in-flight invocations sets its own interval too, backing off as the wait grows
        // so a long one stops re-running the admin query twice a minute forever.
        Error::DeploymentDraining {
            requeue_after: Some(requeue_after),
        }
        | Error::DeploymentInUse {
            requeue_after: Some(requeue_after),
            ..
        }
        | Error::DeletionDrainOverdue {
            requeue_after: Some(requeue_after),
            ..
        } => Action::requeue(*requeue_after),
        _ => Action::requeue(Duration::from_secs(30)),
    }
}

/// Controller-framework errors do not pass through `reconcile_status`, including finalizer
/// failures during deletion. Apply the same endpoint-scoped protection there while preserving a
/// real drain deadline when one exists.
fn restate_deployment_error_policy(
    rsd: Arc<RestateDeployment>,
    err: &Error,
    ctx: Arc<Context>,
) -> Action {
    match root_cause(err) {
        Error::ExpensiveOperationDeferred { requeue_after } => Action::requeue(*requeue_after),
        Error::DeploymentDraining {
            requeue_after: Some(requeue_after),
        } => Action::requeue(*requeue_after),
        Error::DeploymentInUse {
            requeue_after: Some(requeue_after),
            ..
        } => Action::requeue((*requeue_after).max(ctx.expensive_retry_after(&rsd))),
        Error::AdminCallFailed(_) | Error::AdminCallRejected { .. } => {
            Action::requeue(ctx.expensive_retry_after(&rsd))
        }
        _ => Action::requeue(Duration::from_secs(30)),
    }
}

impl RestateDeployment {
    /// Resolve the RestateCloudEnvironment values a `tunnelMode: in-process`
    /// deployment derives its identity from (None for every other mode). They feed
    /// the revision hash, the env vars injected into the pods, the registered URL,
    /// and the status labelSelector — all of which must agree.
    fn in_process_tunnel_params(&self, ctx: &Context) -> Result<Option<InProcessTunnelParams>> {
        if !self.spec.restate.is_in_process_tunnel() {
            return Ok(None);
        }
        let Some(cloud) = self.spec.restate.register.cloud.as_deref() else {
            return Err(Error::InvalidRestateConfig(
                "tunnelMode: in-process requires registering against a RestateCloudEnvironment (spec.restate.register.cloud)"
                    .into(),
            ));
        };
        let Some(rce) = ctx.rce_store.get(&ObjectRef::new(cloud)) else {
            return Err(Error::RestateCloudEnvironmentNotFound(cloud.into()));
        };
        Ok(Some(InProcessTunnelParams::from_rce(rce.as_ref())?))
    }

    // Reconcile (for non-finalizer related changes)
    async fn reconcile(
        &self,
        ctx: Arc<Context>,
        namespace: &str,
    ) -> Result<(ReplicaSet, Option<chrono::DateTime<chrono::Utc>>)> {
        let rsc_api: Api<RestateCluster> = Api::all(ctx.client.clone());
        let rs_api = Api::<ReplicaSet>::namespaced(ctx.client.clone(), namespace);
        let svc_api = Api::<Service>::namespaced(ctx.client.clone(), namespace);

        // tunnelMode: in-process needs the RestateCloudEnvironment values up front:
        // they feed the revision hash, the env vars injected into the pods, and the
        // registered URL, all of which must agree within one reconcile.
        let in_process_tunnel = self.in_process_tunnel_params(&ctx)?;

        let pod_template_annotation = reconcilers::replicaset::pod_template_annotation(self);

        // Generate a hash for the pod template
        let hash = reconcilers::replicaset::generate_pod_template_hash(
            self,
            &pod_template_annotation,
            in_process_tunnel.as_ref(),
        );
        let deployment_name = self.name_any();
        let versioned_name = format!("{deployment_name}-{hash}");

        let replicaset_selector = match self
            .spec
            .selector
            .as_ref()
            .and_then(|s| s.match_labels.as_ref())
        {
            None => BTreeMap::from([(POD_TEMPLATE_HASH_LABEL.to_owned(), hash.clone())]),
            Some(match_labels) => {
                let mut match_labels = match_labels.clone();
                match_labels.insert(POD_TEMPLATE_HASH_LABEL.to_owned(), hash.clone());
                match_labels
            }
        };

        // The rsd's annotations are copied onto the ReplicaSet/Service below; drop
        // the ones the operator manages itself so a user-set value on the rsd can't
        // shadow them on the way through.
        let mut annotations = self.annotations().clone();
        // kubectl bookkeeping; meaningful only on the object it was applied to
        annotations.remove("kubectl.kubernetes.io/last-applied-configuration");
        // recorded by the operator on the ReplicaSet at creation (in-process tunnel
        // mode) and read back when building the registration URL
        annotations.remove(RESTATE_TUNNEL_NAME_ANNOTATION);
        // the drain deadline; a copy would land under a manager the clear can't give up
        annotations.remove(RESTATE_REMOVE_VERSION_AT_ANNOTATION);

        // Create/update the ReplicaSet for this version
        let reconcile_result = reconcilers::replicaset::reconcile_replicaset(
            &ctx.client,
            self,
            namespace,
            &versioned_name,
            replicaset_selector.clone(),
            {
                let mut annotations = annotations.clone();
                // we use this annotation to compare templates to see if we have a hash collision
                annotations.insert(
                    RESTATE_POD_TEMPLATE_ANNOTATION.to_string(),
                    pod_template_annotation.to_string(),
                );
                if in_process_tunnel.is_some() {
                    // persist the tunnel name the pods were created with, so the
                    // registered URL keeps matching them even if a future operator
                    // version computes the hash differently
                    annotations.insert(
                        RESTATE_TUNNEL_NAME_ANNOTATION.to_string(),
                        versioned_name.clone(),
                    );
                }
                annotations
            },
            &hash,
            in_process_tunnel.as_ref(),
        )
        .await;

        let my_uid = self.uid().expect("RestateDeployment to have a uid");

        let replicaset = match reconcile_result {
            Ok(replicaset) => replicaset,
            Err(Error::KubeError(kube::Error::Api(err))) if err.reason == "AlreadyExists" => {
                let existing_replicaset = rs_api.get(&versioned_name).await?;

                let controller = existing_replicaset
                    .metadata
                    .owner_references
                    .as_ref()
                    .and_then(|r| r.first());

                let existing_pod_template_annotation = existing_replicaset
                    .annotations()
                    .get(RESTATE_POD_TEMPLATE_ANNOTATION);

                if controller.as_ref().map(|c| c.uid.as_str()) == Some(my_uid.as_str())
                    && existing_pod_template_annotation == Some(&pod_template_annotation)
                {
                    debug!(
                        "Found an existing ReplicaSet {versioned_name} in namespace {namespace}, ensuring it matches the deployment",
                    );

                    // the replicaset already exists, ensure its scaled and annotated appropriately
                    rs_api
                        .patch_scale(
                            &versioned_name,
                            &PatchParams::apply("restate-operator/propagate-replicas").force(),
                            &Patch::Apply(serde_json::json!({
                                "apiVersion": Scale::api_version(&()),
                                "kind": Scale::kind(&()),
                                "spec": { "replicas": self.spec.replicas }
                            })),
                        )
                        .await?;

                    rs_api
                        .patch_metadata(
                            &versioned_name,
                            &PatchParams::apply("restate-operator/propagate-annotations").force(),
                            &Patch::Apply(
                                ObjectMeta {
                                    // ensure the base annotations from the rsd are kept up to date
                                    annotations: Some(annotations.clone()),
                                    ..Default::default()
                                }
                                .into_request_partial::<ReplicaSet>(),
                            ),
                        )
                        .await?;

                    // This ReplicaSet was superseded before, so it can still carry the
                    // deadline it drained under; nothing else removes it, and the next
                    // rollout would tear it down early.
                    unschedule_version_removal(&rs_api, &existing_replicaset).await?;

                    existing_replicaset
                } else {
                    debug!(
                        "Found a hash collision ({versioned_name}) for deployment {deployment_name} in namespace {namespace}, incrementing collision count",
                    );

                    return Err(Error::HashCollision);
                }
            }
            Err(err) => return Err(err),
        };

        // The latest version is scaled via the RD scale subresource (the operator
        // propagates spec.replicas to its ReplicaSet), never by a per-version HPA.
        // If a draining version becomes latest again — a rollback, or a reintroduced
        // identical spec re-adopting its ReplicaSet via the AlreadyExists path above —
        // it can still carry the operator HPA stamped while it drained. Remove it,
        // otherwise that HPA and propagate-replicas would fight over the ReplicaSet's
        // scale every reconcile. Gated on the owned-HPA cache so it's a no-op (no API
        // call) in the common case.
        let latest_hpa =
            ObjectRef::<HorizontalPodAutoscaler>::new(&versioned_name).within(namespace);
        if ctx.hpa_store.get(&latest_hpa).is_some() {
            reconcilers::autoscaling::delete_version_hpa(&ctx.client, namespace, &versioned_name)
                .await?;
        }

        let mut service_labels = propagated_labels(self.labels());
        service_labels.insert(
            APP_MANAGED_BY_LABEL.to_string(),
            "restate-operator".to_string(),
        );
        service_labels.insert(OWNED_BY_LABEL.to_string(), deployment_name.clone());

        // Create/update the Service for this version
        reconcilers::service::reconcile_service(
            namespace,
            &svc_api,
            &versioned_name,
            replicaset_selector,
            service_labels,
            annotations,
            &replicaset,
        )
        .await?;

        let service_endpoint = match &in_process_tunnel {
            Some(params) => {
                // The pods hold their own tunnel connections, registered under the
                // versioned name; the Service plays no part in routing. Prefer the
                // name persisted on the ReplicaSet at creation — that is the value
                // injected into the pods.
                let tunnel_name = replicaset
                    .annotations()
                    .get(RESTATE_TUNNEL_NAME_ANNOTATION)
                    .map(String::as_str)
                    .unwrap_or(versioned_name.as_str());
                params.tunnel_url(tunnel_name, self.spec.restate.service_path.as_deref())?
            }
            None => self.spec.restate.register.service_url(
                &ctx.rce_store,
                &versioned_name,
                namespace,
                self.spec.restate.service_path.as_deref(),
                &ctx.cluster_dns,
            )?,
        };

        // this path only runs for a live RestateDeployment; deletion goes to `cleanup`.
        let existing_deployment_id = replicaset
            .annotations()
            .get(RESTATE_DEPLOYMENT_ID_ANNOTATION)
            .cloned();

        // Optimisation: only run the expensive invocation-status query when we really need it,
        // not in the common case where this version is already latest with nothing to drain.
        if !registration::has_other_owned(
            &ctx.replicasets_store,
            namespace,
            &my_uid,
            &versioned_name,
        ) && let Some(recorded_id) = existing_deployment_id.as_deref()
        {
            let latest_ids =
                registration::latest_deployment_ids(&ctx, &self.spec.restate.register).await?;
            if latest_ids.contains(recorded_id) {
                return Ok((replicaset, None));
            }
        }

        let mut deployments = self.list_deployments(&ctx, CleanupMode::Rollout).await?;

        let action = registration::plan_registration(
            existing_deployment_id.as_deref(),
            &deployments,
            || registration::owned_deployment_ids(&ctx.replicasets_store, namespace, &my_uid),
        );

        // This branch leaves the old ReplicaSets alone: the reconcile returns before
        // `cleanup_old_replicasets`. That is deliberate — draining a version while we cannot
        // tell who is serving its services risks removing the endpoint still taking traffic.
        if action == RegistrationAction::Conflict {
            // Deliberately no admin write. See `RegistrationAction::Conflict`.
            return Err(Error::DeploymentNotLatest {
                message: format!(
                    "Deployment {} is registered but superseded, and no version of this \
                     RestateDeployment is serving its services. Something outside this \
                     RestateDeployment has registered them; check `GET /services` against the \
                     Restate admin API. The operator will not force a promotion here.",
                    existing_deployment_id.as_deref().unwrap_or("<unknown>"),
                ),
                reason: "ForeignDeployment".into(),
                requeue_after: None,
            });
        }

        if !matches!(action, RegistrationAction::AlreadyLatest { .. }) {
            let valid = async {
                if let Some(cluster_name) = &self.spec.restate.register.cluster {
                    // wait for the cluster to be ready before registering to it
                    validate_cluster_status(rsc_api, cluster_name).await?;
                }

                // wait for the replicaset to be ready before registering it
                validate_replica_set_status(replicaset.status.as_ref(), self.spec.replicas)?;

                Ok(())
            }
            .await;

            match valid {
                Ok(()) => {}
                // there is a chicken and egg situation if the cluster is out of capacity; the new version can't become ready until
                // old versions are removed. so we remove them aggressively here
                Err(ready_err @ Error::DeploymentNotReady { .. }) => {
                    match reconcilers::replicaset::cleanup_old_replicasets(
                        namespace,
                        &ctx,
                        &rs_api,
                        &my_uid,
                        self,
                        CleanupMode::Rollout,
                        &deployments,
                        Some(&versioned_name), // exclude the replicaset which may not be registered
                    )
                    .await
                    {
                        Ok(_) => return Err(ready_err),
                        Err(cleanup_err) => {
                            error!(
                                "Failed to clean up old replicasets while waiting for current replicaset to become ready: {cleanup_err}"
                            );
                            return Err(ready_err);
                        }
                    }
                }
                Err(err) => return Err(err),
            }

            // Register the latest version with Restate cluster using the service URL.
            // A promotion re-registers the same endpoint with overwrite, so Restate bumps
            // its service revisions past the current latest without minting a new
            // deployment id — leaving invocations already pinned to it undisturbed.
            let registered = registration::register_deployment(
                &ctx,
                self,
                &service_endpoint,
                self.spec.restate.use_http11.as_ref().cloned(),
                action.overwrite(),
            )
            .await?;

            // Recorded before the routing is confirmed, because the id is true either way:
            // it is what Restate holds for this endpoint. Deferring the annotation until
            // after confirmation would strand a version whose registration landed on an
            // endpoint Restate already knew but was not routing to — with nothing recorded,
            // the next reconcile plans a plain `Register` again rather than a promotion, and
            // repeats that forever.
            if existing_deployment_id.as_deref() != Some(registered.id.as_str()) {
                debug!(
                    "Updating deployment-id annotation of ReplicaSet/Service {versioned_name} in namespace {namespace}"
                );

                // store the id against the versioned objects
                let params = PatchParams::apply("restate-operator/deployment-id").force();
                let patch = ObjectMeta {
                    annotations: Some(
                        [(
                            RESTATE_DEPLOYMENT_ID_ANNOTATION.to_string(),
                            registered.id.clone(),
                        )]
                        .into(),
                    ),
                    ..Default::default()
                };
                rs_api
                    .patch_metadata(
                        &versioned_name,
                        &params,
                        &Patch::Apply(patch.clone().into_request_partial::<ReplicaSet>()),
                    )
                    .await?;
                svc_api
                    .patch_metadata(
                        &versioned_name,
                        &params,
                        &Patch::Apply(patch.into_request_partial::<Service>()),
                    )
                    .await?;
            }

            // Registration's own response cannot distinguish "promoted" from "already
            // existed, nothing changed" — both are a 200 carrying this deployment id. Ask
            // Restate what it actually routes before treating this as done, or `Ready=True`
            // would go on meaning "the pods are up" rather than "new work lands here".
            registration::confirm_latest(&ctx, &self.spec.restate.register, &registered).await?;

            if let RegistrationAction::Promote { superseded_by } = &action {
                ctx.recorder
                    .publish(
                        &Event {
                            type_: EventType::Normal,
                            reason: "Promoted".into(),
                            // Worth an event rather than only a log line: a promotion is a
                            // forced re-registration, which Restate treats as permitting
                            // breaking schema changes, and it resets the deployment's
                            // registration time in Restate. Both deserve an audit trail.
                            note: Some(format!(
                                "Promoted deployment {} back to latest for {}, superseding {superseded_by}",
                                registered.id,
                                versioned_name.as_str(),
                            )),
                            action: "Reconcile".into(),
                            secondary: None,
                        },
                        &self.object_ref(&()),
                    )
                    .await?;
            }

            // Confirmed above, so this is Restate's answer rather than an assumption.
            //
            // The deployment this one superseded keeps its stale `latest_for_service` until
            // the next reconcile re-runs the query, so cleanup reads it as active and defers
            // its drain by one pass. A promotion names it, but not whether it lost latest for
            // *every* service it serves — it may still be the endpoint for one this version
            // never discovered — so clearing the flag here could drain a version still taking
            // traffic. One extra pass is the cheaper mistake.
            deployments.insert(
                registered.id.clone(),
                DeploymentUsage {
                    latest_for_service: true,
                    ..Default::default()
                },
            );
        }

        // Clean up old ReplicaSets that are no longer needed

        let outcome = reconcilers::replicaset::cleanup_old_replicasets(
            namespace,
            &ctx,
            &rs_api,
            &my_uid,
            self,
            CleanupMode::Rollout,
            &deployments,
            Some(&versioned_name),
        )
        .await?;

        Ok((replicaset, outcome.next_removal))
    }

    /// Note the suspension in the status, and leave everything else alone.
    async fn reconcile_suspended(&self, rsd_api: &Api<RestateDeployment>) -> Result<Action> {
        let name = self.name_any();
        let message = suspended_message();
        let reason = ReconciliationState::Disabled.as_str();
        let previous = self.status.as_ref().and_then(|s| s.reconciliation);

        log_suspended("RestateDeployment", &name, previous);

        let existing = self
            .status
            .as_ref()
            .and_then(|s| s.conditions.as_ref())
            .and_then(|c| c.iter().find(|cond| cond.r#type == RECONCILING_CONDITION));
        let now = Time(Utc::now());

        let suspended = RestateDeploymentCondition {
            // Only the start of the suspension is a transition.
            last_transition_time: Some(
                match existing {
                    Some(cond) if cond.status == "False" => cond.last_transition_time.clone(),
                    _ => None,
                }
                .unwrap_or(now),
            ),
            message: Some(message),
            reason: Some(reason.into()),
            status: "False".into(),
            r#type: RECONCILING_CONDITION.into(),
        };

        let mut rsd_status = self.status.clone().unwrap_or_default();

        // Leave the other conditions alone. A stale `Ready` is still worth having, and it
        // says when it was last true.
        let mut conditions: Vec<_> = rsd_status
            .conditions
            .take()
            .unwrap_or_default()
            .into_iter()
            .filter(|cond| cond.r#type != RECONCILING_CONDITION)
            .collect();
        conditions.push(suspended);
        rsd_status.conditions = Some(conditions);
        rsd_status.reconciliation = Some(ReconciliationState::Disabled);

        let new_status = json!({
            "apiVersion": RestateDeployment::api_version(&()),
            "kind": RestateDeployment::kind(&()),
            "status": rsd_status,
        });

        let ps = PatchParams::apply("restate-operator").force();
        rsd_api
            .patch_status(&name, &ps, &Patch::Apply(new_status))
            .await?;

        Ok(Action::await_change())
    }

    async fn reconcile_status(&self, ctx: Arc<Context>, namespace: &str) -> Result<Action> {
        use crate::resources::restatedeployments::DeploymentMode;

        // Check if Knative mode is enabled
        let is_knative = matches!(self.spec.deployment_mode, Some(DeploymentMode::Knative));

        trace!(
            deployment_mode = if is_knative { "Knative" } else { "ReplicaSet" },
            name = %self.metadata.name.as_deref().unwrap_or("unknown"),
            namespace = %namespace,
            "Determined deployment mode"
        );

        let rsd_api: Api<RestateDeployment> = Api::namespaced(ctx.client.clone(), namespace);

        let now = chrono::Utc::now();

        let mut rsd_status = self.status.clone().unwrap_or_default();

        // Build ready condition based on current state
        let existing_ready = self
            .status
            .as_ref()
            .and_then(|s| s.conditions.as_ref())
            .and_then(|c| c.iter().find(|cond| cond.r#type == "Ready"));

        let (mut result, message, reason, status) = if is_knative {
            // Delegate to Knative reconciler
            let knative_result = if self.spec.restate.is_in_process_tunnel() {
                // An in-process tunnel carries no traffic the Knative autoscaler can
                // see, so scale-to-zero would take the deployment down permanently.
                // Raised here rather than before the status machinery, so the Ready
                // condition reflects the misconfiguration.
                Err(Error::InvalidRestateConfig(
                    "tunnelMode: in-process is not supported in Knative mode".into(),
                ))
            } else {
                reconcilers::knative::reconcile_knative(&ctx, self, namespace, &mut rsd_status)
                    .await
            };
            match knative_result {
                Ok(next_removal) => {
                    let action = match next_removal {
                        Some(next_removal) if next_removal < now => Action::requeue(Duration::ZERO),
                        Some(next_removal) => {
                            let secs = (next_removal - now).num_seconds() as u64;
                            if secs < 5 * 60 {
                                Action::requeue(Duration::from_secs(secs))
                            } else {
                                Action::requeue(self.healthy_requeue_after())
                            }
                        }
                        None => Action::requeue(self.healthy_requeue_after()),
                    };

                    (
                        Ok(action),
                        "RestateDeployment is deployed".into(),
                        "Deployed".into(),
                        "True".into(),
                    )
                }
                Err(Error::RouteNotReady {
                    message,
                    reason,
                    requeue_after,
                }) => {
                    let requeue_after = requeue_after.unwrap_or(Duration::from_secs(10));
                    debug!(
                        name = %self.metadata.name.as_deref().unwrap_or("unknown"),
                        namespace = %namespace,
                        reason = %reason,
                        requeue_after_secs = %requeue_after.as_secs(),
                        "Knative Route not ready, requeueing"
                    );
                    (
                        Ok(Action::requeue(requeue_after)),
                        message,
                        "RouteNotReady".into(),
                        "False".into(),
                    )
                }
                Err(Error::ConfigurationNotReady {
                    message,
                    reason,
                    requeue_after,
                }) => {
                    let requeue_after = requeue_after.unwrap_or(Duration::from_secs(10));
                    debug!(
                        name = %self.metadata.name.as_deref().unwrap_or("unknown"),
                        namespace = %namespace,
                        reason = %reason,
                        requeue_after_secs = %requeue_after.as_secs(),
                        "Knative Configuration not ready, requeueing"
                    );
                    (
                        Ok(Action::requeue(requeue_after)),
                        message,
                        "ConfigurationNotReady".into(),
                        "False".into(),
                    )
                }
                Err(Error::AdminCallFailed(ref err)) => {
                    let message = format!("Failed to make Restate admin API call: {}", err);
                    let requeue_after = Duration::from_secs(30);
                    warn!(
                        name = %self.metadata.name.as_deref().unwrap_or("unknown"),
                        namespace = %namespace,
                        error = %err,
                        requeue_after_secs = %requeue_after.as_secs(),
                        "Admin API call failed, requeueing"
                    );
                    (
                        Ok(Action::requeue(requeue_after)),
                        message,
                        "AdminCallFailed".into(),
                        "False".into(),
                    )
                }
                Err(Error::AdminCallRejected {
                    ref status,
                    ref url,
                    ref body,
                }) => {
                    let message = format!("Restate admin API call failed ({status}): {body}");
                    let requeue_after = Duration::from_secs(30);
                    warn!(
                        name = %self.metadata.name.as_deref().unwrap_or("unknown"),
                        namespace = %namespace,
                        %status,
                        %url,
                        body = %body,
                        requeue_after_secs = %requeue_after.as_secs(),
                        "Admin API call rejected, requeueing"
                    );
                    (
                        Ok(Action::requeue(requeue_after)),
                        message,
                        "AdminCallRejected".into(),
                        "False".into(),
                    )
                }
                Err(Error::HashCollision) => {
                    rsd_status.collision_count = Some(rsd_status.collision_count.unwrap_or(0) + 1);

                    (
                        // requeue immediately
                        Ok(Action::requeue(Duration::ZERO)),
                        "Encountered a hash collision, will retry with a new template hash".into(),
                        "HashCollision".into(),
                        "False".into(),
                    )
                }
                // See the ReplicaSet-mode arm below: healthy pods, stale routing.
                Err(Error::DeploymentNotLatest {
                    ref message,
                    ref reason,
                    requeue_after,
                }) => {
                    let requeue_after = requeue_after.unwrap_or(Duration::from_secs(30));
                    warn!(
                        name = %self.metadata.name.as_deref().unwrap_or("unknown"),
                        namespace = %namespace,
                        reason = %reason,
                        "{message}"
                    );
                    (
                        Ok(Action::requeue(requeue_after)),
                        message.clone(),
                        reason.clone(),
                        "False".into(),
                    )
                }
                Err(err) => {
                    let message = err.to_string();
                    (
                        Err(err),
                        message,
                        "FailedReconcile".into(),
                        "Unknown".into(),
                    )
                }
            }
        } else {
            // ReplicaSet mode
            match self.reconcile(ctx.clone(), namespace).await {
                Ok((current_replicaset, next_removal)) => {
                    let action = match next_removal {
                        Some(next_removal) if next_removal < now => Action::requeue(Duration::ZERO), // immediate requeue
                        Some(next_removal) => {
                            let secs = (next_removal - now).num_seconds() as u64;
                            if secs < 5 * 60 {
                                Action::requeue(Duration::from_secs(secs))
                            } else {
                                Action::requeue(self.healthy_requeue_after())
                            }
                        }
                        None => Action::requeue(self.healthy_requeue_after()),
                    };

                    status_from_replica_set(
                        self.spec.replicas,
                        &mut rsd_status,
                        current_replicaset.status.as_ref(),
                    );

                    if let Some(id) = current_replicaset
                        .annotations()
                        .get(RESTATE_DEPLOYMENT_ID_ANNOTATION)
                    {
                        rsd_status.deployment_id = Some(id.clone());
                    }

                    (
                        Ok(action),
                        "RestateDeployment is deployed".into(),
                        "Deployed".into(),
                        "True".into(),
                    )
                }
                Err(Error::DeploymentNotReady {
                    message,
                    reason,
                    requeue_after,
                    replica_set_status,
                }) => {
                    let requeue_after = requeue_after.unwrap_or(Duration::from_secs(60));

                    status_from_replica_set(
                        self.spec.replicas,
                        &mut rsd_status,
                        replica_set_status.as_deref(),
                    );

                    (
                        Ok(Action::requeue(requeue_after)),
                        message,
                        reason,
                        "False".into(),
                    )
                }
                Err(Error::HashCollision) => {
                    rsd_status.collision_count = Some(rsd_status.collision_count.unwrap_or(0) + 1);

                    (
                        // requeue immediately
                        Ok(Action::requeue(Duration::ZERO)),
                        "Encountered a hash collision, will retry with a new template hash".into(),
                        "HashCollision".into(),
                        "False".into(),
                    )
                }
                // The desired version exists in Restate but is not what new invocations go
                // to. Reported as not-Ready rather than as a reconcile failure: the pods are
                // healthy, and a `Ready=True` here would be the exact false assurance that
                // let a silent rollback failure pass for a successful one.
                Err(Error::DeploymentNotLatest {
                    ref message,
                    ref reason,
                    requeue_after,
                }) => {
                    let requeue_after = requeue_after.unwrap_or(Duration::from_secs(30));
                    warn!(
                        name = %self.metadata.name.as_deref().unwrap_or("unknown"),
                        namespace = %namespace,
                        reason = %reason,
                        "{message}"
                    );
                    (
                        Ok(Action::requeue(requeue_after)),
                        message.clone(),
                        reason.clone(),
                        "False".into(),
                    )
                }
                Err(Error::AdminCallFailed(ref err)) => {
                    let message = format!("Failed to make Restate admin API call: {}", err);
                    let requeue_after = Duration::from_secs(30);
                    warn!(
                        name = %self.metadata.name.as_deref().unwrap_or("unknown"),
                        namespace = %namespace,
                        error = %err,
                        requeue_after_secs = %requeue_after.as_secs(),
                        "Admin API call failed, requeueing"
                    );
                    (
                        Ok(Action::requeue(requeue_after)),
                        message,
                        "AdminCallFailed".into(),
                        "False".into(),
                    )
                }
                Err(Error::AdminCallRejected {
                    ref status,
                    ref url,
                    ref body,
                }) => {
                    let message = format!("Restate admin API call failed ({status}): {body}");
                    let requeue_after = Duration::from_secs(30);
                    warn!(
                        name = %self.metadata.name.as_deref().unwrap_or("unknown"),
                        namespace = %namespace,
                        %status,
                        %url,
                        body = %body,
                        requeue_after_secs = %requeue_after.as_secs(),
                        "Admin API call rejected, requeueing"
                    );
                    (
                        Ok(Action::requeue(requeue_after)),
                        message,
                        "AdminCallRejected".into(),
                        "False".into(),
                    )
                }
                Err(err) => {
                    let message = err.to_string();
                    (
                        Err(err),
                        message,
                        "FailedReconcile".into(),
                        "Unknown".into(),
                    )
                }
            }
        };

        // These states all ran, or are about to immediately re-run, deployment usage
        // accounting against Restate. Coordinate their retries per endpoint so one stuck
        // resource cannot keep a full scan at a fixed cadence, and several resources sharing
        // an environment cannot line up their retries. Route/Configuration readiness is
        // deliberately not included: those paths fail before the Restate query and retain
        // their short Kubernetes readiness polling.
        if status == "True" {
            ctx.reset_expensive_retries(self);
        } else if result.is_ok() && requeues_after_expensive_admin_work(&reason) {
            let requeue_after = ctx.expensive_retry_after(self);
            debug!(
                name = %self.name_any(),
                namespace = %namespace,
                reason = %reason,
                requeue_after_secs = %requeue_after.as_secs(),
                "Backing off an expensive RestateDeployment retry"
            );
            result = Ok(Action::requeue(requeue_after));
        }

        // Emit a K8s Warning event for admin API failures so they're visible
        // via `kubectl describe` and `kubectl get events`
        if reason == "AdminCallFailed" || reason == "AdminCallRejected" {
            ctx.recorder
                .publish(
                    &Event {
                        type_: EventType::Warning,
                        reason: reason.clone(),
                        note: Some(message.clone()),
                        action: "Reconcile".into(),
                        secondary: None,
                    },
                    &self.object_ref(&()),
                )
                .await?;
        }

        let last_transition_time = if existing_ready.is_none_or(|r| r.status != status) {
            Time(now)
        } else {
            existing_ready
                .and_then(|r| r.last_transition_time.clone())
                .unwrap_or(Time(now))
        };

        let ready_condition = RestateDeploymentCondition {
            last_transition_time: Some(last_transition_time),
            message: Some(message),
            reason: Some(reason),
            status,
            r#type: "Ready".into(),
        };
        let ready_status = ready_condition.status.clone();

        // Replacing the whole list is what clears the `Reconciling` condition after a
        // suspension.
        rsd_status.conditions = Some(vec![ready_condition]);
        rsd_status.reconciliation = Some(state_after_reconcile(
            self.status.as_ref().and_then(|s| s.reconciliation),
            &ready_status,
        ));

        // Only set labelSelector for ReplicaSet mode (Knative manages pods directly)
        if !is_knative {
            // The selector hash must use the same inputs as the latest ReplicaSet's
            // hash, including the in-process tunnel params. If those can't be
            // resolved the reconcile above already failed; keep the previous
            // selector rather than scoping to a hash no ReplicaSet has.
            if let Ok(in_process_tunnel) = self.in_process_tunnel_params(&ctx) {
                rsd_status.label_selector =
                    latest_version_label_selector(self, in_process_tunnel.as_ref());
            }
        }
        rsd_status.observed_generation = self.metadata.generation;

        // Create the status update
        let new_status = json!({
            "apiVersion": RestateDeployment::api_version(&()),
            "kind": RestateDeployment::kind(&()),
            "status": rsd_status,
        });

        let name = self.name_any();

        debug!("Updating status of RestateDeployment {name} in namespace {namespace}");

        let ps = PatchParams::apply("restate-operator").force();
        let _o = rsd_api
            .patch_status(&name, &ps, &Patch::Apply(new_status))
            .await?;

        result
    }

    pub(super) async fn list_deployments(
        &self,
        ctx: &Context,
        mode: CleanupMode,
    ) -> Result<DeploymentUsageMap> {
        ctx.admit_expensive_operation(self)?;
        let response = self.query_deployment_usage(ctx, mode).await;
        ctx.finish_expensive_operation(self);
        match response {
            Ok(response) => Ok(response.into_map()),
            Err(err) => Err(err),
        }
    }

    async fn query_deployment_usage(
        &self,
        ctx: &Context,
        mode: CleanupMode,
    ) -> Result<DeploymentUsageRows> {
        let sql_query = deployment_usage_query(mode);
        let resp = ctx
            .request(Method::POST, &self.spec.restate.register, "/query")?
            .header(reqwest::header::ACCEPT, "application/json")
            .json(&serde_json::json!({
                "query": sql_query
            }))
            .send()
            .await
            .map_err(Error::AdminCallFailed)?;
        check_admin_response(resp)
            .await?
            .json()
            .await
            .map_err(Error::AdminCallFailed)
    }

    /// How long ago deletion was requested, for pacing the retries of a blocked one.
    /// Zero for an object that is not being deleted, and for a clock that has gone
    /// backwards since the deletion timestamp was stamped.
    fn blocked_for(&self) -> Duration {
        self.metadata
            .deletion_timestamp
            .as_ref()
            .and_then(|deleted_at| (chrono::Utc::now() - deleted_at.0).to_std().ok())
            .unwrap_or_default()
    }

    /// Whether the drain is past its deadline and still waiting, so it should say so.
    ///
    /// Only `onTimeout: hold` is: it is the one setting still stuck once the deadline
    /// passes. An `onTimeout: force` drain can be past its deadline for a single pass --
    /// it crossed while that pass was running -- but force-deregisters on the next one,
    /// so reporting it `Overdue` and telling the user to force would be wrong.
    /// `deletePolicy: force` never waits at all.
    fn drain_overdue(&self) -> bool {
        use crate::resources::restatedeployments::{DeletePolicy, OnTimeout};

        self.spec.restate.delete_policy() == DeletePolicy::Drain
            && self.spec.restate.drain_on_timeout() == OnTimeout::Hold
            && drain_deadline(self).is_some_and(|deadline| deadline <= chrono::Utc::now())
    }

    fn deletion_status(
        &self,
        phase: DeletionPhase,
        message: String,
        blocking: &[BlockingVersion],
    ) -> DeletionStatus {
        let policy = self.spec.restate.delete_policy();
        DeletionStatus {
            policy,
            phase,
            started_at: self.metadata.deletion_timestamp.clone(),
            deadline: if policy == crate::resources::restatedeployments::DeletePolicy::Force {
                None
            } else {
                drain_deadline(self).map(Time)
            },
            // `force` has no deadline to reach, so there is nothing for it to do there
            on_timeout: match policy {
                crate::resources::restatedeployments::DeletePolicy::Force => None,
                crate::resources::restatedeployments::DeletePolicy::Drain => {
                    Some(self.spec.restate.drain_on_timeout())
                }
            },
            message: Some(message),
            total_pending_invocations: blocking
                .iter()
                .map(|version| version.usage.in_flight_invocations() as i64)
                .sum(),
            pending_invocations: blocking
                .iter()
                .map(|version| PendingInvocations {
                    version: version.name.clone(),
                    deployment_id: version.deployment_id.clone(),
                    pinned: version.usage.pinned_invocations as i64,
                    unpinned: version.usage.unpinned_invocations as i64,
                })
                .collect(),
        }
    }

    /// The conditions to keep, or `None` when there are no suspension marks to clear and
    /// so nothing to write.
    fn suspension_to_clear(&self) -> Option<Vec<RestateDeploymentCondition>> {
        let status = self.status.as_ref();

        let stale_state = status
            .and_then(|status| status.reconciliation)
            .is_some_and(|state| state != ReconciliationState::Reconciling);
        let stale_condition = status
            .and_then(|status| status.conditions.as_ref())
            .is_some_and(|conditions| {
                conditions
                    .iter()
                    .any(|cond| cond.r#type == RECONCILING_CONDITION)
            });

        if !stale_state && !stale_condition {
            return None;
        }

        // Applying `conditions` claims the whole list, so send it back minus the one that
        // no longer applies. A stale `Ready` is still worth having; it says when the
        // deployment was last healthy.
        Some(
            status
                .and_then(|status| status.conditions.clone())
                .unwrap_or_default()
                .into_iter()
                .filter(|cond| cond.r#type != RECONCILING_CONDITION)
                .collect(),
        )
    }

    /// Clear the suspension marks left behind by [`Self::reconcile_suspended`].
    ///
    /// A paused RestateDeployment reconciles again the moment it is deleted, so its
    /// `Disabled` state and `Reconciling` condition go stale immediately: they say the
    /// operator is leaving the resource alone while it is in fact tearing it down, next
    /// to a `.status.deletion` that updates every pass. Nothing else clears them, because
    /// the main status apply doesn't run during a deletion.
    ///
    /// Written under the field manager that set them, so this is a hand-off rather than a
    /// fight, and only when there is something to hand over.
    async fn clear_suspension(&self, ctx: &Context, namespace: &str) {
        let Some(conditions) = self.suspension_to_clear() else {
            return;
        };

        let rsd_api: Api<RestateDeployment> = Api::namespaced(ctx.client.clone(), namespace);
        let patch = json!({
            "apiVersion": RestateDeployment::api_version(&()),
            "kind": RestateDeployment::kind(&()),
            "status": {
                "reconciliation": ReconciliationState::Reconciling,
                "conditions": conditions,
            },
        });

        if let Err(err) = rsd_api
            .patch_status(
                &self.name_any(),
                &PatchParams::apply("restate-operator").force(),
                &Patch::Apply(patch),
            )
            .await
        {
            // Cosmetic next to the teardown itself, which is about to carry on regardless.
            warn!(
                "Failed to clear the suspended status of deleting RestateDeployment '{}' in namespace {namespace}: {err}",
                self.name_any(),
            );
        }
    }

    /// Report deletion progress on `.status.deletion`, under its own field manager: the
    /// main status apply doesn't claim that field, and doesn't run at all while deleting.
    async fn report_deletion(&self, ctx: &Context, namespace: &str, deletion: DeletionStatus) {
        let rsd_api: Api<RestateDeployment> = Api::namespaced(ctx.client.clone(), namespace);
        let patch = json!({
            "apiVersion": RestateDeployment::api_version(&()),
            "kind": RestateDeployment::kind(&()),
            "status": { "deletion": deletion },
        });

        if let Err(err) = rsd_api
            .patch_status(
                &self.name_any(),
                &PatchParams::apply("restate-operator/deletion").force(),
                &Patch::Apply(patch),
            )
            .await
        {
            // the error the caller is about to return says the same thing
            warn!(
                "Failed to report deletion status of RestateDeployment '{}' in namespace {namespace}: {err}",
                self.name_any(),
            );
        }
    }

    /// How long until the drain gives up waiting, if it hasn't already.
    fn until_drain_deadline(&self) -> Option<Duration> {
        let deadline = drain_deadline(self)?;
        match (deadline - chrono::Utc::now()).to_std() {
            Ok(remaining) => Some(remaining),
            // An `onTimeout: force` drain that crosses its deadline during an awaited
            // operation must get another pass immediately so cleanup can switch to force
            // mode. A `hold` has no mode transition to wake up for once its reporting
            // deadline is in the past.
            Err(_)
                if self.spec.restate.delete_policy()
                    == crate::resources::restatedeployments::DeletePolicy::Drain
                    && self.spec.restate.drain_on_timeout()
                        == crate::resources::restatedeployments::OnTimeout::Force =>
            {
                Some(Duration::ZERO)
            }
            Err(_) => None,
        }
    }

    // Finalizer cleanup (the object was deleted, ensure nothing is orphaned)
    async fn cleanup(&self, ctx: Arc<Context>, namespace: &str) -> Result<Action> {
        // Before anything reports progress: a paused object arrives here still claiming
        // the operator is leaving it alone.
        self.clear_suspension(&ctx, namespace).await;

        ctx.recorder
            .publish(
                &Event {
                    type_: EventType::Normal,
                    reason: "DeleteRequested".into(),
                    note: Some(format!("Delete `{}`", self.name_any())),
                    action: "Deleting".into(),
                    secondary: None,
                },
                &self.object_ref(&()),
            )
            .await?;

        let rsc_api = Api::<RestateCluster>::all(ctx.client.clone());
        let rs_api = Api::<ReplicaSet>::namespaced(ctx.client.clone(), namespace);

        if let Some(cluster) = &self.spec.restate.register.cluster {
            match rsc_api.get_opt(cluster).await {
                Ok(Some(_)) => {}
                Ok(None) => {
                    // cluster is deleted; no point blocking deletion of the services registered against it.
                    return Ok(Action::await_change());
                }
                Err(err) => {
                    return Err(Error::InvalidRestateConfig(format!(
                        "Referenced Restate cluster '{}' not found: {}",
                        cluster, err
                    )));
                }
            };
        }

        let query_mode = CleanupMode::for_rsd(self);
        if query_mode == CleanupMode::ForceDeleting {
            // Write this before the first admin call, so a force deletion that is
            // blocked on an unavailable Restate endpoint still says what it is doing.
            self.report_deletion(
                &ctx,
                namespace,
                self.deletion_status(
                    DeletionPhase::Forcing,
                    "Deregistering from Restate without waiting for in-flight invocations".into(),
                    &[],
                ),
            )
            .await;
        }

        let deployments = self.list_deployments(&ctx, query_mode).await?;

        // The usage query can be expensive. Re-evaluate after it so a
        // an `onTimeout: force` drain whose deadline passed while the query ran tears down
        // in this pass instead of waiting for the next scheduled reconcile.
        let mode = CleanupMode::for_rsd(self);

        let my_uid = self.uid().expect("RestateDeployment to have a uid");

        // Check if Knative mode
        let is_knative = matches!(
            self.spec.deployment_mode,
            Some(crate::resources::restatedeployments::DeploymentMode::Knative)
        );

        if mode == CleanupMode::ForceDeleting && query_mode != CleanupMode::ForceDeleting {
            // The deadline crossed during the usage query, so this pass changed to force
            // mode after the initial status-reporting point.
            self.report_deletion(
                &ctx,
                namespace,
                self.deletion_status(
                    DeletionPhase::Forcing,
                    "Deregistering from Restate without waiting for in-flight invocations".into(),
                    &[],
                ),
            )
            .await;
        }

        let CleanupOutcome {
            blocking,
            next_removal,
            abandoned,
        } = if is_knative {
            // Knative cleanup path
            reconcilers::knative::cleanup_old_configurations(
                namespace,
                &ctx,
                &my_uid,
                self,
                mode,
                &deployments,
                None,
            )
            .await?
        } else {
            // ReplicaSet cleanup path
            reconcilers::replicaset::cleanup_old_replicasets(
                namespace,
                &ctx,
                &rs_api,
                &my_uid,
                self,
                mode,
                &deployments,
                None,
            )
            .await?
        };

        if !abandoned.is_empty() {
            let abandoned = describe_abandoned_versions(&abandoned);

            warn!(
                "Force-deleting RestateDeployment '{}' from Restate with unfinished invocations against {abandoned}",
                self.name_any(),
            );

            // The teardown above already happened, so a failure to publish here must not
            // fail the pass: the next one finds nothing abandoned and would never retry
            // the event. The warning above it is the record that survives either way.
            if let Err(err) = ctx
                .recorder
                .publish(
                    &Event {
                        type_: EventType::Warning,
                        reason: "ForcedDeletion".into(),
                        note: Some(format!(
                            "Deregistered version(s) with unfinished invocations: {abandoned}"
                        )),
                        action: "Deleting".into(),
                        secondary: None,
                    },
                    &self.object_ref(&()),
                )
                .await
            {
                warn!(
                    "Failed to publish ForcedDeletion event for RestateDeployment '{}' in namespace {namespace}: {err}",
                    self.name_any(),
                );
            }
        }

        if !blocking.is_empty() {
            let blocked_by = describe_blocking_versions(&blocking);
            let timeout_seconds = self.spec.restate.drain_timeout_seconds();
            let overdue = self.drain_overdue();

            debug!(
                "Cannot process deletion of RestateDeployment '{}' from Restate as {} version(s) still have unfinished invocations: {blocked_by}",
                self.name_any(),
                blocking.len(),
            );

            let (phase, message) = if overdue {
                (
                    DeletionPhase::Overdue,
                    format!(
                        "Still waiting on {blocked_by} after the {timeout_seconds}s drain timeout"
                    ),
                )
            } else {
                (
                    DeletionPhase::Draining,
                    format!("Waiting for in-flight invocations against {blocked_by}"),
                )
            };
            self.report_deletion(
                &ctx,
                namespace,
                self.deletion_status(phase, message, &blocking),
            )
            .await;

            let requeue_after = Some(blocked_deletion_requeue(
                self.blocked_for(),
                self.until_drain_deadline(),
            ));

            // Named versions and counts rather than the bare message: `reconcile` publishes
            // this as a Warning event, and it is the only place a stuck deletion explains
            // itself. Unpinned work includes scheduled invocations, whose execution time
            // can be arbitrarily far out, so "wait for it" is not always sound advice.
            if overdue {
                return Err(Error::DeletionDrainOverdue {
                    blocked_by,
                    timeout_seconds,
                    requeue_after,
                });
            }

            return Err(Error::DeploymentInUse {
                blocked_by,
                requeue_after,
            });
        }

        if let Some(next_removal) = next_removal {
            debug!(
                "Cannot process deletion of RestateDeployment '{}' from Restate as there are deployments in the drain holding period",
                self.name_any()
            );

            self.report_deletion(
                &ctx,
                namespace,
                self.deletion_status(
                    DeletionPhase::Draining,
                    "Drained; waiting out the drain delay before removing old versions".into(),
                    &[],
                ),
            )
            .await;

            // Floor at 1s: `num_seconds` truncates, so a deadline under a second away would
            // otherwise requeue with no delay and spin the reconciler — each turn of which
            // re-runs the admin query above — until the deadline passes.
            let secs_until_next_removal = (next_removal - chrono::Utc::now()).num_seconds().max(1);
            let mut requeue_after = Duration::from_secs(secs_until_next_removal as u64);

            // An `onTimeout: force` deadline can pass while cleanup is awaiting Kubernetes
            // or Restate. Never let the drain-delay timer hide that transition; wake up
            // immediately and recompute the cleanup mode.
            if self.spec.restate.delete_policy()
                == crate::resources::restatedeployments::DeletePolicy::Drain
                && self.spec.restate.drain_on_timeout()
                    == crate::resources::restatedeployments::OnTimeout::Force
                && let Some(until_deadline) = self.until_drain_deadline()
            {
                requeue_after = requeue_after.min(until_deadline.max(Duration::from_secs(1)));
            }

            return Err(Error::DeploymentDraining {
                requeue_after: Some(requeue_after),
            });
        }

        Ok(Action::await_change())
    }
}

/// Readiness conditions that occur after the normal ReplicaSet-mode usage query. The controller
/// reports them as `Ready=False` rather than reconciliation errors, so they must opt into the
/// shared retry coordinator here instead of relying on the controller framework's error policy.
fn requeues_after_expensive_admin_work(reason: &str) -> bool {
    matches!(
        reason,
        "AdminCallFailed"
            | "AdminCallRejected"
            | "ForeignDeployment"
            | "NotLatest"
            | "ClusterNotReady"
            | "ReplicaSetNoStatus"
            | "ReplicaSetScaling"
            | "ReplicaSetPodNotReady"
            | "ReplicaSetPodNotAvailable"
    )
}

/// Build the `.status.labelSelector` for a ReplicaSet-mode RestateDeployment,
/// scoped to the latest version's pods by appending the pod-template-hash.
///
/// An HPA targeting the RD scale subresource reads this selector to decide which
/// pods' metrics to aggregate. Without the hash it would also match old
/// ReplicaSets still draining pinned invocations, polluting the averaged metric
/// for the whole (potentially multi-hour) drain window and under-provisioning
/// the genuinely-busy latest version. The hash is computed the same way as the
/// latest versioned ReplicaSet, so the selector matches exactly that RS's pods —
/// which is why it takes the same in-process tunnel params as the hash.
/// See #139.
fn latest_version_label_selector(
    rsd: &RestateDeployment,
    in_process_tunnel: Option<&InProcessTunnelParams>,
) -> Option<String> {
    let pod_template_annotation = reconcilers::replicaset::pod_template_annotation(rsd);
    let latest_hash = reconcilers::replicaset::generate_pod_template_hash(
        rsd,
        &pod_template_annotation,
        in_process_tunnel,
    );

    let mut label_selector = rsd.spec.selector.clone().unwrap_or_default();
    label_selector
        .match_labels
        .get_or_insert_default()
        .insert(POD_TEMPLATE_HASH_LABEL.to_owned(), latest_hash);

    let selector: Option<Selector> = label_selector.try_into().ok();
    selector.as_ref().map(Selector::to_string)
}

fn status_from_replica_set(
    expected_replicas: i32,
    rsd_status: &mut RestateDeploymentStatus,
    rs_status: Option<&ReplicaSetStatus>,
) {
    // Get status information from the current ReplicaSet
    let status_replicas = rs_status.map(|s| s.replicas).unwrap_or(0);
    rsd_status.replicas = status_replicas;
    rsd_status.desired_replicas = Some(expected_replicas);
    rsd_status.ready_replicas = Some(rs_status.and_then(|s| s.ready_replicas).unwrap_or(0));
    let available_replicas = rs_status.and_then(|s| s.available_replicas).unwrap_or(0);
    rsd_status.available_replicas = Some(available_replicas);

    // Calculate unavailable replicas
    let unavailable_replicas = (expected_replicas - available_replicas).max(0);
    rsd_status.unavailable_replicas = Some(unavailable_replicas);
}

pub fn validate_replica_set_status(
    status: Option<&ReplicaSetStatus>,
    expected_replicas: i32,
) -> Result<(), Error> {
    let status = if let Some(status) = status {
        status
    } else {
        return Err(Error::DeploymentNotReady {
            message: "ReplicaSet has no status set; it may have just been created".into(),
            reason: "ReplicaSetNoStatus".into(),
            requeue_after: None,
            replica_set_status: status.cloned().map(Box::new),
        });
    };

    let ReplicaSetStatus {
        replicas,
        ready_replicas,
        available_replicas,
        ..
    } = status;

    let replica_set_status = Some(Box::new(status.clone()));

    if replicas != &expected_replicas {
        return Err(Error::DeploymentNotReady {
            reason: "ReplicaSetScaling".into(),
            message: format!(
                "ReplicaSet has {replicas} replicas instead of the expected {expected_replicas}; it may be scaling up or down"
            ),
            requeue_after: None,
            replica_set_status,
        });
    };

    let ready_replicas = ready_replicas.unwrap_or(0);

    if ready_replicas < expected_replicas {
        return Err(Error::DeploymentNotReady {
            reason: "ReplicaSetPodNotReady".into(),
            message: format!(
                "ReplicaSet has {ready_replicas} ready replicas instead of the expected {expected_replicas}; a pod may not be ready"
            ),
            requeue_after: None,
            replica_set_status,
        });
    }

    let available_replicas = available_replicas.unwrap_or(0);

    if available_replicas < expected_replicas {
        return Err(Error::DeploymentNotReady {
            reason: "ReplicaSetPodNotAvailable".into(),
            message: format!(
                "ReplicaSet has {available_replicas} available replicas instead of the expected {expected_replicas}; a pod may not be available"
            ),
            requeue_after: None,
            replica_set_status,
        });
    }

    Ok(())
}

async fn validate_cluster_status(rsc_api: Api<RestateCluster>, cluster_name: &str) -> Result<()> {
    // Check if the RestateCluster exists and is ready
    let cluster = match rsc_api.get(cluster_name).await {
        Ok(cluster) => cluster,
        Err(kube::Error::Api(err)) if err.reason == "NotFound" => {
            return Err(Error::InvalidRestateConfig(format!(
                "Referenced Restate cluster '{}' not found",
                cluster_name
            )));
        }
        Err(err) => return Err(Error::KubeError(err)),
    };

    // Check if the cluster is ready
    let cluster_ready = cluster
        .status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .and_then(|c| c.iter().find(|cond| cond.r#type == "Ready"))
        .map(|c| c.status == "True")
        .unwrap_or(false);

    if !cluster_ready {
        return Err(Error::DeploymentNotReady {
            message: format!("Referenced Restate cluster '{}' is not ready", cluster_name),
            reason: "ClusterNotReady".into(),
            requeue_after: Some(Duration::from_secs(30)),
            replica_set_status: None,
        });
    }

    Ok(())
}

/// Run the RestateDeployment controller
pub async fn run(client: Client, metrics: Metrics, state: State) {
    let replicasets: Api<ReplicaSet> = Api::all(client.clone());
    let rce: Api<RestateCloudEnvironment> = Api::all(client.clone());
    let secrets: Api<Secret> = Api::namespaced(client.clone(), &state.operator_namespace);
    let services: Api<Service> = Api::all(client.clone());
    let hpas: Api<HorizontalPodAutoscaler> = Api::all(client.clone());

    match wait_for_crd::<RestateDeployment>(
        ReadinessGate::RestateDeployment,
        &client,
        &metrics,
        &state,
    )
    .await
    {
        Ok(CrdWait::Available) => {}
        Ok(CrdWait::ShuttingDown) => return,
        Err(e) => {
            error!("Could not determine whether the RestateDeployment CRD is installed; {e:?}");
            std::process::exit(1);
        }
    }

    // all resources we create have this label
    let cfg = Config::default().labels("app.kubernetes.io/managed-by=restate-operator");
    // but restatedeployment, restatecloudenvironments, secrets dont
    let not_created_cfg = Config::default();

    let (replicasets_store, replicasets_writer) = kube::runtime::reflector::store();
    // Prewarmed, because the registration planner tells a rollback apart from a foreign
    // controller by asking this store which other versions of a RestateDeployment hold
    // Restate's latest revision. An unsynced store answers "none of ours", which reads as a
    // conflict and would park an otherwise healthy rollback at Ready=False until it filled.
    let replicaset_reflector = prewarmed_reflector(
        replicasets_store.clone(),
        replicasets_writer,
        kube::runtime::watcher(replicasets, cfg.clone()),
    )
    .await;

    // A reflector (rather than `.owns(...)`) also gives a queryable cache, so we can
    // check whether a draining version still has an HPA before issuing a delete —
    // avoiding a wasted API call every reconcile.
    let (hpa_store, hpa_writer) = kube::runtime::reflector::store();
    let hpa_reflector =
        kube::runtime::reflector(hpa_writer, kube::runtime::watcher(hpas, cfg.clone()))
            .touched_objects()
            .default_backoff()
            // Wake the owner on a managed HPA's spec change, not the controller's frequent
            // status heartbeats. After the reflector write, so `hpa_store` still sees every update.
            .predicate_filter(predicates::generation);

    let (rce_store, rce_writer) = kube::runtime::reflector::store();
    let rce_reflector = kube::runtime::reflector(
        rce_writer,
        kube::runtime::watcher(rce, not_created_cfg.clone()),
    )
    .touched_objects()
    .default_backoff();

    let (secret_store, secret_writer) = kube::runtime::reflector::store();
    let secret_reflector = kube::runtime::reflector(
        secret_writer,
        kube::runtime::watcher(secrets, not_created_cfg.clone()),
    )
    .touched_objects()
    .default_backoff();

    // RestateDeployment reflector - watch metadata and spec (ignore status-only updates)
    let deployments_for_reflector: Api<RestateDeployment> = Api::all(client.clone());
    let (deployments_store, deployments_writer) = reflector::store();
    let deployments_reflector = reflector(
        deployments_writer,
        watcher(deployments_for_reflector, not_created_cfg.clone()),
    )
    .touched_objects()
    .default_backoff()
    .predicate_filter(
        predicates::generation
            .combine(predicates::labels)
            .combine(predicates::annotations)
            .combine(predicates::finalizers),
    );

    // Create a controller for RestateDeployment
    // Use deployments_reflector with generation predicate to filter out status-only changes
    let mut controller =
        controller::Controller::for_stream(deployments_reflector, deployments_store)
            .shutdown_on_signal()
            .owns_stream(replicaset_reflector);

    let (revision_store, revision_writer) = reflector::store_shared(32);
    let (configuration_store, configuration_writer) = reflector::store_shared(32);
    let configurations: Api<Configuration> = Api::all(client.clone());

    // Check if Knative is installed by checking if the serving.knative.dev API group exists
    let knative_installed = client
        .list_api_groups()
        .await
        .map(|groups| {
            groups
                .groups
                .iter()
                .any(|g| g.name == "serving.knative.dev")
        })
        .unwrap_or(false);

    if knative_installed {
        info!("Knative detected; enabling Knative support");
    } else {
        info!("Knative not detected; disabling Knative support");
    }

    if knative_installed {
        let config_reflector = prewarmed_reflector(
            configuration_store.clone(),
            configuration_writer,
            watcher(configurations, cfg.clone()),
        )
        .await;

        let routes: Api<Route> = Api::all(client.clone());
        let route_watcher = metadata_watcher(routes, cfg.clone())
            .touched_objects()
            .default_backoff();

        let revisions: Api<Revision> = Api::all(client.clone());
        let revision_reflector = prewarmed_reflector(
            revision_store.clone(),
            revision_writer,
            watcher(revisions, cfg.clone()),
        )
        .await;

        controller = controller
            .owns_stream(config_reflector)
            .watches_stream(route_watcher, |meta| {
                // Extract parent RestateDeployment name from annotation
                let name = meta.annotations().get("restate.dev/deployment")?;
                let namespace = meta.namespace()?;
                Some(ObjectRef::new(name).within(&namespace))
            })
            .watches_stream(revision_reflector, |obj| {
                // Extract parent RestateDeployment name from annotation
                let name = obj.annotations().get("restate.dev/deployment")?;
                let namespace = obj.namespace()?;
                Some(ObjectRef::new(name).within(&namespace))
            });
    }

    controller
        // just so that these get polled; we have no way to figure out which rsd may use the updated rce or secret
        .watches_stream(rce_reflector, |_| std::iter::empty())
        .watches_stream(secret_reflector, |_| std::iter::empty())
        .owns(services, cfg.clone())
        .owns_stream(hpa_reflector)
        .run(
            reconcile,
            restate_deployment_error_policy,
            Context::new(
                client,
                replicasets_store,
                rce_store,
                secret_store,
                revision_store,
                configuration_store,
                hpa_store,
                metrics,
                state,
            ),
        )
        .filter_map(|x| async move { Result::ok(x) })
        .for_each(|_| futures::future::ready(()))
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `reconcile` hands every error to the error policy wrapped by the finalizer
    /// machinery, so a policy that matches on the reconciler's own variants only fires if
    /// it looks through the wrapper first.
    #[test]
    fn drain_requeue_survives_the_finalizer_wrapper() {
        let draining = || Error::DeploymentDraining {
            requeue_after: Some(Duration::from_secs(7)),
        };

        let wrapped = Error::FinalizerError(Box::new(
            kube::runtime::finalizer::Error::CleanupFailed(draining()),
        ));

        assert_eq!(
            error_policy(Arc::new(()), &wrapped, ()),
            Action::requeue(Duration::from_secs(7))
        );
        assert_eq!(
            error_policy(Arc::new(()), &draining(), ()),
            Action::requeue(Duration::from_secs(7))
        );
    }

    /// A deletion held by in-flight invocations paces its own retries, so the interval it
    /// computed has to survive the same wrapper.
    #[test]
    fn blocked_deletion_backoff_survives_the_finalizer_wrapper() {
        let wrapped = Error::FinalizerError(Box::new(
            kube::runtime::finalizer::Error::CleanupFailed(Error::DeploymentInUse {
                blocked_by: "greeter-abc123 (1 pinned, 0 unpinned invocations)".into(),
                requeue_after: Some(Duration::from_secs(120)),
            }),
        ));

        assert_eq!(
            error_policy(Arc::new(()), &wrapped, ()),
            Action::requeue(Duration::from_secs(120))
        );
    }

    /// A timed-out drain paces its retries the same way.
    #[test]
    fn timed_out_drain_backoff_survives_the_finalizer_wrapper() {
        let wrapped = Error::FinalizerError(Box::new(
            kube::runtime::finalizer::Error::CleanupFailed(Error::DeletionDrainOverdue {
                blocked_by: "greeter-abc123 (1 pinned, 0 unpinned invocations)".into(),
                timeout_seconds: 3600,
                requeue_after: Some(Duration::from_secs(300)),
            }),
        ));

        assert_eq!(
            error_policy(Arc::new(()), &wrapped, ()),
            Action::requeue(Duration::from_secs(300))
        );
    }

    #[test]
    fn deletion_status_reports_the_versions_holding_the_deletion() {
        let mut rsd: RestateDeployment = serde_json::from_value(json!({
            "apiVersion": "restate.dev/v1beta1",
            "kind": "RestateDeployment",
            "metadata": { "name": "greeter", "namespace": "apps" },
            "spec": {
                "replicas": 1,
                "revisionHistoryLimit": 10,
                "template": { "spec": {} },
                "restate": {
                    "register": { "url": "http://restate:9070/" },
                    "deletePolicy": "drain",
                    "drain": { "timeoutSeconds": 60, "onTimeout": "force" },
                },
            },
        }))
        .expect("test RestateDeployment deserializes");

        let deleted_at = chrono::Utc::now() - chrono::TimeDelta::seconds(600);
        rsd.metadata.deletion_timestamp = Some(Time(deleted_at));

        // an `onTimeout: force` drain past its deadline is about to force, not stuck: it never
        // reports itself timed out, however far past the deadline it is
        assert!(!rsd.drain_overdue());

        let status = rsd.deletion_status(
            DeletionPhase::Overdue,
            "held up".into(),
            &[
                BlockingVersion {
                    name: "greeter-abc123".into(),
                    deployment_id: Some("dp_abc123".into()),
                    usage: DeploymentUsage {
                        latest_for_service: true,
                        pinned_invocations: 2,
                        unpinned_invocations: 3,
                    },
                },
                BlockingVersion {
                    name: "greeter-def456".into(),
                    deployment_id: Some("dp_def456".into()),
                    usage: DeploymentUsage {
                        latest_for_service: false,
                        pinned_invocations: 1,
                        unpinned_invocations: 0,
                    },
                },
            ],
        );

        assert_eq!(status.total_pending_invocations, 6);
        assert_eq!(status.started_at, Some(Time(deleted_at)));
        assert_eq!(
            status.deadline,
            Some(Time(deleted_at + chrono::TimeDelta::seconds(60)))
        );
        assert_eq!(
            status
                .pending_invocations
                .iter()
                .map(|pending| (
                    pending.version.as_str(),
                    pending.deployment_id.as_deref(),
                    pending.pinned,
                    pending.unpinned
                ))
                .collect::<Vec<_>>(),
            vec![
                ("greeter-abc123", Some("dp_abc123"), 2, 3),
                ("greeter-def456", Some("dp_def456"), 1, 0),
            ],
        );

        // ...whereas a holding drain has nothing else to do at the deadline, so it does
        rsd.spec
            .restate
            .drain
            .as_mut()
            .expect("drain block")
            .on_timeout = Some(crate::resources::restatedeployments::OnTimeout::Hold);
        assert!(rsd.drain_overdue());

        // ...and `force` never waits in the first place, so it neither times out nor
        // carries a deadline
        rsd.spec.restate.delete_policy =
            Some(crate::resources::restatedeployments::DeletePolicy::Force);
        assert!(!rsd.drain_overdue());
        let force_status = rsd.deletion_status(DeletionPhase::Forcing, "forcing".into(), &[]);
        assert_eq!(
            force_status.policy,
            crate::resources::restatedeployments::DeletePolicy::Force
        );
        assert_eq!(force_status.deadline, None);
    }

    fn paused_rsd(policy: &str, on_timeout: &str) -> RestateDeployment {
        use crate::controllers::{RECONCILE_ANNOTATION, RECONCILE_DISABLED};

        serde_json::from_value(json!({
            "apiVersion": "restate.dev/v1beta1",
            "kind": "RestateDeployment",
            "metadata": {
                "name": "greeter",
                "namespace": "apps",
                "annotations": { RECONCILE_ANNOTATION: RECONCILE_DISABLED },
            },
            "spec": {
                "replicas": 1,
                "revisionHistoryLimit": 10,
                "template": { "spec": {} },
                "restate": {
                    "register": { "url": "http://restate:9070/" },
                    "deletePolicy": policy,
                    "drain": { "timeoutSeconds": 60, "onTimeout": on_timeout },
                },
            },
        }))
        .expect("test RestateDeployment deserializes")
    }

    /// The pause annotation suspends management of a RestateDeployment, not its teardown:
    /// a paused RSD that is deleted has to reconcile anyway, or it is left registered in
    /// Restate with in-flight invocations and nothing coming back for it.
    #[test]
    fn deleting_a_paused_restatedeployment_is_not_suspended() {
        let mut rsd = paused_rsd("drain", "hold");

        // while it is alive the annotation does what it says
        assert!(reconciliation_suspended(&rsd));

        rsd.metadata.deletion_timestamp = Some(Time(chrono::Utc::now()));
        assert!(
            !reconciliation_suspended(&rsd),
            "a deleted RestateDeployment must reconcile even while paused",
        );
    }

    /// The `Disabled` state and its `Reconciling` condition are wrong the moment a paused
    /// object starts deleting, and nothing else clears them -- the main status apply does
    /// not run during a deletion. Clearing keeps the other conditions: a stale `Ready`
    /// still says when the deployment was last healthy.
    #[test]
    fn a_deleting_rsd_no_longer_claims_to_be_suspended() {
        let mut rsd = paused_rsd("drain", "hold");
        rsd.metadata.deletion_timestamp = Some(Time(chrono::Utc::now()));

        let ready = RestateDeploymentCondition {
            last_transition_time: None,
            message: Some("was healthy".into()),
            reason: Some("Available".into()),
            status: "True".into(),
            r#type: "Ready".into(),
        };
        let suspended = RestateDeploymentCondition {
            last_transition_time: None,
            message: Some(suspended_message()),
            reason: Some(ReconciliationState::Disabled.as_str().into()),
            status: "False".into(),
            r#type: RECONCILING_CONDITION.into(),
        };

        // nothing to hand over: no suspension marks, so no write at all
        let mut status = RestateDeploymentStatus {
            conditions: Some(vec![ready.clone()]),
            reconciliation: Some(ReconciliationState::Reconciling),
            ..Default::default()
        };
        rsd.status = Some(status.clone());
        assert_eq!(rsd.suspension_to_clear(), None);

        // ...whereas a paused one hands over the state and drops only its own condition
        status.conditions = Some(vec![ready.clone(), suspended]);
        status.reconciliation = Some(ReconciliationState::Disabled);
        rsd.status = Some(status.clone());

        let kept = rsd
            .suspension_to_clear()
            .expect("a suspended deleting RSD has marks to clear");
        assert_eq!(
            kept.iter()
                .map(|cond| cond.r#type.as_str())
                .collect::<Vec<_>>(),
            vec!["Ready"],
            "clearing the suspension must not take the other conditions with it",
        );

        // a resume that never finished is just as wrong once the object is deleting
        status.reconciliation = Some(ReconciliationState::ResumingReconciliation);
        status.conditions = Some(vec![ready]);
        rsd.status = Some(status);
        assert!(rsd.suspension_to_clear().is_some());
    }

    /// The finalizer wrapper must not swallow the error's identity on the way to metrics,
    /// or every RestateDeployment failure is labelled `FinalizerError`.
    #[test]
    fn metrics_label_the_error_the_reconciler_raised() {
        let wrapped = Error::FinalizerError(Box::new(
            kube::runtime::finalizer::Error::CleanupFailed(Error::DeletionDrainOverdue {
                blocked_by: "greeter-abc123 (1 pinned, 0 unpinned invocations)".into(),
                timeout_seconds: 3600,
                requeue_after: Some(Duration::from_secs(300)),
            }),
        ));

        assert_eq!(wrapped.metric_label(), "FinalizerError");
        assert_eq!(root_cause(&wrapped).metric_label(), "DeletionDrainOverdue");
    }

    #[test]
    fn an_expired_force_on_timeout_deadline_requeues_immediately() {
        let mut rsd: RestateDeployment = serde_json::from_value(json!({
            "apiVersion": "restate.dev/v1beta1",
            "kind": "RestateDeployment",
            "metadata": { "name": "greeter", "namespace": "apps" },
            "spec": {
                "replicas": 1,
                "revisionHistoryLimit": 10,
                "template": { "spec": {} },
                "restate": {
                    "register": { "url": "http://restate:9070/" },
                    "deletePolicy": "drain",
                    "drain": { "timeoutSeconds": 60, "onTimeout": "force" },
                },
            },
        }))
        .expect("test RestateDeployment deserializes");
        rsd.metadata.deletion_timestamp =
            Some(Time(chrono::Utc::now() - chrono::TimeDelta::seconds(61)));

        assert_eq!(rsd.until_drain_deadline(), Some(Duration::ZERO));

        // A holding drain uses the same deadline for status only; once it has passed,
        // normal backoff applies because there is no cleanup-mode transition to wake.
        rsd.spec
            .restate
            .drain
            .as_mut()
            .expect("drain block")
            .on_timeout = Some(crate::resources::restatedeployments::OnTimeout::Hold);
        assert_eq!(rsd.until_drain_deadline(), None);

        // ...and neither does a `force`, which never had a drain to wait out
        rsd.spec.restate.delete_policy =
            Some(crate::resources::restatedeployments::DeletePolicy::Force);
        rsd.spec
            .restate
            .drain
            .as_mut()
            .expect("drain block")
            .on_timeout = Some(crate::resources::restatedeployments::OnTimeout::Force);
        assert_eq!(rsd.until_drain_deadline(), None);
    }

    #[test]
    fn unrelated_errors_keep_the_blanket_interval() {
        let wrapped = Error::FinalizerError(Box::new(
            kube::runtime::finalizer::Error::CleanupFailed(Error::HashCollision),
        ));

        assert_eq!(
            error_policy(Arc::new(()), &wrapped, ()),
            Action::requeue(Duration::from_secs(30))
        );
        assert_eq!(
            error_policy(Arc::new(()), &Error::HashCollision, ()),
            Action::requeue(Duration::from_secs(30))
        );
    }

    /// Build a minimal ReplicaSet-mode RestateDeployment for selector tests.
    fn make_rsd(match_labels: Option<&[(&str, &str)]>, image: &str) -> RestateDeployment {
        let selector = match_labels.map(|labels| {
            let map: serde_json::Map<String, serde_json::Value> = labels
                .iter()
                .map(|(k, v)| (k.to_string(), json!(v)))
                .collect();
            json!({ "matchLabels": map })
        });

        let spec = serde_json::from_value(json!({
            "replicas": 1,
            "revisionHistoryLimit": 10,
            "selector": selector,
            "template": {
                "metadata": null,
                "spec": { "containers": [{ "name": "main", "image": image }] }
            },
            "restate": {
                "register": { "cluster": null, "cloud": null, "service": null, "url": "http://localhost:9070/" },
                "servicePath": null,
                "useHttp11": null,
                "drainDelaySeconds": null
            }
        }))
        .expect("test RestateDeploymentSpec deserializes");

        RestateDeployment::new("greeter", spec)
    }

    /// The hash the latest versioned ReplicaSet would use, recomputed independently.
    fn latest_hash(rsd: &RestateDeployment) -> String {
        let annotation = reconcilers::replicaset::pod_template_annotation(rsd);
        reconcilers::replicaset::generate_pod_template_hash(rsd, &annotation, None)
    }

    #[test]
    fn label_selector_appends_pod_template_hash_when_no_user_selector() {
        let rsd = make_rsd(None, "greeter:v1");
        let selector = latest_version_label_selector(&rsd, None).expect("selector is set");

        // With no user selector the result is exactly the version scoping.
        assert_eq!(
            selector,
            format!("{}={}", POD_TEMPLATE_HASH_LABEL, latest_hash(&rsd))
        );
    }

    #[test]
    fn label_selector_preserves_user_match_labels() {
        let rsd = make_rsd(Some(&[("app", "greeter")]), "greeter:v1");
        let selector = latest_version_label_selector(&rsd, None).expect("selector is set");

        // Both the user's label and the version scoping must be present (order
        // is not guaranteed by Selector::to_string).
        assert!(
            selector.contains("app=greeter"),
            "user label missing from {selector:?}"
        );
        assert!(
            selector.contains(&format!(
                "{}={}",
                POD_TEMPLATE_HASH_LABEL,
                latest_hash(&rsd)
            )),
            "pod-template-hash missing from {selector:?}"
        );
    }

    #[test]
    fn label_selector_hash_matches_latest_replicaset_name() {
        // The core #139 guarantee: the hash baked into the status selector is the
        // same one used to name/select the latest versioned ReplicaSet, so the
        // HPA aggregates exactly that RS's pods and not old draining versions'.
        let rsd = make_rsd(Some(&[("app", "greeter")]), "greeter:v1");
        let selector = latest_version_label_selector(&rsd, None).expect("selector is set");

        let versioned_name = format!("{}-{}", rsd.name_any(), latest_hash(&rsd));
        let hash = versioned_name
            .rsplit_once('-')
            .map(|(_, h)| h)
            .expect("versioned name has a hash suffix");

        assert!(
            selector.contains(&format!("{}={}", POD_TEMPLATE_HASH_LABEL, hash)),
            "selector {selector:?} does not scope to latest RS hash {hash}"
        );
    }

    #[test]
    fn label_selector_tracks_in_process_tunnel_params() {
        // The #139 guarantee must hold for tunnelMode: in-process too: the selector
        // hash incorporates the same tunnel params as the latest ReplicaSet's hash —
        // computed without them it would scope the HPA to a hash no RS has.
        use crate::resources::restatecloudenvironments::InProcessTunnelParams;

        let rsd = make_rsd(Some(&[("app", "greeter")]), "greeter:v1");
        let params = InProcessTunnelParams {
            environment_id: "env_123".into(),
            region: "us".into(),
            signing_public_key: "publickeyv1_abc".into(),
        };

        let annotation = reconcilers::replicaset::pod_template_annotation(&rsd);
        let rs_hash =
            reconcilers::replicaset::generate_pod_template_hash(&rsd, &annotation, Some(&params));

        let selector = latest_version_label_selector(&rsd, Some(&params)).expect("selector is set");
        assert!(
            selector.contains(&format!("{}={}", POD_TEMPLATE_HASH_LABEL, rs_hash)),
            "selector {selector:?} does not scope to the in-process RS hash {rs_hash}"
        );
        assert_ne!(
            selector,
            latest_version_label_selector(&rsd, None).expect("selector without params"),
            "params must change the selector, or the two hash sites have diverged"
        );
    }

    #[test]
    fn label_selector_changes_with_pod_template() {
        // A different pod template => different version => different selector,
        // so the status selector actually tracks the current version.
        let v1 = make_rsd(Some(&[("app", "greeter")]), "greeter:v1");
        let v2 = make_rsd(Some(&[("app", "greeter")]), "greeter:v2");

        let s1 = latest_version_label_selector(&v1, None).expect("v1 selector");
        let s2 = latest_version_label_selector(&v2, None).expect("v2 selector");

        assert_ne!(s1, s2, "selector should differ across pod templates");

        // ...and it is deterministic for the same template.
        let s1_again = latest_version_label_selector(&v1, None).expect("v1 selector again");
        assert_eq!(s1, s1_again, "selector should be deterministic");
    }

    #[test]
    fn propagated_labels_exclude_applyset_bookkeeping() {
        let labels = BTreeMap::from([
            ("app.kubernetes.io/name".to_string(), "greeter".to_string()),
            (
                "applyset.kubernetes.io/part-of".to_string(),
                "applyset-parent-id".to_string(),
            ),
            (
                "applyset.kubernetes.io/id".to_string(),
                "applyset-parent-id".to_string(),
            ),
        ]);

        assert_eq!(
            propagated_labels(&labels),
            BTreeMap::from([("app.kubernetes.io/name".to_string(), "greeter".to_string())])
        );
    }
}
