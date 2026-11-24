//! Azure Workload Identity through Azure Service Operator (ASO).
//!
//! Every RestateCluster gets its own managed identity, a federated credential that trusts the
//! cluster's `restate` ServiceAccount, and a role assignment on the snapshot container that is
//! limited by an ABAC condition to the blob prefix of the cluster's own namespace.

use std::time::Duration;

use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::ConfigMap;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{DeleteParams, Patch, PatchParams};
use kube::{Api, Client, Resource};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tracing::debug;

use crate::Error;
use crate::resources::azureidentities::{
    ArmIdOwner, AzureResourceStatus, ConfigMapReference, FederatedIdentityCredential,
    FederatedIdentityCredentialSpec, NamedOwner, RoleAssignment, RoleAssignmentSpec,
    RoleDefinitionReference, UserAssignedIdentity, UserAssignedIdentityConfigMaps,
    UserAssignedIdentityOperatorSpec, UserAssignedIdentitySpec,
};

use super::object_meta;

pub const IDENTITY_NAME: &str = "restate";
pub const CREDENTIAL_NAME: &str = "restate";
pub const ROLE_ASSIGNMENT_NAME: &str = "restate-snapshots";
pub const IDENTITY_CONFIGMAP_NAME: &str = "restate-azure-identity";
pub const CLIENT_ID_KEY: &str = "clientId";
pub const PRINCIPAL_ID_KEY: &str = "principalId";
pub const CANARY_JOB_NAME: &str = "restate-azure-wi-canary";

/// The label that makes the AKS Workload Identity webhook inject the federated token.
pub const USE_LABEL: &str = "azure.workload.identity/use";
/// The ServiceAccount annotation that names the managed identity the webhook acts as.
pub const CLIENT_ID_ANNOTATION: &str = "azure.workload.identity/client-id";

const TOKEN_EXCHANGE_AUDIENCE: &str = "api://AzureADTokenExchange";
const STORAGE_BLOB_DATA_CONTRIBUTOR: &str = "ba92f5b4-2d11-453d-a403-e96b0029c9fe";

/// How long the canary keeps trying. A new role assignment can take up to about ten minutes to
/// take effect in Azure, so the canary has to outlast that before it reports a failure.
pub const CANARY_RETRY_SECONDS: u32 = 660;
/// Bounds scheduling, the image pull and the retry window together.
pub const CANARY_ACTIVE_DEADLINE_SECONDS: i64 = 900;

/// The blob container that holds every environment's snapshots.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageContainer {
    pub id: String,
    pub subscription_id: String,
    pub account: String,
    pub container: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AzureWorkloadIdentityConfig {
    /// `/subscriptions/<sub>/resourceGroups/<rg>`, a group that holds only environment identities
    pub identity_resource_group_id: String,
    pub location: String,
    pub oidc_issuer: String,
    pub storage_container: StorageContainer,
}

impl AzureWorkloadIdentityConfig {
    /// Builds the configuration from the four settings. Returns `None` when none is set, and
    /// refuses a partial or malformed configuration.
    pub fn from_parts(
        identity_resource_group_id: Option<String>,
        location: Option<String>,
        oidc_issuer: Option<String>,
        storage_container_id: Option<String>,
    ) -> anyhow::Result<Option<Self>> {
        match (
            identity_resource_group_id,
            location,
            oidc_issuer,
            storage_container_id,
        ) {
            (None, None, None, None) => Ok(None),
            (
                Some(identity_resource_group_id),
                Some(location),
                Some(oidc_issuer),
                Some(container_id),
            ) => Self::new(
                identity_resource_group_id,
                location,
                oidc_issuer,
                container_id,
            )
            .map(Some),
            _ => anyhow::bail!(
                "Azure Workload Identity needs all of AZURE_WI_IDENTITY_RESOURCE_GROUP_ID, AZURE_WI_LOCATION, AZURE_WI_OIDC_ISSUER and AZURE_WI_STORAGE_CONTAINER_ID, or none of them"
            ),
        }
    }

