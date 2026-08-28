use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use futures::StreamExt;

use k8s_openapi::api::apps::v1::Deployment;

use kube::api::{Api, ObjectMeta, Patch, PatchParams};
use kube::client::Client;
use kube::runtime::controller;
use kube::runtime::controller::Action;
use kube::runtime::events::{Event, EventType, Recorder};
use kube::runtime::finalizer::{Event as Finalizer, finalizer};
use kube::runtime::watcher::Config;

use kube::{Resource, ResourceExt};
use serde_json::json;
use tokio::sync::RwLock;
use tracing::*;

use crate::controllers::{
    CrdWait, Diagnostics, RECONCILING_CONDITION, ReadinessGate, State, log_suspended,
    reconciliation_suspended, state_after_reconcile, suspended_message, wait_for_crd,
};
use crate::metrics::Metrics;
use crate::resources::ReconciliationState;
use crate::resources::restatecloudenvironments::{
    RESTATE_CLOUD_ENVIRONMENT_FINALIZER, RestateCloudEnvironment, RestateCloudEnvironmentCondition,
    RestateCloudEnvironmentStatus,
};
use crate::telemetry;
use crate::{Error, Result};

// Import our reconcilers
use crate::controllers::restatecloudenvironment::reconcilers;

pub(super) struct Context {
    /// Kubernetes client
    pub client: Client,
    /// Kubernetes event recorder
    pub recorder: Recorder,
    /// The namespace in which this operator runs
    pub operator_namespace: String,
    /// The default image to use for tunnel client pods
    pub tunnel_client_default_image: String,
    /// Diagnostics read by the web server
    pub diagnostics: Arc<RwLock<Diagnostics>>,
    /// Prometheus metrics
    pub metrics: Metrics,
}

impl Context {
    pub fn new(client: Client, metrics: Metrics, state: State) -> Arc<Context> {
        Arc::new(Context {
            client: client.clone(),
            recorder: Recorder::new(client, "restate-operator".into()),
            operator_namespace: state.operator_namespace,
            tunnel_client_default_image: state.tunnel_client_default_image,
            metrics,
            diagnostics: state.diagnostics.clone(),
        })
    }
}

