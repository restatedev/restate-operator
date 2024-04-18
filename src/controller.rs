use std::borrow::Cow;
use std::collections::BTreeMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use futures::StreamExt;
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{
    EnvVar, Namespace, PersistentVolumeClaim, Pod, PodDNSConfig, ResourceRequirements, Service,
    ServiceAccount,
};
use k8s_openapi::api::networking::v1;
use k8s_openapi::api::networking::v1::{NetworkPolicy, NetworkPolicyPeer, NetworkPolicyPort};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{APIGroup, ObjectMeta};
use kube::core::object::HasStatus;
use kube::core::PartialObjectMeta;
use kube::runtime::reflector::{ObjectRef, Store};
use kube::runtime::{metadata_watcher, reflector, watcher, Predicate, WatchStreamExt};
use kube::{
    api::{Api, ListParams, Patch, PatchParams, ResourceExt},
    client::Client,
    runtime::{
        controller::{Action, Controller},
        events::{Event, EventType, Recorder, Reporter},
        finalizer::{finalizer, Event as Finalizer},
        watcher::Config,
    },
    CustomResource, Resource,
};
use schemars::schema::{Schema, SchemaObject};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::{sync::RwLock, time::Duration};
use tracing::*;

use crate::podidentityassociations::PodIdentityAssociation;
use crate::reconcilers::compute::reconcile_compute;
use crate::reconcilers::network_policies::reconcile_network_policies;
use crate::reconcilers::object_meta;
use crate::securitygrouppolicies::SecurityGroupPolicy;
use crate::{telemetry, Error, Metrics, Result};

pub static RESTATE_CLUSTER_FINALIZER: &str = "clusters.restate.dev";

/// Represents the configuration of a Restate Cluster
#[derive(CustomResource, Deserialize, Serialize, Clone, Debug, JsonSchema)]
#[cfg_attr(test, derive(Default))]
#[kube(
    kind = "RestateCluster",
    group = "restate.dev",
    version = "v1",
    schema = "manual",
    printcolumn = r#"{"name":"Ready", "type":"string", "jsonPath":".status.conditions[?(@.type==\"Ready\")].status"}"#,
    printcolumn = r#"{"name":"Status", "priority": 1, "type":"string", "jsonPath":".status.conditions[?(@.type==\"Ready\")].message"}"#,
    printcolumn = r#"{"name":"Age", "description": "CreationTimestamp is a timestamp representing the server time when this object was created. It is not guaranteed to be set in happens-before order across separate operations. Clients may not set this value. It is represented in RFC3339 form and is in UTC", "type":"date", "jsonPath":".metadata.creationTimestamp"}"#
)]
#[kube(status = "RestateClusterStatus", shortname = "rsc")]
pub struct RestateClusterSpec {
    pub storage: RestateClusterStorage,
    pub compute: RestateClusterCompute,
    pub security: Option<RestateClusterSecurity>,
}

// Hoisted from the derived implementation so that we can restrict names to be valid namespace names
impl schemars::JsonSchema for RestateCluster {
    fn schema_name() -> String {
        "RestateCluster".to_owned()
    }
    fn schema_id() -> Cow<'static, str> {
        "restate_operator::controller::RestateCluster".into()
    }
    fn json_schema(gen: &mut schemars::gen::SchemaGenerator) -> Schema {
        {
            let mut schema_object = SchemaObject {
                instance_type: Some(
                    schemars::schema::InstanceType::Object.into(),
                ),
                metadata: Some(Box::new(schemars::schema::Metadata {
                    description: Some(
                        "RestateCluster describes the configuration and status of a Restate cluster."
                            .to_owned(),
                    ),
                    ..Default::default()
                })),
                ..Default::default()
            };
            let object_validation = schema_object.object();

            object_validation
                .properties
                .insert(
                    "metadata".to_owned(),
                    serde_json::from_value(json!({
                                "type": "object",
                                "properties": {
                                    "name": {
                                        "type": "string",
                                        "minLength": 1,
                                        "maxLength": 63,
                                        "pattern": "^[a-z0-9]([-a-z0-9]*[a-z0-9])?(\\.[a-z0-9]([-a-z0-9]*[a-z0-9])?)*$",
                                    }
                                }
                            })).unwrap(),
                );
            object_validation.required.insert("metadata".to_owned());

            object_validation
                .properties
                .insert("spec".to_owned(), gen.subschema_for::<RestateClusterSpec>());
            object_validation.required.insert("spec".to_owned());

            object_validation.properties.insert(
                "status".to_owned(),
                gen.subschema_for::<Option<RestateClusterStatus>>(),
            );
            Schema::Object(schema_object)
        }
    }
}

