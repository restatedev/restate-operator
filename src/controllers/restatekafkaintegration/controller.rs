use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use futures::StreamExt;
use k8s_openapi::api::apps::v1::{Deployment, DeploymentStatus};
use k8s_openapi::api::core::v1::ConfigMap;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::api::{Api, ObjectMeta, Patch, PatchParams};
use kube::client::Client;
use kube::runtime::WatchStreamExt;
use kube::runtime::controller::{self, Action};
use kube::runtime::events::{Event, EventType, Recorder};
use kube::runtime::reflector::Store;
use kube::runtime::watcher::Config;
use kube::{Resource, ResourceExt};
use serde_json::json;
use tokio::sync::RwLock;
use tracing::*;

use crate::controllers::restatekafkaintegration::reconcilers;
use crate::controllers::{CrdWait, Diagnostics, ReadinessGate, State, wait_for_crd};
use crate::metrics::Metrics;
use crate::resources::restatecloudenvironments::RestateCloudEnvironment;
use crate::resources::restatekafkaintegrations::{
    RestateKafkaIntegration, RestateKafkaIntegrationCondition, RestateKafkaIntegrationSpec,
};
use crate::telemetry;
use crate::{Error, Result};

/// How often a healthy RestateKafkaIntegration is re-reconciled, as a backstop for anything
/// the watches miss.
const REQUEUE_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// How long to wait before looking again at a Deployment that is not yet available.
const NOT_READY_REQUEUE_INTERVAL: Duration = Duration::from_secs(10);

pub(super) struct Context {
    /// Kubernetes client
    pub client: Client,
    /// Kubernetes event recorder
    pub recorder: Recorder,
    /// Cache of RestateCloudEnvironments, for `spec.restate.ingress.cloud`
    pub rce_store: Store<RestateCloudEnvironment>,
    /// The cluster DNS suffix (e.g. "cluster.local")
    pub cluster_dns: String,
    /// The default image to use for Kafka integration pods
    pub kafka_integration_default_image: String,
    /// Diagnostics read by the web server
    pub diagnostics: Arc<RwLock<Diagnostics>>,
    /// Prometheus metrics
    pub metrics: Metrics,
}

impl Context {
    pub fn new(
        client: Client,
        rce_store: Store<RestateCloudEnvironment>,
        metrics: Metrics,
        state: State,
    ) -> Arc<Context> {
        Arc::new(Context {
            client: client.clone(),
            recorder: Recorder::new(client, "restate-operator".into()),
            rce_store,
            cluster_dns: state.cluster_dns.clone(),
            kafka_integration_default_image: state.kafka_integration_default_image.clone(),
            metrics,
            diagnostics: state.diagnostics.clone(),
        })
    }
}

#[instrument(skip(ctx, rki), fields(trace_id))]
async fn reconcile(rki: Arc<RestateKafkaIntegration>, ctx: Arc<Context>) -> Result<Action> {
    if let Some(trace_id) = telemetry::get_trace_id() {
        Span::current().record("trace_id", field::display(&trace_id));
    }
    let _timer = ctx.metrics.count_and_measure::<RestateKafkaIntegration>();
    ctx.diagnostics.write().await.last_event = Utc::now();

    let namespace = match rki.metadata.namespace.as_deref() {
        Some("") | None => "default",
        Some(namespace) => namespace,
    };

    debug!(
        "Reconciling RestateKafkaIntegration {} in namespace {namespace}",
        rki.name_any()
    );

    // There is nothing to deregister on the Restate side when a RestateKafkaIntegration goes
    // away -- the Deployment and ConfigMap are owned, so garbage collection is enough -- so
    // unlike the other controllers this one needs no finalizer.
    match rki.reconcile_status(ctx.clone(), namespace).await {
        Ok(action) => Ok(action),
        Err(err) => {
            warn!("reconcile failed: {err:?}");

            ctx.recorder
                .publish(
                    &Event {
                        type_: EventType::Warning,
                        reason: "FailedReconcile".into(),
                        note: Some(err.to_string()),
                        action: "Reconcile".into(),
                        secondary: None,
                    },
                    &rki.object_ref(&()),
                )
                .await?;

            ctx.metrics.reconcile_failure(rki.as_ref(), &err);
            Err(err)
        }
    }
}

