use std::collections::BTreeMap;
use std::fmt::Display;

use k8s_openapi::{api::core::v1::Secret, apimachinery::pkg::apis::meta::v1::LabelSelector};
use kube::{
    runtime::reflector::{ObjectRef, Store},
    CELSchema, CustomResource,
};
use schemars::schema::Schema;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
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
    /// Example: tag "v1.0" → Configuration "my-service-v1-0" → Restate deployment "dp_abc123"
    ///          Multiple Knative Revisions (00001, 00002, ...) all serve this deployment.
    #[serde(skip_serializing_if = "Option::is_none")]
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

    /// Custom annotations to apply to the Knative Revision template.
    /// These will be merged with operator-managed autoscaling annotations.
    /// Operator-managed annotations take precedence in case of conflicts.
    ///
    /// Common use cases:
    /// - `autoscaling.knative.dev/target-utilization-percentage`: "70"
    /// - `autoscaling.knative.dev/scale-down-delay`: "15m"
    /// - `autoscaling.knative.dev/scale-to-zero-pod-retention-period`: "10m"
    ///
    /// See: https://knative.dev/docs/serving/autoscaling/
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision_annotations: Option<BTreeMap<String, String>>,

    /// Custom annotations to apply to the Knative Route resource.
    /// These will be merged with operator-managed Route annotations.
    /// Operator-managed annotations take precedence in case of conflicts.
    ///
    /// Common use cases:
    /// - `serving.knative.dev/rollout-duration`: "380s"
    ///
    /// See: https://knative.dev/docs/serving/rolling-out-latest-revision/
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route_annotations: Option<BTreeMap<String, String>>,
}

/// RestateDeployment is similar to a Kubernetes Deployment but tailored for Restate services.
/// It maintains ReplicaSets and Services for each version to support Restate's versioning requirements,
/// ensuring old versions remain available until all invocations against them are complete.
#[derive(CustomResource, Deserialize, Serialize, Clone, Debug, CELSchema)]
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
    printcolumn = r#"{"name":"Containers", "type":"string", "jsonPath":".spec.template.spec.containers[*].name", "priority": 1}"#,
    printcolumn = r#"{"name":"Images", "type":"string", "jsonPath":".spec.template.spec.containers[*].image", "priority": 1}"#,
    printcolumn = r#"{"name":"Selector", "type":"string", "jsonPath":".status.labelSelector", "priority": 1}"#
)]
#[kube(status = "RestateDeploymentStatus", shortname = "rsd")]
#[serde(rename_all = "camelCase")]
pub struct RestateDeploymentSpec {
    /// Deployment mode: replicaset (default) or knative
    /// If not specified and knative field is present, defaults to knative mode
    /// This field is immutable after creation
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cel_validate(rule = Rule::new("self == oldSelf").message("deploymentMode is immutable after creation"))]
    pub deployment_mode: Option<DeploymentMode>,

    /// Knative-specific configuration
    /// When specified, enables Knative Serving mode
    #[serde(skip_serializing_if = "Option::is_none")]
    pub knative: Option<KnativeDeploymentSpec>,

    /// Number of desired pods. Defaults to 1.
    /// Only used in ReplicaSet mode
    #[schemars(default = "default_replicas", range(min = 0))]
    pub replicas: i32,

    /// The number of old ReplicaSets to retain to allow rollback. Defaults to 10.
    /// Only used in ReplicaSet mode
    #[schemars(default = "default_revision_history_limit", range(min = 0))]
    pub revision_history_limit: i32,

    /// Minimum number of seconds for which a newly created pod should be ready.
    /// Only used in ReplicaSet mode
    #[schemars(range(min = 0))]
    pub min_ready_seconds: Option<i32>,

    /// Label selector for pods. Must match the pod template's labels.
    /// Only used in ReplicaSet mode
    #[serde(default)]
    #[schemars(schema_with = "label_selector_schema")]
    pub selector: Option<LabelSelector>,

    /// Template describes the pods that will be created.
    pub template: PodTemplateSpec,

    /// Restate specific configuration
    pub restate: RestateSpec,
}

fn default_replicas() -> i32 {
    1
}

fn default_revision_history_limit() -> i32 {
    10
}

