use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use chrono::Utc;
use futures::StreamExt;
use k8s_openapi::api::apps::v1::{StatefulSet, StatefulSetStatus};
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{
    ConfigMap, Namespace, PersistentVolumeClaim, Service, ServiceAccount, ServiceSpec,
};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use k8s_openapi::api::policy::v1::PodDisruptionBudget;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{APIGroup, ObjectMeta};

use kube::core::object::HasStatus;
use kube::core::PartialObjectMeta;
use kube::runtime::events::Recorder;
use kube::runtime::reflector::{ObjectRef, Store};
use kube::runtime::{metadata_watcher, reflector, watcher, Predicate, WatchStreamExt};
use kube::{
    api::{Api, ListParams, Patch, PatchParams, ResourceExt},
    client::Client,
    runtime::{
        controller::{Action, Controller},
        events::{Event, EventType},
        finalizer::{finalizer, Event as Finalizer},
        watcher::Config,
    },
    Resource,
};
use serde::Serialize;
use serde_json::json;
use tokio::{sync::RwLock, time::Duration};
use tracing::*;

use crate::controllers::{Diagnostics, State};
use crate::resources::podidentityassociations::PodIdentityAssociation;
use crate::resources::restateclusters::{
    RestateCluster, RestateClusterCondition, RestateClusterStatus, RESTATE_CLUSTER_FINALIZER,
};
use crate::resources::secretproviderclasses::SecretProviderClass;
use crate::resources::securitygrouppolicies::SecurityGroupPolicy;
use crate::{telemetry, Error, Metrics, Result};

use super::reconcilers::compute::reconcile_compute;
use super::reconcilers::network_policies::reconcile_network_policies;
use super::reconcilers::object_meta;
use super::reconcilers::signing_key::reconcile_signing_key;

// Context for our reconciler
#[derive(Clone)]
pub(super) struct Context {
    /// Kubernetes client
    pub client: Client,
    /// Kubernetes event recorder
    pub recorder: Recorder,
    // Store for pvc metadata
    pub pvc_meta_store: Store<PartialObjectMeta<PersistentVolumeClaim>>,
    // Store for statefulsets
    pub ss_store: Store<StatefulSet>,
    // If set, watch PodIdentityAssociation resources, and if requested create them against this cluster
    pub aws_pod_identity_association_cluster: Option<String>,
    // Our namespace, needed to support the case where restate clusters need to be reached by the operator
    pub operator_namespace: Option<String>,
    // The name of a label that can select the operator, needed to support the case where restate clusters need to be reached by the operator
    pub operator_label_name: Option<String>,
    // The value of the label named operator_label_name that will select the operator, needed to support the case where restate clusters need to be reached by the operator
    pub operator_label_value: Option<String>,
    // Whether the EKS SecurityGroupPolicy CRD is installed
    pub security_group_policy_installed: bool,
    // Whether the SecretProviderClass CRD is installed
    pub secret_provider_class_installed: bool,
    /// Diagnostics read by the web server
    pub diagnostics: Arc<RwLock<Diagnostics>>,
    /// Prometheus metrics
    pub metrics: Metrics,
}

impl Context {
    pub fn new(
        client: Client,
        metrics: Metrics,
        state: State,
        pvc_meta_store: Store<PartialObjectMeta<PersistentVolumeClaim>>,
        ss_store: Store<StatefulSet>,
        security_group_policy_installed: bool,
        secret_provider_class_installed: bool,
    ) -> Arc<Context> {
        Arc::new(Context {
            client: client.clone(),
            recorder: Recorder::new(client, "restate-operator".into()),
            pvc_meta_store,
            ss_store,
            aws_pod_identity_association_cluster: state
                .aws_pod_identity_association_cluster
                .clone(),
            operator_namespace: state.operator_namespace.clone(),
            operator_label_name: state.operator_label_name.clone(),
            operator_label_value: state.operator_label_value.clone(),
            security_group_policy_installed,
            secret_provider_class_installed,
            diagnostics: state.diagnostics.clone(),
            metrics,
        })
    }
}

