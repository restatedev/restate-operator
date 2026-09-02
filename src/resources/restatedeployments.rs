use std::fmt::Display;

use k8s_openapi::{api::core::v1::Secret, apimachinery::pkg::apis::meta::v1::LabelSelector};
use kube::{
    CustomResource, KubeSchema,
    runtime::reflector::{ObjectRef, Store},
};
use schemars::JsonSchema;
use schemars::Schema;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::{
    controllers::service_url, resources::restatecloudenvironments::RestateCloudEnvironment,
};

pub static RESTATE_DEPLOYMENT_FINALIZER: &str = "deployments.restate.dev";

/// Deployment mode determines how the RestateDeployment runs workloads
#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DeploymentMode {
    /// ReplicaSet mode (default): Manages Pods via Kubernetes ReplicaSets
    Replicaset,
    /// Knative mode: Manages workloads via Knative Serving Configurations and Routes
    Knative,
}

/// Tunnel mode determines how Restate Cloud reaches a deployment registered
/// against a RestateCloudEnvironment
#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TunnelMode {
    /// External mode (default): invocations are forwarded to this deployment's
    /// Service by the tunnel-client pods managed by the RestateCloudEnvironment
    External,
    /// In-process mode: the deployment's pods hold their own outbound tunnel
    /// connections (e.g. with @restatedev/restate-sdk-tunnel), so no inbound
    /// networking to them is needed. The operator injects RESTATE_INPROC_*
    /// environment variables into the pod template and registers the
    /// per-revision tunnel URL directly
    InProcess,
}

/// What to do about in-flight invocations when a RestateDeployment is deleted
#[derive(Deserialize, Serialize, Clone, Copy, Debug, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum DeletePolicy {
    /// Drain (default): wait for in-flight invocations to finish before removing the
    /// deployment from Restate. `drain` sets how long to wait and what to do at the
    /// deadline
    Drain,
    /// Deregister and tear down immediately, without draining
    Force,
}

/// What a drain does once its timeout has passed
#[derive(Deserialize, Serialize, Clone, Copy, Debug, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum OnTimeout {
    /// Hold (default): keep waiting. The deletion stays held until the invocations
    /// finish, and `.status.deletion` reports the drain as overdue
    Hold,
    /// Force-deregister, abandoning whatever is still in flight
    Force,
}

/// How a `deletePolicy: drain` waits
#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DrainSpec {
    /// Seconds to wait for in-flight invocations to finish. Defaults to 3600 (1 hour).
    /// Under `onTimeout: hold` this only sets when the drain reports itself overdue; it
    /// never lets the deletion through
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0))]
    pub timeout_seconds: Option<i64>,

    /// What to do once `timeoutSeconds` has passed. Defaults to `hold`.
    /// - `hold`: keep waiting, and report the drain as overdue
    /// - `force`: force-deregister, abandoning whatever is left
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "on_timeout_schema")]
    pub on_timeout: Option<OnTimeout>,
}

/// Knative-specific deployment configuration
#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct KnativeDeploymentSpec {
    /// Deployment tag - determines Restate deployment identity.
    ///
    /// A Restate deployment is a specific, versioned instance of your service code.
    /// Each deployment is immutable: once registered with Restate, its endpoint and
    /// identity (deployment ID) must not change.
    ///
    /// The tag acts as a stable label that groups multiple Knative Revisions under
    /// a single Restate deployment:
    /// - **Same tag**: In-place updates create new Knative Revisions within the same
    ///   Restate deployment (no new registration)
    /// - **Changed tag**: Creates a new Restate deployment with a new deployment ID
    ///   (versioned update)
    /// - **No tag specified**: Uses template hash as tag, causing every template change
    ///   to create a new Restate deployment
    ///
    /// The tag must be a valid DNS-1035 label: a lowercase RFC 1123 label that consists of
    /// lower case alphanumeric characters or '-', and must start and end with an alphanumeric
    /// character (e.g. 'my-name',  or '123-abc', regex used for validation is
    /// '[a-z]([-a-z0-9]*[a-z0-9])?').
    ///
    /// Example: tag "v1-0" → Configuration "my-service-v1-0" → Restate deployment "dp_abc123"
    ///          Multiple Knative Revisions (00001, 00002, ...) all serve this deployment.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(regex(pattern = r"^[a-z]([-a-z0-9]*[a-z0-9])?$"))]
    pub tag: Option<String>,

    /// Minimum number of replicas (default: 0 for scale-to-zero)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0))]
    pub min_scale: Option<i32>,

    /// Maximum number of replicas (default: unlimited)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0))]
    pub max_scale: Option<i32>,

    /// Target concurrent requests per replica (default: 100)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0))]
    pub target: Option<i32>,
}

