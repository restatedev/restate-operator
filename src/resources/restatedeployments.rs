use std::borrow::Cow;

use k8s_openapi::api::core::v1::PodTemplateSpec;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;
use kube::CustomResource;
use schemars::schema::{Schema, SchemaObject};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

pub static RESTATE_DEPLOYMENT_FINALIZER: &str = "deployments.restate.dev";

/// RestateDeployment is similar to a Kubernetes Deployment but tailored for Restate services.
/// It maintains ReplicaSets and Services for each version to support Restate's versioning requirements,
/// ensuring old versions remain available until all invocations against them are complete.
#[derive(CustomResource, Deserialize, Serialize, Clone, Debug, JsonSchema)]
#[cfg_attr(test, derive(Default))]
#[kube(
    kind = "RestateDeployment",
    group = "restate.dev",
    version = "v1",
    schema = "manual",
    namespaced,
    printcolumn = r#"{"name":"Ready", "type":"string", "jsonPath":".status.conditions[?(@.type==\"Ready\")].status"}"#,
    printcolumn = r#"{"name":"UpToDate", "type":"integer", "jsonPath":".status.updatedReplicas"}"#,
    printcolumn = r#"{"name":"Available", "type":"integer", "jsonPath":".status.availableReplicas"}"#,
    printcolumn = r#"{"name":"Age", "type":"date", "jsonPath":".metadata.creationTimestamp"}"#
)]
#[kube(status = "RestateDeploymentStatus", shortname = "rsd")]
#[serde(rename_all = "camelCase")]
pub struct RestateDeploymentSpec {
    /// Number of desired pods. Defaults to 1.
    pub replicas: Option<i32>,

    /// The number of old ReplicaSets to retain to allow rollback. Defaults to 10.
    pub revision_history_limit: Option<i32>,

    /// Minimum number of seconds for which a newly created pod should be ready.
    pub min_ready_seconds: Option<i32>,

    /// Label selector for pods. Must match the pod template's labels.
    pub selector: LabelSelector,

    /// Template describes the pods that will be created.
    pub template: PodTemplateSpec,

    /// Configuration specific to Restate integration
    pub restate: RestateConfig,
}

// Hoisted from the derived implementation for RestateDeployment
impl schemars::JsonSchema for RestateDeployment {
    fn schema_name() -> String {
        "RestateDeployment".to_owned()
    }
    fn schema_id() -> Cow<'static, str> {
        "restate_operator::RestateDeployment::RestateDeployment".into()
    }
    fn json_schema(gen: &mut schemars::gen::SchemaGenerator) -> Schema {
        {
            let mut schema_object = SchemaObject {
                instance_type: Some(
                    schemars::schema::InstanceType::Object.into(),
                ),
                metadata: Some(Box::new(schemars::schema::Metadata {
                    description: Some(
                        "RestateDeployment manages deployments of Restate services with proper versioning".to_owned(),
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

            object_validation.properties.insert(
                "spec".to_owned(),
                gen.subschema_for::<RestateDeploymentSpec>(),
            );
            object_validation.required.insert("spec".to_owned());

            object_validation.properties.insert(
                "status".to_owned(),
                gen.subschema_for::<Option<RestateDeploymentStatus>>(),
            );
            Schema::Object(schema_object)
        }
    }
}

/// Restate specific configuration
#[derive(Deserialize, Serialize, Clone, Default, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RestateConfig {
    /// Reference to the Restate cluster
    pub cluster_ref: String,
}

/// Endpoint definition for Restate services
#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RestateEndpoint {
    /// Name of the endpoint
    pub name: String,

    /// HTTP method for this endpoint (GET, POST, etc.)
    pub method: Option<String>,

    /// URL path component for this endpoint
    pub path: Option<String>,
}

/// Status of the RestateDeployment
/// This is set and managed automatically by the controller
#[derive(Deserialize, Serialize, Clone, Default, Debug, JsonSchema)]
pub struct RestateDeploymentStatus {
    /// Total number of non-terminated pods targeted by this RestateDeployment
    pub replicas: Option<i32>,

    /// Total number of available pods (ready for at least minReadySeconds)
    pub available_replicas: Option<i32>,

    /// Total number of unavailable pods
    pub unavailable_replicas: Option<i32>,

    /// Total number of pods that have the desired template spec
    pub updated_replicas: Option<i32>,

    /// The generation observed by the controller
    pub observed_generation: Option<i64>,

    /// Information about active ReplicaSets and their versions
    pub versions: Option<Vec<RestateDeploymentVersion>>,

    /// Represents the latest available observations of current state
    pub conditions: Option<Vec<RestateDeploymentCondition>>,
}

/// Information about active versions and their resources
#[derive(Deserialize, Serialize, Clone, Default, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RestateDeploymentVersion {
    /// Name of the ReplicaSet/Service for this version
    pub name: String,

    /// Number of replicas
    pub replicas: i32,

    /// Number of available replicas
    pub available_replicas: Option<i32>,

    /// Creation timestamp
    pub created_at: String,

    /// Whether this is the current active version
    pub current: bool,

    /// Status of this version (Active, Draining, Drained, Inactive)
    pub status: String,
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