/// Storage configuration
#[derive(Deserialize, Serialize, Clone, Default, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RestateClusterStorage {
    /// storageClassName is the name of the StorageClass required by the claim. More info: https://kubernetes.io/docs/concepts/storage/persistent-volumes#class-1
    /// this field is immutable
    #[schemars(default, schema_with = "immutable_storage_class_name")]
    pub storage_class_name: Option<String>,
    /// storageRequestBytes is the amount of storage to request in volume claims. It is allowed to increase but not decrease.
    #[schemars(schema_with = "expanding_volume_request", range(min = 1))]
    pub storage_request_bytes: i64,
}

fn immutable_storage_class_name(
    _: &mut schemars::gen::SchemaGenerator,
) -> schemars::schema::Schema {
    serde_json::from_value(json!({
        "nullable": true,
        "type": "string",
        "x-kubernetes-validations": [{
            "rule": "self == oldSelf",
            "message": "storageClassName is immutable"
        }]
    }))
    .unwrap()
}

fn expanding_volume_request(_: &mut schemars::gen::SchemaGenerator) -> Schema {
    serde_json::from_value(json!({
        "format": "int64",
        "type": "integer",
        "x-kubernetes-validations": [
            {
                "rule": "self >= oldSelf",
                "message": "storageRequestBytes cannot be decreased"
            }
        ]
    }))
    .unwrap()
}

/// Compute configuration
#[derive(Deserialize, Serialize, Clone, Default, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RestateClusterCompute {
    /// replicas is the desired number of Restate nodes. If unspecified, defaults to 1.
    pub replicas: Option<i32>,
    /// Container image name. More info: https://kubernetes.io/docs/concepts/containers/images.
    pub image: String,
    /// Image pull policy. One of Always, Never, IfNotPresent. Defaults to Always if :latest tag is specified, or IfNotPresent otherwise. More info: https://kubernetes.io/docs/concepts/containers/images#updating-images
    pub image_pull_policy: Option<String>,
    /// List of environment variables to set in the container; these may override defaults
    #[schemars(default, schema_with = "env_schema")]
    pub env: Option<Vec<EnvVar>>,
    /// Compute Resources for the Restate container. More info: https://kubernetes.io/docs/concepts/configuration/manage-resources-containers/
    pub resources: Option<ResourceRequirements>,
    /// Specifies the DNS parameters of the Restate pod. Parameters specified here will be merged to the generated DNS configuration based on DNSPolicy.
    pub dns_config: Option<PodDNSConfig>,
    /// Set DNS policy for the pod. Defaults to "ClusterFirst". Valid values are 'ClusterFirstWithHostNet', 'ClusterFirst', 'Default' or 'None'. DNS parameters given in DNSConfig will be merged with the policy selected with DNSPolicy.
    pub dns_policy: Option<String>,
}

fn env_schema(g: &mut schemars::gen::SchemaGenerator) -> Schema {
    serde_json::from_value(json!({
        "items": EnvVar::json_schema(g),
        "nullable": true,
        "type": "array",
        "x-kubernetes-list-map-keys": ["name"],
        "x-kubernetes-list-type": "map"
    }))
    .unwrap()
}

/// Security configuration
#[derive(Deserialize, Serialize, Clone, Default, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RestateClusterSecurity {
    pub service_annotations: Option<BTreeMap<String, String>>,
    pub service_account_annotations: Option<BTreeMap<String, String>>,
    /// If set, create an AWS PodIdentityAssociation using the ACK CRD in order to give the Restate pod access to this role and
    /// allow the cluster to reach the Pod Identity agent.
    pub aws_pod_identity_association_role_arn: Option<String>,
    /// If set, create an AWS SecurityGroupPolicy CRD object to place the Restate pod into these security groups
    pub aws_pod_security_groups: Option<Vec<String>>,
    /// Network peers to allow inbound access to restate ports
    /// If unset, will not allow any new traffic. Set any of these to [] to allow all traffic - not recommended.
    pub network_peers: Option<RestateClusterNetworkPeers>,
    /// Egress rules to allow the cluster to make outbound requests; this is in addition to the default
    /// of allowing public internet access and cluster DNS access. Providing a single empty rule will allow
    /// all outbound traffic - not recommended
    pub network_egress_rules: Option<Vec<NetworkPolicyEgressRule>>,
}