/// RestateDeployment is similar to a Kubernetes Deployment but tailored for Restate services.
/// It maintains ReplicaSets and Services for each version to support Restate's versioning requirements,
/// ensuring old versions remain available until all invocations against them are complete.
#[derive(CustomResource, Deserialize, Serialize, Clone, Debug, KubeSchema)]
#[kube(
    kind = "RestateDeployment",
    group = "restate.dev",
    version = "v1beta1",
    namespaced,
    scale = r#"{"specReplicasPath": ".spec.replicas", "statusReplicasPath": ".status.replicas", "labelSelectorPath": ".status.labelSelector"}"#,
    printcolumn = r#"{"name":"Desired", "type":"integer", "jsonPath":".status.desiredReplicas"}"#,
    printcolumn = r#"{"name":"Up-To-Date", "type":"integer", "jsonPath":".status.replicas"}"#,
    printcolumn = r#"{"name":"Ready", "type":"integer", "jsonPath":".status.readyReplicas"}"#,
    printcolumn = r#"{"name":"Available", "type":"integer", "jsonPath":".status.availableReplicas"}"#,
    printcolumn = r#"{"name":"Age", "type":"date", "jsonPath":".metadata.creationTimestamp"}"#,
    printcolumn = r#"{"name":"Deployment ID", "type":"string", "jsonPath":".status.deploymentId", "priority": 1}"#,
    printcolumn = r#"{"name":"Containers", "type":"string", "jsonPath":".spec.template.spec.containers[*].name", "priority": 1}"#,
    printcolumn = r#"{"name":"Images", "type":"string", "jsonPath":".spec.template.spec.containers[*].image", "priority": 1}"#,
    printcolumn = r#"{"name":"Selector", "type":"string", "jsonPath":".status.labelSelector", "priority": 1}"#,
    printcolumn = r#"{"name":"Deletion", "type":"string", "jsonPath":".status.deletion.phase", "priority": 1}"#,
    printcolumn = r#"{"name":"Pending Invocations", "type":"integer", "jsonPath":".status.deletion.totalPendingInvocations", "priority": 1}"#
)]
#[kube(status = "RestateDeploymentStatus", shortname = "rsd")]
#[serde(rename_all = "camelCase")]
// Per-version autoscaling is only wired into the ReplicaSet path; in Knative mode
// Knative's own autoscaler handles scaling, so reject the field rather than
// silently ignoring it.
#[x_kube(validation = Rule::new(
    "!has(self.autoscaling) || !has(self.deploymentMode) || self.deploymentMode != 'knative'"
).message("spec.autoscaling is only supported in replicaset mode (not knative)"))]
pub struct RestateDeploymentSpec {
    /// Deployment mode: replicaset (default) or knative.
    /// This field is immutable after creation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "deployment_mode_schema")]
    #[x_kube(validation = Rule::new("self == oldSelf").message("deploymentMode is immutable after creation"))]
    pub deployment_mode: Option<DeploymentMode>,

    /// Knative-specific configuration.
    /// When specified, enables Knative Serving mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub knative: Option<KnativeDeploymentSpec>,

    /// Number of desired pods. Defaults to 1.
    /// Only used in ReplicaSet mode.
    #[schemars(default = "default_replicas", range(min = 0))]
    pub replicas: i32,