    fn new(
        identity_resource_group_id: String,
        location: String,
        oidc_issuer: String,
        storage_container_id: String,
    ) -> anyhow::Result<Self> {
        let rg_segments: Vec<&str> = identity_resource_group_id.split('/').collect();
        anyhow::ensure!(
            matches!(rg_segments.as_slice(),
                ["", sub, id, rg, name]
                    if sub.eq_ignore_ascii_case("subscriptions")
                        && !id.is_empty()
                        && rg.eq_ignore_ascii_case("resourceGroups")
                        && !name.is_empty()),
            "AZURE_WI_IDENTITY_RESOURCE_GROUP_ID must look like /subscriptions/<id>/resourceGroups/<name>, got '{identity_resource_group_id}'"
        );
        anyhow::ensure!(
            !location.is_empty() && location.chars().all(|c| c.is_ascii_alphanumeric()),
            "AZURE_WI_LOCATION must be an Azure region name such as swedencentral, got '{location}'"
        );
        anyhow::ensure!(
            oidc_issuer.starts_with("https://") && oidc_issuer.len() > "https://".len(),
            "AZURE_WI_OIDC_ISSUER must be an https URL, got '{oidc_issuer}'"
        );
        let storage_container = StorageContainer::parse(&storage_container_id)?;

        Ok(Self {
            identity_resource_group_id,
            location,
            oidc_issuer,
            storage_container,
        })
    }
}

impl StorageContainer {
    /// Parses `/subscriptions/<sub>/resourceGroups/<rg>/providers/Microsoft.Storage/storageAccounts/<account>/blobServices/default/containers/<container>`.
    pub fn parse(id: &str) -> anyhow::Result<Self> {
        let malformed = || {
            anyhow::anyhow!(
                "AZURE_WI_STORAGE_CONTAINER_ID must look like /subscriptions/<id>/resourceGroups/<name>/providers/Microsoft.Storage/storageAccounts/<account>/blobServices/default/containers/<container>, got '{id}'"
            )
        };
        let segments: Vec<&str> = id.split('/').collect();
        let [
            "",
            subscriptions,
            subscription_id,
            resource_groups,
            resource_group,
            providers,
            provider,
            storage_accounts,
            account,
            blob_services,
            default,
            containers,
            container,
        ] = segments.as_slice()
        else {
            return Err(malformed());
        };
        let keywords_match = subscriptions.eq_ignore_ascii_case("subscriptions")
            && resource_groups.eq_ignore_ascii_case("resourceGroups")
            && providers.eq_ignore_ascii_case("providers")
            && provider.eq_ignore_ascii_case("Microsoft.Storage")
            && storage_accounts.eq_ignore_ascii_case("storageAccounts")
            && blob_services.eq_ignore_ascii_case("blobServices")
            && default.eq_ignore_ascii_case("default")
            && containers.eq_ignore_ascii_case("containers");
        // The names end up in a shell command in the canary, so keep them to the characters
        // Azure allows in them.
        let is_name = |s: &str, extra: &[char]| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || extra.contains(&c))
        };
        if !keywords_match
            || !is_name(subscription_id, &['-'])
            || resource_group.is_empty()
            || !is_name(account, &[])
            || !is_name(container, &['-'])
        {
            return Err(malformed());
        }

        Ok(Self {
            id: id.to_owned(),
            subscription_id: (*subscription_id).to_owned(),
            account: (*account).to_owned(),
            container: (*container).to_owned(),
        })
    }
}

pub fn user_assigned_identity(
    ns: &str,
    base_metadata: &ObjectMeta,
    config: &AzureWorkloadIdentityConfig,
) -> UserAssignedIdentity {
    UserAssignedIdentity {
        metadata: object_meta(base_metadata, IDENTITY_NAME),
        spec: UserAssignedIdentitySpec {
            location: config.location.clone(),
            owner: ArmIdOwner {
                arm_id: config.identity_resource_group_id.clone(),
            },
            azure_name: format!("{ns}-restate"),
            operator_spec: UserAssignedIdentityOperatorSpec {
                config_maps: UserAssignedIdentityConfigMaps {
                    client_id: ConfigMapReference {
                        name: IDENTITY_CONFIGMAP_NAME.into(),
                        key: CLIENT_ID_KEY.into(),
                    },
                    principal_id: ConfigMapReference {
                        name: IDENTITY_CONFIGMAP_NAME.into(),
                        key: PRINCIPAL_ID_KEY.into(),
                    },
                },
            },
        },
        status: None,
    }
}

