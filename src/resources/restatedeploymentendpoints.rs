//! `RestateDeploymentEndpoint`: one Restate deployment in a cluster whose workflow registry is
//! the operator (`spec.multitenancy.workflowRegistry.mode: operator` on the RestateCluster).
//!
//! The spec says where the deployment is; the status holds what the operator discovered there,
//! reduced to what the registry lookups answer from (the "projection"): no docs, no metadata, no
//! JSON schemas. Created by a RestateDeployment for each of its versions, or by hand for a
//! deployment that runs anywhere else.

use std::collections::BTreeMap;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use kube::{CustomResource, KubeSchema};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The label on a Namespace that makes it a tenant: its value is the tenant's scope.
pub const TENANT_LABEL: &str = "restate.dev/tenant";

/// The label the operator stamps on every endpoint with the name of its RestateCluster, and the
/// selector the registry's watch uses.
pub const CLUSTER_LABEL: &str = "restate.dev/cluster";

/// The longest scope the stack accepts (the registry's and the relay's scope validation).
pub const MAX_SCOPE_LENGTH: usize = 36;

/// A Restate deployment served by the operator's workflow registry. The spec is immutable: a new
/// address (or anything else) is a new deployment, so a new object.
#[derive(CustomResource, Deserialize, Serialize, Clone, Debug, KubeSchema)]
#[kube(
    kind = "RestateDeploymentEndpoint",
    group = "restate.dev",
    version = "v1beta1",
    namespaced,
    status = "RestateDeploymentEndpointStatus",
    shortname = "rsde",
    printcolumn = r#"{"name":"Tenant", "type":"string", "jsonPath":".status.tenant"}"#,
    printcolumn = r#"{"name":"Services", "type":"string", "jsonPath":".status.summary"}"#,
    printcolumn = r#"{"name":"Phase", "type":"string", "jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Age", "type":"date", "jsonPath":".metadata.creationTimestamp"}"#,
    printcolumn = r#"{"name":"Deployment ID", "type":"string", "jsonPath":".status.deploymentId", "priority": 1}"#,
    printcolumn = r#"{"name":"Address", "type":"string", "jsonPath":".spec.address", "priority": 1}"#
)]
#[serde(rename_all = "camelCase")]
#[x_kube(validation = Rule::new("self == oldSelf").message(
    "the spec of a RestateDeploymentEndpoint is immutable: create a new one for a new deployment"
))]
pub struct RestateDeploymentEndpointSpec {
    /// The name of the RestateCluster whose workflow registry serves this deployment. Its
    /// `spec.multitenancy.workflowRegistry.mode` must be `operator`.
    pub cluster: String,

    /// The deployment's base URL (http or https). The operator discovers it at
    /// `<address>discover`, the relay invokes `<address>invoke/{service}/{handler}`.
    pub address: String,

    /// Use HTTP/1.1 instead of HTTP/2. Not supported by the relay yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_http11: Option<bool>,

    /// Headers sent on discovery and, by the relay, on every request to the deployment.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub additional_headers: BTreeMap<String, String>,
}

/// Status of the RestateDeploymentEndpoint. Written by the operator only.
#[derive(Deserialize, Serialize, Clone, Default, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RestateDeploymentEndpointStatus {
    /// Where the endpoint is in its life: Pending (not discovered yet), Ready (served), Failed
    /// (discovery or validation failed; see the conditions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "endpoint_phase_schema")]
    pub phase: Option<EndpointPhase>,

    /// The tenant (scope) this deployment belongs to, from its namespace's `restate.dev/tenant`
    /// label. The registry trusts this field, not the endpoint's own labels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,

    /// The Restate deployment id (`dp_...`), derived from the object's uid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment_id: Option<String>,

    /// The services and their revisions, e.g. `checkout@3`. For display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,

    /// What discovery found, reduced to what the registry lookups need. Set once, when the
    /// endpoint becomes Ready; only the revisions change after that (a rollback).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovered: Option<DiscoveredDeployment>,

    /// The generation observed by the operator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conditions: Option<Vec<Condition>>,
}

// Not a doc comment: a described enum inside an `Option` renders as an `anyOf` the API server
// rejects (see `crate::resources::reconciliation_schema`).
#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq, JsonSchema)]
pub enum EndpointPhase {
    Pending,
    Ready,
    Failed,
}

impl EndpointPhase {
    pub const ALL: [Self; 3] = [Self::Pending, Self::Ready, Self::Failed];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::Ready => "Ready",
            Self::Failed => "Failed",
        }
    }
}

fn endpoint_phase_schema(_g: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "string",
        "enum": EndpointPhase::ALL.map(EndpointPhase::as_str).to_vec(),
        "nullable": true,
    })
}

/// What discovery found at the endpoint, in the shape the registry lookups answer from.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredDeployment {
    /// When discovery succeeded.
    pub discovered_at: Time,
    pub protocol_type: ProtocolType,
    pub http_version: HttpVersion,
    pub min_protocol_version: i32,
    pub max_protocol_version: i32,
    /// The `x-restate-server` header of the discovery response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sdk_version: Option<String>,
    pub services: Vec<ProjectedService>,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq, JsonSchema)]
pub enum ProtocolType {
    BidiStream,
    RequestResponse,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq, JsonSchema)]
pub enum HttpVersion {
    #[serde(rename = "HTTP/1.1")]
    Http11,
    #[serde(rename = "HTTP/2")]
    Http2,
}