    /// The number of old ReplicaSets to retain to allow rollback. Defaults to 10.
    /// Only used in ReplicaSet mode.
    #[schemars(default = "default_revision_history_limit", range(min = 0))]
    pub revision_history_limit: i32,

    /// Minimum number of seconds for which a newly created pod should be ready.
    /// Only used in ReplicaSet mode.
    #[schemars(range(min = 0))]
    pub min_ready_seconds: Option<i32>,

    /// Label selector for pods. Must match the pod template's labels.
    /// Only used in ReplicaSet mode.
    #[serde(default)]
    #[schemars(schema_with = "label_selector_schema")]
    pub selector: Option<LabelSelector>,

    /// Template describes the pods that will be created.
    pub template: PodTemplateSpec,

    /// Restate specific configuration
    pub restate: RestateSpec,

    /// Optional autoscaling for draining (non-latest) versions. ReplicaSet mode only.
    ///
    /// When set, the operator creates one HorizontalPodAutoscaler per non-latest
    /// version that still has active invocations, so old versions shed compute as
    /// their load falls instead of holding full replicas for the entire (possibly
    /// multi-hour) drain window. The HPA is removed — and the version then scaled
    /// to zero by the operator — once Restate reports the version has no remaining
    /// invocations.
    ///
    /// This is a pass-through HorizontalPodAutoscaler `.spec`: provide
    /// `minReplicas`, `maxReplicas`, `metrics` and optionally `behavior`. The
    /// operator injects `scaleTargetRef` per version, so it must be omitted.
    ///
    /// Notes:
    /// - The latest version is autoscaled separately, via an HPA targeting the
    ///   RestateDeployment scale subresource; it is not covered here.
    /// - `minReplicas` is floored at 1 (there is no scale-to-zero in ReplicaSet
    ///   mode); a value below 1 is raised to 1.
    /// - CPU/memory metrics require container resource `requests` to be set;
    ///   prefer CPU over memory (memory does not scale back down).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "autoscaling_schema")]
    pub autoscaling: Option<serde_json::Value>,
}

fn default_replicas() -> i32 {
    1
}

fn autoscaling_schema(_g: &mut schemars::SchemaGenerator) -> Schema {
    // `type: object` makes the field structurally known (so the spec-level CEL
    // rule can reference `self.autoscaling`), while preserve-unknown keeps its
    // contents a free-form pass-through HPA spec. An HPA `.spec` is always an
    // object, so this is also simply the more accurate schema.
    schemars::json_schema!({
        "type": "object",
        "x-kubernetes-preserve-unknown-fields": true
    })
}

fn default_revision_history_limit() -> i32 {
    10
}

fn deployment_mode_schema(_g: &mut schemars::SchemaGenerator) -> Schema {
    schemars::json_schema!({
        "description": "Deployment mode determines how the RestateDeployment runs workloads",
        "enum": ["replicaset", "knative"],
        "type": "string",
        "nullable": true
    })
}

fn label_selector_schema(_g: &mut schemars::SchemaGenerator) -> Schema {
    schemars::json_schema!({
        "properties": {
            "matchExpressions": {
                "description": "matchExpressions is a list of label selector requirements. The requirements are ANDed.",
                "items": {
                    "description": "A label selector requirement is a selector that contains values, a key, and an operator that\nrelates the key and values.",
                    "properties": {
                        "key": {
                            "description": "key is the label key that the selector applies to.",
                            "type": "string"
                        },
                        "operator": {
                            "description": "operator represents a key's relationship to a set of values.\nValid operators are In, NotIn, Exists and DoesNotExist.",
                            "type": "string"
                        },
                        "values": {
                            "description": "values is an array of string values. If the operator is In or NotIn,\nthe values array must be non-empty. If the operator is Exists or DoesNotExist,\nthe values array must be empty. This array is replaced during a strategic\nmerge patch.",
                            "items": {
                                "type": "string"
                            },
                            "type": "array",
                            "x-kubernetes-list-type": "atomic"
                        }
                    },
                    "required": [
                        "key",
                        "operator"
                    ],
                    "type": "object"
                },
                "type": "array",
                "x-kubernetes-list-type": "atomic"
            },
            "matchLabels": {
                "additionalProperties": {
                    "type": "string"
                },
                "description": "matchLabels is a map of {key,value} pairs. A single {key,value} in the matchLabels\nmap is equivalent to an element of matchExpressions, whose key field is \"key\", the\noperator is \"In\", and the values array contains only \"value\". The requirements are ANDed.",
                "type": "object"
            }
        },
        "type": "object",
        "x-kubernetes-map-type": "atomic"
    })
}