#[derive(Deserialize, Serialize, Clone, Default, Debug, JsonSchema)]
pub struct RestateClusterNetworkPeers {
    #[schemars(default, schema_with = "network_peers_schema")]
    pub ingress: Option<Vec<NetworkPolicyPeer>>,
    #[schemars(default, schema_with = "network_peers_schema")]
    pub admin: Option<Vec<NetworkPolicyPeer>>,
    #[schemars(default, schema_with = "network_peers_schema")]
    pub metrics: Option<Vec<NetworkPolicyPeer>>,
}

fn network_peers_schema(g: &mut schemars::gen::SchemaGenerator) -> Schema {
    serde_json::from_value(json!({
        "items": NetworkPolicyPeer::json_schema(g),
        "nullable": true,
        "type": "array",
        "x-kubernetes-list-type": "atomic"
    }))
    .unwrap()
}

/// NetworkPolicyEgressRule describes a particular set of traffic that is allowed out of pods matched by a NetworkPolicySpec's podSelector. The traffic must match both ports and to. This type is beta-level in 1.8
#[derive(Deserialize, Serialize, Clone, Default, Debug, JsonSchema)]
pub struct NetworkPolicyEgressRule {
    /// ports is a list of destination ports for outgoing traffic. Each item in this list is combined using a logical OR. If this field is empty or missing, this rule matches all ports (traffic not restricted by port). If this field is present and contains at least one item, then this rule allows traffic only if the traffic matches at least one port in the list.
    #[schemars(default, schema_with = "network_ports_schema")]
    pub ports: Option<Vec<NetworkPolicyPort>>,

    /// to is a list of destinations for outgoing traffic of pods selected for this rule. Items in this list are combined using a logical OR operation. If this field is empty or missing, this rule matches all destinations (traffic not restricted by destination). If this field is present and contains at least one item, this rule allows traffic only if the traffic matches at least one item in the to list.
    #[schemars(default, schema_with = "network_peers_schema")]
    pub to: Option<Vec<NetworkPolicyPeer>>,
}

impl From<NetworkPolicyEgressRule> for v1::NetworkPolicyEgressRule {
    fn from(value: NetworkPolicyEgressRule) -> Self {
        Self {
            ports: value.ports,
            to: value.to,
        }
    }
}

fn network_ports_schema(_: &mut schemars::gen::SchemaGenerator) -> Schema {
    serde_json::from_value(json!({
          "items": {
            "description": "NetworkPolicyPort describes a port to allow traffic on",
            "properties": {
              "endPort": {
                "description": "endPort indicates that the range of ports from port to endPort if set, inclusive, should be allowed by the policy. This field cannot be defined if the port field is not defined or if the port field is defined as a named (string) port. The endPort must be equal or greater than port.",
                "format": "int32",
                "type": "integer"
              },
              "port": {
                "x-kubernetes-int-or-string": true,
                "anyOf": [{"type": "integer"}, {"type": "string"}],
                "description": "port represents the port on the given protocol. This can either be a numerical or named port on a pod. If this field is not provided, this matches all port names and numbers. If present, only traffic on the specified protocol AND port will be matched."
              },
              "protocol": {
                "description": "protocol represents the protocol (TCP, UDP, or SCTP) which traffic must match. If not specified, this field defaults to TCP.",
                "type": "string"
              }
            },
            "type": "object",
          },
          "nullable": true,
          "type": "array",
          "x-kubernetes-list-type": "atomic"
        }))
        .unwrap()
}

/// Status of the RestateCluster.
/// This is set and managed automatically.
/// Read-only.
/// More info: https://git.k8s.io/community/contributors/devel/sig-architecture/api-conventions.md#spec-and-status
#[derive(Deserialize, Serialize, Clone, Default, Debug, JsonSchema)]
pub struct RestateClusterStatus {
    conditions: Option<Vec<RestateClusterCondition>>,
}

