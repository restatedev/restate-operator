use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;
use kube::{CELSchema, CustomResource};
use schemars::schema::Schema;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

pub static RESTATE_DEPLOYMENT_FINALIZER: &str = "deployments.restate.dev";

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
    printcolumn = r#"{"name":"Desired", "type":"integer", "jsonPath":".spec.replicas"}"#,
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
    /// Number of desired pods. Defaults to 1.
    #[schemars(default = "default_replicas", range(min = 0))]
    pub replicas: i32,

    /// The number of old ReplicaSets to retain to allow rollback. Defaults to 10.
    #[schemars(default = "default_revision_history_limit", range(min = 0))]
    pub revision_history_limit: i32,

    /// Minimum number of seconds for which a newly created pod should be ready.
    #[schemars(range(min = 0))]
    pub min_ready_seconds: Option<i32>,

    /// Label selector for pods. Must match the pod template's labels.
    #[schemars(schema_with = "label_selector_schema")]
    pub selector: LabelSelector,

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

fn label_selector_schema(_g: &mut schemars::gen::SchemaGenerator) -> Schema {
    serde_json::from_value(serde_json::json!({
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

#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema)]
pub struct PodTemplateSpec {
    /// Standard object's metadata. More info: https://git.k8s.io/community/contributors/devel/sig-architecture/api-conventions.md#metadata
    pub metadata: Option<PodTemplateMetadata>,

    /// Specification of the desired behavior of the pod. More info: https://git.k8s.io/community/contributors/devel/sig-architecture/api-conventions.md#spec-and-status.
    /// The contents of this field are passed through directly from the operator to the created ReplicaSet and are not validated.
    #[schemars(schema_with = "pod_spec_schema")]
    pub spec: Option<serde_json::Value>,
}

fn pod_spec_schema(_g: &mut schemars::gen::SchemaGenerator) -> Schema {
    serde_json::from_value(serde_json::json!({
            "x-kubernetes-preserve-unknown-fields": true,
    }))
    .unwrap()
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
pub struct RestateSpec {
    /// The location of the Restate Admin API to register this deployment against
    pub register: RestateAdminEndpoint,

    /// Optional path to append to the service endpoint when registering with Restate.
    /// If not provided, the service will be registered at the root path "/".
    /// Path must start with "/" and should not end with "/".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_path: Option<String>,
}

/// The location of the Restate Admin API to register this deployment against
#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RestateAdminEndpoint {
    /// The name of a RestateCluster against which to register the deployment.
    /// Exactly one of `cluster`, `service` or `url` must be specified
    pub cluster: Option<String>,
    /// A reference to a Service pointing against which to register the deployment.
    /// Exactly one of `cluster`, `service` or `url` must be specified
    pub service: Option<ServiceReference>,
    /// A url of the restate admin endpoint against which to register the deployment
    /// Exactly one of `cluster`, `service` or `url` must be specified
    pub url: Option<String>,
}

// Custom JsonSchema implementation so that we can make one of cluster, service, url required.
impl JsonSchema for RestateAdminEndpoint {
    fn schema_name() -> String {
        "RestateAdminEndpoint".into()
    }

    fn json_schema(gen: &mut schemars::r#gen::SchemaGenerator) -> Schema {
        let mut service_schema = serde_json::to_value(ServiceReference::json_schema(gen)).unwrap();
        if let Some(object) = service_schema.as_object_mut() {
            object.insert("description".into(), serde_json::Value::String("A reference to a Service pointing against which to register the deployment. Exactly one of `cluster`, `service` or `url` must be specified".into()));
        }

        serde_json::from_value(json!({
                "description": "The location of the Restate Admin API to register this deployment against",
                "properties": {
                  "cluster": {
                    "description": "The name of a RestateCluster against which to register the deployment. Exactly one of `cluster`, `service` or `url` must be specified",
                    "type": "string"
                  },
                  "service": service_schema,
                  "url": {
                    "description": "A url of the restate admin endpoint against which to register the deployment Exactly one of `cluster`, `service` or `url` must be specified",
                    "type": "string"
                  }
                },
                "oneOf": [
                    {"required": ["cluster"]},
                    {"required": ["service"]},
                    {"required": ["url"]}
                ],
                "type": "object"
            }))
            .unwrap()
    }
}

impl RestateAdminEndpoint {
    pub fn url(&self) -> Result<String, crate::Error> {
        match (
            self.cluster.as_deref(),
            self.service.as_ref(),
            self.url.as_ref(),
        ) {
            (Some(cluster), None, None) => {
                Ok(format!("http://restate.{cluster}.svc.cluster.local:9070"))
            }
            (None, Some(service), None) => Ok(format!(
                "http://{}.{}.svc.cluster.local:{}{}",
                service.name,
                service.namespace,
                service.port.unwrap_or(9070),
                service.path.as_deref().unwrap_or(""),
            )),
            (None, None, Some(url)) => Ok(url.clone()),
            _ => Err(crate::Error::InvalidRestateConfig(
                "Exactly one of `cluster`, `service` or `url` must be specified in spec.restate"
                    .into(),
            )),
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
    /// Total number of updated non-terminated pods targeted by this RestateDeployment
    pub replicas: i32,

    /// Total number of updated ready pods
    pub ready_replicas: Option<i32>,

    /// Total number of updated available pods (ready for at least minReadySeconds)
    pub available_replicas: Option<i32>,

    /// Total number of updated unavailable pods
    pub unavailable_replicas: Option<i32>,

    /// Count of hash collisions for the RestateDeployment. The controller uses this field as a collision avoidance mechanism when it needs to create the name for the newest ReplicaSet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collision_count: Option<i32>,

    /// The generation observed by the controller
    pub observed_generation: Option<i64>,

    /// Represents the latest available observations of current state
    pub conditions: Option<Vec<RestateDeploymentCondition>>,

    /// The label selector of the RestateDeployment as a string, for `kubectl get rsd -o wide`
    pub label_selector: Option<String>,
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

impl RestateSpec {
    /// Get the service path with validation, defaulting to "/" if not specified
    pub fn validated_service_path(&self) -> Result<String, crate::Error> {
        match &self.service_path {
            None => Ok("/".to_string()),
            Some(path) => {
                if path.is_empty() {
                    return Err(crate::Error::InvalidRestateConfig(
                        "service_path cannot be empty, use null/undefined to register at root path".into(),
                    ));
                }
                
                if !path.starts_with('/') {
                    return Err(crate::Error::InvalidRestateConfig(
                        format!("service_path '{}' must start with '/'", path),
                    ));
                }
                
                if path != "/" && path.ends_with('/') {
                    return Err(crate::Error::InvalidRestateConfig(
                        format!("service_path '{}' should not end with '/' (except for root path)", path),
                    ));
                }
                
                Ok(path.to_string())
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validated_service_path_defaults_to_root() {
        let spec = RestateSpec {
            register: RestateAdminEndpoint {
                cluster: Some("test-cluster".to_string()),
                service: None,
                url: None,
            },
            service_path: None,
        };
        
        assert_eq!(spec.validated_service_path().unwrap(), "/");
    }

    #[test]
    fn validated_service_path_accepts_valid_paths() {
        let test_cases = vec![
            "/",
            "/api",
            "/api/v1",
            "/some/nested/path",
            "/api-v1",
            "/api_v1",
        ];
        
        for path in test_cases {
            let spec = RestateSpec {
                register: RestateAdminEndpoint {
                    cluster: Some("test-cluster".to_string()),
                    service: None,
                    url: None,
                },
                service_path: Some(path.to_string()),
            };
            
            assert_eq!(spec.validated_service_path().unwrap(), path);
        }
    }

    #[test]
    fn validated_service_path_rejects_empty_string() {
        let spec = RestateSpec {
            register: RestateAdminEndpoint {
                cluster: Some("test-cluster".to_string()),
                service: None,
                url: None,
            },
            service_path: Some("".to_string()),
        };
        
        assert!(spec.validated_service_path().is_err());
    }

    #[test]
    fn validated_service_path_rejects_paths_not_starting_with_slash() {
        let invalid_paths = vec![
            "api",
            "api/v1",
            "some/path",
        ];
        
        for path in invalid_paths {
            let spec = RestateSpec {
                register: RestateAdminEndpoint {
                    cluster: Some("test-cluster".to_string()),
                    service: None,
                    url: None,
                },
                service_path: Some(path.to_string()),
            };
            
            assert!(spec.validated_service_path().is_err());
        }
    }

    #[test]
    fn validated_service_path_rejects_paths_ending_with_slash_except_root() {
        let invalid_paths = vec![
            "/api/",
            "/api/v1/",
            "/some/path/",
        ];
        
        for path in invalid_paths {
            let spec = RestateSpec {
                register: RestateAdminEndpoint {
                    cluster: Some("test-cluster".to_string()),
                    service: None,
                    url: None,
                },
                service_path: Some(path.to_string()),
            };
            
            assert!(spec.validated_service_path().is_err());
        }
    }

    #[test]
    fn validated_service_path_allows_root_path() {
        let spec = RestateSpec {
            register: RestateAdminEndpoint {
                cluster: Some("test-cluster".to_string()),
                service: None,
                url: None,
            },
            service_path: Some("/".to_string()),
        };
        
        assert_eq!(spec.validated_service_path().unwrap(), "/");
    }

    #[test]
    fn service_endpoint_construction() {
        // Test that the service endpoint is constructed correctly with various paths
        let test_cases = vec![
            (None, "http://my-service.default.svc.cluster.local:9080/"),
            (Some("/".to_string()), "http://my-service.default.svc.cluster.local:9080/"),
            (Some("/api".to_string()), "http://my-service.default.svc.cluster.local:9080/api"),
            (Some("/api/v1".to_string()), "http://my-service.default.svc.cluster.local:9080/api/v1"),
        ];
        
        for (service_path, expected_endpoint) in test_cases {
            let spec = RestateSpec {
                register: RestateAdminEndpoint {
                    cluster: Some("test-cluster".to_string()),
                    service: None,
                    url: None,
                },
                service_path,
            };
            
            let validated_path = spec.validated_service_path().unwrap();
            let versioned_name = "my-service";
            let namespace = "default";
            let service_endpoint = format!(
                "http://{versioned_name}.{namespace}.svc.cluster.local:9080{validated_path}"
            );
            
            assert_eq!(service_endpoint, expected_endpoint);
        }
    }
}