pub fn federated_identity_credential(
    ns: &str,
    base_metadata: &ObjectMeta,
    config: &AzureWorkloadIdentityConfig,
) -> FederatedIdentityCredential {
    FederatedIdentityCredential {
        metadata: object_meta(base_metadata, CREDENTIAL_NAME),
        spec: FederatedIdentityCredentialSpec {
            // By name, never by ARM id: the name resolves to the identity in this namespace, so
            // the credential cannot be pointed at another environment's identity.
            owner: NamedOwner {
                name: IDENTITY_NAME.into(),
            },
            audiences: vec![TOKEN_EXCHANGE_AUDIENCE.into()],
            issuer: config.oidc_issuer.clone(),
            subject: format!("system:serviceaccount:{ns}:restate"),
        },
        status: None,
    }
}

pub fn role_assignment(
    ns: &str,
    base_metadata: &ObjectMeta,
    config: &AzureWorkloadIdentityConfig,
) -> RoleAssignment {
    let container = &config.storage_container;
    RoleAssignment {
        metadata: object_meta(base_metadata, ROLE_ASSIGNMENT_NAME),
        spec: RoleAssignmentSpec {
            owner: ArmIdOwner {
                arm_id: container.id.clone(),
            },
            // Never a literal principal id: the id comes from this namespace's identity, which
            // is what keeps an environment from granting itself another environment's identity.
            principal_id_from_config: ConfigMapReference {
                name: IDENTITY_CONFIGMAP_NAME.into(),
                key: PRINCIPAL_ID_KEY.into(),
            },
            principal_type: "ServicePrincipal".into(),
            role_definition_reference: RoleDefinitionReference {
                arm_id: format!(
                    "/subscriptions/{}/providers/Microsoft.Authorization/roleDefinitions/{STORAGE_BLOB_DATA_CONTRIBUTOR}",
                    container.subscription_id
                ),
            },
            condition: role_assignment_condition(ns),
            condition_version: "2.0".into(),
        },
        status: None,
    }
}

/// Limits Storage Blob Data Contributor to the blobs under `<ns>/`. Reads, writes and deletes
/// are tested on the blob path. Listing has no blob path, so it is tested on the list prefix;
/// leaving that clause out would let the identity list every environment's blob names.
pub fn role_assignment_condition(ns: &str) -> String {
    format!(
        "((!(ActionMatches{{'Microsoft.Storage/storageAccounts/blobServices/containers/blobs/read'}} AND NOT SubOperationMatches{{'Blob.List'}}) AND !(ActionMatches{{'Microsoft.Storage/storageAccounts/blobServices/containers/blobs/write'}}) AND !(ActionMatches{{'Microsoft.Storage/storageAccounts/blobServices/containers/blobs/add/action'}}) AND !(ActionMatches{{'Microsoft.Storage/storageAccounts/blobServices/containers/blobs/delete'}})) OR (@Resource[Microsoft.Storage/storageAccounts/blobServices/containers/blobs:path] StringStartsWith '{ns}/')) AND ((!(ActionMatches{{'Microsoft.Storage/storageAccounts/blobServices/containers/blobs/read'}} AND SubOperationMatches{{'Blob.List'}})) OR (@Request[Microsoft.Storage/storageAccounts/blobServices/containers/blobs:prefix] StringStartsWith '{ns}/'))"
    )
}