#[derive(Deserialize, Serialize, Clone, Default, Debug, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RestateClusterCondition {
    /// Last time the condition transitioned from one status to another.
    pub last_transition_time: Option<k8s_openapi::apimachinery::pkg::apis::meta::v1::Time>,

    /// Human-readable message indicating details about last transition.
    pub message: Option<String>,

    /// Unique, one-word, CamelCase reason for the condition's last transition.
    pub reason: Option<String>,

    /// Status is the status of the condition. Can be True, False, Unknown.
    pub status: String,

    /// Type of the condition, known values are (`Ready`).
    pub r#type: String,
}

// Context for our reconciler
#[derive(Clone)]
pub struct Context {
    /// Kubernetes client
    // Store for pvc metadata
    pub client: Client,
    // Store for pvc metadata
    pub pvc_meta_store: Store<PartialObjectMeta<PersistentVolumeClaim>>,
    // Store for statefulsets
    pub ss_store: Store<StatefulSet>,
    // If set, watch PodIdentityAssociation resources, and if requested create them against this cluster
    pub aws_pod_identity_association_cluster: Option<String>,
    // Whether the EKS SecurityGroupPolicy CRD is installed
    pub security_group_policy_installed: bool,
    /// Diagnostics read by the web server
    pub diagnostics: Arc<RwLock<Diagnostics>>,
    /// Prometheus metrics
    pub metrics: Metrics,
}

#[instrument(skip(ctx, rc), fields(trace_id))]
async fn reconcile(rc: Arc<RestateCluster>, ctx: Arc<Context>) -> Result<Action> {
    if let Some(trace_id) = telemetry::get_trace_id() {
        Span::current().record("trace_id", &field::display(&trace_id));
    }
    let recorder = ctx
        .diagnostics
        .read()
        .await
        .recorder(ctx.client.clone(), &rc);
    let _timer = ctx.metrics.count_and_measure();
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
            ctx.metrics.reconcile_failure(&rc, &err);
            Err(err)
        }
    }
}

fn error_policy(_rc: Arc<RestateCluster>, _error: &Error, _ctx: Arc<Context>) -> Action {
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
            ctx.client.clone(),
            name,
            &base_metadata,
            self.spec
                .security
                .as_ref()
                .and_then(|s| s.network_peers.as_ref()),
            self.spec
                .security
                .as_ref()
                .and_then(|s| s.network_egress_rules.as_deref()),
            self.spec
                .security
                .as_ref()
                .map_or(false, |s| s.aws_pod_identity_association_role_arn.is_some()),
        )
        .await?;

        reconcile_compute(&ctx, name, &base_metadata, &self.spec).await?;

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
        let recorder = ctx
            .diagnostics
            .read()
            .await
            .recorder(ctx.client.clone(), self);
        // RestateCluster doesn't have any real cleanup, so we just publish an event
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

async fn apply_namespace(nss: &Api<Namespace>, ns: Namespace) -> std::result::Result<(), Error> {
    let name = ns.metadata.name.as_ref().unwrap();
    let params = PatchParams::apply("restate-operator").force();
    debug!("Applying Namespace {}", name);
    nss.patch(name, &params, &Patch::Apply(&ns)).await?;
    Ok(())
}

/// Diagnostics to be exposed by the web server
#[derive(Clone, Serialize)]
pub struct Diagnostics {
    #[serde(deserialize_with = "from_ts")]
    pub last_event: DateTime<Utc>,
    #[serde(skip)]
    pub reporter: Reporter,
}

impl Default for Diagnostics {
    fn default() -> Self {
        Self {
            last_event: Utc::now(),
            reporter: "restate-operator".into(),
        }
    }
}

impl Diagnostics {
    fn recorder(&self, client: Client, rc: &RestateCluster) -> Recorder {
        Recorder::new(client, self.reporter.clone(), rc.object_ref(&()))
    }
}

/// State shared between the controller and the web server
#[derive(Clone, Default)]
pub struct State {
    /// Diagnostics populated by the reconciler
    diagnostics: Arc<RwLock<Diagnostics>>,
    /// Metrics registry
    registry: prometheus::Registry,
    /// If set, watch AWS PodIdentityAssociation resources, and if requested create them against this cluster
    aws_pod_identity_association_cluster: Option<String>,
}

