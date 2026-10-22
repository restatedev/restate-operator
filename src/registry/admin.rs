//! Wire models of the per-scope read API (`/v1/scopes/{scope}/…`, `02-contract.md` §4.1 in the
//! design docs): the shapes of Restate's admin API, so the web UI's deployments and services views
//! work unchanged against the registry.
//!
//! Copied from the schema registry (`schema-registry/src/admin.rs`), itself copied from the runtime
//! and pruned to what the registry has: `HttpDeploymentResponse`,
//! `HttpDetailedDeploymentResponse`, `ListDeploymentsResponse` (`crates/admin-rest-model/src/deployments.rs`),
//! `ServiceMetadata`, `HandlerMetadata` and their retry policies (`crates/types/src/schema/service.rs`).
//! The field names, serde renames and skipped fields must stay as they are there.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Serialize;

use super::api::Friendly;

/// The runtime's defaults for what a service must answer and the registry may not store
/// (`DEFAULT_IDEMPOTENCY_RETENTION`, `DEFAULT_INACTIVITY_TIMEOUT`, `DEFAULT_ABORT_TIMEOUT`, the
/// `ServiceRetryPolicyMetadata` defaults).
pub const DEFAULT_IDEMPOTENCY_RETENTION: Duration = Duration::from_secs(60 * 60 * 24);
pub const DEFAULT_INACTIVITY_TIMEOUT: Duration = Duration::from_secs(60);
pub const DEFAULT_ABORT_TIMEOUT: Duration = Duration::from_secs(600);
pub const DEFAULT_RETRY_INITIAL_INTERVAL: Duration = Duration::from_millis(100);
pub const DEFAULT_RETRY_EXPONENTIATION_FACTOR: f32 = 2.0;

