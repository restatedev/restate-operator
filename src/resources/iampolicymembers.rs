use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// IAMPolicyMemberSpec defines the desired state of an IAM policy binding.
/// This is a minimal representation of the Config Connector IAMPolicyMember CRD,
/// containing only the fields needed by the operator.
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "iam.cnrm.cloud.google.com",
    version = "v1beta1",
    kind = "IAMPolicyMember",
    plural = "iampolicymembers"
)]
#[kube(namespaced)]
#[kube(status = "IAMPolicyMemberStatus")]
#[serde(rename_all = "camelCase")]
pub struct IAMPolicyMemberSpec {
    /// The identity to grant the role to, e.g.
    /// `serviceAccount:PROJECT.svc.id.goog[NAMESPACE/KSA_NAME]`
    pub member: String,

    /// The IAM role to grant, e.g. `roles/iam.workloadIdentityUser`
    pub role: String,

    /// Reference to the GCP resource to apply the IAM binding to
    pub resource_ref: IAMPolicyMemberResourceRef,
}

/// Reference to a GCP resource managed by Config Connector
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct IAMPolicyMemberResourceRef {
    /// The Config Connector kind, e.g. `IAMServiceAccount`
    pub kind: String,

    /// External reference to a GCP resource not managed by Config Connector,
    /// e.g. `sa@project.iam.gserviceaccount.com`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external: Option<String>,

    /// Name of a Config Connector resource in the same namespace
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Namespace of the referenced Config Connector resource
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

/// Status of the IAMPolicyMember resource as reported by Config Connector
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Hash)]
#[serde(rename_all = "camelCase")]
pub struct IAMPolicyMemberStatus {
    /// Conditions represent the latest available observations of the resource's state
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conditions: Option<Vec<IAMPolicyMemberCondition>>,
}

/// A condition on an IAMPolicyMember resource
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Hash)]
#[serde(rename_all = "camelCase")]
pub struct IAMPolicyMemberCondition {
    /// Type of condition, e.g. `Ready`
    #[serde(rename = "type")]
    pub r#type: String,

    /// Status of the condition: `True`, `False`, or `Unknown`
    pub status: String,

    /// Human-readable message indicating details about the condition
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,

    /// Machine-readable reason for the condition's last transition
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,

    /// Last time the condition transitioned from one status to another
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_transition_time: Option<String>,
}
