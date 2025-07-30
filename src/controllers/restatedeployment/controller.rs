use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use futures::StreamExt;

use k8s_openapi::api::apps::v1::{ReplicaSet, ReplicaSetStatus};
use k8s_openapi::api::core::v1::Service;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::api::{
    Api, ListParams, ObjectMeta, PartialObjectMetaExt, Patch, PatchParams, ResourceExt,
};
use kube::client::Client;
use kube::core::subresource::Scale;
use kube::core::Selector;
use kube::runtime::controller::Action;
use kube::runtime::events::{Event, EventType, Recorder};
use kube::runtime::finalizer::{finalizer, Event as Finalizer};
use kube::runtime::reflector::Store;
use kube::runtime::watcher::Config;
use kube::runtime::{controller, WatchStreamExt};

use kube::Resource;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::RwLock;
use tracing::*;
use url::Url;

use crate::controllers::{service_url, Diagnostics, State};
use crate::metrics::Metrics;
use crate::resources::restateclusters::RestateCluster;
use crate::resources::restatedeployments::{
    RestateDeployment, RestateDeploymentCondition, RestateDeploymentStatus,
    RESTATE_DEPLOYMENT_FINALIZER,
};
use crate::telemetry;
use crate::{Error, Result};

// Import our reconcilers
use crate::controllers::restatedeployment::reconcilers;

use super::reconcilers::replicaset::{POD_TEMPLATE_HASH_LABEL, RESTATE_POD_TEMPLATE_ANNOTATION};

pub(super) const RESTATE_DEPLOYMENT_ID_ANNOTATION: &str = "restate.dev/deployment-id";
pub(super) const OWNED_BY_LABEL: &str = "restate.dev/owned-by";
pub(super) const APP_MANAGED_BY_LABEL: &str = "app.kubernetes.io/managed-by";

pub(super) struct Context {
    /// Kubernetes client
    pub client: Client,
    /// Kubernetes event recorder
    pub recorder: Recorder,
    /// Store for replica sets
    pub replicasets_store: Store<ReplicaSet>,
    /// Diagnostics read by the web server
    pub diagnostics: Arc<RwLock<Diagnostics>>,
    /// Prometheus metrics
    pub metrics: Metrics,
    /// HTTP client
    pub http_client: reqwest::Client,
}

impl Context {
    pub fn new(
        client: Client,
        replicasets_store: Store<ReplicaSet>,
        metrics: Metrics,
        state: State,
    ) -> Arc<Context> {
        Arc::new(Context {
            client: client.clone(),
            recorder: Recorder::new(client, "restate-operator".into()),
            replicasets_store,
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
    let _timer = ctx.metrics.count_and_measure::<RestateDeployment>();
    ctx.diagnostics.write().await.last_event = Utc::now();

    let namespace = match rs.metadata.namespace.as_deref() {
        Some("") | None => "default",
        Some(ns) => ns,
    };

    let services_api: Api<RestateDeployment> = Api::namespaced(ctx.client.clone(), namespace);

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
            ctx.metrics.reconcile_failure(rs.as_ref(), &err);
            Err(err)
        }
    }
}

fn error_policy<K, C>(_rs: Arc<K>, _: &Error, _ctx: C) -> Action {
    Action::requeue(Duration::from_secs(30))
}

impl RestateDeployment {
    // Reconcile (for non-finalizer related changes)
    async fn reconcile(
        &self,
        ctx: Arc<Context>,
        namespace: &str,
    ) -> Result<(ReplicaSet, Option<chrono::DateTime<chrono::Utc>>)> {
        let rsc_api: Api<RestateCluster> = Api::all(ctx.client.clone());
        let rs_api = Api::<ReplicaSet>::namespaced(ctx.client.clone(), namespace);
        let svc_api = Api::<Service>::namespaced(ctx.client.clone(), namespace);

        let admin_endpoint = self.spec.restate.register.admin_url()?;

        let pod_template_annotation = reconcilers::replicaset::pod_template_annotation(self);

        // Generate a hash for the pod template
        let hash =
            reconcilers::replicaset::generate_pod_template_hash(self, &pod_template_annotation);
        let deployment_name = self.name_any();
        let versioned_name = format!("{deployment_name}-{hash}");

        let replicaset_selector = match &self.spec.selector.match_labels {
            None => BTreeMap::from([(POD_TEMPLATE_HASH_LABEL.to_owned(), hash.clone())]),
            Some(match_labels) => {
                let mut match_labels = match_labels.clone();
                match_labels.insert(POD_TEMPLATE_HASH_LABEL.to_owned(), hash.clone());
                match_labels
            }
        };

        let mut annotations = self.annotations().clone();
        // if this is set on the rsd, don't propagate it
        annotations.remove("kubectl.kubernetes.io/last-applied-configuration");

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
                annotations
            },
            &hash,
        )
        .await;

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

