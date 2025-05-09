use std::collections::HashSet;
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

use serde::Deserialize;
use serde_json::json;
use tokio::sync::RwLock;
use tracing::*;

use crate::controllers::{Diagnostics, State};
use crate::metrics::Metrics;
use crate::resources::restateclusters::RestateCluster;
use crate::resources::restatedeployments::{
    RestateDeployment, RestateDeploymentCondition, RestateDeploymentStatus,
    RestateDeploymentVersion, RESTATE_DEPLOYMENT_FINALIZER,
};
use crate::telemetry;
use crate::{Error, Result};

// Import our reconcilers
use crate::controllers::restatedeployment::reconcilers;

use super::reconcilers::replicaset::RESTATE_REMOVE_VERSION_AT_ANNOTATION;

pub(super) struct Context {
    /// Kubernetes client
    pub client: Client,
    /// Diagnostics read by the web server
    pub diagnostics: Arc<RwLock<Diagnostics>>,
    /// Prometheus metrics
    pub metrics: Metrics,
    pub http_client: reqwest::Client,
}

impl Context {
    pub fn new(client: Client, metrics: Metrics, state: State) -> Arc<Context> {
        Arc::new(Context {
            client,
            metrics,
            diagnostics: state.diagnostics.clone(),
            http_client: reqwest::Client::new(),
        })
    }
}