#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema)]
pub struct PodTemplateSpec {
    /// Standard object's metadata. More info: https://git.k8s.io/community/contributors/devel/sig-architecture/api-conventions.md#metadata
    pub metadata: Option<PodTemplateMetadata>,

    /// Specification of the desired behavior of the pod. More info: https://git.k8s.io/community/contributors/devel/sig-architecture/api-conventions.md#spec-and-status.
    ///
    /// The contents of this field are passed through directly from the operator to the created workload:
    /// - **ReplicaSet mode**: Passed to the ReplicaSet's pod template spec.
    /// - **Knative mode**: Passed to the Configuration's revision template spec. This supports standard PodSpec fields (containers, serviceAccountName, volumes, etc.) as well as Knative-specific fields (timeoutSeconds, containerConcurrency, etc.).
    #[schemars(schema_with = "pod_spec_schema")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spec: Option<serde_json::Value>,
}

fn pod_spec_schema(_g: &mut schemars::SchemaGenerator) -> Schema {
    schemars::json_schema!({
        "x-kubernetes-preserve-unknown-fields": true
    })
}

/// PodTemplateMetadata is a subset of ObjectMeta that is valid for pod templates
#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
pub struct PodTemplateMetadata {
    /// Annotations is an unstructured key value map stored with a resource that may be set by external tools to store and retrieve arbitrary metadata. They are not queryable and should be preserved when modifying objects. More info: https://kubernetes.io/docs/concepts/overview/working-with-objects/annotations
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<std::collections::BTreeMap<String, String>>,

    /// Map of string keys and values that can be used to organize and categorize (scope and select) objects. May match selectors of replication controllers and services. More info: https://kubernetes.io/docs/concepts/overview/working-with-objects/labels
    #[serde(skip_serializing_if = "Option::is_none")]
    pub labels: Option<std::collections::BTreeMap<String, String>>,
}

/// Restate specific configuration
#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RestateSpec {
    /// The location of the Restate Admin API to register this deployment against
    pub register: RestateAdminEndpoint,

    /// Optional path to append to the Service url when registering with Restate.
    /// If not provided, the service will be registered at the root path "/".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_path: Option<String>,

    /// Force the use of HTTP/1.1 when registering with Restate
    #[serde(skip_serializing_if = "Option::is_none")]
    pub use_http11: Option<bool>,

    /// How Restate Cloud reaches this deployment. Only takes effect with
    /// `register.cloud` (`in-process` requires it, and is not supported in Knative
    /// mode).
    /// - `external` (default): invocations are forwarded to this deployment's Service
    ///   by the tunnel-client pods managed by the RestateCloudEnvironment.
    /// - `in-process`: the deployment's pods hold their own outbound tunnel connections
    ///   (e.g. with @restatedev/restate-sdk-tunnel), so no inbound networking to them is
    ///   needed. The operator injects RESTATE_INPROC_* environment variables identifying
    ///   the revision into the pod template and registers the per-revision tunnel URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "tunnel_mode_schema")]
    pub tunnel_mode: Option<TunnelMode>,

    /// Seconds to wait before removing old versions after they are drained.
    /// Defaults to 300 (5 minutes).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0))]
    pub drain_delay_seconds: Option<i64>,

    /// What to do about in-flight invocations when this RestateDeployment is deleted.
    /// Defaults to `drain`.
    /// - `drain`: wait for in-flight invocations to finish, on the terms set by `drain`.
    /// - `force`: deregister and tear down immediately, without draining.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "delete_policy_schema")]
    pub delete_policy: Option<DeletePolicy>,

    /// How `deletePolicy: drain` waits: how long, and whether it eventually gives up.
    /// Ignored by `deletePolicy: force`, which never waits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drain: Option<DrainSpec>,
}

