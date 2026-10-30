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
use kube::runtime::events::Recorder;
use kube::runtime::reflector::{ObjectRef, Store};
use kube::runtime::{Predicate, WatchStreamExt};
use kube::{
    Resource,
    api::{Api, Patch, PatchParams, ResourceExt},
    client::Client,
    runtime::{
        controller::{Action, Controller},
        events::{Event, EventType},
        finalizer::{Event as Finalizer, finalizer},
        watcher::Config,
    },
};
use serde::Serialize;
use serde_json::json;
use tokio::{sync::RwLock, time::Duration};
use tracing::*;

use crate::controllers::owned_objects::OwnedObjects;
use crate::controllers::{
    CrdWait, Diagnostics, RECONCILING_CONDITION, ReadinessGate, ResyncInterval, State,
    cluster_scoped_owners, log_suspended, reconciliation_suspended, reflected,
    state_after_reconcile, status_unchanged, suspended_message, wait_for_api_groups, wait_for_crd,
};
use crate::resources::ReconciliationState;
use crate::resources::iampolicymembers::IAMPolicyMember;
use crate::resources::podidentityassociations::PodIdentityAssociation;
use crate::resources::restateclusters::{
    RESTATE_CLUSTER_FINALIZER, RestateCluster, RestateClusterCondition, RestateClusterStatus,
};
use crate::resources::secretproviderclasses::SecretProviderClass;
use crate::resources::securitygrouppolicies::SecurityGroupPolicy;
use crate::{Error, Metrics, Result, telemetry};

use super::reconcilers::compute::reconcile_compute;
use super::reconcilers::network_policies::reconcile_network_policies;
use super::reconcilers::object_meta;
use super::reconcilers::signing_key::reconcile_signing_key;

// Context for our reconciler
pub(super) struct Context {
    /// Kubernetes client
    pub client: Client,
    /// Kubernetes event recorder
    pub recorder: Recorder,
    // Store for the PVCs of every RestateCluster
    pub pvc_store: Store<PersistentVolumeClaim>,
    pub namespaces: OwnedObjects<Namespace>,
    pub network_policies: OwnedObjects<NetworkPolicy>,
    pub service_accounts: OwnedObjects<ServiceAccount>,
    pub config_maps: OwnedObjects<ConfigMap>,
    pub services: OwnedObjects<Service>,
    pub pod_disruption_budgets: OwnedObjects<PodDisruptionBudget>,
    pub stateful_sets: OwnedObjects<StatefulSet>,
    pub persistent_volume_claims: OwnedObjects<PersistentVolumeClaim>,
    /// The canary Jobs that check cloud credentials are injected into pods
    pub jobs: OwnedObjects<Job>,
    pub pod_identity_associations: OwnedObjects<PodIdentityAssociation>,
    pub security_group_policies: OwnedObjects<SecurityGroupPolicy>,
    pub secret_provider_classes: OwnedObjects<SecretProviderClass>,
    pub iam_policy_members: OwnedObjects<IAMPolicyMember>,
    // If set, watch PodIdentityAssociation resources, and if requested create them against this cluster
    pub aws_pod_identity_association_cluster: Option<String>,
    // Our namespace, needed to support the case where restate clusters need to be reached by the operator
    pub operator_namespace: String,
    // The name of a label that can select the operator, needed to support the case where restate clusters need to be reached by the operator
    pub operator_label_name: Option<String>,
    // The value of the label named operator_label_name that will select the operator, needed to support the case where restate clusters need to be reached by the operator
    pub operator_label_value: Option<String>,
    // Whether the EKS SecurityGroupPolicy CRD is installed
    pub security_group_policy_installed: bool,
    // Whether the SecretProviderClass CRD is installed
    pub secret_provider_class_installed: bool,
    // Whether GCP Workload Identity management is enabled
    pub gcp_workload_identity: bool,
    /// The cluster DNS suffix (e.g. "cluster.local")
    pub cluster_dns: String,
    /// The container image to use for canary jobs
    pub canary_image: String,
    /// When a cluster that reconciled cleanly is next reconciled unprompted
    pub resync: ResyncInterval,
    /// Diagnostics read by the web server
    pub diagnostics: Arc<RwLock<Diagnostics>>,
    /// Prometheus metrics
    pub metrics: Metrics,
}