/// A service of the deployment: what the lookups need, and its revision in the tenant.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectedService {
    pub name: String,
    pub ty: ServiceType,
    /// The revision of this service in the tenant, assigned once at discovery (re-assigned on a
    /// rollback). The highest revision among the Ready endpoints is the latest.
    pub revision: u32,
    /// Reachable from the ingress (the manifest's `ingressPrivate`, inverted).
    pub public: bool,
    #[serde(default, skip_serializing_if = "ProjectedOptions::is_empty")]
    pub options: ProjectedOptions,
    pub handlers: Vec<ProjectedHandler>,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq, JsonSchema)]
pub enum ServiceType {
    Service,
    VirtualObject,
    Workflow,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectedHandler {
    pub name: String,
    pub target_ty: TargetType,
    /// When not set, the service's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public: Option<bool>,
    /// Milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_completion_retention: Option<u64>,
    #[serde(default, skip_serializing_if = "ProjectedOptions::is_empty")]
    pub options: ProjectedOptions,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<InputRules>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<OutputRules>,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq, JsonSchema)]
pub enum TargetType {
    Service,
    VirtualObjectExclusive,
    VirtualObjectShared,
    Workflow,
    WorkflowShared,
}

/// The settings of a service or a handler. Durations are in milliseconds; unset means not
/// configured (the server's defaults apply).
#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProjectedOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_retention: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub journal_retention: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inactivity_timeout: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abort_timeout: Option<u64>,
    /// Which state is preloaded: all of it (lazy state off), or only some keys (lazy state on).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_preload: Option<StatePreload>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_initial_interval: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_exponentiation_factor: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_max_attempts: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_max_interval: Option<u64>,
    /// `Pause` or `Kill`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_on_max_attempts: Option<String>,
}

/// The values of [`ProjectedOptions::retry_on_max_attempts`].
pub const ON_MAX_ATTEMPTS_PAUSE: &str = "Pause";
pub const ON_MAX_ATTEMPTS_KILL: &str = "Kill";

impl ProjectedOptions {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// The state preload policy (the runtime's `StatePreloadPolicy`): `lazy: false` preloads every key
/// (`All`), `lazy: true` preloads only `keys` and fetches the rest lazily (`Partial(keys)`; no keys:
/// fully lazy).
#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StatePreload {
    /// Lazy state: only `keys` are preloaded. Not set: every key is.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub lazy: bool,
    /// The keys preloaded when `lazy`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<String>,
}

/// What the ingress accepts as input (the registry's `InputPayload`).
#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InputRules {
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    pub json: bool,
}

/// How the ingress returns the output (the registry's `OutputPayload`).
#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OutputRules {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    pub set_content_type_if_empty: bool,
}

/// The Restate deployment id of an endpoint: `dp_1` + the 128 bits of its `metadata.uid` in the
/// runtime's base62 encoding (`DeploymentId` in the runtime's `crates/types/src/identifiers.rs`;
/// the schema registry's `src/id.rs` does the same from a ULID). `None` if the uid isn't a UUID.
pub fn deployment_id_from_uid(uid: &str) -> Option<String> {
    const ALPHABET: &[u8; 62] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    const ENCODED_LENGTH: usize = 22;

    let hex: String = uid.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 {
        return None;
    }
    let value = u128::from_str_radix(&hex, 16).ok()?;
    // The runtime encodes `u128::to_be()` of the ULID: on the little endian hosts it runs on,
    // that swaps the bytes.
    let mut n = value.swap_bytes();
    let mut digits = [b'0'; ENCODED_LENGTH];
    for digit in digits.iter_mut().rev() {
        if n == 0 {
            break;
        }
        *digit = ALPHABET[(n % 62) as usize];
        n /= 62;
    }
    Some(format!(
        "dp_1{}",
        std::str::from_utf8(&digits).expect("base62 digits are ascii")
    ))
}

/// Whether a value is a valid scope (the registry's and the relay's rules).
pub fn is_valid_scope(scope: &str) -> bool {
    !scope.is_empty()
        && scope.len() <= MAX_SCOPE_LENGTH
        && scope
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ULID `0:1` of the runtime's tests is the uid `00000000-0000-0000-0000-000000000001`.
    #[test]
    fn deployment_ids_encode_like_the_runtime() {
        assert_eq!(
            deployment_id_from_uid("00000000-0000-0000-0000-000000000001").as_deref(),
            Some("dp_101SZwviYzes2mjOamuMJWw")
        );
        assert_eq!(
            deployment_id_from_uid("00000000-0000-0000-0000-000000000000").as_deref(),
            Some("dp_10000000000000000000000")
        );
    }

    /// The id/ULID pair of the runtime's `identifiers.rs` tests (`01JH0Z3FJF2MAK9T8VK8GQ6T9D`).
    #[test]
    fn deployment_ids_match_the_runtime_for_a_real_ulid() {
        let ulid = 0x019441f1be4f151534e91b9a2173692d_u128; // 01JH0Z3FJF2MAK9T8VK8GQ6T9D
        let uid = format!("{ulid:032x}");
        assert_eq!(
            deployment_id_from_uid(&uid).as_deref(),
            Some("dp_11nGQpCRmau6ypL82KH2TnP")
        );
    }

    #[test]
    fn bad_uids_have_no_id() {
        assert_eq!(deployment_id_from_uid("not-a-uuid"), None);
    }

    #[test]
    fn scopes() {
        assert!(is_valid_scope("t1"));
        assert!(is_valid_scope("a.b_c-d"));
        assert!(!is_valid_scope(""));
        assert!(!is_valid_scope(&"x".repeat(37)));
        assert!(!is_valid_scope("a/b"));
    }
}
