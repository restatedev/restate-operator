//! Azure Workload Identity through Azure Service Operator (ASO).
//!
//! Every RestateCluster gets its own managed identity, a federated credential that trusts the
//! cluster's `restate` ServiceAccount, and a role assignment on the snapshot container that is
//! limited by an ABAC condition to the blob prefix of the cluster's own namespace.

use std::time::Duration;

use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{ConfigMap, ServiceAccount};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{DeleteParams, Patch, PatchParams};
use kube::{Api, Client, Resource};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tracing::{debug, warn};

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

/// The pod template annotation that records which identity the pods were rolled for.
pub const POD_CLIENT_ID_ANNOTATION: &str = "restate.dev/azure-workload-identity-client-id";

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
    /// `/subscriptions/<sub>/providers/Microsoft.Authorization/roleDefinitions/<guid>`, a custom
    /// role that carries only the blob data actions. The built-in Storage Blob Data Contributor
    /// also carries container management actions, which an ABAC condition cannot limit.
    pub role_definition_id: String,
}

impl AzureWorkloadIdentityConfig {
    /// Builds the configuration from the five settings. Returns `None` when none is set, and
    /// refuses a partial or malformed configuration.
    pub fn from_parts(
        identity_resource_group_id: Option<String>,
        location: Option<String>,
        oidc_issuer: Option<String>,
        storage_container_id: Option<String>,
        role_definition_id: Option<String>,
    ) -> anyhow::Result<Option<Self>> {
        match (
            identity_resource_group_id,
            location,
            oidc_issuer,
            storage_container_id,
            role_definition_id,
        ) {
            (None, None, None, None, None) => Ok(None),
            (
                Some(identity_resource_group_id),
                Some(location),
                Some(oidc_issuer),
                Some(container_id),
                Some(role_definition_id),
            ) => Self::new(
                identity_resource_group_id,
                location,
                oidc_issuer,
                container_id,
                role_definition_id,
            )
            .map(Some),
            _ => anyhow::bail!(
                "Azure Workload Identity needs all of AZURE_WI_IDENTITY_RESOURCE_GROUP_ID, AZURE_WI_LOCATION, AZURE_WI_OIDC_ISSUER, AZURE_WI_STORAGE_CONTAINER_ID and AZURE_WI_ROLE_DEFINITION_ID, or none of them"
            ),
        }
    }

    fn new(
        identity_resource_group_id: String,
        location: String,
        oidc_issuer: String,
        storage_container_id: String,
        role_definition_id: String,
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
        anyhow::ensure!(
            matches!(role_definition_id.split('/').collect::<Vec<_>>().as_slice(),
                ["", sub, id, providers, provider, role_definitions, guid]
                    if sub.eq_ignore_ascii_case("subscriptions")
                        && !id.is_empty()
                        && providers.eq_ignore_ascii_case("providers")
                        && provider.eq_ignore_ascii_case("Microsoft.Authorization")
                        && role_definitions.eq_ignore_ascii_case("roleDefinitions")
                        && is_guid(guid)),
            "AZURE_WI_ROLE_DEFINITION_ID must look like /subscriptions/<id>/providers/Microsoft.Authorization/roleDefinitions/<guid>, got '{role_definition_id}'"
        );

        Ok(Self {
            identity_resource_group_id,
            location,
            oidc_issuer,
            storage_container,
            role_definition_id,
        })
    }
}