impl RestateSpec {
    pub fn drain_delay_seconds(&self) -> i64 {
        self.drain_delay_seconds.unwrap_or(300).max(0)
    }

    pub fn is_in_process_tunnel(&self) -> bool {
        matches!(self.tunnel_mode, Some(TunnelMode::InProcess))
    }

    pub fn delete_policy(&self) -> DeletePolicy {
        self.delete_policy.unwrap_or(DeletePolicy::Drain)
    }

    pub fn drain_timeout_seconds(&self) -> i64 {
        self.drain
            .as_ref()
            .and_then(|drain| drain.timeout_seconds)
            .unwrap_or(3600)
            .max(0)
    }

    /// What the drain does at its deadline. `force` never gets this far, so it reads as
    /// the default rather than as a decision anyone made.
    pub fn drain_on_timeout(&self) -> OnTimeout {
        self.drain
            .as_ref()
            .and_then(|drain| drain.on_timeout)
            .unwrap_or(OnTimeout::Hold)
    }
}

fn tunnel_mode_schema(_g: &mut schemars::SchemaGenerator) -> Schema {
    schemars::json_schema!({
        "description": "How Restate Cloud reaches this deployment. Only takes effect with `register.cloud` (`in-process` requires it, and is not supported in Knative mode). `external` (default): invocations are forwarded to this deployment's Service by the tunnel-client pods managed by the RestateCloudEnvironment. `in-process`: the deployment's pods hold their own outbound tunnel connections (e.g. with @restatedev/restate-sdk-tunnel), so no inbound networking to them is needed; the operator injects RESTATE_INPROC_* environment variables identifying the revision into the pod template and registers the per-revision tunnel URL.",
        "enum": ["external", "in-process"],
        "type": "string",
        "nullable": true
    })
}

fn delete_policy_schema(_g: &mut schemars::SchemaGenerator) -> Schema {
    schemars::json_schema!({
        "description": "What to do about in-flight invocations when this RestateDeployment is deleted. Defaults to `drain`. `drain`: wait for in-flight invocations to finish, on the terms set by `drain` (how long, and whether to give up). `force`: deregister and tear down immediately, without draining.",
        "enum": ["drain", "force"],
        "type": "string",
        "nullable": true
    })
}

fn on_timeout_schema(_g: &mut schemars::SchemaGenerator) -> Schema {
    schemars::json_schema!({
        "description": "What to do once the drain's `timeoutSeconds` has passed. Defaults to `hold`. `hold`: keep waiting; the deletion stays held until the invocations finish, and `.status.deletion` reports the drain as overdue. `force`: force-deregister, abandoning whatever is still in flight.",
        "enum": ["hold", "force"],
        "type": "string",
        "nullable": true
    })
}

/// The location of the Restate Admin API to register this deployment against
#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RestateAdminEndpoint {
    /// The name of a RestateCluster against which to register the deployment.
    /// Exactly one of `cluster`, `cloud`, `service` or `url` must be specified
    pub cluster: Option<String>,
    /// The name of a RestateCloudEnvironment against which to register the deployment.
    /// Exactly one of `cluster`, `cloud`, `service` or `url` must be specified
    pub cloud: Option<String>,
    /// A reference to a Service against which to register the deployment.
    /// Exactly one of `cluster`, `cloud`, `service` or `url` must be specified
    pub service: Option<ServiceReference>,
    /// A url of the restate admin endpoint against which to register the deployment
    /// Exactly one of `cluster`, `cloud`, `service` or `url` must be specified
    pub url: Option<Url>,
}

