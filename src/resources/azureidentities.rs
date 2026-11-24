//! Minimal representations of the Azure Service Operator (ASO) kinds the operator uses to give
//! each RestateCluster its own Azure managed identity. Only the fields the operator writes are
//! modelled, plus the status conditions it reads.

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Reference to an ASO destination ConfigMap key.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConfigMapReference {
    /// Name of the ConfigMap in the same namespace
    pub name: String,
    /// Key within the ConfigMap
    pub key: String,
}

/// Owner of an ASO resource named by its ARM id
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ArmIdOwner {
    pub arm_id: String,
}

/// Owner of an ASO resource named by the name of an ASO resource in the same namespace
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NamedOwner {
    pub name: String,
}

/// UserAssignedIdentitySpec is a minimal representation of the ASO
/// `managedidentity.azure.com/v1api20230131` UserAssignedIdentity.
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "managedidentity.azure.com",
    version = "v1api20230131",
    kind = "UserAssignedIdentity",
    plural = "userassignedidentities"
)]
#[kube(namespaced)]
#[kube(status = "AzureResourceStatus")]
#[serde(rename_all = "camelCase")]
pub struct UserAssignedIdentitySpec {
    /// The Azure region of the identity
    pub location: String,
    /// The resource group the identity is created in
    pub owner: ArmIdOwner,
    /// The name of the identity in Azure
    pub azure_name: String,
    /// Where ASO publishes attributes of the created identity
    pub operator_spec: UserAssignedIdentityOperatorSpec,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UserAssignedIdentityOperatorSpec {
    pub config_maps: UserAssignedIdentityConfigMaps,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UserAssignedIdentityConfigMaps {
    pub client_id: ConfigMapReference,
    pub principal_id: ConfigMapReference,
}

/// FederatedIdentityCredentialSpec is a minimal representation of the ASO
/// `managedidentity.azure.com/v1api20230131` FederatedIdentityCredential.
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "managedidentity.azure.com",
    version = "v1api20230131",
    kind = "FederatedIdentityCredential",
    plural = "federatedidentitycredentials"
)]
#[kube(namespaced)]
#[kube(status = "AzureResourceStatus")]
#[serde(rename_all = "camelCase")]
pub struct FederatedIdentityCredentialSpec {
    /// The UserAssignedIdentity in the same namespace this credential belongs to
    pub owner: NamedOwner,
    pub audiences: Vec<String>,
    pub issuer: String,
    pub subject: String,
}

/// RoleAssignmentSpec is a minimal representation of the ASO
/// `authorization.azure.com/v1api20220401` RoleAssignment.
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "authorization.azure.com",
    version = "v1api20220401",
    kind = "RoleAssignment",
    plural = "roleassignments"
)]
#[kube(namespaced)]
#[kube(status = "AzureResourceStatus")]
#[serde(rename_all = "camelCase")]
pub struct RoleAssignmentSpec {
    /// The scope the role is assigned at
    pub owner: ArmIdOwner,
    /// The principal to assign the role to, read from a ConfigMap key
    pub principal_id_from_config: ConfigMapReference,
    pub principal_type: String,
    pub role_definition_reference: RoleDefinitionReference,
    /// ABAC condition limiting the assignment
    pub condition: String,
    pub condition_version: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RoleDefinitionReference {
    pub arm_id: String,
}

/// Status of an ASO resource
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Hash)]
#[serde(rename_all = "camelCase")]
pub struct AzureResourceStatus {
    /// Conditions represent the latest available observations of the resource's state
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conditions: Option<Vec<AzureResourceCondition>>,
}

/// A condition on an ASO resource
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Hash)]
#[serde(rename_all = "camelCase")]
pub struct AzureResourceCondition {
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

    /// The generation of the object this condition was computed for
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,

    /// Last time the condition transitioned from one status to another
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_transition_time: Option<String>,
}