#[instrument(skip(ctx, rc), fields(trace_id))]
async fn reconcile(rc: Arc<RestateCluster>, ctx: Arc<Context>) -> Result<Action> {
    if let Some(trace_id) = telemetry::get_trace_id() {
        Span::current().record("trace_id", field::display(&trace_id));
    }
    let _timer = ctx.metrics.count_and_measure::<RestateCluster>();
    ctx.diagnostics.write().await.last_event = Utc::now();
    let rcs: Api<RestateCluster> = Api::all(ctx.client.clone());

    // Check this before the finalizer helper, which adds our finalizer to resources that
    // don't have one yet. That's a write, and we've been asked not to write to this one.
    if reconciliation_suspended(rc.as_ref()) {
        return rc.reconcile_suspended(&rcs).await;
    }

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

        let namespace = Namespace {
            metadata: object_meta(&base_metadata, name),
            ..Default::default()
        };
        let existing_owners = match ctx.namespaces.cached(&nss, name) {
            Some(existing) => Some(existing.metadata.owner_references.clone()),
            // Namespaces we create carry the label the cache selects on, so a miss means the
            // namespace is either absent or not ours; only the apiserver can say which.
            None => nss
                .get_metadata_opt(name)
                .await?
                .map(|existing| existing.metadata.owner_references),
        };
        // A namespace someone else created is used as it is, without mutation.
        if existing_owners.is_none_or(|owners| owners.is_some_and(|owners| owners.contains(&oref)))
        {
            ctx.namespaces.apply(&nss, &namespace).await?;
        }

        reconcile_network_policies(
            &ctx,
            name,
            &base_metadata,
            self.spec.security.as_ref(),
            self.spec.cluster.as_ref(),
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

        reconcile_compute(
            &ctx,
            name,
            &base_metadata,
            &self.spec,
            self.status.as_ref(),
            signing_key,
        )
        .await?;

        Ok(())
    }

    /// Note the suspension in the status, and leave everything else alone.
    ///
    /// The status is ours to write, so this doesn't disturb anything that was changed by hand.
    /// The rest of it stays as the last real reconcile left it, `observedGeneration` included:
    /// if the spec has moved on since, that's how you can tell.
    async fn reconcile_suspended(&self, rcs: &Api<RestateCluster>) -> Result<Action> {
        let name = self.name_any();
        let message = suspended_message();
        let reason = ReconciliationState::Disabled.as_str();
        let previous = self.status.as_ref().and_then(|s| s.reconciliation);

        log_suspended("RestateCluster", &name, previous);

        let existing = self
            .status
            .as_ref()
            .and_then(|s| s.conditions.as_ref())
            .and_then(|c| c.iter().find(|cond| cond.r#type == RECONCILING_CONDITION));
        let now = k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(Utc::now());

        let suspended = RestateClusterCondition {
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

        // Leave the other conditions alone. A stale `Ready` is still worth having, and it
        // says when it was last true.
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
            "apiVersion": "restate.dev/v1",
            "kind": "RestateCluster",
            "status": RestateClusterStatus {
                conditions: Some(conditions),
                reconciliation: Some(ReconciliationState::Disabled),
                provisioned: self.status.as_ref().and_then(|s| s.provisioned),
            }
        }));
        let ps = PatchParams::apply("restate-operator").force();
        rcs.patch_status(&name, &ps, &new_status).await?;

        Ok(Action::await_change())
    }

    async fn reconcile_status(&self, ctx: Arc<Context>) -> Result<Action> {
        let rcs: Api<RestateCluster> = Api::all(ctx.client.clone());

        let name = self.name_any();
        let resync = ctx.resync;

        // Track whether we should set provisioned = true in status
        let mut set_provisioned = false;

        let (result, message, reason, status) = match self.reconcile(ctx, &name).await {
            Ok(()) => {
                let action = Action::requeue(resync.next());

                (
                    Ok(action),
                    "Restate Cluster reconciled successfully".into(),
                    "Reconciled".into(),
                    "True".into(),
                )
            }
            Err(Error::Provisioned) => {
                // Cluster was successfully provisioned, mark it in status and requeue immediately
                set_provisioned = true;
                info!("Cluster provisioned successfully, updating status");

                (
                    // Requeue immediately to continue reconciliation
                    Ok(Action::requeue(Duration::ZERO)),
                    "Cluster provisioned, continuing reconciliation".into(),
                    "Provisioned".into(),
                    "False".into(),
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
            Err(Error::ProvisioningFailed(ref message)) => {
                // Retry provisioning failures after 5 seconds - transient gRPC failures
                // should recover quickly once the pod's gRPC server is fully initialized
                warn!("Cluster provisioning failed, will retry: {message}");

                (
                    Ok(Action::requeue(Duration::from_secs(5))),
                    format!("Provisioning failed: {message}"),
                    "ProvisioningFailed".into(),
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

        let ready_status = ready.status.clone();

        // Preserve the provisioned status from the existing status, or set it if we just provisioned
        let provisioned = if set_provisioned {
            Some(true)
        } else {
            self.status.as_ref().and_then(|s| s.provisioned)
        };
        let new_status = RestateClusterStatus {
            // Replacing the whole list is what clears the `Reconciling` condition after
            // a suspension.
            conditions: Some(vec![ready]),
            reconciliation: Some(state_after_reconcile(
                self.status.as_ref().and_then(|s| s.reconciliation),
                &ready_status,
            )),
            provisioned,
        };
        if !status_unchanged(self.status.as_ref(), &new_status) {
            let new_status = Patch::Apply(json!({
                "apiVersion": "restate.dev/v1",
                "kind": "RestateCluster",
                "status": new_status,
            }));
            let ps = PatchParams::apply("restate-operator").force();
            rcs.patch_status(&name, &ps, &new_status).await?;
        }

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

// Initialize the controller and shared state (given the crd is installed)
pub async fn run(client: Client, metrics: Metrics, state: State) {
    let api_groups = match wait_for_api_groups(&client).await {
        Ok(Some(list)) => list,
        Ok(None) => return,
        Err(e) => {
            error!("Could not list api groups: {e:?}");
            std::process::exit(1);
        }
    };

    let (
        security_group_policy_installed,
        pod_identity_association_installed,
        secret_provider_class_installed,
        iam_policy_member_installed,
    ) = api_groups.groups.iter().fold(
        (false, false, false, false),
        |(sgp, pia, spc, ipm), group| {
            fn group_matches<R: Resource<DynamicType = ()>>(group: &APIGroup) -> bool {
                group.name == R::group(&())
                    && group.versions.iter().any(|v| v.version == R::version(&()))
            }
            (
                sgp || group_matches::<SecurityGroupPolicy>(group),
                pia || group_matches::<PodIdentityAssociation>(group),
                spc || group_matches::<SecretProviderClass>(group),
                ipm || group_matches::<IAMPolicyMember>(group),
            )
        },
    );

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
    let ipm_api = Api::<IAMPolicyMember>::all(client.clone());
    let sgp_api = Api::<SecurityGroupPolicy>::all(client.clone());
    let spc_api = Api::<SecretProviderClass>::all(client.clone());

    if state.aws_pod_identity_association_cluster.is_some() && !pod_identity_association_installed {
        error!(
            "PodIdentityAssociation is not available on apiserver, but a pod identity association cluster was provided. Is the CRD installed?"
        );
        std::process::exit(1);
    }

    match wait_for_crd::<RestateCluster>(ReadinessGate::RestateCluster, &client, &metrics, &state)
        .await
    {
        Ok(CrdWait::Available) => {}
        Ok(CrdWait::ShuttingDown) => return,
        Err(e) => {
            error!("Could not determine whether the RestateCluster CRD is installed; {e:?}");
            std::process::exit(1);
        }
    }

    // all resources we create have this label
    let cfg = Config::default().labels("app.kubernetes.io/name=restate");
    // but restateclusters themselves dont
    let rc_cfg = Config::default();

    let (pvc_store, pvc_reflector) = reflected(pvc_api, cfg.clone());
    let (ss_store, ss_reflector) = reflected(ss_api, cfg.clone());
    let ss_reflector =
        ss_reflector.predicate_filter(changed_predicate.combine(status_predicate_serde));
    let (np_store, np_watcher) = reflected(np_api, cfg.clone());
    let np_watcher = np_watcher.predicate_filter(changed_predicate);
    let (ns_store, ns_watcher) = reflected(ns_api, cfg.clone());
    let ns_watcher = ns_watcher.predicate_filter(changed_predicate);
    let (svcacc_store, svcacc_watcher) = reflected(svcacc_api, cfg.clone());
    let svcacc_watcher = svcacc_watcher.predicate_filter(changed_predicate);
    let (pdb_store, pdb_watcher) = reflected(pdb_api, cfg.clone());
    let pdb_watcher = pdb_watcher.predicate_filter(changed_predicate);
    let (svc_store, svc_watcher) = reflected(svc_api, cfg.clone());
    // svc has no generation so we hash the spec to check for changes
    let svc_watcher = svc_watcher.predicate_filter(changed_predicate.combine(spec_predicate_serde));
    let (cm_store, cm_watcher) = reflected(cm_api, cfg.clone());
    // cm has no generation so we hash the data to check for changes
    let cm_watcher = cm_watcher.predicate_filter(changed_predicate.combine(spec_predicate));

    let controller = Controller::new(rc_api, rc_cfg.clone())
        .shutdown_on_signal()
        .watches_stream(svc_watcher, cluster_scoped_owners)
        .watches_stream(cm_watcher, cluster_scoped_owners)
        .watches_stream(ns_watcher, cluster_scoped_owners)
        .watches_stream(svcacc_watcher, cluster_scoped_owners)
        .watches_stream(pdb_watcher, cluster_scoped_owners)
        .watches_stream(np_watcher, cluster_scoped_owners)
        .watches_stream(ss_reflector, cluster_scoped_owners)
        .watches_stream(pvc_reflector, |pvc| -> Option<ObjectRef<RestateCluster>> {
            let name = pvc.labels().get("app.kubernetes.io/name")?.as_str();
            if name != "restate" {
                // should have been caught by the label selector
                return None;
            }

            let instance = pvc.labels().get("app.kubernetes.io/instance")?.as_str();

            Some(ObjectRef::new(instance))
        });

    let mut pod_identity_associations = OwnedObjects::default();
    let controller = if pod_identity_association_installed {
        let (pia_store, pia_watcher) = reflected(pia_api, cfg.clone());
        pod_identity_associations = OwnedObjects::watched(pia_store);
        // avoid apply loops that seem to happen with crds
        controller.watches_stream(
            pia_watcher.predicate_filter(changed_predicate.combine(status_predicate)),
            cluster_scoped_owners,
        )
    } else {
        controller
    };
    let watch_iam_policy_members = state.gcp_workload_identity && iam_policy_member_installed;
    // One cache for both kinds of canary Job
    let mut jobs = OwnedObjects::default();
    let controller = if pod_identity_association_installed || watch_iam_policy_members {
        let (job_store, job_watcher) = reflected(
            Api::<Job>::all(client.clone()),
            Config::default()
                .labels("app.kubernetes.io/name in (restate-pia-canary,restate-wi-canary)"),
        );
        jobs = OwnedObjects::watched(job_store);
        controller.watches_stream(
            job_watcher.predicate_filter(changed_predicate),
            cluster_scoped_owners,
        )
    } else {
        controller
    };
    let mut security_group_policies = OwnedObjects::default();
    let controller = if security_group_policy_installed {
        let (sgp_store, sgp_watcher) = reflected(sgp_api, cfg.clone());
        security_group_policies = OwnedObjects::watched(sgp_store);
        // avoid apply loops that seem to happen with crds
        controller.watches_stream(
            sgp_watcher.predicate_filter(changed_predicate),
            cluster_scoped_owners,
        )
    } else {
        controller
    };
    let mut secret_provider_classes = OwnedObjects::default();
    let controller = if secret_provider_class_installed {
        let (spc_store, spc_watcher) = reflected(spc_api, cfg.clone());
        secret_provider_classes = OwnedObjects::watched(spc_store);
        // avoid apply loops that seem to happen with crds
        controller.watches_stream(
            spc_watcher.predicate_filter(changed_predicate),
            cluster_scoped_owners,
        )
    } else {
        controller
    };
    let mut iam_policy_members = OwnedObjects::default();
    let controller = if watch_iam_policy_members {
        let (ipm_store, ipm_watcher) = reflected(ipm_api, cfg.clone());
        iam_policy_members = OwnedObjects::watched(ipm_store);
        controller.watches_stream(
            ipm_watcher.predicate_filter(changed_predicate.combine(status_predicate)),
            cluster_scoped_owners,
        )
    } else {
        controller
    };

    let context = Arc::new(Context {
        client: client.clone(),
        recorder: Recorder::new(client, "restate-operator".into()),
        pvc_store: pvc_store.clone(),
        namespaces: OwnedObjects::watched(ns_store),
        network_policies: OwnedObjects::watched(np_store),
        service_accounts: OwnedObjects::watched(svcacc_store),
        config_maps: OwnedObjects::watched(cm_store),
        services: OwnedObjects::watched(svc_store),
        pod_disruption_budgets: OwnedObjects::watched(pdb_store),
        stateful_sets: OwnedObjects::watched(ss_store),
        persistent_volume_claims: OwnedObjects::watched(pvc_store),
        jobs,
        pod_identity_associations,
        security_group_policies,
        secret_provider_classes,
        iam_policy_members,
        aws_pod_identity_association_cluster: state.aws_pod_identity_association_cluster.clone(),
        operator_namespace: state.operator_namespace.clone(),
        operator_label_name: state.operator_label_name.clone(),
        operator_label_value: state.operator_label_value.clone(),
        security_group_policy_installed,
        secret_provider_class_installed,
        gcp_workload_identity: state.gcp_workload_identity,
        cluster_dns: state.cluster_dns.clone(),
        canary_image: state.canary_image.clone(),
        resync: state.resync,
        diagnostics: state.diagnostics.clone(),
        metrics,
    });

    controller
        .run(reconcile, error_policy, context)
        .filter_map(|x| async move { Result::ok(x) })
        .for_each(|_| futures::future::ready(()))
        .await;
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
