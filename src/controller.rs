use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use futures::StreamExt;
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{
    EnvVar, Namespace, PersistentVolumeClaim, ResourceRequirements, Service, ServiceAccount,
};
use k8s_openapi::api::networking::v1;
use k8s_openapi::api::networking::v1::{NetworkPolicy, NetworkPolicyPeer, NetworkPolicyPort};
use kube::core::PartialObjectMeta;
use kube::runtime::reflector::{ObjectRef, Store};
use kube::runtime::{metadata_watcher, reflector, watcher, WatchStreamExt};
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
use crate::{telemetry, Error, Metrics, Result};

pub static RESTATE_CLUSTER_FINALIZER: &str = "clusters.restate.dev";

/// Represents the configuration of a Restate Cluster
#[derive(CustomResource, Deserialize, Serialize, Clone, Debug, JsonSchema)]
#[cfg_attr(test, derive(Default))]
#[kube(
    kind = "RestateCluster",
    group = "restate.dev",
    version = "v1",
    schema = "manual"
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
    /// if set, create a AWS PodIdentityAssociation using the ACK CRD in order to give the Restate pod access to this role and
    /// allow the cluster to reach the Pod Identity agent.
    pub aws_pod_identity_association_role_arn: Option<String>,
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

#[derive(Deserialize, Serialize, Clone, Default, Debug, JsonSchema)]
pub struct RestateClusterStatus {}

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
            Finalizer::Apply(rc) => rc.reconcile(ctx.clone()).await,
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
    async fn reconcile(&self, ctx: Arc<Context>) -> Result<Action> {
        let client = ctx.client.clone();
        let name = self.name_any();
        let rcs: Api<RestateCluster> = Api::all(client.clone());
        let nss: Api<Namespace> = Api::all(client.clone());

        let oref = self.controller_owner_ref(&()).unwrap();

        if let Some(ns) = nss.get_metadata_opt(&name).await? {
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
                metadata: object_meta(&oref, &name),
                ..Default::default()
            },
        )
        .await?;

        reconcile_network_policies(
            ctx.client.clone(),
            &name,
            &oref,
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

        reconcile_compute(&ctx, &name, &oref, &self.spec).await?;

        // always overwrite status object with what we saw
        let new_status = Patch::Apply(json!({
            "apiVersion": "restate.dev/v1",
            "kind": "RestateCluster",
            "status": RestateClusterStatus {}
        }));
        let ps = PatchParams::apply("restate-operator").force();
        let _o = rcs.patch_status(&name, &ps, &new_status).await?;

        // If no events were received, check back every 5 minutes
        Ok(Action::requeue(Duration::from_secs(5 * 60)))
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
    ) -> Arc<Context> {
        Arc::new(Context {
            client,
            pvc_meta_store,
            ss_store,
            aws_pod_identity_association_cluster: self.aws_pod_identity_association_cluster.clone(),
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
    let rc_api = Api::<RestateCluster>::all(client.clone());
    let ns_api = Api::<Namespace>::all(client.clone());
    let ss_api = Api::<StatefulSet>::all(client.clone());
    let pvc_api = Api::<PersistentVolumeClaim>::all(client.clone());
    let svc_api = Api::<Service>::all(client.clone());
    let svcacc_api = Api::<ServiceAccount>::all(client.clone());
    let np_api = Api::<NetworkPolicy>::all(client.clone());
    let pia_api = Api::<PodIdentityAssociation>::all(client.clone());

    if state.aws_pod_identity_association_cluster.is_some() {
        if let Err(e) = pia_api.list(&ListParams::default().limit(1)).await {
            error!("PodIdentityAssociation is not queryable; {e:?}. Is the CRD installed?");
            std::process::exit(1);
        }
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

    let controller = Controller::new(rc_api, rc_cfg.clone())
        .shutdown_on_signal()
        .owns(ns_api, cfg.clone())
        .owns(svc_api, cfg.clone())
        .owns(svcacc_api, cfg.clone())
        .owns(np_api, cfg.clone())
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
    let controller = if state.aws_pod_identity_association_cluster.is_some() {
        controller.owns(pia_api, cfg.clone())
    } else {
        controller
    };
    controller
        .run(
            reconcile,
            error_policy,
            state.to_context(client, pvc_meta_store, ss_store),
        )
        .filter_map(|x| async move { Result::ok(x) })
        .for_each(|_| futures::future::ready(()))
        .await;
}