/// State wrapper around the controller outputs for the web server
impl State {
    /// Metrics getter
    pub fn metrics(&self) -> Vec<prometheus::proto::MetricFamily> {
        self.registry.gather()
    }

    /// State getter
    pub async fn diagnostics(&self) -> Diagnostics {
        self.diagnostics.read().await.clone()
    }

    pub fn with_aws_pod_identity_association_cluster(
        self,
        aws_pod_identity_association_cluster: Option<String>,
    ) -> Self {
        Self {
            aws_pod_identity_association_cluster,
            ..self
        }
    }

    // Create a Controller Context that can update State
    pub fn to_context(
        &self,
        client: Client,
        pvc_meta_store: Store<PartialObjectMeta<PersistentVolumeClaim>>,
        ss_store: Store<StatefulSet>,
        security_group_policy_installed: bool,
    ) -> Arc<Context> {
        Arc::new(Context {
            client,
            pvc_meta_store,
            ss_store,
            aws_pod_identity_association_cluster: self.aws_pod_identity_association_cluster.clone(),
            security_group_policy_installed,
            metrics: Metrics::default().register(&self.registry).unwrap(),
            diagnostics: self.diagnostics.clone(),
        })
    }
}

/// Initialize the controller and shared state (given the crd is installed)
pub async fn run(state: State) {
    let client = Client::try_default()
        .await
        .expect("failed to create kube Client");

    let api_groups = match client.list_api_groups().await {
        Ok(list) => list,
        Err(e) => {
            error!("Could not list api groups: {e:?}");
            std::process::exit(1);
        }
    };

    let (security_group_policy_installed, pod_identity_association_installed) = api_groups
        .groups
        .iter()
        .fold((false, false), |(sgp, pia), group| {
            fn group_matches<R: Resource<DynamicType = ()>>(group: &APIGroup) -> bool {
                group.name == R::group(&())
                    && group.versions.iter().any(|v| v.version == R::version(&()))
            }
            (
                sgp || group_matches::<SecurityGroupPolicy>(group),
                pia || group_matches::<PodIdentityAssociation>(group),
            )
        });

    let rc_api = Api::<RestateCluster>::all(client.clone());
    let ns_api = Api::<Namespace>::all(client.clone());
    let ss_api = Api::<StatefulSet>::all(client.clone());
    let pvc_api = Api::<PersistentVolumeClaim>::all(client.clone());
    let svc_api = Api::<Service>::all(client.clone());
    let svcacc_api = Api::<ServiceAccount>::all(client.clone());
    let np_api = Api::<NetworkPolicy>::all(client.clone());
    let pia_api = Api::<PodIdentityAssociation>::all(client.clone());
    let pod_api = Api::<Pod>::all(client.clone());
    let sgp_api = Api::<SecurityGroupPolicy>::all(client.clone());

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
        .touched_objects()
        .default_backoff();

    let np_watcher = metadata_watcher(np_api, cfg.clone())
        .touched_objects()
        // netpols are really bad for apply-loops for some reason?
        .predicate_filter(changed_predicate);

    let controller = Controller::new(rc_api, rc_cfg.clone())
        .shutdown_on_signal()
        .owns(ns_api, cfg.clone())
        .owns(svc_api, cfg.clone())
        .owns(svcacc_api, cfg.clone())
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
            .touched_objects()
            // avoid apply loops that seem to happen with crds
            .predicate_filter(changed_predicate.combine(status_predicate));

        controller
            .owns_stream(pia_watcher)
            .owns(pod_api, cfg.clone())
    } else {
        controller
    };
    let controller = if security_group_policy_installed {
        let sgp_watcher = metadata_watcher(sgp_api, cfg.clone())
            .touched_objects()
            // avoid apply loops that seem to happen with crds
            .predicate_filter(changed_predicate);

        controller.owns_stream(sgp_watcher)
    } else {
        controller
    };
    controller
        .run(
            reconcile,
            error_policy,
            state.to_context(
                client,
                pvc_meta_store,
                ss_store,
                security_group_policy_installed,
            ),
        )
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
    // we don't care about status (and don't currently watch anything but metadata)
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