#[derive(Debug, Clone, Serialize)]
pub struct ListDeploymentsResponse {
    pub deployments: Vec<DeploymentResponse<ServiceNameRevPair>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ListServicesResponse {
    pub services: Vec<ServiceMetadata>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ListServiceHandlersResponse {
    pub handlers: Vec<HandlerMetadata>,
}

/// `HttpDeploymentResponse` with `S` = [`ServiceNameRevPair`], `HttpDetailedDeploymentResponse`
/// with `S` = [`ServiceMetadata`].
#[derive(Debug, Clone, Serialize)]
pub struct DeploymentResponse<S> {
    #[serde(rename = "type")]
    pub ty: DeploymentType,
    pub id: String,
    pub uri: String,
    pub protocol_type: ProtocolType,
    /// `HTTP/2.0` or `HTTP/1.1`, like `http_serde::version`.
    pub http_version: &'static str,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub additional_headers: BTreeMap<String, String>,
    /// `tenant` = the scope, for a tenant's own deployment.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
    /// RFC 3339.
    pub created_at: String,
    pub min_protocol_version: i32,
    pub max_protocol_version: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sdk_version: Option<String>,
    pub services: Vec<S>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DeploymentType {
    Http,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub enum ProtocolType {
    RequestResponse,
    BidiStream,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServiceNameRevPair {
    pub name: String,
    pub revision: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServiceMetadata {
    pub name: String,
    pub ty: ServiceType,
    pub handlers: Vec<HandlerMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
    pub deployment_id: String,
    pub revision: u32,
    pub public: bool,
    pub idempotency_retention: Friendly,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow_completion_retention: Option<Friendly>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub journal_retention: Option<Friendly>,
    pub inactivity_timeout: Friendly,
    pub abort_timeout: Friendly,
    pub enable_lazy_state: bool,
    pub state_preload_policy: StatePreloadPolicy,
    pub retry_policy: ServiceRetryPolicyMetadata,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ServiceType {
    Service,
    VirtualObject,
    Workflow,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServiceRetryPolicyMetadata {
    pub initial_interval: Friendly,
    pub exponentiation_factor: f32,
    pub max_attempts: Option<u64>,
    pub max_interval: Option<Friendly>,
    pub on_max_attempts: OnMaxAttempts,
}

#[derive(Debug, Clone, Serialize)]
pub struct HandlerMetadata {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ty: Option<HandlerType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotency_retention: Option<Friendly>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub journal_retention: Option<Friendly>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inactivity_timeout: Option<Friendly>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub abort_timeout: Option<Friendly>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_preload_policy: Option<StatePreloadPolicy>,
    pub public: bool,
    pub input_description: String,
    pub output_description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_json_schema: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_json_schema: Option<serde_json::Value>,
    pub retry_policy: HandlerRetryPolicyMetadata,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum HandlerType {
    Exclusive,
    Shared,
    Workflow,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct HandlerRetryPolicyMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initial_interval: Option<Friendly>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exponentiation_factor: Option<f32>,
    pub max_attempts: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_interval: Option<Friendly>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_max_attempts: Option<OnMaxAttempts>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub enum OnMaxAttempts {
    #[default]
    Pause,
    Kill,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub enum StatePreloadPolicy {
    #[default]
    All,
    Partial(Vec<String>),
}

/// The admin API's error body.
#[derive(Debug, Clone, Serialize)]
pub struct ErrorDescriptionResponse {
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

    /// What the runtime's admin API answers for a deployment with one virtual object, from
    /// `crates/admin-rest-model` and `crates/types/src/schema/service.rs` serialized with these
    /// values.
    #[test]
    fn detailed_deployment_golden() {
        let response = DeploymentResponse {
            ty: DeploymentType::Http,
            id: "dp_101SZwviYzes2mjOamuMJWw".to_owned(),
            uri: "http://counter:9080/".to_owned(),
            protocol_type: ProtocolType::BidiStream,
            http_version: "HTTP/2.0",
            additional_headers: BTreeMap::new(),
            metadata: BTreeMap::new(),
            created_at: "2026-10-21T10:00:00Z".to_owned(),
            min_protocol_version: 5,
            max_protocol_version: 7,
            sdk_version: None,
            services: vec![ServiceMetadata {
                name: "Counter".to_owned(),
                ty: ServiceType::VirtualObject,
                handlers: vec![HandlerMetadata {
                    name: "add".to_owned(),
                    ty: Some(HandlerType::Exclusive),
                    documentation: None,
                    metadata: BTreeMap::new(),
                    idempotency_retention: None,
                    journal_retention: None,
                    inactivity_timeout: None,
                    abort_timeout: None,
                    state_preload_policy: None,
                    public: true,
                    input_description: "none".to_owned(),
                    output_description: "application/json".to_owned(),
                    input_json_schema: None,
                    output_json_schema: None,
                    retry_policy: HandlerRetryPolicyMetadata::default(),
                }],
                documentation: None,
                metadata: BTreeMap::new(),
                deployment_id: "dp_101SZwviYzes2mjOamuMJWw".to_owned(),
                revision: 1,
                public: true,
                idempotency_retention: Friendly(DEFAULT_IDEMPOTENCY_RETENTION),
                workflow_completion_retention: None,
                journal_retention: None,
                inactivity_timeout: Friendly(DEFAULT_INACTIVITY_TIMEOUT),
                abort_timeout: Friendly(DEFAULT_ABORT_TIMEOUT),
                enable_lazy_state: false,
                state_preload_policy: StatePreloadPolicy::All,
                retry_policy: ServiceRetryPolicyMetadata {
                    initial_interval: Friendly(DEFAULT_RETRY_INITIAL_INTERVAL),
                    exponentiation_factor: DEFAULT_RETRY_EXPONENTIATION_FACTOR,
                    max_attempts: None,
                    max_interval: None,
                    on_max_attempts: OnMaxAttempts::Pause,
                },
            }],
        };

        assert_eq!(
            serde_json::to_value(&response).unwrap(),
            json!({
                "type": "http",
                "id": "dp_101SZwviYzes2mjOamuMJWw",
                "uri": "http://counter:9080/",
                "protocol_type": "BidiStream",
                "http_version": "HTTP/2.0",
                "created_at": "2026-10-21T10:00:00Z",
                "min_protocol_version": 5,
                "max_protocol_version": 7,
                "services": [{
                    "name": "Counter",
                    "ty": "VirtualObject",
                    "handlers": [{
                        "name": "add",
                        "ty": "Exclusive",
                        "public": true,
                        "input_description": "none",
                        "output_description": "application/json",
                        "retry_policy": {"max_attempts": null},
                    }],
                    "deployment_id": "dp_101SZwviYzes2mjOamuMJWw",
                    "revision": 1,
                    "public": true,
                    "idempotency_retention": "1d",
                    "inactivity_timeout": "1m",
                    "abort_timeout": "10m",
                    "enable_lazy_state": false,
                    "state_preload_policy": "All",
                    "retry_policy": {
                        "initial_interval": "100ms",
                        "exponentiation_factor": 2.0,
                        "max_attempts": null,
                        "max_interval": null,
                        "on_max_attempts": "Pause",
                    },
                }],
            })
        );
    }

    #[test]
    fn partial_preload_encoding() {
        assert_eq!(
            serde_json::to_value(StatePreloadPolicy::Partial(vec!["a".to_owned()])).unwrap(),
            json!({"Partial": ["a"]})
        );
    }
}