fn is_guid(s: &str) -> bool {
    s.len() == 36
        && s.chars().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
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
                arm_id: config.role_definition_id.clone(),
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

/// The canary proves the whole path an environment depends on: the federated token exchange, and
/// the role assignment, by listing the environment's own prefix and by writing and deleting a
/// probe blob under it. The condition has separate clauses for listing, writing and deleting, so
/// each one needs proving. It retries, because a new role assignment can take several minutes to
/// take effect, and it fails closed: every step must return the status it expects.
///
/// busybox wget cannot send PUT or DELETE, so the script needs curl in the canary image. It
/// installs nothing at run time, because a cluster may have no route to a package mirror. Every value interpolated into the script is validated at startup or
/// is a Kubernetes name, a GUID or a storage name, so none of them can carry shell syntax.
pub fn canary_command(
    ns: &str,
    container: &StorageContainer,
    client_id: &str,
    role_definition_id: &str,
) -> Vec<String> {
    const SCRIPT: &str = r##"
# identity @CLIENT_ID@, role definition @ROLE@
if ! command -v curl >/dev/null 2>&1; then
  echo "the canary image has no curl; set canaryImage to an image that provides it" >&2
  exit 1
fi
if [ "$AZURE_CLIENT_ID" != "@CLIENT_ID@" ]; then
  echo "the pod's AZURE_CLIENT_ID is '$AZURE_CLIENT_ID', expected @CLIENT_ID@; the ServiceAccount annotation or the Workload Identity webhook is not applied" >&2
  exit 1
fi
base="https://@ACCOUNT@.blob.core.windows.net/@CONTAINER@"
blob="$base/@NAMESPACE@/.restate-canary-$$"
body=/tmp/canary-body
call() {
  what=$1; want=$2; shift 2
  got=$(curl -sS --max-time 15 -o "$body" -w '%{http_code}' "$@") || { echo "$what: request failed" >&2; return 1; }
  if [ "$got" != "$want" ]; then
    echo "$what: HTTP $got, expected $want" >&2
    head -c 1000 "$body" >&2
    echo >&2
    return 1
  fi
}
attempt() {
  assertion=$(cat "$AZURE_FEDERATED_TOKEN_FILE") || return 1
  call "token exchange" 200 -d "client_id=$AZURE_CLIENT_ID" -d "scope=https%3A%2F%2Fstorage.azure.com%2F.default" -d "grant_type=client_credentials" -d "client_assertion_type=urn%3Aietf%3Aparams%3Aoauth%3Aclient-assertion-type%3Ajwt-bearer" --data-urlencode "client_assertion=$assertion" "$AZURE_AUTHORITY_HOST$AZURE_TENANT_ID/oauth2/v2.0/token" || return 1
  token=$(sed -n 's/.*"access_token":"\([^"]*\)".*/\1/p' "$body")
  [ -n "$token" ] || { echo "token exchange: the response has no access_token" >&2; return 1; }
  call "list @NAMESPACE@/" 200 -H "Authorization: Bearer $token" -H "x-ms-version: 2023-11-03" "$base?restype=container&comp=list&prefix=@NAMESPACE@/&maxresults=1" || return 1
  call "write probe blob" 201 -X PUT -H "Authorization: Bearer $token" -H "x-ms-version: 2023-11-03" -H "x-ms-blob-type: BlockBlob" -H "Content-Type: text/plain" --data-binary canary "$blob" || return 1
  call "delete probe blob" 202 -X DELETE -H "Authorization: Bearer $token" -H "x-ms-version: 2023-11-03" "$blob" || return 1
}
end=$(( $(date +%s) + @RETRY_SECONDS@ ))
while :; do
  if attempt; then
    echo "token exchange, listing, write and delete under @NAMESPACE@/ succeeded"
    exit 0
  fi
  echo "the role assignment may not be in effect yet" >&2
  [ "$(date +%s)" -ge "$end" ] && exit 1
  sleep 10
done
"##;
    let script = SCRIPT
        .replace("@RETRY_SECONDS@", &CANARY_RETRY_SECONDS.to_string())
        .replace("@ACCOUNT@", &container.account)
        .replace("@CONTAINER@", &container.container)
        .replace("@NAMESPACE@", ns)
        .replace("@CLIENT_ID@", client_id)
        .replace("@ROLE@", role_definition_id);
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

/// The uid of the RestateCluster that owns everything created for it.
pub fn cluster_uid(base_metadata: &ObjectMeta) -> &str {
    base_metadata
        .owner_references
        .as_deref()
        .and_then(|refs| refs.first())
        .map(|owner| owner.uid.as_str())
        .expect("the base metadata always carries the RestateCluster owner reference")
}

fn is_owned_by(meta: &ObjectMeta, uid: &str) -> bool {
    meta.owner_references
        .iter()
        .flatten()
        .any(|owner| owner.uid == uid)
}

/// ASO sets `observedGeneration` on its Ready condition, so a Ready=True that is older than the
/// object's current spec does not count. An object that is being deleted is never ready.
fn is_ready(meta: &ObjectMeta, status: Option<&AzureResourceStatus>) -> bool {
    meta.deletion_timestamp.is_none()
        && status
            .and_then(|s| s.conditions.as_ref())
            .and_then(|cs| cs.iter().find(|c| c.r#type == "Ready"))
            .is_some_and(|c| c.status == "True" && c.observed_generation == meta.generation)
}

/// Why an object is not ready, with ASO's own explanation when it has given one.
fn not_ready_description(
    kind: &str,
    meta: &ObjectMeta,
    status: Option<&AzureResourceStatus>,
) -> String {
    let name = meta.name.as_deref().unwrap_or_default();
    if meta.deletion_timestamp.is_some() {
        return format!("{kind} {name}: terminating");
    }
    let condition = status
        .and_then(|s| s.conditions.as_ref())
        .and_then(|cs| cs.iter().find(|c| c.r#type == "Ready"));
    match condition {
        Some(c) if c.status == "True" => {
            format!("{kind} {name}: ASO has not yet reconciled the latest spec")
        }
        Some(c) => match c.message.as_deref().filter(|m| !m.is_empty()) {
            Some(message) => format!("{kind} {name}: {message}"),
            None => format!("{kind} {name}"),
        },
        None => format!("{kind} {name}"),
    }
}

/// Server-side applies the object, unless an object with that name exists that this
/// RestateCluster did not create. A forced apply would take it over.
async fn apply<K>(namespace: &str, api: &Api<K>, object: K, uid: &str) -> Result<K, Error>
where
    K: Resource<DynamicType = ()> + Serialize + DeserializeOwned + Clone + std::fmt::Debug,
{
    let name = object.meta().name.clone().unwrap();
    let kind = K::kind(&());

    // Metadata only: an object somebody else made may not have the fields of our spec
    if let Some(existing) = api.get_metadata_opt(&name).await?
        && !is_owned_by(&existing.metadata, uid)
    {
        return Err(Error::NotReady {
            reason: "AzureIdentityConflict".into(),
            message: format!(
                "{kind} {name} already exists in namespace {namespace} and is not owned by this RestateCluster; the operator will not take it over"
            ),
            requeue_after: Some(Duration::from_secs(60)),
        });
    }

    debug!("Applying {kind} {name} in namespace {namespace}");
    let params = PatchParams::apply("restate-operator").force();
    match api.patch(&name, &params, &Patch::Apply(&object)).await {
        Ok(applied) => Ok(applied),
        Err(kube::Error::Api(kube::error::ErrorResponse { code: 404, .. })) => {
            Err(Error::NotReady {
                reason: "AzureIdentityCRDNotFound".into(),
                message: format!(
                    "{kind} CRD not found - is Azure Service Operator installed with the managedidentity and authorization resources?"
                ),
                requeue_after: Some(Duration::from_secs(60)),
            })
        }
        Err(err) => Err(err.into()),
    }
}

/// Deletes the object if it exists, is not already terminating, and carries an owner reference
/// to this RestateCluster. Anything else with the same name is somebody else's and stays.
pub async fn delete_owned<K>(
    namespace: &str,
    api: &Api<K>,
    name: &str,
    uid: &str,
    params: &DeleteParams,
) -> Result<(), Error>
where
    K: Resource<DynamicType = ()> + DeserializeOwned + Clone + std::fmt::Debug,
{
    let kind = K::kind(&());
    let Some(existing) = api.get_metadata_opt(name).await? else {
        return Ok(());
    };
    if existing.metadata.deletion_timestamp.is_some() {
        return Ok(());
    }
    if !is_owned_by(&existing.metadata, uid) {
        debug!(
            "Leaving {kind} {name} in namespace {namespace} alone: it is not owned by this RestateCluster"
        );
        return Ok(());
    }
    debug!("Deleting {kind} {name} in namespace {namespace}");
    match api.delete(name, params).await {
        Err(kube::Error::Api(kube::error::ErrorResponse { code: 404, .. })) => Ok(()),
        Err(err) => Err(err.into()),
        Ok(_) => Ok(()),
    }
}

/// What the identity objects have reached.
pub enum IdentityState {
    Ready {
        client_id: String,
    },
    /// ASO has not finished, or has not published a usable client id yet.
    Pending {
        message: String,
    },
}

/// Applies the three ASO objects and reports whether they are all Ready and the client id of
/// the managed identity, which ASO exports into a ConfigMap, is available. A missing CRD or an
/// object this RestateCluster does not own is an error.
pub async fn reconcile_identity(
    client: &Client,
    namespace: &str,
    base_metadata: &ObjectMeta,
    config: &AzureWorkloadIdentityConfig,
) -> Result<IdentityState, Error> {
    let uid = cluster_uid(base_metadata);
    let identity_api: Api<UserAssignedIdentity> = Api::namespaced(client.clone(), namespace);
    let credential_api: Api<FederatedIdentityCredential> =
        Api::namespaced(client.clone(), namespace);
    let role_assignment_api: Api<RoleAssignment> = Api::namespaced(client.clone(), namespace);
    let cm_api: Api<ConfigMap> = Api::namespaced(client.clone(), namespace);

    let identity = apply(
        namespace,
        &identity_api,
        user_assigned_identity(namespace, base_metadata, config),
        uid,
    )
    .await?;
    let credential = apply(
        namespace,
        &credential_api,
        federated_identity_credential(namespace, base_metadata, config),
        uid,
    )
    .await?;
    let role_assignment = apply(
        namespace,
        &role_assignment_api,
        role_assignment(namespace, base_metadata, config),
        uid,
    )
    .await?;

    let waiting: Vec<String> = [
        ("UserAssignedIdentity", &identity.metadata, &identity.status),
        (
            "FederatedIdentityCredential",
            &credential.metadata,
            &credential.status,
        ),
        (
            "RoleAssignment",
            &role_assignment.metadata,
            &role_assignment.status,
        ),
    ]
    .into_iter()
    .filter(|(_, meta, status)| !is_ready(meta, status.as_ref()))
    .map(|(kind, meta, status)| not_ready_description(kind, meta, status.as_ref()))
    .collect();

    if !waiting.is_empty() {
        return Ok(IdentityState::Pending {
            message: format!(
                "Waiting for Azure Service Operator to provision: {}",
                waiting.join("; ")
            ),
        });
    }

    // ASO owns this ConfigMap through the identity. A ConfigMap of that name that the
    // identity does not own was put there by somebody else, and its client id is not trusted.
    let identity_uid = identity.metadata.uid.as_deref().unwrap_or_default();
    let client_id = cm_api
        .get_opt(IDENTITY_CONFIGMAP_NAME)
        .await?
        .filter(|cm| !identity_uid.is_empty() && is_owned_by(&cm.metadata, identity_uid))
        .and_then(|cm| cm.data)
        .and_then(|mut data| data.remove(CLIENT_ID_KEY))
        .filter(|id| is_guid(id));

    Ok(match client_id {
        Some(client_id) => IdentityState::Ready { client_id },
        None => IdentityState::Pending {
            message: format!(
                "Waiting for Azure Service Operator to publish the client id in ConfigMap {IDENTITY_CONFIGMAP_NAME}"
            ),
        },
    })
}

/// The client id this cluster's pods were last rolled for, when it is also on the ServiceAccount.
/// That is the case once the StatefulSet exists and the identity was in use, and it lets a
/// running cluster ride out an ASO hiccup instead of stalling.
pub fn known_client_id(
    stateful_set: Option<&StatefulSet>,
    service_account: Option<&ServiceAccount>,
) -> Option<String> {
    let rolled_for = stateful_set?
        .spec
        .as_ref()?
        .template
        .metadata
        .as_ref()?
        .annotations
        .as_ref()?
        .get(POD_CLIENT_ID_ANNOTATION)?;
    let on_service_account = service_account?
        .metadata
        .annotations
        .as_ref()?
        .get(CLIENT_ID_ANNOTATION)?;
    (rolled_for == on_service_account).then(|| rolled_for.clone())
}

pub fn warn_continuing_with(client_id: &str, namespace: &str, message: &str) {
    warn!(
        "Azure identity objects in namespace {namespace} are not ready ({message}); continuing with the known client id {client_id}"
    );
}

/// Asks ASO to remove the Azure resources by deleting the objects this RestateCluster created.
/// The three deletes are only issued here: ASO then removes the role assignment, the credential
/// and the identity from Azure independently of each other.
pub async fn delete_identity(
    client: &Client,
    namespace: &str,
    base_metadata: &ObjectMeta,
) -> Result<(), Error> {
    let uid = cluster_uid(base_metadata);
    let params = DeleteParams::default();
    delete_owned(
        namespace,
        &Api::<RoleAssignment>::namespaced(client.clone(), namespace),
        ROLE_ASSIGNMENT_NAME,
        uid,
        &params,
    )
    .await?;
    delete_owned(
        namespace,
        &Api::<FederatedIdentityCredential>::namespaced(client.clone(), namespace),
        CREDENTIAL_NAME,
        uid,
        &params,
    )
    .await?;
    delete_owned(
        namespace,
        &Api::<UserAssignedIdentity>::namespaced(client.clone(), namespace),
        IDENTITY_NAME,
        uid,
        &params,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::azureidentities::AzureResourceCondition;
    use k8s_openapi::api::apps::v1::StatefulSetSpec;
    use k8s_openapi::api::core::v1::PodTemplateSpec;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{OwnerReference, Time};
    use std::collections::BTreeMap;

    const RG_ID: &str = "/subscriptions/df370aa7-c354-429b-8bed-9efd950b0e07/resourceGroups/bh-byok-azure-spike-envs";
    const CONTAINER_ID: &str = "/subscriptions/df370aa7-c354-429b-8bed-9efd950b0e07/resourceGroups/bh-byok-azure-spike/providers/Microsoft.Storage/storageAccounts/bhbyokspike18158/blobServices/default/containers/snapshots";
    const ROLE_ID: &str = "/subscriptions/df370aa7-c354-429b-8bed-9efd950b0e07/providers/Microsoft.Authorization/roleDefinitions/fdd44b01-e141-4434-a094-94da261cfe39";
    const ISSUER: &str = "https://swedencentral.oic.prod-aks.azure.com/tenant/cluster/";
    const CLIENT_ID: &str = "9e8a3ba0-08d6-4380-8d04-f6d3e25b1531";

    fn parts(
        rg: &str,
        location: &str,
        issuer: &str,
        container: &str,
        role: &str,
    ) -> anyhow::Result<Option<AzureWorkloadIdentityConfig>> {
        AzureWorkloadIdentityConfig::from_parts(
            Some(rg.into()),
            Some(location.into()),
            Some(issuer.into()),
            Some(container.into()),
            Some(role.into()),
        )
    }

    fn config() -> AzureWorkloadIdentityConfig {
        parts(RG_ID, "swedencentral", ISSUER, CONTAINER_ID, ROLE_ID)
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
    fn config_parses_every_setting() {
        let config = config();
        assert_eq!(config.storage_container.account, "bhbyokspike18158");
        assert_eq!(config.storage_container.container, "snapshots");
        assert_eq!(
            config.storage_container.subscription_id,
            "df370aa7-c354-429b-8bed-9efd950b0e07"
        );
        assert_eq!(config.storage_container.id, CONTAINER_ID);
        assert_eq!(config.role_definition_id, ROLE_ID);
        assert_eq!(config.identity_resource_group_id, RG_ID);
    }

    #[test]
    fn config_is_all_or_nothing_and_refuses_each_malformed_setting() {
        assert_eq!(
            AzureWorkloadIdentityConfig::from_parts(None, None, None, None, None).unwrap(),
            None
        );
        // Every setting missing on its own is an error
        let all = [RG_ID, "swedencentral", ISSUER, CONTAINER_ID, ROLE_ID];
        for missing in 0..all.len() {
            let [rg, location, issuer, container, role] =
                std::array::from_fn(|i| (i != missing).then(|| all[i].to_owned()));
            let err =
                AzureWorkloadIdentityConfig::from_parts(rg, location, issuer, container, role)
                    .unwrap_err();
            assert!(
                err.to_string().contains("AZURE_WI_ROLE_DEFINITION_ID"),
                "{err}"
            );
        }

        let bad_rgs = [
            "",
            "/subscriptions/s/resourceGroups",
            "/subscriptions/s/resourceGroups/rg/providers/x",
            "/subscriptions//resourceGroups/rg",
            "/tenants/s/resourceGroups/rg",
        ];
        for bad in bad_rgs {
            let err = parts(bad, "swedencentral", ISSUER, CONTAINER_ID, ROLE_ID).unwrap_err();
            assert!(
                err.to_string().contains("IDENTITY_RESOURCE_GROUP_ID"),
                "{bad}"
            );
        }
        for bad in ["", "sweden central", "swe;rm"] {
            let err = parts(RG_ID, bad, ISSUER, CONTAINER_ID, ROLE_ID).unwrap_err();
            assert!(err.to_string().contains("AZURE_WI_LOCATION"), "{bad}");
        }
        for bad in ["", "http://issuer/", "https://", "issuer"] {
            let err = parts(RG_ID, "swedencentral", bad, CONTAINER_ID, ROLE_ID).unwrap_err();
            assert!(err.to_string().contains("AZURE_WI_OIDC_ISSUER"), "{bad}");
        }
        let bad_containers = [
            "",
            "/subscriptions/s/resourceGroups/rg",
            "/subscriptions/s/resourceGroups/rg/providers/Microsoft.Storage/storageAccounts/acct",
            "/subscriptions/s/resourceGroups/rg/providers/Microsoft.Storage/storageAccounts/acct/blobServices/default/containers/",
            "/subscriptions/s/resourceGroups/rg/providers/Microsoft.Compute/storageAccounts/acct/blobServices/default/containers/c",
            "/subscriptions/s/resourceGroups/rg/providers/Microsoft.Storage/storageAccounts/acct/blobServices/default/containers/c;rm",
            "/subscriptions/s/resourceGroups/rg/providers/Microsoft.Storage/storageAccounts/ac-ct/blobServices/default/containers/c",
        ];
        for bad in bad_containers {
            let err = parts(RG_ID, "swedencentral", ISSUER, bad, ROLE_ID).unwrap_err();
            assert!(err.to_string().contains("STORAGE_CONTAINER_ID"), "{bad}");
        }
        let bad_roles = [
            "",
            "fdd44b01-e141-4434-a094-94da261cfe39",
            "/subscriptions//providers/Microsoft.Authorization/roleDefinitions/fdd44b01-e141-4434-a094-94da261cfe39",
            "/subscriptions/df370aa7-c354-429b-8bed-9efd950b0e07/providers/Microsoft.Authorization/roleDefinitions/not-a-guid",
            "/subscriptions/df370aa7-c354-429b-8bed-9efd950b0e07/providers/Microsoft.Authorization/roleAssignments/fdd44b01-e141-4434-a094-94da261cfe39",
            "/subscriptions/df370aa7-c354-429b-8bed-9efd950b0e07/providers/Microsoft.Authorization/roleDefinitions/fdd44b01-e141-4434-a094-94da261cfe39/x",
        ];
        for bad in bad_roles {
            let err = parts(RG_ID, "swedencentral", ISSUER, CONTAINER_ID, bad).unwrap_err();
            assert!(
                err.to_string().contains("AZURE_WI_ROLE_DEFINITION_ID"),
                "{bad}"
            );
        }
    }

    #[test]
    fn identity_is_named_per_namespace_and_exports_its_ids() {
        let uai = user_assigned_identity("env-c", &base_metadata(), &config());
        assert_eq!(uai.metadata.name.as_deref(), Some("restate"));
        assert_eq!(uai.spec.azure_name, "env-c-restate");
        assert_eq!(uai.spec.location, "swedencentral");
        assert_eq!(uai.spec.owner.arm_id, RG_ID);
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
        assert_eq!(json["spec"]["issuer"], ISSUER);
        assert_eq!(
            json["spec"]["subject"],
            "system:serviceaccount:env-c:restate"
        );
    }

    #[test]
    fn role_assignment_uses_the_custom_role_and_carries_the_prefix_condition() {
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
        assert_eq!(json["spec"]["roleDefinitionReference"]["armId"], ROLE_ID);
        assert_eq!(json["spec"]["conditionVersion"], "2.0");
        assert_eq!(
            json["spec"]["condition"],
            "((!(ActionMatches{'Microsoft.Storage/storageAccounts/blobServices/containers/blobs/read'} AND NOT SubOperationMatches{'Blob.List'}) AND !(ActionMatches{'Microsoft.Storage/storageAccounts/blobServices/containers/blobs/write'}) AND !(ActionMatches{'Microsoft.Storage/storageAccounts/blobServices/containers/blobs/add/action'}) AND !(ActionMatches{'Microsoft.Storage/storageAccounts/blobServices/containers/blobs/delete'})) OR (@Resource[Microsoft.Storage/storageAccounts/blobServices/containers/blobs:path] StringStartsWith 'env-c/')) AND ((!(ActionMatches{'Microsoft.Storage/storageAccounts/blobServices/containers/blobs/read'} AND SubOperationMatches{'Blob.List'})) OR (@Request[Microsoft.Storage/storageAccounts/blobServices/containers/blobs:prefix] StringStartsWith 'env-c/'))"
        );
    }

    #[test]
    fn canary_proves_list_write_and_delete_under_the_prefix_and_pins_the_identity() {
        let command = canary_command("env-c", &config().storage_container, CLIENT_ID, ROLE_ID);
        assert_eq!(&command[..2], ["sh", "-c"]);
        let script = &command[2];
        assert!(script.contains("https://bhbyokspike18158.blob.core.windows.net/snapshots"));
        assert!(script.contains("prefix=env-c/&maxresults=1"));
        assert!(script.contains("$base/env-c/.restate-canary-$$"));
        assert!(script.contains("-X PUT"));
        assert!(script.contains("x-ms-blob-type: BlockBlob"));
        assert!(script.contains("-X DELETE"));
        assert!(script.contains("+ 660 ))"));
        assert!(script.contains("$AZURE_AUTHORITY_HOST$AZURE_TENANT_ID/oauth2/v2.0/token"));
        // The pod template, and so the Job, changes with the identity and the role
        assert!(script.contains(CLIENT_ID));
        assert!(script.contains(ROLE_ID));
        for placeholder in [
            "@RETRY_SECONDS@",
            "@ACCOUNT@",
            "@CONTAINER@",
            "@NAMESPACE@",
            "@CLIENT_ID@",
            "@ROLE@",
        ] {
            assert!(!script.contains(placeholder), "{placeholder}");
        }
        // Every request has a timeout, and the response body of a failure is printed
        assert_eq!(script.matches("curl -sS").count(), 1);
        assert!(script.contains("--max-time 15"));
        assert!(script.contains("head -c 1000"));
        assert!(CANARY_ACTIVE_DEADLINE_SECONDS > i64::from(CANARY_RETRY_SECONDS) + 120);
        let other = canary_command("env-c", &config().storage_container, "other", ROLE_ID);
        assert_ne!(command, other);
    }

    fn status(condition_status: &str, observed_generation: Option<i64>) -> AzureResourceStatus {
        AzureResourceStatus {
            conditions: Some(vec![AzureResourceCondition {
                r#type: "Ready".into(),
                status: condition_status.into(),
                message: Some("waiting".into()),
                reason: None,
                observed_generation,
                last_transition_time: None,
            }]),
        }
    }

    fn meta(generation: Option<i64>, terminating: bool) -> ObjectMeta {
        ObjectMeta {
            name: Some("restate".into()),
            generation,
            deletion_timestamp: terminating.then(|| Time(chrono::Utc::now())),
            ..Default::default()
        }
    }

    #[test]
    fn readiness_needs_a_current_ready_condition_on_an_object_that_is_not_terminating() {
        let ready = status("True", Some(3));
        assert!(is_ready(&meta(Some(3), false), Some(&ready)));
        // Ready for an older spec
        assert!(!is_ready(&meta(Some(4), false), Some(&ready)));
        assert!(!is_ready(
            &meta(Some(3), false),
            Some(&status("True", None))
        ));
        assert!(!is_ready(
            &meta(Some(3), false),
            Some(&status("False", Some(3)))
        ));
        assert!(!is_ready(
            &meta(Some(3), false),
            Some(&AzureResourceStatus { conditions: None })
        ));
        assert!(!is_ready(&meta(Some(3), false), None));
        // Terminating
        assert!(!is_ready(&meta(Some(3), true), Some(&ready)));

        assert_eq!(
            not_ready_description("RoleAssignment", &meta(Some(3), true), Some(&ready)),
            "RoleAssignment restate: terminating"
        );
        assert_eq!(
            not_ready_description("RoleAssignment", &meta(Some(4), false), Some(&ready)),
            "RoleAssignment restate: ASO has not yet reconciled the latest spec"
        );
        assert_eq!(
            not_ready_description(
                "RoleAssignment",
                &meta(Some(3), false),
                Some(&status("False", Some(3)))
            ),
            "RoleAssignment restate: waiting"
        );
    }

    #[test]
    fn only_objects_with_an_owner_reference_to_the_cluster_are_ours() {
        let owned = base_metadata();
        assert!(is_owned_by(&owned, "uid"));
        assert!(!is_owned_by(&owned, "another-uid"));
        assert!(!is_owned_by(&ObjectMeta::default(), "uid"));
        assert_eq!(cluster_uid(&owned), "uid");
    }

    fn stateful_set_rolled_for(client_id: &str) -> StatefulSet {
        StatefulSet {
            spec: Some(StatefulSetSpec {
                template: PodTemplateSpec {
                    metadata: Some(ObjectMeta {
                        annotations: Some(BTreeMap::from([(
                            POD_CLIENT_ID_ANNOTATION.to_owned(),
                            client_id.to_owned(),
                        )])),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn service_account_with(client_id: &str) -> ServiceAccount {
        ServiceAccount {
            metadata: ObjectMeta {
                annotations: Some(BTreeMap::from([(
                    CLIENT_ID_ANNOTATION.to_owned(),
                    client_id.to_owned(),
                )])),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn a_client_id_is_known_only_when_pods_and_service_account_agree() {
        let ss = stateful_set_rolled_for(CLIENT_ID);
        assert_eq!(
            known_client_id(Some(&ss), Some(&service_account_with(CLIENT_ID))).as_deref(),
            Some(CLIENT_ID)
        );
        // First enable: no StatefulSet yet, or one that was never rolled for an identity
        assert_eq!(
            known_client_id(None, Some(&service_account_with(CLIENT_ID))),
            None
        );
        assert_eq!(
            known_client_id(
                Some(&StatefulSet::default()),
                Some(&service_account_with(CLIENT_ID))
            ),
            None
        );
        // A client id a user put on the ServiceAccount does not count
        assert_eq!(
            known_client_id(Some(&ss), Some(&service_account_with("user"))),
            None
        );
        assert_eq!(
            known_client_id(Some(&ss), Some(&ServiceAccount::default())),
            None
        );
        assert_eq!(known_client_id(Some(&ss), None), None);
    }
}