impl Display for RestateAdminEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (&self.cluster, &self.cloud, &self.service, &self.url) {
            (Some(cluster), _, _, _) => write!(f, "RestateCluster/{cluster}"),
            (_, Some(cloud), _, _) => write!(f, "RestateCloudEnvironment/{cloud}"),
            (_, _, Some(service), _) => write!(f, "Service/{}/{}", service.namespace, service.name),
            (_, _, _, Some(url)) => write!(f, "{url}"),
            _ => write!(f, "N/A"),
        }
    }
}

// Custom JsonSchema implementation so that we can make one of cluster, service, url required.
impl JsonSchema for RestateAdminEndpoint {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "RestateAdminEndpoint".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> Schema {
        let mut service_schema = generator.subschema_for::<ServiceReference>();
        service_schema.insert("description".into(), serde_json::Value::String("A reference to a Service pointing against which to register the deployment. Exactly one of `cluster`, `cloud`, `service` or `url` must be specified".into()));

        schemars::json_schema!({
            "description": "The location of the Restate Admin API to register this deployment against",
            "properties": {
                "cluster": {
                    "description": "The name of a RestateCluster against which to register the deployment. Exactly one of `cluster`, `cloud`, `service` or `url` must be specified",
                    "type": "string"
                },
                "cloud": {
                    "description": "The name of a RestateCloudEnvironment against which to register the deployment. Exactly one of `cluster`, `cloud`, `service` or `url` must be specified",
                    "type": "string"
                },
                "service": service_schema,
                "url": {
                    "description": "A url of the restate admin endpoint against which to register the deployment Exactly one of `cluster`, `cloud`, `service` or `url` must be specified",
                    "type": "string"
                }
            },
            "oneOf": [
                {"required": ["cluster"]},
                {"required": ["cloud"]},
                {"required": ["service"]},
                {"required": ["url"]}
            ],
            "type": "object"
        })
    }
}

impl RestateAdminEndpoint {
    pub fn admin_url(
        &self,
        rce_store: &Store<RestateCloudEnvironment>,
        cluster_dns: &str,
    ) -> crate::Result<Url> {
        match (
            self.cluster.as_deref(),
            self.cloud.as_deref(),
            self.service.as_ref(),
            self.url.as_ref(),
        ) {
            (Some(cluster), None, None, None) => {
                Ok(service_url("restate", cluster, 9070, None, cluster_dns)?)
            }
            (None, Some(cloud), None, None) => {
                let Some(rce) = rce_store.get(&ObjectRef::new(cloud)) else {
                    return Err(crate::Error::RestateCloudEnvironmentNotFound(cloud.into()))
                };

                Ok(rce.admin_url()?)
            }
            (None, None, Some(service), None) => Ok(service_url(
                &service.name,
                &service.namespace,
                service.port.unwrap_or(9070),
                service.path.as_deref(),
                cluster_dns,
            )?),
            (None, None, None, Some(url)) => Ok(url.clone()),
            _ => Err(crate::Error::InvalidRestateConfig(
                "Exactly one of `cluster`, `cloud`, `service` or `url` must be specified in spec.restate"
                    .into(),
            )),
        }
    }

    pub fn service_url(
        &self,
        rce_store: &Store<RestateCloudEnvironment>,
        service_name: &str,
        service_namespace: &str,
        service_path: Option<&str>,
        cluster_dns: &str,
    ) -> crate::Result<Url> {
        self.validate_exactly_one_set()?;
        let url = service_url(
            service_name,
            service_namespace,
            9080,
            service_path,
            cluster_dns,
        )?;
        self.maybe_tunnel_url(rce_store, url)
    }

    fn validate_exactly_one_set(&self) -> crate::Result<()> {
        let count = self.cluster.is_some() as u8
            + self.cloud.is_some() as u8
            + self.service.is_some() as u8
            + self.url.is_some() as u8;
        if count != 1 {
            return Err(crate::Error::InvalidRestateConfig(
                "Exactly one of `cluster`, `cloud`, `service` or `url` must be specified in spec.restate"
                    .into(),
            ));
        }
        Ok(())
    }