/// The canary proves two things: that the federated token can be exchanged for a storage token,
/// and that the role assignment is in effect, by listing the environment's own prefix. It
/// retries, because a new role assignment can take several minutes to take effect.
pub fn canary_command(ns: &str, container: &StorageContainer) -> Vec<String> {
    const SCRIPT: &str = r#"
end=$(( $(date +%s) + @RETRY_SECONDS@ ))
while :; do
  assertion=$(cat "$AZURE_FEDERATED_TOKEN_FILE")
  token=$(wget -q -O - --post-data "client_id=$AZURE_CLIENT_ID&scope=https%3A%2F%2Fstorage.azure.com%2F.default&grant_type=client_credentials&client_assertion_type=urn%3Aietf%3Aparams%3Aoauth%3Aclient-assertion-type%3Ajwt-bearer&client_assertion=$assertion" "$AZURE_AUTHORITY_HOST$AZURE_TENANT_ID/oauth2/v2.0/token" | sed -n 's/.*"access_token":"\([^"]*\)".*/\1/p')
  if [ -z "$token" ]; then
    echo "token exchange failed" >&2
  elif wget -q -O /dev/null --header "Authorization: Bearer $token" --header "x-ms-version: 2023-11-03" "https://@ACCOUNT@.blob.core.windows.net/@CONTAINER@?restype=container&comp=list&prefix=@NAMESPACE@/&maxresults=1"; then
    echo "listing @CONTAINER@/@NAMESPACE@/ succeeded"
    exit 0
  else
    echo "listing @CONTAINER@/@NAMESPACE@/ failed; the role assignment may not be in effect yet" >&2
  fi
  [ "$(date +%s)" -ge "$end" ] && exit 1
  sleep 10
done
"#;
    let script = SCRIPT
        .replace("@RETRY_SECONDS@", &CANARY_RETRY_SECONDS.to_string())
        .replace("@ACCOUNT@", &container.account)
        .replace("@CONTAINER@", &container.container)
        .replace("@NAMESPACE@", ns);
    vec!["sh".into(), "-c".into(), script]
}

/// Labels the pod template so the AKS Workload Identity webhook injects the federated token.
pub fn use_workload_identity(stateful_set: &mut StatefulSet) {
    let template = stateful_set
        .spec
        .as_mut()
        .map(|spec| &mut spec.template)
        .expect("restate_statefulset always sets a spec");
    template
        .metadata
        .get_or_insert_with(Default::default)
        .labels
        .get_or_insert_with(Default::default)
        .insert(USE_LABEL.into(), "true".into());
}

fn ready_status(status: Option<&AzureResourceStatus>) -> bool {
    status.is_some_and(AzureResourceStatus::is_ready)
}