#[instrument(skip(ctx, rc), fields(trace_id))]
async fn reconcile(rc: Arc<RestateCluster>, ctx: Arc<Context>) -> Result<Action> {
    if let Some(trace_id) = telemetry::get_trace_id() {
        Span::current().record("trace_id", field::display(&trace_id));
    }
    let _timer = ctx.metrics.count_and_measure::<RestateCluster>();
    ctx.diagnostics.write().await.last_event = Utc::now();
    let rcs: Api<RestateCluster> = Api::all(ctx.client.clone());

    info!("Reconciling RestateCluster \"{}\"", rc.name_any());
    match finalizer(&rcs, RESTATE_CLUSTER_FINALIZER, rc.clone(), |event| async {
        match event {
            Finalizer::Apply(rc) => rc.reconcile_status(ctx.clone()).await,
            Finalizer::Cleanup(rc) => rc.cleanup(ctx.clone()).await,
        }
    })
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
                    &rc.object_ref(&()),
                )
                .await?;

            let err = Error::FinalizerError(Box::new(err));
            ctx.metrics.reconcile_failure(rc.as_ref(), &err);
            Err(err)
        }
    }
}

fn error_policy<K, C>(_rc: Arc<K>, _error: &Error, _ctx: C) -> Action {
    Action::requeue(Duration::from_secs(30))
}

impl RestateCluster {
    // Reconcile (for non-finalizer related changes)
    async fn reconcile(&self, ctx: Arc<Context>, name: &str) -> Result<()> {
        let client = ctx.client.clone();
        let nss: Api<Namespace> = Api::all(client.clone());

        let oref = self.controller_owner_ref(&()).unwrap();

        let base_metadata = ObjectMeta {
            name: Some(name.into()),
            labels: Some(self.labels().clone()),
            annotations: Some(self.annotations().clone()),
            owner_references: Some(vec![oref.clone()]),
            ..Default::default()
        };

        if let Some(ns) = nss.get_metadata_opt(name).await? {
            // check to see if extant namespace is managed by us
            if !ns
                .metadata
                .owner_references
                .map(|orefs| orefs.contains(&oref))
                .unwrap_or(false)
            {
                return Err(Error::NameConflict);
            }
        }

        apply_namespace(
            &nss,
            Namespace {
                metadata: object_meta(&base_metadata, name),
                ..Default::default()
            },
        )
        .await?;

        reconcile_network_policies(
            &ctx,
            name,
            &base_metadata,
            self.spec
                .security
                .as_ref()
                .and_then(|s| s.network_peers.as_ref()),
            self.spec
                .security
                .as_ref()
                .and_then(|s| s.allow_operator_access_to_admin)
                .unwrap_or(true),
            self.spec
                .security
                .as_ref()
                .and_then(|s| s.network_egress_rules.as_deref()),
            self.spec
                .security
                .as_ref()
                .is_some_and(|s| s.aws_pod_identity_association_role_arn.is_some()),
        )
        .await?;

        let signing_key = reconcile_signing_key(
            &ctx,
            name,
            &base_metadata,
            self.spec
                .security
                .as_ref()
                .and_then(|s| s.request_signing_private_key.as_ref()),
        )
        .await?;

        reconcile_compute(&ctx, name, &base_metadata, &self.spec, signing_key).await?;

        Ok(())
    }