    /// If this endpoint is configured to use Restate Cloud, rewrite the given URL
    /// to go through the cloud tunnel. Otherwise, return the URL unchanged.
    pub fn maybe_tunnel_url(
        &self,
        rce_store: &Store<RestateCloudEnvironment>,
        url: Url,
    ) -> crate::Result<Url> {
        if let Some(cloud) = self.cloud.as_deref() {
            let Some(rce) = rce_store.get(&ObjectRef::new(cloud)) else {
                return Err(crate::Error::RestateCloudEnvironmentNotFound(cloud.into()));
            };
            Ok(rce.tunnel_url(url)?)
        } else {
            Ok(url)
        }
    }

    pub fn bearer_token(
        &self,
        rce_store: &Store<RestateCloudEnvironment>,
        secret_store: &Store<Secret>,
        operator_namespace: &str,
    ) -> crate::Result<Option<String>> {
        if let Some(cloud) = self.cloud.as_deref() {
            let Some(rce) = rce_store.get(&ObjectRef::new(cloud)) else {
                return Err(crate::Error::RestateCloudEnvironmentNotFound(cloud.into()));
            };

            Ok(Some(rce.bearer_token(secret_store, operator_namespace)?))
        } else {
            Ok(None)
        }
    }
}

/// ServiceReference describes a reference to a Kubernetes Service that hosts the Restate admin API
#[derive(Deserialize, Serialize, Clone, Default, Debug, JsonSchema)]
pub struct ServiceReference {
    /// `name` is the name of the service. Required
    pub name: String,

    /// `namespace` is the namespace of the service. Required
    pub namespace: String,

    /// `path` is an optional URL path which will be prepended before admin api paths. Should not end in a /.
    pub path: Option<String>,

    /// If specified, the port on the service that hosts the admin api. Defaults to 9070. `port` should be a valid port number (1-65535, inclusive).
    #[schemars(range(min = 1, max = 65535))]
    pub port: Option<i32>,
}

/// Status of the RestateDeployment
/// This is set and managed automatically by the controller
#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RestateDeploymentStatus {
    /// Restate deployment ID for the current tag
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deployment_id: Option<String>,

    /// What the operator is currently doing with this deployment.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(default, schema_with = "crate::resources::reconciliation_schema")]
    pub reconciliation: Option<crate::resources::ReconciliationState>,

    /// Knative-specific status
    #[serde(skip_serializing_if = "Option::is_none")]
    pub knative: Option<KnativeDeploymentStatus>,

    /// Total number of updated non-terminated pods targeted by this RestateDeployment
    pub replicas: i32,

    /// Desired number of replicas.
    /// - For ReplicaSet mode: reflects spec.replicas
    /// - For Knative mode: reflects revision.status.desired_replicas from latest revision
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desired_replicas: Option<i32>,

    /// Total number of updated ready pods
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ready_replicas: Option<i32>,

    /// Total number of updated available pods (ready for at least minReadySeconds)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_replicas: Option<i32>,

    /// Total number of updated unavailable pods
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unavailable_replicas: Option<i32>,

    /// Count of hash collisions for the RestateDeployment. The controller uses this field as a collision avoidance mechanism when it needs to create the name for the newest ReplicaSet or Configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collision_count: Option<i32>,

    /// The generation observed by the controller
    pub observed_generation: Option<i64>,

    /// Represents the latest available observations of current state
    pub conditions: Option<Vec<RestateDeploymentCondition>>,

    /// The label selector of the RestateDeployment as a string, for `kubectl get rsd -o wide`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label_selector: Option<String>,

    /// Progress of an in-flight deletion. Only set once the RestateDeployment has been
    /// deleted and the operator is working through `spec.restate.deletePolicy`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deletion: Option<DeletionStatus>,
}