#[instrument(skip(ctx, rs), fields(trace_id))]
async fn reconcile(rs: Arc<RestateCloudEnvironment>, ctx: Arc<Context>) -> Result<Action> {
    if let Some(trace_id) = telemetry::get_trace_id() {
        Span::current().record("trace_id", field::display(&trace_id));
    }
    let _timer = ctx.metrics.count_and_measure::<RestateCloudEnvironment>();
    ctx.diagnostics.write().await.last_event = Utc::now();

    let services_api: Api<RestateCloudEnvironment> = Api::all(ctx.client.clone());

    // Check this before the finalizer helper, which adds our finalizer to resources that
    // don't have one yet. That's a write, and we've been asked not to write to this one.
    if reconciliation_suspended(rs.as_ref()) {
        return rs.reconcile_suspended(&services_api).await;
    }

    info!("Reconciling RestateCloudEnvironment {}", rs.name_any(),);
    match finalizer(
        &services_api,
        RESTATE_CLOUD_ENVIRONMENT_FINALIZER,
        rs.clone(),
        |event| async {
            match event {
                Finalizer::Apply(rs) => rs.reconcile_status(ctx.clone()).await,
                Finalizer::Cleanup(rs) => rs.cleanup(ctx.clone()).await,
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
            ctx.metrics.reconcile_failure(rs.as_ref(), &err);
            Err(err)
        }
    }
}

fn error_policy<K, C>(_rs: Arc<K>, _: &Error, _ctx: C) -> Action {
    Action::requeue(Duration::from_secs(30))
}

impl RestateCloudEnvironment {
    /// Note the suspension in the status, and leave everything else alone.
    async fn reconcile_suspended(&self, rces: &Api<RestateCloudEnvironment>) -> Result<Action> {
        let name = self.name_any();
        let message = suspended_message();
        let previous = self.status.as_ref().and_then(|s| s.reconciliation);

        log_suspended("RestateCloudEnvironment", &name, previous);

        let existing = self
            .status
            .as_ref()
            .and_then(|s| s.conditions.as_ref())
            .and_then(|c| c.iter().find(|cond| cond.r#type == RECONCILING_CONDITION));
        let now = k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(Utc::now());

        let suspended = RestateCloudEnvironmentCondition {
            last_transition_time: Some(
                match existing {
                    Some(cond) if cond.status == "False" => cond.last_transition_time.clone(),
                    _ => None,
                }
                .unwrap_or(now),
            ),
            message: Some(message),
            reason: Some(ReconciliationState::Disabled.as_str().into()),
            status: "False".into(),
            r#type: RECONCILING_CONDITION.into(),
        };

        let mut conditions: Vec<_> = self
            .status
            .as_ref()
            .and_then(|s| s.conditions.clone())
            .unwrap_or_default()
            .into_iter()
            .filter(|cond| cond.r#type != RECONCILING_CONDITION)
            .collect();
        conditions.push(suspended);

        let new_status = Patch::Apply(json!({
            "apiVersion": RestateCloudEnvironment::api_version(&()),
            "kind": RestateCloudEnvironment::kind(&()),
            "status": RestateCloudEnvironmentStatus {
                conditions: Some(conditions),
                reconciliation: Some(ReconciliationState::Disabled),
            }
        }));
        let ps = PatchParams::apply("restate-operator").force();
        rces.patch_status(&name, &ps, &new_status).await?;

        Ok(Action::await_change())
    }

    /// Reconcile, then record the outcome in the status.
    ///
    /// Without this write a resumed environment would keep the `Disabled` left over from its
    /// suspension, and claim forever that we aren't managing it.
    async fn reconcile_status(&self, ctx: Arc<Context>) -> Result<Action> {
        let rces: Api<RestateCloudEnvironment> = Api::all(ctx.client.clone());
        let name = self.name_any();

        let (result, message, reason, status) = match self.reconcile(ctx, &name).await {
            Ok(action) => (
                Ok(action),
                "Restate Cloud environment reconciled successfully".into(),
                "Reconciled".into(),
                "True".into(),
            ),
            Err(err) => {
                let message = err.to_string();
                (
                    Err(err),
                    message,
                    "FailedReconcile".into(),
                    "Unknown".into(),
                )
            }
        };

        let existing_ready = self
            .status
            .as_ref()
            .and_then(|s| s.conditions.as_ref())
            .and_then(|c| c.iter().find(|cond| cond.r#type == "Ready"));
        let now = k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(Utc::now());

        let mut ready = RestateCloudEnvironmentCondition {
            last_transition_time: Some(
                existing_ready
                    .and_then(|r| r.last_transition_time.clone())
                    .unwrap_or_else(|| now.clone()),
            ),
            message: Some(message),
            reason: Some(reason),
            status,
            r#type: "Ready".into(),
        };

        if existing_ready.map(|r| &r.status) != Some(&ready.status) {
            ready.last_transition_time = Some(now)
        }

        let ready_status = ready.status.clone();
        let new_status = Patch::Apply(json!({
            "apiVersion": RestateCloudEnvironment::api_version(&()),
            "kind": RestateCloudEnvironment::kind(&()),
            "status": RestateCloudEnvironmentStatus {
                // Replacing the whole list is what clears the `Reconciling` condition after
                // a suspension.
                conditions: Some(vec![ready]),
                reconciliation: Some(state_after_reconcile(
                    self.status.as_ref().and_then(|s| s.reconciliation),
                    &ready_status,
                )),
            }
        }));
        let ps = PatchParams::apply("restate-operator").force();
        rces.patch_status(&name, &ps, &new_status).await?;

        result
    }

    // Reconcile (for non-finalizer related changes)
    async fn reconcile(&self, ctx: Arc<Context>, name: &str) -> Result<Action> {
        let oref = self.controller_owner_ref(&()).unwrap();

        let mut annotations = self.annotations().clone();
        // if this is set on the rce, don't propagate it
        annotations.remove("kubectl.kubernetes.io/last-applied-configuration");

        let base_metadata = ObjectMeta {
            name: Some(format!("tunnel-{name}")),
            namespace: Some(ctx.operator_namespace.clone()),
            labels: Some(self.labels().clone()),
            annotations: Some(annotations),
            owner_references: Some(vec![oref.clone()]),
            ..Default::default()
        };

        let tunnel_name = self
            .metadata
            .uid
            .as_deref()
            .expect("RestateCloudEnvironment should have a uid");

        reconcilers::tunnel::reconcile_tunnel(
            &ctx,
            &ctx.operator_namespace,
            &base_metadata,
            tunnel_name,
            &self.spec,
        )
        .await?;

        Ok(Action::requeue(Duration::from_secs(5 * 60)))
    }

    // Finalizer cleanup (the object was deleted, ensure nothing is orphaned)
    async fn cleanup(&self, ctx: Arc<Context>) -> Result<Action> {
        // RestateCloudEnvironment doesn't have any real cleanup, so we just publish an event
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
        Ok(Action::await_change())
    }
}

/// Run the RestateCloudEnvironment controller
pub async fn run(client: Client, metrics: Metrics, state: State) {
    let rce: Api<RestateCloudEnvironment> = Api::all(client.clone());
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), &state.operator_namespace);

    match wait_for_crd::<RestateCloudEnvironment>(
        ReadinessGate::RestateCloudEnvironment,
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
                "Could not determine whether the RestateCloudEnvironment CRD is installed; {e:?}"
            );
            std::process::exit(1);
        }
    }

    // all resources we create have this label
    let cfg = Config::default().labels("app.kubernetes.io/managed-by=restate-operator");
    // but RestateCloudEnvironment doesn\t
    let not_created_cfg = Config::default();

    // Create a controller for RestateCloudEnvironment
    controller::Controller::new(rce, not_created_cfg)
        .shutdown_on_signal()
        .owns(deployments, cfg.clone())
        .run(
            reconcile,
            error_policy,
            Context::new(client, metrics, state),
        )
        .filter_map(|x| async move { Result::ok(x) })
        .for_each(|_| futures::future::ready(()))
        .await;
}