fn error_policy<K, C>(_rki: Arc<K>, _err: &Error, _ctx: C) -> Action {
    Action::requeue(Duration::from_secs(30))
}

/// The `Ready` condition a reconcile outcome implies.
struct ReadyCondition {
    status: &'static str,
    reason: &'static str,
    message: String,
}

/// Whether a Deployment status means the integration is running.
///
/// `replicas: 0` is a legitimate configuration (a paused integration), and there is nothing
/// unready about it, so it counts as ready.
fn ready_condition(desired_replicas: i32, status: Option<&DeploymentStatus>) -> ReadyCondition {
    let available = status
        .and_then(|status| status.available_replicas)
        .unwrap_or(0);

    if desired_replicas == 0 {
        return ReadyCondition {
            status: "True",
            reason: "ScaledToZero",
            message: "Scaled to zero replicas".into(),
        };
    }

    if available >= 1 {
        ReadyCondition {
            status: "True",
            reason: "Available",
            message: format!("{available}/{desired_replicas} replicas available"),
        }
    } else {
        ReadyCondition {
            status: "False",
            reason: "DeploymentNotAvailable",
            message: format!("{available}/{desired_replicas} replicas available"),
        }
    }
}

/// The condition reason to report for a failed reconcile.
fn error_reason(err: &Error) -> &'static str {
    match err {
        Error::InvalidRestateConfig(_) => "InvalidConfiguration",
        Error::RestateCloudEnvironmentNotFound(_) => "RestateCloudEnvironmentNotFound",
        Error::KubeError(_) => "ApiCallFailed",
        _ => "FailedReconcile",
    }
}

/// Reject a `cloud` reference with no bearer token to go with it.
///
/// `cloud` speaks to Restate Cloud over the public internet, which always wants a token. The
/// operator cannot supply one itself: the RestateCloudEnvironment's own credentials live in a
/// Secret in the operator's namespace, and reaching across namespaces to copy it would need a
/// cluster-wide grant on Secrets. Failing here gives a legible condition instead of pods that
/// crash-loop on 401s.
fn validate_auth_token(spec: &RestateKafkaIntegrationSpec) -> Result<()> {
    if spec.restate.ingress.cloud.is_some() && spec.restate.auth_token.is_none() {
        return Err(Error::InvalidRestateConfig(
            "spec.restate.ingress.cloud requires spec.restate.authToken, a Secret key in this namespace holding a Restate Cloud API token".into(),
        ));
    }

    Ok(())
}

impl RestateKafkaIntegration {
    /// Reconcile the children, returning the Deployment status and the ingress URL used.
    async fn reconcile(
        &self,
        ctx: &Context,
        namespace: &str,
    ) -> Result<(Option<DeploymentStatus>, String)> {
        let name = self.name_any();
        let oref = self
            .controller_owner_ref(&())
            .expect("RestateKafkaIntegration should have a uid");

        let mut annotations = self.annotations().clone();
        // if this is set on the RestateKafkaIntegration, don't propagate it to the children
        annotations.remove("kubectl.kubernetes.io/last-applied-configuration");

        let base_metadata = ObjectMeta {
            name: Some(name.clone()),
            namespace: Some(namespace.to_owned()),
            labels: Some(self.labels().clone()),
            annotations: Some(annotations),
            owner_references: Some(vec![oref]),
            ..Default::default()
        };

        validate_auth_token(&self.spec)?;

        let ingress_url = self
            .spec
            .restate
            .ingress
            .ingress_url(&ctx.rce_store, &ctx.cluster_dns)?;

        // Created before the Deployment that mounts it, and removed only after the Deployment
        // has stopped mounting it, so neither switch between `config` and `configFrom` leaves
        // running pods pointing at an object that is not there.
        if let Some(config) = self.spec.config.as_deref() {
            reconcilers::config::apply_config_map(ctx, namespace, &base_metadata, config).await?;
        }

        let status = reconcilers::deployment::reconcile_deployment(
            ctx,
            namespace,
            &base_metadata,
            &self.spec,
            ingress_url.as_str(),
        )
        .await?;

        if self.spec.config.is_none() {
            reconcilers::config::delete_config_map(ctx, namespace, &base_metadata).await?;
        }

        Ok((status, ingress_url.into()))
    }