/// Where a deletion has got to
#[derive(Deserialize, Serialize, Clone, Copy, Debug, JsonSchema, PartialEq, Eq)]
pub enum DeletionPhase {
    /// Waiting for in-flight invocations to finish, or for the drain delay to elapse
    Draining,
    /// The drain's `timeoutSeconds` has passed and invocations are still in flight.
    /// Under `onTimeout: hold` the deletion stays blocked until they finish, the
    /// timeout is raised, or the policy is changed to `force`
    Overdue,
    /// Removing the deployment from Restate without waiting for in-flight invocations
    Forcing,
}

/// Progress of an in-flight deletion
#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeletionStatus {
    /// The policy being applied
    #[schemars(schema_with = "delete_policy_status_schema")]
    pub policy: DeletePolicy,

    /// Where the deletion has got to
    #[schemars(schema_with = "deletion_phase_schema")]
    pub phase: DeletionPhase,

    /// When deletion was requested
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<k8s_openapi::apimachinery::pkg::apis::meta::v1::Time>,

    /// When the drain timeout expires. Unset under `deletePolicy: force`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deadline: Option<k8s_openapi::apimachinery::pkg::apis::meta::v1::Time>,

    /// What happens at the `deadline`: `hold` keeps waiting, `force` gives up and
    /// deregisters. Unset under `deletePolicy: force`, which has no deadline to reach
    // `default` is what tells schemars the field is optional: `schema_with` hides the
    // `Option` from it, and a required field the operator never sets under `force` would
    // have the API server reject the status apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "on_timeout_schema")]
    pub on_timeout: Option<OnTimeout>,

    /// Human-readable explanation of what the deletion is waiting for
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,

    /// Total invocations across all versions holding the deletion
    pub total_pending_invocations: i64,

    /// The versions holding the deletion, and the invocations they are waiting on
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_invocations: Vec<PendingInvocations>,
}

/// Invocations holding one version of a deleting RestateDeployment
#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PendingInvocations {
    /// The ReplicaSet (or Knative Configuration) name
    pub version: String,

    /// The Restate deployment ID this version is registered as
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deployment_id: Option<String>,

    /// Unfinished invocations already bound to this version
    pub pinned: i64,

    /// Unfinished invocations not yet bound to any version, but targeting a service this
    /// version is the endpoint for. Not counted under `deletePolicy: force`, which does
    /// not run the query that attributes them
    pub unpinned: i64,
}

fn delete_policy_status_schema(_g: &mut schemars::SchemaGenerator) -> Schema {
    schemars::json_schema!({
        "description": "The policy being applied",
        "enum": ["drain", "force"],
        "type": "string"
    })
}

fn deletion_phase_schema(_g: &mut schemars::SchemaGenerator) -> Schema {
    schemars::json_schema!({
        "description": "Where the deletion has got to. `Draining`: waiting for in-flight invocations to finish, or for the drain delay to elapse. `Overdue`: the drain's `timeoutSeconds` has passed and invocations are still in flight; under `onTimeout: hold` the deletion stays blocked until they finish, the timeout is raised, or the policy is changed to `force`. `Forcing`: removing the deployment from Restate without waiting for in-flight invocations.",
        "enum": ["Draining", "Overdue", "Forcing"],
        "type": "string"
    })
}

/// Knative deployment status
#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct KnativeDeploymentStatus {
    /// Name of the active Configuration for the current tag
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configuration_name: Option<String>,

    /// Name of the active Route for the current tag
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route_name: Option<String>,

    /// Default URL for the current deployment
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,

    /// Latest ready revision name for the current Configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_revision: Option<String>,
}

/// Conditions for the RestateDeployment status
#[derive(Deserialize, Serialize, Clone, Default, Debug, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RestateDeploymentCondition {
    /// Last time the condition transitioned from one status to another
    pub last_transition_time: Option<k8s_openapi::apimachinery::pkg::apis::meta::v1::Time>,

    /// Human-readable message indicating details about last transition
    pub message: Option<String>,

    /// Reason for the condition's last transition
    pub reason: Option<String>,

    /// Status is the status of the condition (True, False, Unknown)
    pub status: String,

    /// Type of condition (Ready, Progressing, Available)
    pub r#type: String,
}