                let my_uid = self.uid().expect("RestateDeployment to have a uid");

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

        let mut service_labels = self.labels().clone();
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

        let service_endpoint = service_url(
            &versioned_name,
            namespace,
            9080,
            self.spec.restate.service_path.as_deref(),
        )?;

        let mut deployments = self
            .list_deployments(&ctx.http_client, &admin_endpoint)
            .await?;

        let existing_deployment_id = replicaset
            .annotations()
            .get(RESTATE_DEPLOYMENT_ID_ANNOTATION);

        // if the repliceset doesn't have a deployment id, or its deployment id is not active, register it
        if existing_deployment_id.is_none_or(|existing_deployment_id| {
            !deployments
                .get(existing_deployment_id)
                .cloned()
                .unwrap_or_default()
        }) {
            if let Some(cluster_name) = &self.spec.restate.register.cluster {
                // wait for the cluster to be ready before registering to it
                validate_cluster_status(rsc_api, cluster_name).await?;
            };

            // wait for the replicaset to be ready before registering it
            validate_replica_set_status(replicaset.status.as_ref(), self.spec.replicas)?;

            // Register the latest version with Restate cluster using the service URL
            let deployment_id = Self::register_service_with_restate(
                &ctx.http_client,
                &admin_endpoint,
                &service_endpoint,
                self.spec.restate.use_http11.as_ref().cloned(),
            )
            .await?;
            // if registration succeeded, treat this as an active endpoint
            // if we fail after this point we will re-register and should get the same deployment id
            deployments.insert(deployment_id.clone(), true);

            debug!("Updating deployment-id annotation of ReplicaSet/Service {versioned_name} in namespace {namespace}");

            // store the id against the versioned objects
            let params = PatchParams::apply("restate-operator/deployment-id").force();
            let patch = ObjectMeta {
                annotations: Some(
                    [(RESTATE_DEPLOYMENT_ID_ANNOTATION.to_string(), deployment_id)].into(),
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

        // Clean up old ReplicaSets that are no longer needed
        let (_, next_removal) = reconcilers::replicaset::cleanup_old_replicasets(
            namespace,
            &rs_api,
            &ctx.replicasets_store,
            &ctx.http_client,
            &admin_endpoint,
            self,
            &deployments,
        )
        .await?;

        Ok((replicaset, next_removal))
    }

    async fn reconcile_status(&self, ctx: Arc<Context>, namespace: &str) -> Result<Action> {
        let rsd_api: Api<RestateDeployment> = Api::namespaced(ctx.client.clone(), namespace);

        let now = chrono::Utc::now();

        let mut rsd_status = self.status.clone().unwrap_or_default();

        // Build ready condition based on current state
        let existing_ready = self
            .status
            .as_ref()
            .and_then(|s| s.conditions.as_ref())
            .and_then(|c| c.iter().find(|cond| cond.r#type == "Ready"));

        let (result, message, reason, status) = match self.reconcile(ctx, namespace).await {
            Ok((current_replicaset, next_removal)) => {
                let action = match next_removal {
                    Some(next_removal) if next_removal < now => Action::requeue(Duration::ZERO), // immediate requeue
                    Some(next_removal) => {
                        let secs = (next_removal - now).num_seconds() as u64;
                        if secs < 5 * 60 {
                            Action::requeue(Duration::from_secs(secs))
                        } else {
                            Action::requeue(Duration::from_secs(5 * 60))
                        }
                    }
                    None => Action::requeue(Duration::from_secs(5 * 60)),
                };

                status_from_replica_set(
                    self.spec.replicas,
                    &mut rsd_status,
                    current_replicaset.status.as_ref(),
                );

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
                    // requeue immediately, although in practice the status update should requeue us
                    Ok(Action::requeue(Duration::ZERO)),
                    "Encountered a ReplicaSet hash collision, will retry with a new template hash"
                        .into(),
                    "HashCollision".into(),
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
        };

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

        rsd_status.conditions = Some(vec![ready_condition]);

        let selector: Option<Selector> = self.spec.selector.clone().try_into().ok();
        rsd_status.label_selector = selector.as_ref().map(Selector::to_string);
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

    /// Register a service version with the Restate cluster
    async fn register_service_with_restate(
        client: &reqwest::Client,
        admin_endpoint: &Url,
        service_endpoint: &Url,
        use_http11: Option<bool>,
    ) -> Result<String> {
        debug!("Registering endpoint '{service_endpoint}' to Restate at '{admin_endpoint}'",);

        #[derive(Deserialize)]
        struct DeploymentResponse {
            id: String,
        }

        let mut payload = serde_json::json!({
            "uri": service_endpoint,
        });

        if let Some(use_http11) = use_http11 {
            payload["use_http_11"] = serde_json::Value::Bool(use_http11);
        }

        let resp: DeploymentResponse = client
            .post(admin_endpoint.join("/deployments")?)
            .json(&payload)
            .send()
            .await
            .map_err(Error::AdminCallFailed)?
            .error_for_status()
            .map_err(Error::AdminCallFailed)?
            .json()
            .await
            .map_err(Error::AdminCallFailed)?;

        Ok(resp.id)
    }

    pub async fn list_deployments(
        &self,
        http_client: &reqwest::Client,
        admin_endpoint: &Url,
    ) -> Result<HashMap<String, bool>> {
        // This query finds deployments, noting those that are the latest for a particular service, or have an active invocation
        let sql_query = r#"
            SELECT d.id as deployment_id, (s.name IS NOT NULL OR i.id IS NOT NULL) as active
            FROM sys_deployment d
            LEFT JOIN sys_service s ON (d.id = s.deployment_id)
            LEFT JOIN sys_invocation_status i ON (d.id = i.pinned_deployment_id);
        "#;

        #[derive(Deserialize)]
        struct DeploymentQueryResult {
            rows: Vec<DeploymentQueryResultRow>,
        }

        #[derive(Deserialize)]
        struct DeploymentQueryResultRow {
            deployment_id: String,
            active: bool,
        }

        let response: DeploymentQueryResult = http_client
            .post(admin_endpoint.join("/query")?)
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

        let mut endpoints = HashMap::with_capacity(response.rows.len());

        for row in response.rows {
            match endpoints.entry(row.deployment_id) {
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    // two rows with same deployment id shouldnt happen...
                    // we treat the deployment as active if any row is active
                    if !entry.get() {
                        entry.insert(row.active);
                    }
                }
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(row.active);
                }
            }
        }

        Ok(endpoints)
    }

    // Finalizer cleanup (the object was deleted, ensure nothing is orphaned)
    async fn cleanup(&self, ctx: Arc<Context>, namespace: &str) -> Result<Action> {
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

        let admin_endpoint = self.spec.restate.register.admin_url()?;

        let deployments = self
            .list_deployments(&ctx.http_client, &admin_endpoint)
            .await?;

        let (active_count, next_removal) = reconcilers::replicaset::cleanup_old_replicasets(
            namespace,
            &rs_api,
            &ctx.replicasets_store,
            &ctx.http_client,
            &admin_endpoint,
            self,
            &deployments,
        )
        .await?;

        if active_count > 0 {
            debug!(
                "Cannot process deletion of RestateDeployment '{}' from Restate as there are {} active deployments that rely on it",
                self.name_any(),
                active_count
            );
            return Err(Error::DeploymentInUse);
        }

        if let Some(next_removal) = next_removal {
            debug!(
                "Cannot process deletion of RestateDeployment '{}' from Restate as there are deployments in the drain holding period",
                self.name_any()
            );

            let secs_until_next_removal = (next_removal - chrono::Utc::now()).num_seconds().max(0);

            return Err(Error::DeploymentDraining {
                requeue_after: Some(Duration::from_secs(secs_until_next_removal as u64)),
            });
        }

        Ok(Action::await_change())
    }
}

fn status_from_replica_set(
    expected_replicas: i32,
    rsd_status: &mut RestateDeploymentStatus,
    rs_status: Option<&ReplicaSetStatus>,
) {
    // Get status information from the current ReplicaSet
    let status_replicas = rs_status.map(|s| s.replicas).unwrap_or(0);
    rsd_status.replicas = status_replicas;
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
            message: "ReplicaSetNoStatus".into(),
            reason: "ReplicaSet has no status set; it may have just been created".into(),
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
        return Err(Error::DeploymentNotReady { reason: "ReplicaSetScaling".into(), message: format!("ReplicaSet has {replicas} replicas instead of the expected {expected_replicas}; it may be scaling up or down"), requeue_after: None, replica_set_status });
    };

    let ready_replicas = ready_replicas.unwrap_or(0);

    if ready_replicas < expected_replicas {
        return Err(Error::DeploymentNotReady { reason: "ReplicaSetPodNotReady".into(), message: format!("ReplicaSet has {ready_replicas} ready replicas instead of the expected {expected_replicas}; a pod may not be ready"), requeue_after: None, replica_set_status });
    }

    let available_replicas = available_replicas.unwrap_or(0);

    if available_replicas < expected_replicas {
        return Err(Error::DeploymentNotReady { reason: "ReplicaSetPodNotAvailable".into(), message: format!("ReplicaSet has {available_replicas} available replicas instead of the expected {expected_replicas}; a pod may not be available"), requeue_after: None, replica_set_status });
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
    let deployments: Api<RestateDeployment> = Api::all(client.clone());
    let replicasets: Api<ReplicaSet> = Api::all(client.clone());
    let services: Api<Service> = Api::all(client.clone());

    if let Err(e) = services.list(&ListParams::default().limit(1)).await {
        error!("RestateDeployment is not queryable; {e:?}. Is the CRD installed?");
        std::process::exit(1);
    }

    // all resources we create have this label
    let cfg = Config::default().labels("app.kubernetes.io/managed-by=restate-operator");
    // but restatedeployments themselves dont
    let rsd_cfg = Config::default();

    let (replicasets_store, replicasets_writer) = kube::runtime::reflector::store();
    let replicaset_reflector = kube::runtime::reflector(
        replicasets_writer,
        kube::runtime::watcher(replicasets, cfg.clone()),
    )
    .touched_objects()
    .default_backoff();

    // Create a controller for RestateDeployment
    controller::Controller::new(deployments, rsd_cfg)
        .shutdown_on_signal()
        .owns_stream(replicaset_reflector)
        .owns(services, cfg.clone())
        .run(
            reconcile,
            error_policy,
            Context::new(client, replicasets_store, metrics, state),
        )
        .filter_map(|x| async move { Result::ok(x) })
        .for_each(|_| futures::future::ready(()))
        .await;
}