    async fn reconcile_status(&self, ctx: Arc<Context>) -> Result<Action> {
        let rcs: Api<RestateCluster> = Api::all(ctx.client.clone());

        let name = self.name_any();

        let (result, message, reason, status) = match self.reconcile(ctx, &name).await {
            Ok(()) => {
                // If no events were received, check back every 5 minutes
                let action = Action::requeue(Duration::from_secs(5 * 60));

                (
                    Ok(action),
                    "Restate Cluster provisioned successfully".into(),
                    "Provisioned".into(),
                    "True".into(),
                )
            }
            Err(Error::NotReady {
                message,
                reason,
                requeue_after,
            }) => {
                // default 1 minute in the NotReady case
                let requeue_after = requeue_after.unwrap_or(Duration::from_secs(60));

                info!("RestateCluster is not yet ready: {message}");

                (
                    Ok(Action::requeue(requeue_after)),
                    message,
                    reason,
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

        let existing_ready = self
            .status
            .as_ref()
            .and_then(|s| s.conditions.as_ref())
            .and_then(|c| c.iter().find(|cond| cond.r#type == "Ready"));
        let now = k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(Utc::now());

        let mut ready = RestateClusterCondition {
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
            // update transition time if the status has at all changed
            ready.last_transition_time = Some(now)
        }

        // always overwrite status object with what we saw
        let new_status = Patch::Apply(json!({
            "apiVersion": "restate.dev/v1",
            "kind": "RestateCluster",
            "status": RestateClusterStatus {
                conditions: Some(vec![ready]),
            }
        }));
        let ps = PatchParams::apply("restate-operator").force();
        let _o = rcs.patch_status(&name, &ps, &new_status).await?;

        result
    }

    // Finalizer cleanup (the object was deleted, ensure nothing is orphaned)
    async fn cleanup(&self, ctx: Arc<Context>) -> Result<Action> {
        // RestateCluster doesn't have any real cleanup, so we just publish an event
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

async fn apply_namespace(nss: &Api<Namespace>, ns: Namespace) -> std::result::Result<(), Error> {
    let name = ns.metadata.name.as_ref().unwrap();
    let params = PatchParams::apply("restate-operator").force();
    debug!("Applying Namespace {}", name);
    nss.patch(name, &params, &Patch::Apply(&ns)).await?;
    Ok(())
}

// Initialize the controller and shared state (given the crd is installed)
pub async fn run(client: Client, metrics: Metrics, state: State) {
    let api_groups = match client.list_api_groups().await {
        Ok(list) => list,
        Err(e) => {
            error!("Could not list api groups: {e:?}");
            std::process::exit(1);
        }
    };

    let (
        security_group_policy_installed,
        pod_identity_association_installed,
        secret_provider_class_installed,
    ) = api_groups
        .groups
        .iter()
        .fold((false, false, false), |(sgp, pia, spc), group| {
            fn group_matches<R: Resource<DynamicType = ()>>(group: &APIGroup) -> bool {
                group.name == R::group(&())
                    && group.versions.iter().any(|v| v.version == R::version(&()))
            }
            (
                sgp || group_matches::<SecurityGroupPolicy>(group),
                pia || group_matches::<PodIdentityAssociation>(group),
                spc || group_matches::<SecretProviderClass>(group),
            )
        });

    let rc_api = Api::<RestateCluster>::all(client.clone());
    let ns_api = Api::<Namespace>::all(client.clone());
    let ss_api = Api::<StatefulSet>::all(client.clone());
    let pvc_api = Api::<PersistentVolumeClaim>::all(client.clone());
    let svc_api = Api::<Service>::all(client.clone());
    let svcacc_api = Api::<ServiceAccount>::all(client.clone());
    let pdb_api = Api::<PodDisruptionBudget>::all(client.clone());
    let cm_api = Api::<ConfigMap>::all(client.clone());
    let np_api = Api::<NetworkPolicy>::all(client.clone());
    let pia_api = Api::<PodIdentityAssociation>::all(client.clone());
    let sgp_api = Api::<SecurityGroupPolicy>::all(client.clone());
    let spc_api = Api::<SecretProviderClass>::all(client.clone());

    if state.aws_pod_identity_association_cluster.is_some() && !pod_identity_association_installed {
        error!("PodIdentityAssociation is not available on apiserver, but a pod identity association cluster was provided. Is the CRD installed?");
        std::process::exit(1);
    }

    if let Err(e) = rc_api.list(&ListParams::default().limit(1)).await {
        error!("RestateCluster is not queryable; {e:?}. Is the CRD installed?");
        std::process::exit(1);
    }

    // all resources we create have this label
    let cfg = Config::default().labels("app.kubernetes.io/name=restate");
    // but restateclusters themselves dont
    let rc_cfg = Config::default();

    let (pvc_meta_store, pvc_meta_writer) = reflector::store();
    let pvc_meta_reflector = reflector(pvc_meta_writer, metadata_watcher(pvc_api, cfg.clone()))
        .touched_objects()
        .default_backoff();

    let (ss_store, ss_writer) = reflector::store();
    let ss_reflector = reflector(ss_writer, watcher(ss_api, cfg.clone()))
        .map(|event| ensure_deletion_change(event))
        .touched_objects()
        .default_backoff()
        .predicate_filter(changed_predicate.combine(status_predicate_serde));

    let np_watcher = metadata_watcher(np_api, cfg.clone())
        .map(|event| ensure_deletion_change(event))
        .touched_objects()
        .predicate_filter(changed_predicate);

    let ns_watcher = metadata_watcher(ns_api, cfg.clone())
        .map(|event| ensure_deletion_change(event))
        .touched_objects()
        .predicate_filter(changed_predicate);

    let svcacc_watcher = metadata_watcher(svcacc_api, cfg.clone())
        .map(|event| ensure_deletion_change(event))
        .touched_objects()
        .predicate_filter(changed_predicate);

    let pdb_watcher = metadata_watcher(pdb_api, cfg.clone())
        .map(|event| ensure_deletion_change(event))
        .touched_objects()
        .predicate_filter(changed_predicate);

    let svc_watcher = watcher(svc_api, cfg.clone())
        .map(|event| ensure_deletion_change(event))
        .touched_objects()
        // svc has no generation so we hash the spec to check for changes
        .predicate_filter(changed_predicate.combine(spec_predicate_serde));

    let cm_watcher = watcher(cm_api, cfg.clone())
        .map(|event| ensure_deletion_change(event))
        .touched_objects()
        // cm has no generation so we hash the data to check for changes
        .predicate_filter(changed_predicate.combine(spec_predicate));

    let controller = Controller::new(rc_api, rc_cfg.clone())
        .shutdown_on_signal()
        .owns_stream(svc_watcher)
        .owns_stream(cm_watcher)
        .owns_stream(ns_watcher)
        .owns_stream(svcacc_watcher)
        .owns_stream(pdb_watcher)
        .owns_stream(np_watcher)
        .owns_stream(ss_reflector)
        .watches_stream(
            pvc_meta_reflector,
            |pvc| -> Option<ObjectRef<RestateCluster>> {
                let name = pvc.labels().get("app.kubernetes.io/name")?.as_str();
                if name != "restate" {
                    // should have been caught by the label selector
                    return None;
                }

                let instance = pvc.labels().get("app.kubernetes.io/instance")?.as_str();

                Some(ObjectRef::new(instance))
            },
        );
    let controller = if pod_identity_association_installed {
        let pia_watcher = watcher(pia_api, cfg.clone())
            .map(|event| ensure_deletion_change(event))
            .touched_objects()
            // avoid apply loops that seem to happen with crds
            .predicate_filter(changed_predicate.combine(status_predicate));

        let job_api = Api::<Job>::all(client.clone());

        let job_watcher = metadata_watcher(
            job_api,
            Config::default().labels("app.kubernetes.io/name=restate-pia-canary"),
        )
        .map(|event| ensure_deletion_change(event))
        .touched_objects()
        .predicate_filter(changed_predicate);

        controller.owns_stream(pia_watcher).owns_stream(job_watcher)
    } else {
        controller
    };
    let controller = if security_group_policy_installed {
        let sgp_watcher = metadata_watcher(sgp_api, cfg.clone())
            .map(|event| ensure_deletion_change(event))
            .touched_objects()
            // avoid apply loops that seem to happen with crds
            .predicate_filter(changed_predicate);

        controller.owns_stream(sgp_watcher)
    } else {
        controller
    };
    let controller = if secret_provider_class_installed {
        let spc_watcher = metadata_watcher(spc_api, cfg.clone())
            .map(|event| ensure_deletion_change(event))
            .touched_objects()
            // avoid apply loops that seem to happen with crds
            .predicate_filter(changed_predicate);

        controller.owns_stream(spc_watcher)
    } else {
        controller
    };
    controller
        .run(
            reconcile,
            error_policy,
            Context::new(
                client,
                metrics,
                state,
                pvc_meta_store,
                ss_store,
                security_group_policy_installed,
                secret_provider_class_installed,
            ),
        )
        .filter_map(|x| async move { Result::ok(x) })
        .for_each(|_| futures::future::ready(()))
        .await;
}

// deletion apparently doesn't lead to any change in metadata otherwise, which means the changed_predicate
// would drop them.
fn ensure_deletion_change<K: Resource, E>(
    mut event: Result<kube::runtime::watcher::Event<K>, E>,
) -> Result<kube::runtime::watcher::Event<K>, E> {
    if let Ok(kube::runtime::watcher::Event::Delete(ref mut object)) = event {
        let meta = object.meta_mut();
        meta.generation = match meta.generation {
            Some(val) => Some(val + 1),
            None => Some(0),
        }
    }
    event
}

fn changed_predicate<K: Resource>(obj: &K) -> Option<u64> {
    let mut hasher = DefaultHasher::new();
    if let Some(g) = obj.meta().generation {
        // covers spec but not metadata or status
        g.hash(&mut hasher)
    }
    obj.labels().hash(&mut hasher);
    obj.annotations().hash(&mut hasher);
    // ignore status
    Some(hasher.finish())
}

fn status_predicate<K: Resource + HasStatus>(obj: &K) -> Option<u64>
where
    K::Status: Hash,
{
    let mut hasher = DefaultHasher::new();
    if let Some(s) = obj.status() {
        s.hash(&mut hasher)
    }
    Some(hasher.finish())
}

trait MyHasStatus {
    type Status;

    fn status(&self) -> Option<&Self::Status>;
}

impl MyHasStatus for StatefulSet {
    type Status = StatefulSetStatus;

    fn status(&self) -> Option<&Self::Status> {
        self.status.as_ref()
    }
}

fn status_predicate_serde<K: Resource + MyHasStatus>(obj: &K) -> Option<u64>
where
    K::Status: Serialize,
{
    let mut hasher = DefaultHasher::new();
    if let Some(s) = obj.status() {
        serde_hashkey::to_key(s)
            .expect("serde_hashkey never to return an error")
            .hash(&mut hasher);
    }
    Some(hasher.finish())
}

pub trait MyHasSpec {
    type Spec;

    fn spec(&self) -> &Self::Spec;
}

impl MyHasSpec for Service {
    type Spec = Option<ServiceSpec>;

    fn spec(&self) -> &Self::Spec {
        &self.spec
    }
}

impl MyHasSpec for ConfigMap {
    type Spec = Option<std::collections::BTreeMap<String, String>>;

    fn spec(&self) -> &Self::Spec {
        &self.data
    }
}

fn spec_predicate<K: Resource + MyHasSpec>(obj: &K) -> Option<u64>
where
    K::Spec: Hash,
{
    let mut hasher = DefaultHasher::new();
    obj.spec().hash(&mut hasher);
    Some(hasher.finish())
}

fn spec_predicate_serde<K: Resource + MyHasSpec>(obj: &K) -> Option<u64>
where
    K::Spec: Serialize,
{
    let mut hasher = DefaultHasher::new();
    serde_hashkey::to_key(obj.spec())
        .expect("serde_hashkey never to return an error")
        .hash(&mut hasher);
    Some(hasher.finish())
}
