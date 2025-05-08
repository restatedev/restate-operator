use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use futures::StreamExt;

use k8s_openapi::api::apps::v1::{Deployment, ReplicaSet};
use k8s_openapi::api::core::v1::Service;

use kube::api::{Api, ListParams, Patch, PatchParams, ResourceExt};
use kube::client::Client;
use kube::runtime::controller;
use kube::runtime::controller::Action;
use kube::runtime::events::{Event, EventType};
use kube::runtime::finalizer::{finalizer, Event as Finalizer};
use kube::runtime::watcher::Config;

use serde_json::json;
use tokio::sync::RwLock;
use tracing::*;

use crate::controllers::{Diagnostics, State};
use crate::metrics::Metrics;
use crate::resources::restateclusters::RestateCluster;
use crate::resources::restateservices::{
    RestateService, RestateServiceCondition, RestateServiceStatus, RESTATE_SERVICE_FINALIZER,
};
use crate::telemetry;
use crate::{Error, Result};

pub(super) struct Context {
    /// Kubernetes client
    pub client: Client,
    /// Diagnostics read by the web server
    pub diagnostics: Arc<RwLock<Diagnostics>>,
    /// Prometheus metrics
    pub metrics: Metrics,
}

impl Context {
    pub fn new(client: Client, metrics: Metrics, state: State) -> Arc<Context> {
        Arc::new(Context {
            client,
            metrics,
            diagnostics: state.diagnostics.clone(),
        })
    }
}

#[instrument(skip(ctx, rs), fields(trace_id))]
async fn reconcile(rs: Arc<RestateService>, ctx: Arc<Context>) -> Result<Action> {
    if let Some(trace_id) = telemetry::get_trace_id() {
        Span::current().record("trace_id", field::display(&trace_id));
    }
    let recorder = ctx
        .diagnostics
        .read()
        .await
        .recorder(ctx.client.clone(), rs.as_ref());
    let _timer = ctx.metrics.count_and_measure::<RestateService>();
    ctx.diagnostics.write().await.last_event = Utc::now();
    let services_api: Api<RestateService> =
        Api::namespaced(ctx.client.clone(), rs.namespace().as_ref().unwrap());

    info!("Reconciling RestateService \"{}\"", rs.name_any());
    match finalizer(
        &services_api,
        RESTATE_SERVICE_FINALIZER,
        rs.clone(),
        |event| async {
            match event {
                Finalizer::Apply(rs) => rs.reconcile(ctx.clone()).await,
                Finalizer::Cleanup(rs) => rs.cleanup(ctx.clone()).await,
            }
        },
    )
    .await
    {
        Ok(action) => Ok(action),
        Err(err) => {
            warn!("reconcile failed: {:?}", err);

            recorder
                .publish(Event {
                    type_: EventType::Warning,
                    reason: "FailedReconcile".into(),
                    note: Some(err.to_string()),
                    action: "Reconcile".into(),
                    secondary: None,
                })
                .await?;

            let err = Error::FinalizerError(Box::new(err));
            ctx.metrics.reconcile_failure(rs.as_ref(), &err);
            Err(err)
        }
    }
}

fn error_policy<K, C>(_rs: Arc<K>, _error: &Error, _ctx: C) -> Action {
    Action::requeue(Duration::from_secs(30))
}

impl RestateService {
    // Reconcile (for non-finalizer related changes)
    async fn reconcile(&self, ctx: Arc<Context>) -> Result<Action> {
        let client = ctx.client.clone();

        // Check if the RestateCluster exists and is ready
        let cluster_name = &self.spec.restate.cluster_ref;
        let clusters_api: Api<RestateCluster> = Api::all(client.clone());
        let cluster = match clusters_api.get(cluster_name).await {
            Ok(cluster) => cluster,
            Err(err) => {
                return Err(Error::InvalidRestateConfig(format!(
                    "Referenced Restate cluster '{}' not found: {}",
                    cluster_name, err
                )));
            }
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
            return Err(Error::ServiceNotReady {
                message: format!("Referenced Restate cluster '{}' is not ready", cluster_name),
                reason: "ClusterNotReady".into(),
                requeue_after: Some(Duration::from_secs(30)),
            });
        }

        // TODO: Implement reconciliation logic for ReplicaSets and Services
        // This would include:
        // 1. Creating/updating Deployments/ReplicaSets
        // 2. Creating/updating Services for each version
        // 3. Integration with Restate for service registration

        // For now, set a simple status
        self.update_status(client).await?;

        // If no events were received, check back every 5 minutes
        Ok(Action::requeue(Duration::from_secs(5 * 60)))
    }

    async fn update_status(&self, client: Client) -> Result<()> {
        let services_api: Api<RestateService> =
            Api::namespaced(client.clone(), self.namespace().as_ref().unwrap());

        // Create a minimal status for now
        let new_status = Patch::Apply(json!({
            "apiVersion": "restate.dev/v1",
            "kind": "RestateService",
            "status": RestateServiceStatus {
                replicas: self.spec.replicas,
                available_replicas: Some(0),
                unavailable_replicas: self.spec.replicas,
                updated_replicas: Some(0),
                observed_generation: Some(self.metadata.generation.unwrap_or(0)),
                versions: Some(vec![]),
                conditions: Some(vec![
                    RestateServiceCondition {
                        last_transition_time: Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(Utc::now())),
                        message: Some("RestateService controller is initializing".into()),
                        reason: Some("Initializing".into()),
                        status: "False".into(),
                        r#type: "Ready".into(),
                    }
                ]),
            }
        }));

        let ps = PatchParams::apply("restate-operator").force();
        let _o = services_api
            .patch_status(&self.name_any(), &ps, &new_status)
            .await?;

        Ok(())
    }

    // Finalizer cleanup (the object was deleted, ensure nothing is orphaned)
    async fn cleanup(&self, ctx: Arc<Context>) -> Result<Action> {
        let recorder = ctx
            .diagnostics
            .read()
            .await
            .recorder(ctx.client.clone(), self);

        // TODO: Implement cleanup of resources:
        // 1. Deregister from Restate
        // 2. Delete Services for each version
        // 3. Ensure ReplicaSets/Deployments are deleted

        recorder
            .publish(Event {
                type_: EventType::Normal,
                reason: "DeleteRequested".into(),
                note: Some(format!("Delete `{}`", self.name_any())),
                action: "Deleting".into(),
                secondary: None,
            })
            .await?;

        Ok(Action::await_change())
    }
}

/// Run the RestateService controller
pub async fn run(client: Client, metrics: Metrics, state: State) {
    let services: Api<RestateService> = Api::all(client.clone());
    let deployments: Api<Deployment> = Api::all(client.clone());
    let replicasets: Api<ReplicaSet> = Api::all(client.clone());
    let kube_services: Api<Service> = Api::all(client.clone());

    if let Err(e) = services.list(&ListParams::default().limit(1)).await {
        error!("RestateService is not queryable; {e:?}. Is the CRD installed?");
        std::process::exit(1);
    }

    // Common label selector for all resources
    let config = Config::default().labels("app.kubernetes.io/managed-by=restate-operator");

    // Create a controller for RestateService
    controller::Controller::new(services, config.clone())
        .shutdown_on_signal()
        .owns(deployments, config.clone())
        .owns(replicasets, config.clone())
        .owns(kube_services, config.clone())
        .run(
            reconcile,
            error_policy,
            Context::new(client, metrics, state),
        )
        .filter_map(|x| async move { Result::ok(x) })
        .for_each(|_| futures::future::ready(()))
        .await;
}