#[instrument(skip(ctx, rs), fields(trace_id))]
async fn reconcile(rs: Arc<RestateDeployment>, ctx: Arc<Context>) -> Result<Action> {
    if let Some(trace_id) = telemetry::get_trace_id() {
        Span::current().record("trace_id", field::display(&trace_id));
    }
    let recorder = ctx
        .diagnostics
        .read()
        .await
        .recorder(ctx.client.clone(), rs.as_ref());
    let _timer = ctx.metrics.count_and_measure::<RestateDeployment>();
    ctx.diagnostics.write().await.last_event = Utc::now();
    let services_api: Api<RestateDeployment> =
        Api::namespaced(ctx.client.clone(), rs.namespace().as_ref().unwrap());

    info!("Reconciling RestateDeployment \"{}\"", rs.name_any());
    match finalizer(
        &services_api,
        RESTATE_DEPLOYMENT_FINALIZER,
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

impl RestateDeployment {
    // Reconcile (for non-finalizer related changes)
    async fn reconcile(&self, ctx: Arc<Context>) -> Result<Action> {
        let namespace = match self.metadata.namespace.as_deref() {
            Some("") | None => "default",
            Some(ns) => ns,
        };

        let rsc_api: Api<RestateCluster> = Api::all(ctx.client.clone());
        let rss_api = Api::<RestateDeployment>::all(ctx.client.clone());
        let rs_api = Api::<ReplicaSet>::namespaced(ctx.client.clone(), namespace);
        let svc_api = Api::<Service>::namespaced(ctx.client.clone(), namespace);

        // Check if the RestateCluster exists and is ready
        let cluster_name = &self.spec.restate.cluster_ref;
        let cluster = match rsc_api.get(cluster_name).await {
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

        // Generate a hash for the pod template
        let hash = reconcilers::replicaset::generate_pod_template_hash(self);

        let versioned_name = format!("{}-{}", self.name_any(), hash);

        // Create/update the ReplicaSet for this version
        let (replicaset, replicaset_selector) = reconcilers::replicaset::reconcile_replicaset(
            &rs_api,
            self,
            namespace,
            &versioned_name,
            &hash,
        )
        .await?;

        // Create/update the Service for this version
        reconcilers::service::reconcile_service(
            namespace,
            &svc_api,
            &versioned_name,
            replicaset_selector,
            &replicaset,
        )
        .await?;

        let admin_endpoint = format!("http://restate.{}.svc.cluster.local:9070", cluster_name);
        let service_endpoint =
            format!("http://{versioned_name}.{namespace}.svc.cluster.local:9080");

        let active_endpoints = self.list_active_endpoints(&admin_endpoint).await?;

        // Register with Restate cluster using the service URL
        Self::register_service_with_restate(&ctx.http_client, &admin_endpoint, &service_endpoint)
            .await?;

        // Clean up old ReplicaSets that are no longer needed
        let (_active_count, _next_removal) = reconcilers::replicaset::cleanup_old_replicasets(
            namespace,
            &rs_api,
            self,
            &active_endpoints,
        )
        .await?;

        // Update status based on ReplicaSets
        self.update_status_with_replicasets(&rss_api, &rs_api, replicaset)
            .await?;

        // If no events were received, check back every 5 minutes
        Ok(Action::requeue(Duration::from_secs(5 * 60)))
    }

    /// Update status with information from all ReplicaSets
    async fn update_status_with_replicasets(
        &self,
        rss_api: &Api<RestateDeployment>,
        rs_api: &Api<ReplicaSet>,
        current_replicaset: ReplicaSet,
    ) -> Result<()> {
        // Get all current ReplicaSets for this service
        let label_selector = format!("app.kubernetes.io/instance={}", self.name_any());
        let list_params = ListParams::default().labels(&label_selector);
        let all_replicasets = rs_api.list(&list_params).await?;

        // Build the list of versions
        let mut versions = Vec::new();

        let current_name = current_replicaset.name_any();

        for rs in all_replicasets.items {
            let rs_name = rs.name_any();

            // Extract status information
            let rs_status_replicas = rs.status.as_ref().map(|s| s.replicas).unwrap_or(0);
            let rs_available_replicas = rs
                .status
                .as_ref()
                .and_then(|s| s.available_replicas)
                .unwrap_or(0);

            let removing = rs
                .annotations()
                .get(RESTATE_REMOVE_VERSION_AT_ANNOTATION)
                .is_some();

            // Determine if this is the current version
            let is_current = rs_name == current_name;

            // Create the version info
            let version_info = RestateDeploymentVersion {
                name: rs.name_any(),
                replicas: rs_status_replicas,
                available_replicas: Some(rs_available_replicas),
                created_at: rs
                    .metadata
                    .creation_timestamp
                    .as_ref()
                    .map(|t| t.0.to_rfc3339())
                    .unwrap_or_else(|| Utc::now().to_rfc3339()),
                current: is_current,
                status: match (is_current, removing, rs_status_replicas > 0) {
                    (true, _, _) => "Active",
                    (false, false, _) => "Draining", // an outdated service revision that is still needed
                    (false, true, true) => "Drained", // an outdated service revision we haven't scaled down yet
                    (false, true, false) => "Inactive", // an outdated service revision we have scaled down
                }
                .to_owned(),
            };

            versions.push(version_info);
        }

        // Sort versions by creation timestamp (newest first)
        versions.sort_by(|a, b| b.created_at.cmp(&a.created_at));

        // Get status information from the current ReplicaSet
        let status_replicas = current_replicaset
            .status
            .as_ref()
            .map(|spec| spec.replicas)
            .unwrap_or(0);
        let available_replicas = current_replicaset
            .status
            .as_ref()
            .and_then(|s| s.available_replicas)
            .unwrap_or(0);

        // Build ready condition based on current state
        let is_ready = available_replicas > 0 && available_replicas >= status_replicas;
        let ready_condition = RestateDeploymentCondition {
            last_transition_time: Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                Utc::now(),
            )),
            message: Some(if is_ready {
                "RestateDeployment is ready".into()
            } else {
                "Waiting for ReplicaSets to become ready".into()
            }),
            reason: Some(if is_ready {
                "ServiceReady".into()
            } else {
                "WaitingForReplicaSets".into()
            }),
            status: if is_ready {
                "True".into()
            } else {
                "False".into()
            },
            r#type: "Ready".into(),
        };

        // Create progressing condition
        let progressing_condition = RestateDeploymentCondition {
            last_transition_time: Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                Utc::now(),
            )),
            message: Some("RestateDeployment is progressing".into()),
            reason: Some("ServiceProgressing".into()),
            status: "True".into(),
            r#type: "Progressing".into(),
        };

        // Calculate unavailable replicas
        let desired_replicas = self.spec.replicas.unwrap_or(1);
        let unavailable_replicas = desired_replicas - available_replicas;

        // Create the status update
        let new_status = Patch::Apply(json!({
            "apiVersion": "restate.dev/v1",
            "kind": "RestateDeployment",
            "status": RestateDeploymentStatus {
                replicas: Some(status_replicas),
                available_replicas: Some(available_replicas),
                unavailable_replicas: Some(if unavailable_replicas < 0 { 0 } else { unavailable_replicas }),
                updated_replicas: Some(status_replicas),
                observed_generation: Some(self.metadata.generation.unwrap_or(0)),
                versions: Some(versions),
                conditions: Some(vec![ready_condition, progressing_condition]),
            }
        }));

        let ps = PatchParams::apply("restate-operator").force();
        let _o = rss_api
            .patch_status(&self.name_any(), &ps, &new_status)
            .await?;

        Ok(())
    }

    /// Register a service version with the Restate cluster
    async fn register_service_with_restate(
        client: &reqwest::Client,
        admin_endpoint: &str,
        service_endpoint: &str,
    ) -> Result<()> {
        info!("Registering endpoint '{service_endpoint}' to Restate at '{admin_endpoint}'",);

        let _ = client
            .post(format!("{}/deployments/register", admin_endpoint))
            .json(&serde_json::json!({
                "uri": service_endpoint,
            }))
            .send()
            .await
            .map_err(Error::AdminCallFailed)?
            .error_for_status()
            .map_err(Error::AdminCallFailed)?;

        Ok(())
    }

    async fn list_active_endpoints(&self, admin_endpoint: &str) -> Result<HashSet<String>> {
        // This query finds endpoint urls that are the latest for a particular service, or have an active invocation
        let sql_query = r#"
            SELECT d.endpoint
            FROM sys_deployment d
            LEFT JOIN sys_service s ON (d.id = s.deployment_id)
            LEFT JOIN sys_invocation_status i ON (d.id = i.pinned_deployment_id)
            WHERE s.name IS NOT NULL
            OR i.id IS NOT NULL;
        "#;

        #[derive(Deserialize)]
        struct DeploymentQueryResult {
            rows: Vec<DeploymentQueryResultRow>,
        }

        #[derive(Deserialize)]
        struct DeploymentQueryResultRow {
            endpoint: String,
        }

        let client = reqwest::Client::new();
        let response: DeploymentQueryResult = client
            .post(format!("{}/query", admin_endpoint))
            .header(reqwest::header::ACCEPT, "application/json")
            .json(&serde_json::json!({
                "query": sql_query
            }))
            .send()
            .await
            .map_err(Error::AdminCallFailed)?
            .error_for_status()
            .map_err(Error::AdminCallFailed)?
            .json()
            .await
            .map_err(Error::AdminCallFailed)?;

        Ok(response.rows.into_iter().map(|row| row.endpoint).collect())
    }

    // Finalizer cleanup (the object was deleted, ensure nothing is orphaned)
    async fn cleanup(&self, ctx: Arc<Context>) -> Result<Action> {
        let recorder = ctx
            .diagnostics
            .read()
            .await
            .recorder(ctx.client.clone(), self);

        recorder
            .publish(Event {
                type_: EventType::Normal,
                reason: "DeleteRequested".into(),
                note: Some(format!("Delete `{}`", self.name_any())),
                action: "Deleting".into(),
                secondary: None,
            })
            .await?;

        let namespace = match self.metadata.namespace.as_deref() {
            Some("") | None => "default",
            Some(ns) => ns,
        };

        let rsc_api = Api::<RestateCluster>::all(ctx.client.clone());
        let rs_api = Api::<ReplicaSet>::namespaced(ctx.client.clone(), namespace);

        let cluster_name = &self.spec.restate.cluster_ref;
        match rsc_api.get_opt(cluster_name).await {
            Ok(Some(_)) => {}
            Ok(None) => {
                // cluster is deleted; no point blocking deletion of the services registered against it.
                return Ok(Action::await_change());
            }
            Err(err) => {
                return Err(Error::InvalidRestateConfig(format!(
                    "Referenced Restate cluster '{}' not found: {}",
                    cluster_name, err
                )));
            }
        };

        let admin_endpoint = format!("http://restate.{}.svc.cluster.local:9070", cluster_name);

        let active_endpoints = self.list_active_endpoints(&admin_endpoint).await?;

        let (active_count, next_removal) = reconcilers::replicaset::cleanup_old_replicasets(
            namespace,
            &rs_api,
            self,
            &active_endpoints,
        )
        .await?;

        if active_count > 0 {
            info!(
                "Cannot process deletion of RestateDeployment '{}' from Restate as there are {} active deployments that rely on it",
                self.name_any(), active_count
            );
            return Ok(Action::requeue(Duration::from_secs(60 * 5)));
        }

        if let Some(next_removal) = next_removal {
            info!(
                "Cannot process deletion of RestateDeployment '{}' from Restate as there are deployments in the drain holding period",
                self.name_any()
            );
            return Ok(Action::requeue(next_removal));
        }

        Ok(Action::await_change())
    }
}

/// Run the RestateDeployment controller
pub async fn run(client: Client, metrics: Metrics, state: State) {
    let services: Api<RestateDeployment> = Api::all(client.clone());
    let deployments: Api<Deployment> = Api::all(client.clone());
    let replicasets: Api<ReplicaSet> = Api::all(client.clone());
    let kube_services: Api<Service> = Api::all(client.clone());

    if let Err(e) = services.list(&ListParams::default().limit(1)).await {
        error!("RestateDeployment is not queryable; {e:?}. Is the CRD installed?");
        std::process::exit(1);
    }

    // Common label selector for all resources
    let config = Config::default().labels("app.kubernetes.io/managed-by=restate-operator");

    // Create a controller for RestateDeployment
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
