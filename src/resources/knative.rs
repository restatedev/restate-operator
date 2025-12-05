use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, ObjectMeta};
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Knative Configuration manages the lifecycle of Revisions
/// API: serving.knative.dev/v1
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "serving.knative.dev",
    version = "v1",
    kind = "Configuration",
    namespaced
)]
#[kube(status = "ConfigurationStatus")]
pub struct ConfigurationSpec {
    /// Template for the Revision
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template: Option<RevisionTemplateSpec>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
pub struct ConfigurationStatus {
    /// Conditions communicate information about ongoing/complete reconciliation
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conditions: Option<Vec<Condition>>,

    /// Latest created revision name
    #[serde(skip_serializing_if = "Option::is_none", rename = "latestCreatedRevisionName")]
    pub latest_created_revision_name: Option<String>,

    /// Latest ready revision name
    #[serde(skip_serializing_if = "Option::is_none", rename = "latestReadyRevisionName")]
    pub latest_ready_revision_name: Option<String>,

    /// Observed generation
    #[serde(skip_serializing_if = "Option::is_none", rename = "observedGeneration")]
    pub observed_generation: Option<i64>,
}

/// Knative Route provides traffic routing to Revisions
/// API: serving.knative.dev/v1
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "serving.knative.dev",
    version = "v1",
    kind = "Route",
    namespaced
)]
#[kube(status = "RouteStatus")]
pub struct RouteSpec {
    /// Traffic routing configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub traffic: Option<Vec<TrafficTarget>>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
pub struct RouteStatus {
    /// Conditions communicate information about ongoing/complete reconciliation
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conditions: Option<Vec<Condition>>,

    /// Observed generation
    #[serde(skip_serializing_if = "Option::is_none", rename = "observedGeneration")]
    pub observed_generation: Option<i64>,

    /// Traffic routing status
    #[serde(skip_serializing_if = "Option::is_none")]
    pub traffic: Option<Vec<TrafficTarget>>,

    /// URL of the route
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
pub struct RevisionTemplateSpec {
    /// Metadata for the Revision
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<ObjectMeta>,

    /// Spec for the Revision
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spec: Option<RevisionSpec>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
pub struct TrafficTarget {
    /// Configuration name (for routing to latest revision of a Configuration)
    #[serde(skip_serializing_if = "Option::is_none", rename = "configurationName")]
    pub configuration_name: Option<String>,

    /// Percent of traffic to this target
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percent: Option<i64>,

    /// Revision name (for routing to specific revision)
    #[serde(skip_serializing_if = "Option::is_none", rename = "revisionName")]
    pub revision_name: Option<String>,

    /// Tag for named route
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,

    /// Latest revision flag (when true, routes to latest ready revision)
    #[serde(skip_serializing_if = "Option::is_none", rename = "latestRevision")]
    pub latest_revision: Option<bool>,

    /// URL for this traffic target
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// Knative Revision represents an immutable snapshot of code and configuration
/// API: serving.knative.dev/v1
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "serving.knative.dev",
    version = "v1",
    kind = "Revision",
    namespaced
)]
#[kube(status = "RevisionStatus")]
pub struct RevisionSpec {
    /// Containers in the Revision
    #[serde(skip_serializing_if = "Option::is_none")]
    pub containers: Option<Vec<serde_json::Value>>,

    /// Service account name
    #[serde(skip_serializing_if = "Option::is_none", rename = "serviceAccountName")]
    pub service_account_name: Option<String>,

    /// Timeout seconds
    #[serde(skip_serializing_if = "Option::is_none", rename = "timeoutSeconds")]
    pub timeout_seconds: Option<i64>,

    /// Other fields passed through as-is
    #[serde(flatten)]
    pub other: BTreeMap<String, serde_json::Value>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, PartialEq)]
pub struct RevisionStatus {
    /// Conditions communicate information about ongoing/complete reconciliation
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conditions: Option<Vec<Condition>>,

    /// Observed generation
    #[serde(skip_serializing_if = "Option::is_none", rename = "observedGeneration")]
    pub observed_generation: Option<i64>,

    /// Service name
    #[serde(skip_serializing_if = "Option::is_none", rename = "serviceName")]
    pub service_name: Option<String>,

    /// Log URL
    #[serde(skip_serializing_if = "Option::is_none", rename = "logUrl")]
    pub log_url: Option<String>,

    /// Image digest
    #[serde(skip_serializing_if = "Option::is_none", rename = "imageDigest")]
    pub image_digest: Option<String>,

    /// Container statuses
    #[serde(skip_serializing_if = "Option::is_none", rename = "containerStatuses")]
    pub container_statuses: Option<Vec<serde_json::Value>>,

    /// Number of ready pods running this revision
    #[serde(skip_serializing_if = "Option::is_none", rename = "actualReplicas")]
    pub actual_replicas: Option<i32>,

    /// Desired number of pods for this revision
    #[serde(skip_serializing_if = "Option::is_none", rename = "desiredReplicas")]
    pub desired_replicas: Option<i32>,
}