fn label_selector_schema(_gen: &mut schemars::gen::SchemaGenerator) -> Schema {
    serde_json::from_value(serde_json::json!({
      "nullable": true,
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
    })).unwrap()
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct PodTemplateSpec {
    /// Standard object's metadata. More info: https://git.k8s.io/community/contributors/devel/sig-architecture/api-conventions.md#metadata
    pub metadata: Option<PodTemplateMetadata>,

    /// Specification of the desired behavior of the pod. More info: https://git.k8s.io/community/contributors/devel/sig-architecture/api-conventions.md#spec-and-status.
    /// The contents of this field are passed through directly from the operator to the created ReplicaSet and are not validated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spec: Option<serde_json::Value>,
}

// Custom JsonSchema implementation to properly mark spec as optional
impl JsonSchema for PodTemplateSpec {
    fn schema_name() -> String {
        "PodTemplateSpec".to_string()
    }

    fn json_schema(gen: &mut schemars::gen::SchemaGenerator) -> Schema {
        serde_json::from_value(serde_json::json!({
            "description": "Template describes the pods that will be created.",
            "type": "object",
            "properties": {
                "metadata": {
                    "description": "Standard object's metadata. More info: https://git.k8s.io/community/contributors/devel/sig-architecture/api-conventions.md#metadata",
                    "nullable": true,
                    "type": "object",
                    "properties": {
                        "annotations": {
                            "description": "Annotations is an unstructured key value map stored with a resource that may be set by external tools to store and retrieve arbitrary metadata. They are not queryable and should be preserved when modifying objects. More info: https://kubernetes.io/docs/concepts/overview/working-with-objects/annotations",
                            "nullable": true,
                            "type": "object",
                            "additionalProperties": {
                                "type": "string"
                            }
                        },
                        "labels": {
                            "description": "Map of string keys and values that can be used to organize and categorize (scope and select) objects. May match selectors of replication controllers and services. More info: https://kubernetes.io/docs/concepts/overview/working-with-objects/labels",
                            "nullable": true,
                            "type": "object",
                            "additionalProperties": {
                                "type": "string"
                            }
                        }
                    }
                },
                "spec": {
                    "description": "Specification of the desired behavior of the pod. More info: https://git.k8s.io/community/contributors/devel/sig-architecture/api-conventions.md#spec-and-status. The contents of this field are passed through directly from the operator to the created ReplicaSet and are not validated.",
                    "x-kubernetes-preserve-unknown-fields": true,
                    "nullable": true
                }
            }
            // Note: No "required" array here, making both fields optional
        }))
        .unwrap()
    }
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
    fn schema_name() -> String {
        "RestateAdminEndpoint".into()
    }

    fn json_schema(gen: &mut schemars::r#gen::SchemaGenerator) -> Schema {
        let mut service_schema = serde_json::to_value(ServiceReference::json_schema(gen)).unwrap();
        if let Some(object) = service_schema.as_object_mut() {
            object.insert("description".into(), serde_json::Value::String("A reference to a Service pointing against which to register the deployment. Exactly one of `cluster`, `cloud`, `service` or `url` must be specified".into()));
        }

        serde_json::from_value(json!({
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
            }))
            .unwrap()
    }
}

impl RestateAdminEndpoint {
    pub fn admin_url(&self, rce_store: &Store<RestateCloudEnvironment>) -> crate::Result<Url> {
        match (
            self.cluster.as_deref(),
            self.cloud.as_deref(),
            self.service.as_ref(),
            self.url.as_ref(),
        ) {
            (Some(cluster), None, None, None) => {
                Ok(service_url("restate", cluster, 9070, None)?)
            }
            (None, Some(cloud), None, None) => {
                let Some(rce) = rce_store.get(&ObjectRef::new(cloud)) else {
                    return Err(crate::Error::RestateCloudEnvironmentNotFound(cloud.into()))
                };

                Ok(rce.admin_url()?)
            }
            (None, None,Some(service), None) => Ok(service_url(&service.name, &service.namespace, service.port.unwrap_or(9070), service.path.as_deref())?),
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
    ) -> crate::Result<Url> {
        match (
            self.cluster.as_deref(),
            self.cloud.as_deref(),
            self.service.as_ref(),
            self.url.as_ref(),
        ) {
            (Some(_), None, None, None) | (None, None,Some(_), None) | (None, None, None, Some(_)) => {
                Ok(service_url(service_name, service_namespace, 9080, None)?)
            }
            (None, Some(cloud), None, None) => {
                let Some(rce) = rce_store.get(&ObjectRef::new(cloud)) else {
                    return Err(crate::Error::RestateCloudEnvironmentNotFound(cloud.into()))
                };

                let service_url = service_url(service_name, service_namespace, 9080, None)?;

                Ok(rce.tunnel_url(service_url)?)
            }
            _ => Err(crate::Error::InvalidRestateConfig(
                "Exactly one of `cluster`, `cloud`, `service` or `url` must be specified in spec.restate"
                    .into(),
            )),
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
    /// Knative-specific status
    /// Only populated when deployment_mode is knative
    #[serde(skip_serializing_if = "Option::is_none")]
    pub knative: Option<KnativeDeploymentStatus>,

    /// Total number of updated non-terminated pods targeted by this RestateDeployment
    pub replicas: i32,

    /// Desired number of replicas
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

    /// Count of hash collisions for the RestateDeployment. The controller uses this field as a collision avoidance mechanism when it needs to create the name for the newest ReplicaSet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collision_count: Option<i32>,

    /// The generation observed by the controller
    pub observed_generation: Option<i64>,

    /// Represents the latest available observations of current state
    pub conditions: Option<Vec<RestateDeploymentCondition>>,

    /// The label selector of the RestateDeployment as a string, for `kubectl get rsd -o wide`
    /// Only used in ReplicaSet mode
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label_selector: Option<String>,
}

/// Knative deployment status
#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct KnativeDeploymentStatus {
    /// Current tag value from spec (or computed template hash)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_tag: Option<String>,

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

    /// Restate deployment ID for the current tag
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deployment_id: Option<String>,
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