    /// Run [`Self::reconcile`] and write the outcome to `status`.
    ///
    /// The status is written whether the reconcile succeeded or not, so a misconfigured
    /// object says why on `kubectl get rki`, and the error is then returned so the caller
    /// still records the failure and requeues.
    async fn reconcile_status(&self, ctx: Arc<Context>, namespace: &str) -> Result<Action> {
        let name = self.name_any();
        let result = self.reconcile(&ctx, namespace).await;

        let (action, deployment_status, ingress_url, condition) = match &result {
            Ok((deployment_status, ingress_url)) => {
                let condition = ready_condition(self.spec.replicas, deployment_status.as_ref());
                let action = if condition.status == "True" {
                    Action::requeue(REQUEUE_INTERVAL)
                } else {
                    Action::requeue(NOT_READY_REQUEUE_INTERVAL)
                };
                (
                    Some(action),
                    deployment_status.clone(),
                    Some(ingress_url.clone()),
                    condition,
                )
            }
            Err(err) => (
                None,
                None,
                None,
                ReadyCondition {
                    status: "False",
                    reason: error_reason(err),
                    message: err.to_string(),
                },
            ),
        };

        // Only move lastTransitionTime when the status actually flipped, so a flapping
        // message does not look like a state change.
        let previous = self
            .status
            .as_ref()
            .and_then(|status| status.conditions.as_ref())
            .and_then(|conditions| conditions.iter().find(|c| c.r#type == "Ready"));
        let last_transition_time = match previous {
            Some(previous) if previous.status == condition.status => {
                previous.last_transition_time.clone()
            }
            _ => Some(Time(Utc::now())),
        };

        // This is a server-side apply, so a field we leave out (or set to null) is *removed*.
        // A failed reconcile says nothing about the pods that are still running, so carry the
        // last observed numbers forward rather than reporting zero replicas.
        let previous_status = self.status.as_ref();
        let observed = deployment_status.as_ref();
        let status = json!({
            "apiVersion": RestateKafkaIntegration::api_version(&()),
            "kind": RestateKafkaIntegration::kind(&()),
            "status": {
                "replicas": observed
                    .and_then(|s| s.replicas)
                    .or_else(|| previous_status.map(|s| s.replicas))
                    .unwrap_or(0),
                "readyReplicas": observed
                    .and_then(|s| s.ready_replicas)
                    .or_else(|| previous_status.and_then(|s| s.ready_replicas)),
                "availableReplicas": observed
                    .and_then(|s| s.available_replicas)
                    .or_else(|| previous_status.and_then(|s| s.available_replicas)),
                "unavailableReplicas": observed
                    .and_then(|s| s.unavailable_replicas)
                    .or_else(|| previous_status.and_then(|s| s.unavailable_replicas)),
                "observedGeneration": self.metadata.generation,
                "ingressUrl": ingress_url
                    .or_else(|| previous_status.and_then(|s| s.ingress_url.clone())),
                "labelSelector": reconcilers::deployment::label_selector_string(&name),
                "conditions": vec![RestateKafkaIntegrationCondition {
                    last_transition_time,
                    message: Some(condition.message),
                    reason: Some(condition.reason.to_owned()),
                    status: condition.status.to_owned(),
                    r#type: "Ready".to_owned(),
                }],
            },
        });

        let rki_api: Api<RestateKafkaIntegration> = Api::namespaced(ctx.client.clone(), namespace);
        let params = PatchParams::apply("restate-operator").force();
        rki_api
            .patch_status(&name, &params, &Patch::Apply(status))
            .await?;

        match (action, result) {
            (Some(action), _) => Ok(action),
            (None, Err(err)) => Err(err),
            // unreachable: an action is produced for every Ok
            (None, Ok(_)) => Ok(Action::requeue(REQUEUE_INTERVAL)),
        }
    }
}

/// Run the RestateKafkaIntegration controller
pub async fn run(client: Client, metrics: Metrics, state: State) {
    let rki: Api<RestateKafkaIntegration> = Api::all(client.clone());
    let deployments: Api<Deployment> = Api::all(client.clone());
    let config_maps: Api<ConfigMap> = Api::all(client.clone());
    let rce: Api<RestateCloudEnvironment> = Api::all(client.clone());

    match wait_for_crd::<RestateKafkaIntegration>(
        ReadinessGate::RestateKafkaIntegration,
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
                "Could not determine whether the RestateKafkaIntegration CRD is installed; {e:?}"
            );
            std::process::exit(1);
        }
    }