/// ASO's own explanation of why an object is not ready, when it has given one.
fn not_ready_detail(status: Option<&AzureResourceStatus>) -> String {
    status
        .and_then(|s| s.conditions.as_ref())
        .and_then(|cs| cs.iter().find(|c| c.r#type == "Ready"))
        .and_then(|c| c.message.as_deref().filter(|m| !m.is_empty()))
        .map(|m| format!(": {m}"))
        .unwrap_or_default()
}

async fn apply<K>(namespace: &str, api: &Api<K>, object: K) -> Result<K, Error>
where
    K: Resource<DynamicType = ()> + Serialize + DeserializeOwned + Clone + std::fmt::Debug,
{
    let name = object.meta().name.clone().unwrap();
    debug!("Applying {} {name} in namespace {namespace}", K::kind(&()));
    let params = PatchParams::apply("restate-operator").force();
    match api.patch(&name, &params, &Patch::Apply(&object)).await {
        Ok(applied) => Ok(applied),
        Err(kube::Error::Api(kube::error::ErrorResponse { code: 404, .. })) => {
            Err(Error::NotReady {
                reason: "AzureIdentityCRDNotFound".into(),
                message: format!(
                    "{} CRD not found - is Azure Service Operator installed with the managedidentity and authorization resources?",
                    K::kind(&())
                ),
                requeue_after: Some(Duration::from_secs(60)),
            })
        }
        Err(err) => Err(err.into()),
    }
}

async fn delete<K>(namespace: &str, api: &Api<K>, name: &str) -> Result<(), Error>
where
    K: Resource<DynamicType = ()> + Serialize + DeserializeOwned + Clone + std::fmt::Debug,
{
    debug!(
        "Ensuring {} {name} in namespace {namespace} does not exist",
        K::kind(&())
    );
    match api.delete(name, &DeleteParams::default()).await {
        Err(kube::Error::Api(kube::error::ErrorResponse { code: 404, .. })) => Ok(()),
        Err(err) => Err(err.into()),
        Ok(_) => Ok(()),
    }
}

/// Applies the three ASO objects and waits for all of them to be Ready. Returns the client id
/// of the managed identity, which ASO exports into a ConfigMap.
pub async fn reconcile_identity(
    client: &Client,
    namespace: &str,
    base_metadata: &ObjectMeta,
    config: &AzureWorkloadIdentityConfig,
) -> Result<String, Error> {
    let identity_api: Api<UserAssignedIdentity> = Api::namespaced(client.clone(), namespace);
    let credential_api: Api<FederatedIdentityCredential> =
        Api::namespaced(client.clone(), namespace);
    let role_assignment_api: Api<RoleAssignment> = Api::namespaced(client.clone(), namespace);
    let cm_api: Api<ConfigMap> = Api::namespaced(client.clone(), namespace);

    let identity = apply(
        namespace,
        &identity_api,
        user_assigned_identity(namespace, base_metadata, config),
    )
    .await?;
    let credential = apply(
        namespace,
        &credential_api,
        federated_identity_credential(namespace, base_metadata, config),
    )
    .await?;
    let role_assignment = apply(
        namespace,
        &role_assignment_api,
        role_assignment(namespace, base_metadata, config),
    )
    .await?;

    let waiting: Vec<String> = [
        (
            "UserAssignedIdentity",
            IDENTITY_NAME,
            ready_status(identity.status.as_ref()),
            not_ready_detail(identity.status.as_ref()),
        ),
        (
            "FederatedIdentityCredential",
            CREDENTIAL_NAME,
            ready_status(credential.status.as_ref()),
            not_ready_detail(credential.status.as_ref()),
        ),
        (
            "RoleAssignment",
            ROLE_ASSIGNMENT_NAME,
            ready_status(role_assignment.status.as_ref()),
            not_ready_detail(role_assignment.status.as_ref()),
        ),
    ]
    .into_iter()
    .filter(|(_, _, ready, _)| !ready)
    .map(|(kind, name, _, detail)| format!("{kind} {name}{detail}"))
    .collect();

    if !waiting.is_empty() {
        return Err(Error::NotReady {
            reason: "AzureIdentityNotReady".into(),
            message: format!(
                "Waiting for Azure Service Operator to provision: {}",
                waiting.join("; ")
            ),
            requeue_after: Some(Duration::from_secs(10)),
        });
    }

    let client_id = cm_api
        .get_opt(IDENTITY_CONFIGMAP_NAME)
        .await?
        .and_then(|cm| cm.data)
        .and_then(|mut data| data.remove(CLIENT_ID_KEY))
        .filter(|id| !id.is_empty());

    client_id.ok_or_else(|| Error::NotReady {
        reason: "AzureIdentityNotReady".into(),
        message: format!(
            "Waiting for Azure Service Operator to publish the client id in ConfigMap {IDENTITY_CONFIGMAP_NAME}"
        ),
        requeue_after: Some(Duration::from_secs(10)),
    })
}

/// Removes the ASO objects, which makes ASO remove the Azure resources. The role assignment
/// goes first so the identity is never left holding a grant.
pub async fn delete_identity(client: &Client, namespace: &str) -> Result<(), Error> {
    delete(
        namespace,
        &Api::<RoleAssignment>::namespaced(client.clone(), namespace),
        ROLE_ASSIGNMENT_NAME,
    )
    .await?;
    delete(
        namespace,
        &Api::<FederatedIdentityCredential>::namespaced(client.clone(), namespace),
        CREDENTIAL_NAME,
    )
    .await?;
    delete(
        namespace,
        &Api::<UserAssignedIdentity>::namespaced(client.clone(), namespace),
        IDENTITY_NAME,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;

    const CONTAINER_ID: &str = "/subscriptions/df370aa7-c354-429b-8bed-9efd950b0e07/resourceGroups/bh-byok-azure-spike/providers/Microsoft.Storage/storageAccounts/bhbyokspike18158/blobServices/default/containers/snapshots";

    fn config() -> AzureWorkloadIdentityConfig {
        AzureWorkloadIdentityConfig::from_parts(
            Some("/subscriptions/df370aa7-c354-429b-8bed-9efd950b0e07/resourceGroups/bh-byok-azure-spike-envs".into()),
            Some("swedencentral".into()),
            Some("https://swedencentral.oic.prod-aks.azure.com/tenant/cluster/".into()),
            Some(CONTAINER_ID.into()),
        )
        .unwrap()
        .unwrap()
    }

    fn base_metadata() -> ObjectMeta {
        ObjectMeta {
            name: Some("env-c".into()),
            owner_references: Some(vec![OwnerReference {
                uid: "uid".into(),
                name: "env-c".into(),
                kind: "RestateCluster".into(),
                api_version: "restate.dev/v1".into(),
                ..Default::default()
            }]),
            ..Default::default()
        }
    }

    #[test]
    fn config_parses_the_container_id() {
        let config = config();
        assert_eq!(config.storage_container.account, "bhbyokspike18158");
        assert_eq!(config.storage_container.container, "snapshots");
        assert_eq!(
            config.storage_container.subscription_id,
            "df370aa7-c354-429b-8bed-9efd950b0e07"
        );
        assert_eq!(config.storage_container.id, CONTAINER_ID);
    }

    #[test]
    fn config_is_all_or_nothing_and_refuses_malformed_ids() {
        assert_eq!(
            AzureWorkloadIdentityConfig::from_parts(None, None, None, None).unwrap(),
            None
        );
        assert!(
            AzureWorkloadIdentityConfig::from_parts(None, Some("swedencentral".into()), None, None)
                .is_err_and(|e| e.to_string().contains("all of"))
        );
        for bad in [
            "",
            "/subscriptions/s/resourceGroups/rg",
            "/subscriptions/s/resourceGroups/rg/providers/Microsoft.Storage/storageAccounts/acct",
            "/subscriptions/s/resourceGroups/rg/providers/Microsoft.Storage/storageAccounts/acct/blobServices/default/containers/",
            "/subscriptions/s/resourceGroups/rg/providers/Microsoft.Compute/storageAccounts/acct/blobServices/default/containers/c",
            "/subscriptions/s/resourceGroups/rg/providers/Microsoft.Storage/storageAccounts/acct/blobServices/default/containers/c;rm",
        ] {
            assert!(StorageContainer::parse(bad).is_err(), "{bad}");
        }
        assert!(
            AzureWorkloadIdentityConfig::new(
                "/subscriptions/s/resourceGroups/rg/providers/x".into(),
                "swedencentral".into(),
                "https://issuer/".into(),
                CONTAINER_ID.into()
            )
            .is_err()
        );
    }

    #[test]
    fn identity_is_named_per_namespace_and_exports_its_ids() {
        let uai = user_assigned_identity("env-c", &base_metadata(), &config());
        assert_eq!(uai.metadata.name.as_deref(), Some("restate"));
        assert_eq!(uai.spec.azure_name, "env-c-restate");
        assert_eq!(uai.spec.location, "swedencentral");
        assert_eq!(
            uai.spec.owner.arm_id,
            "/subscriptions/df370aa7-c354-429b-8bed-9efd950b0e07/resourceGroups/bh-byok-azure-spike-envs"
        );
        let json = serde_json::to_value(&uai).unwrap();
        assert_eq!(
            json["apiVersion"],
            "managedidentity.azure.com/v1api20230131"
        );
        assert_eq!(
            json["spec"]["operatorSpec"]["configMaps"]["clientId"],
            serde_json::json!({"name": "restate-azure-identity", "key": "clientId"})
        );
        assert_eq!(
            json["spec"]["operatorSpec"]["configMaps"]["principalId"],
            serde_json::json!({"name": "restate-azure-identity", "key": "principalId"})
        );
        assert_eq!(uai.metadata.owner_references.unwrap()[0].uid, "uid");
    }

    #[test]
    fn credential_names_its_owner_and_trusts_only_the_namespace_service_account() {
        let fic = federated_identity_credential("env-c", &base_metadata(), &config());
        let json = serde_json::to_value(&fic).unwrap();
        assert_eq!(
            json["apiVersion"],
            "managedidentity.azure.com/v1api20230131"
        );
        assert_eq!(
            json["spec"]["owner"],
            serde_json::json!({"name": "restate"})
        );
        assert!(json["spec"]["owner"].get("armId").is_none());
        assert_eq!(
            json["spec"]["audiences"],
            serde_json::json!(["api://AzureADTokenExchange"])
        );
        assert_eq!(
            json["spec"]["issuer"],
            "https://swedencentral.oic.prod-aks.azure.com/tenant/cluster/"
        );
        assert_eq!(
            json["spec"]["subject"],
            "system:serviceaccount:env-c:restate"
        );
    }

    #[test]
    fn role_assignment_takes_its_principal_from_config_and_carries_the_prefix_condition() {
        let ra = role_assignment("env-c", &base_metadata(), &config());
        let json = serde_json::to_value(&ra).unwrap();
        assert_eq!(ra.metadata.name.as_deref(), Some("restate-snapshots"));
        assert_eq!(json["apiVersion"], "authorization.azure.com/v1api20220401");
        assert_eq!(json["spec"]["owner"]["armId"], CONTAINER_ID);
        assert_eq!(
            json["spec"]["principalIdFromConfig"],
            serde_json::json!({"name": "restate-azure-identity", "key": "principalId"})
        );
        assert!(json["spec"].get("principalId").is_none());
        assert_eq!(json["spec"]["principalType"], "ServicePrincipal");
        assert_eq!(
            json["spec"]["roleDefinitionReference"]["armId"],
            "/subscriptions/df370aa7-c354-429b-8bed-9efd950b0e07/providers/Microsoft.Authorization/roleDefinitions/ba92f5b4-2d11-453d-a403-e96b0029c9fe"
        );
        assert_eq!(json["spec"]["conditionVersion"], "2.0");
        assert_eq!(
            json["spec"]["condition"],
            "((!(ActionMatches{'Microsoft.Storage/storageAccounts/blobServices/containers/blobs/read'} AND NOT SubOperationMatches{'Blob.List'}) AND !(ActionMatches{'Microsoft.Storage/storageAccounts/blobServices/containers/blobs/write'}) AND !(ActionMatches{'Microsoft.Storage/storageAccounts/blobServices/containers/blobs/add/action'}) AND !(ActionMatches{'Microsoft.Storage/storageAccounts/blobServices/containers/blobs/delete'})) OR (@Resource[Microsoft.Storage/storageAccounts/blobServices/containers/blobs:path] StringStartsWith 'env-c/')) AND ((!(ActionMatches{'Microsoft.Storage/storageAccounts/blobServices/containers/blobs/read'} AND SubOperationMatches{'Blob.List'})) OR (@Request[Microsoft.Storage/storageAccounts/blobServices/containers/blobs:prefix] StringStartsWith 'env-c/'))"
        );
    }

    #[test]
    fn canary_lists_the_namespace_prefix_and_outlasts_role_propagation() {
        let command = canary_command("env-c", &config().storage_container);
        assert_eq!(&command[..2], ["sh", "-c"]);
        let script = &command[2];
        assert!(script.contains("https://bhbyokspike18158.blob.core.windows.net/snapshots?restype=container&comp=list&prefix=env-c/&maxresults=1"));
        assert!(script.contains("+ 660 ))"));
        assert!(script.contains("$AZURE_AUTHORITY_HOST$AZURE_TENANT_ID/oauth2/v2.0/token"));
        assert!(!script.contains('@'), "every placeholder is replaced");
        assert!(CANARY_ACTIVE_DEADLINE_SECONDS > i64::from(CANARY_RETRY_SECONDS) + 120);
    }

    #[test]
    fn readiness_follows_the_ready_condition() {
        use crate::resources::azureidentities::AzureResourceCondition;
        let status = |s: &str| AzureResourceStatus {
            conditions: Some(vec![AzureResourceCondition {
                r#type: "Ready".into(),
                status: s.into(),
                message: Some("waiting".into()),
                reason: None,
                last_transition_time: None,
            }]),
        };
        assert!(ready_status(Some(&status("True"))));
        assert!(!ready_status(Some(&status("False"))));
        assert!(!ready_status(Some(&AzureResourceStatus {
            conditions: None
        })));
        assert!(!ready_status(None));
        assert_eq!(not_ready_detail(Some(&status("False"))), ": waiting");
    }
}