    // all resources we create have this label
    let cfg = Config::default().labels("app.kubernetes.io/managed-by=restate-operator");
    // but RestateKafkaIntegration and RestateCloudEnvironment don't
    let not_created_cfg = Config::default();

    let (rce_store, rce_writer) = kube::runtime::reflector::store();
    let rce_reflector = kube::runtime::reflector(
        rce_writer,
        kube::runtime::watcher(rce, not_created_cfg.clone()),
    )
    .touched_objects()
    .default_backoff();

    controller::Controller::new(rki, not_created_cfg)
        .shutdown_on_signal()
        .owns(deployments, cfg.clone())
        .owns(config_maps, cfg)
        // just so that this gets polled; we have no way to figure out which
        // RestateKafkaIntegration may use an updated RestateCloudEnvironment
        .watches_stream(rce_reflector, |_| std::iter::empty())
        .run(
            reconcile,
            error_policy,
            Context::new(client, rce_store, metrics, state),
        )
        .filter_map(|x| async move { Result::ok(x) })
        .for_each(|_| futures::future::ready(()))
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::apps::v1::DeploymentStatus;

    fn status(available: Option<i32>) -> DeploymentStatus {
        DeploymentStatus {
            available_replicas: available,
            ..Default::default()
        }
    }

    #[test]
    fn one_available_replica_is_ready() {
        let condition = ready_condition(2, Some(&status(Some(1))));
        assert_eq!(condition.status, "True");
        assert_eq!(condition.reason, "Available");
        assert_eq!(condition.message, "1/2 replicas available");
    }

    #[test]
    fn no_available_replicas_is_not_ready() {
        let condition = ready_condition(2, Some(&status(Some(0))));
        assert_eq!(condition.status, "False");
        assert_eq!(condition.reason, "DeploymentNotAvailable");
    }

    #[test]
    fn a_missing_status_is_not_ready() {
        assert_eq!(ready_condition(1, None).status, "False");
    }

    #[test]
    fn scaled_to_zero_is_ready() {
        let condition = ready_condition(0, Some(&status(None)));
        assert_eq!(condition.status, "True");
        assert_eq!(condition.reason, "ScaledToZero");
    }

    #[test]
    fn error_reasons_are_specific_where_it_helps() {
        assert_eq!(
            error_reason(&Error::InvalidRestateConfig("bad".into())),
            "InvalidConfiguration"
        );
        assert_eq!(
            error_reason(&Error::RestateCloudEnvironmentNotFound("env".into())),
            "RestateCloudEnvironmentNotFound"
        );
        assert_eq!(error_reason(&Error::InvalidBearerToken), "FailedReconcile");
    }

    fn spec(json: serde_json::Value) -> RestateKafkaIntegrationSpec {
        serde_json::from_value(json).expect("spec deserializes")
    }

    #[test]
    fn cloud_without_an_auth_token_is_rejected() {
        let err = validate_auth_token(&spec(json!({
            "replicas": 1,
            "restate": {"ingress": {"cloud": "my-env"}}
        })))
        .expect_err("cloud needs a token");
        assert!(
            matches!(err, Error::InvalidRestateConfig(message) if message.contains("authToken"))
        );
    }

    #[test]
    fn cloud_with_an_auth_token_is_accepted() {
        assert!(
            validate_auth_token(&spec(json!({
                "replicas": 1,
                "restate": {
                    "ingress": {"cloud": "my-env"},
                    "authToken": {"name": "tok", "key": "k"}
                }
            })))
            .is_ok()
        );
    }

    #[test]
    fn other_references_do_not_need_an_auth_token() {
        for ingress in [
            json!({"cluster": "my-cluster"}),
            json!({"url": "http://restate:8080/"}),
        ] {
            assert!(
                validate_auth_token(&spec(
                    json!({"replicas": 1, "restate": {"ingress": ingress}})
                ))
                .is_ok()
            );
        }
    }

    #[test]
    fn error_policy_requeues() {
        let action = error_policy(
            Arc::new(()),
            &Error::InvalidRestateConfig("bad".into()),
            Arc::new(()),
        );
        assert_eq!(action, Action::requeue(Duration::from_secs(30)));
    }
}
