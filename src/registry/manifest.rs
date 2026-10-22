//! The endpoint manifest (the SDK's `/discover` response), and its conversion into what a
//! `RestateDeploymentEndpoint` stores (the lookup projection, [`project`]) and what the per-scope
//! read API shows on top of it ([`details`]: docs, metadata, JSON schemas, fetched live).
//!
//! Copied from the schema registry (`schema-registry/src/manifest.rs`): the model, `build` and its
//! checks are the registry's, so an endpoint the operator accepts is one the registry would accept,
//! normalized the same way. Only the subset of `service-protocol/endpoint_manifest_schema.json` the
//! lookup API needs is modelled, every other field is ignored. The conversion and its checks are
//! the runtime's (`crates/types/src/schema/metadata/updater/mod.rs` and
//! `crates/service-protocol-v4/src/discovery.rs`), pruned to what Restate would otherwise reject
//! as a bad lookup answer. One check is added (`56-operator-registry.md`, *Discovery*): a manifest
//! without services is rejected.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::str::FromStr;
use std::time::Duration;

use http::{HeaderName, HeaderValue, Uri};
use serde::{Deserialize, Serialize};

use super::api::{
    HttpVersion, InputPayload, OnMaxAttempts, OutputPayload, ProtocolType, StatePreloadPolicy,
    TargetType, format_duration,
};
use crate::resources::restatedeploymentendpoints as crd;

/// The `accept` header of a discovery request: the endpoint manifest versions this module parses,
/// highest first. The runtime accepts v1 to v5 (`MIN/MAX_SERVICE_DISCOVERY_PROTOCOL_VERSION`,
/// `crates/service-protocol-v4/src/discovery.rs`), all JSON with the same subset of fields read
/// here (later versions only add optional fields, which are ignored when unknown).
pub const DISCOVER_ACCEPT: &str = "application/vnd.restate.endpointmanifest.v5+json, \
    application/vnd.restate.endpointmanifest.v4+json, \
    application/vnd.restate.endpointmanifest.v3+json, \
    application/vnd.restate.endpointmanifest.v2+json, \
    application/vnd.restate.endpointmanifest.v1+json";

/// Why a manifest is rejected.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("can't parse the endpoint manifest: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("{0}")]
    Invalid(String),
}

type Error = ManifestError;

impl ManifestError {
    fn bad_request(message: impl ToString) -> Self {
        Self::Invalid(message.to_string())
    }
}

/// Service protocol versions Restate accepts at discovery (`crates/types/src/service_protocol.rs`).
const MIN_DISCOVERABLE_PROTOCOL_VERSION: i32 = 5;
const MAX_DISCOVERABLE_PROTOCOL_VERSION: i32 = 7;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Endpoint {
    pub protocol_mode: Option<ProtocolMode>,
    pub min_protocol_version: u64,
    pub max_protocol_version: u64,
    pub services: Vec<Service>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ProtocolMode {
    BidiStream,
    RequestResponse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ServiceType {
    VirtualObject,
    Service,
    Workflow,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum HandlerType {
    Workflow,
    Exclusive,
    Shared,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ManifestOnMaxAttempts {
    Pause,
    Kill,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Service {
    pub name: String,
    pub ty: ServiceType,
    pub documentation: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    #[serde(default)]
    pub handlers: Vec<Handler>,
    #[serde(flatten)]
    pub options: ManifestOptions,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Handler {
    pub name: String,
    pub ty: Option<HandlerType>,
    pub documentation: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    pub input: Option<ManifestInput>,
    pub output: Option<ManifestOutput>,
    pub workflow_completion_retention: Option<u64>,
    #[serde(flatten)]
    pub options: ManifestOptions,
}

/// The settings both services and handlers have. Durations are in milliseconds.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestOptions {
    pub ingress_private: Option<bool>,
    pub inactivity_timeout: Option<u64>,
    pub abort_timeout: Option<u64>,
    pub journal_retention: Option<u64>,
    pub idempotency_retention: Option<u64>,
    pub enable_lazy_state: Option<bool>,
    #[serde(default)]
    pub eager_state_keys_whitelist: Vec<String>,
    pub retry_policy_initial_interval: Option<u64>,
    pub retry_policy_max_interval: Option<u64>,
    pub retry_policy_max_attempts: Option<u64>,
    pub retry_policy_exponentiation_factor: Option<f64>,
    pub retry_policy_on_max_attempts: Option<ManifestOnMaxAttempts>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestInput {
    pub required: Option<bool>,
    pub content_type: Option<String>,
    pub json_schema: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestOutput {
    pub content_type: Option<String>,
    pub set_content_type_if_empty: Option<bool>,
    pub json_schema: Option<serde_json::Value>,
}

/// A deployment to store, out of an add request.
#[derive(Debug, Clone, PartialEq)]
pub struct NewDeployment {
    pub address: String,
    pub protocol_type: ProtocolType,
    pub http_version: HttpVersion,
    pub additional_headers: BTreeMap<String, String>,
    pub min_protocol_version: i32,
    pub max_protocol_version: i32,
    pub sdk_version: Option<String>,
    pub services: Vec<NewService>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewService {
    pub name: String,
    pub ty: ServiceType,
    pub public: bool,
    pub options: Options,
    pub docs: Docs,
    pub handlers: Vec<NewHandler>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewHandler {
    pub name: String,
    pub target_ty: TargetType,
    /// When not set, the service's.
    pub public: Option<bool>,
    pub workflow_completion_retention: Option<u64>,
    pub options: Options,
    pub docs: Docs,
    pub input: Option<InputPayload>,
    pub output: Option<OutputPayload>,
    pub input_json_schema: Option<serde_json::Value>,
    pub output_json_schema: Option<serde_json::Value>,
}

/// What the SDK documents about a service or a handler. Only shown (the per-scope read API), never
/// used by the lookups.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Docs {
    pub documentation: Option<String>,
    pub metadata: BTreeMap<String, String>,
}

/// The settings of a service or a handler. Durations are in milliseconds, `None` means not set.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Options {
    pub idempotency_retention: Option<u64>,
    pub journal_retention: Option<u64>,
    pub inactivity_timeout: Option<u64>,
    pub abort_timeout: Option<u64>,
    pub state_preload_policy: Option<StatePreloadPolicy>,
    pub retry_initial_interval: Option<u64>,
    pub retry_exponentiation_factor: Option<f64>,
    pub retry_max_attempts: Option<u64>,
    pub retry_max_interval: Option<u64>,
    pub retry_on_max_attempts: Option<OnMaxAttempts>,
}

fn build(
    uri: &str,
    additional_headers: &BTreeMap<String, String>,
    use_http_11: bool,
    sdk_version: Option<String>,
    endpoint: Endpoint,
) -> Result<NewDeployment, Error> {
    let protocol_type = match endpoint.protocol_mode {
        Some(ProtocolMode::BidiStream) => ProtocolType::BidiStream,
        Some(ProtocolMode::RequestResponse) => ProtocolType::RequestResponse,
        None => return Err(Error::bad_request("the manifest has no protocolMode")),
    };
    let (min_protocol_version, max_protocol_version) =
        protocol_versions(endpoint.min_protocol_version, endpoint.max_protocol_version)?;

    let mut names = HashSet::new();
    let services = endpoint
        .services
        .into_iter()
        .map(|service| {
            if !names.insert(service.name.clone()) {
                return Err(Error::bad_request(format!(
                    "service '{}' is defined twice",
                    service.name
                )));
            }
            build_service(service)
        })
        .collect::<Result<_, _>>()?;

    Ok(NewDeployment {
        address: normalize_address(uri)?,
        protocol_type,
        http_version: if use_http_11 {
            HttpVersion::Http11
        } else {
            HttpVersion::Http2
        },
        additional_headers: additional_headers
            .iter()
            .map(|(name, value)| validate_header(name, value))
            .collect::<Result<_, _>>()?,
        min_protocol_version,
        max_protocol_version,
        sdk_version,
        services,
    })
}

/// The address as the runtime prints it: `http://host:9080` becomes `http://host:9080/`.
pub fn normalize_address(uri: &str) -> Result<String, Error> {
    let parsed =
        Uri::from_str(uri).map_err(|e| Error::bad_request(format!("bad uri '{uri}': {e}")))?;
    if !matches!(parsed.scheme_str(), Some("http" | "https")) || parsed.authority().is_none() {
        return Err(Error::bad_request(format!(
            "bad uri '{uri}': must be an absolute http(s) uri"
        )));
    }
    Ok(parsed.to_string())
}

/// A valid header, its name lowercased.
pub fn validate_header(name: &str, value: &str) -> Result<(String, String), Error> {
    let name = HeaderName::from_str(name)
        .map_err(|e| Error::bad_request(format!("bad header name '{name}': {e}")))?;
    HeaderValue::from_str(value)
        .map_err(|e| Error::bad_request(format!("bad value for header '{name}': {e}")))?;
    Ok((name.as_str().to_owned(), value.to_owned()))
}

fn protocol_versions(min: u64, max: u64) -> Result<(i32, i32), Error> {
    let (Ok(min), Ok(max)) = (i32::try_from(min), i32::try_from(max)) else {
        return Err(Error::bad_request(format!(
            "protocol versions must be in [1, {}]",
            i32::MAX
        )));
    };
    if min < 1 || min > max {
        return Err(Error::bad_request(format!(
            "bad protocol versions: min {min} must be in [1, max {max}]"
        )));
    }
    if min > MAX_DISCOVERABLE_PROTOCOL_VERSION || max < MIN_DISCOVERABLE_PROTOCOL_VERSION {
        return Err(Error::bad_request(format!(
            "unsupported protocol versions [{min}, {max}]: Restate supports [{MIN_DISCOVERABLE_PROTOCOL_VERSION}, {MAX_DISCOVERABLE_PROTOCOL_VERSION}]"
        )));
    }
    Ok((min, max))
}

fn build_service(service: Service) -> Result<NewService, Error> {
    let Service {
        name,
        ty,
        documentation,
        metadata,
        handlers,
        options,
    } = service;

    let mut handler_names = HashSet::new();
    let handlers = handlers
        .into_iter()
        .map(|handler| {
            if !handler_names.insert(handler.name.clone()) {
                return Err(Error::bad_request(format!(
                    "handler '{name}/{}' is defined twice",
                    handler.name
                )));
            }
            build_handler(&name, ty, handler)
        })
        .collect::<Result<_, _>>()?;

    Ok(NewService {
        public: !options.ingress_private.unwrap_or(false),
        options: build_options(&name, options)?,
        docs: Docs {
            documentation,
            metadata,
        },
        name,
        ty,
        handlers,
    })
}

fn build_handler(
    service_name: &str,
    service_ty: ServiceType,
    handler: Handler,
) -> Result<NewHandler, Error> {
    let target = format!("{service_name}/{}", handler.name);

    let target_ty = match (service_ty, handler.ty) {
        (ServiceType::Service, None | Some(HandlerType::Shared)) => TargetType::Service,
        (ServiceType::VirtualObject, None | Some(HandlerType::Exclusive)) => {
            TargetType::VirtualObjectExclusive
        }
        (ServiceType::VirtualObject, Some(HandlerType::Shared)) => TargetType::VirtualObjectShared,
        // Like in the runtime, a workflow handler without a type is shared.
        (ServiceType::Workflow, None | Some(HandlerType::Shared)) => TargetType::WorkflowShared,
        (ServiceType::Workflow, Some(HandlerType::Workflow)) => TargetType::Workflow,
        (service_ty, Some(handler_ty)) => {
            return Err(Error::bad_request(format!(
                "handler '{target}': a {handler_ty:?} handler is not allowed in a {service_ty:?}"
            )));
        }
    };

    if let Some(retention) = handler.workflow_completion_retention {
        check_duration(&target, retention)?;
    }

    // Like in the runtime, the input JSON schema is ignored without a content-type.
    let input_json_schema = handler
        .input
        .as_ref()
        .filter(|input| input.content_type.is_some())
        .and_then(|input| input.json_schema.clone());
    let output_json_schema = handler
        .output
        .as_ref()
        .and_then(|output| output.json_schema.clone());

    Ok(NewHandler {
        target_ty,
        public: handler.options.ingress_private.map(|private| !private),
        workflow_completion_retention: handler.workflow_completion_retention,
        input: handler
            .input
            .map(|input| build_input(&target, input))
            .transpose()?,
        output: handler
            .output
            .map(|output| build_output(&target, output))
            .transpose()?,
        options: build_options(&target, handler.options)?,
        docs: Docs {
            documentation: handler.documentation,
            metadata: handler.metadata,
        },
        input_json_schema,
        output_json_schema,
        name: handler.name,
    })
}

fn build_input(target: &str, input: ManifestInput) -> Result<InputPayload, Error> {
    if let Some(content_type) = &input.content_type {
        validate_input_content_type(content_type).map_err(|reason| {
            Error::bad_request(format!(
                "handler '{target}': can't parse the input content-type '{content_type}': {reason}"
            ))
        })?;
    }
    Ok(InputPayload {
        required: input.required.unwrap_or(false),
        // Like in the runtime, the JSON schema is ignored without a content-type.
        json: input.content_type.is_some() && input.json_schema.is_some(),
        content_type: input.content_type,
    })
}

/// The rules of the runtime's `InputContentType::from_str`.
fn validate_input_content_type(content_type: &str) -> Result<(), &'static str> {
    match content_type.trim().split_once('/') {
        None => Err("must contain one /"),
        Some(("*", sub_type)) if sub_type != "*" => {
            Err("the wildcard format */subType is not supported")
        }
        Some(_) => Ok(()),
    }
}

fn build_output(target: &str, output: ManifestOutput) -> Result<OutputPayload, Error> {
    if let Some(content_type) = &output.content_type {
        HeaderValue::from_str(content_type).map_err(|e| {
            Error::bad_request(format!(
                "handler '{target}': bad output content-type '{content_type}': {e}"
            ))
        })?;
    }
    Ok(OutputPayload {
        content_type: output.content_type,
        set_content_type_if_empty: output.set_content_type_if_empty.unwrap_or(false),
    })
}

fn build_options(target: &str, options: ManifestOptions) -> Result<Options, Error> {
    for duration in [
        options.inactivity_timeout,
        options.abort_timeout,
        options.journal_retention,
        options.idempotency_retention,
        options.retry_policy_initial_interval,
        options.retry_policy_max_interval,
    ]
    .into_iter()
    .flatten()
    {
        check_duration(target, duration)?;
    }

    Ok(Options {
        idempotency_retention: options.idempotency_retention,
        journal_retention: options.journal_retention,
        inactivity_timeout: options.inactivity_timeout,
        abort_timeout: options.abort_timeout,
        state_preload_policy: state_preload_policy(
            options.enable_lazy_state,
            options.eager_state_keys_whitelist,
        ),
        retry_initial_interval: options.retry_policy_initial_interval,
        retry_exponentiation_factor: options.retry_policy_exponentiation_factor,
        // Restate wants at least one attempt.
        retry_max_attempts: options.retry_policy_max_attempts.map(|n| n.max(1)),
        retry_max_interval: options.retry_policy_max_interval,
        retry_on_max_attempts: options.retry_policy_on_max_attempts.map(|on| match on {
            ManifestOnMaxAttempts::Pause => OnMaxAttempts::Pause,
            ManifestOnMaxAttempts::Kill => OnMaxAttempts::Kill,
        }),
    })
}

/// The runtime's `state_preload_policy_discovery`, with empty keys dropped and duplicates removed.
fn state_preload_policy(
    enable_lazy_state: Option<bool>,
    eager_state_keys: Vec<String>,
) -> Option<StatePreloadPolicy> {
    let mut seen = HashSet::new();
    let eager_state_keys: Vec<String> = eager_state_keys
        .into_iter()
        .filter(|key| !key.is_empty() && seen.insert(key.clone()))
        .collect();

    match enable_lazy_state {
        Some(false) => Some(StatePreloadPolicy::All),
        Some(true) => Some(StatePreloadPolicy::Partial(eager_state_keys)),
        None if !eager_state_keys.is_empty() => Some(StatePreloadPolicy::Partial(eager_state_keys)),
        None => None,
    }
}

/// Durations Restate can't print or parse are rejected here, not at lookup time.
fn check_duration(target: &str, millis: u64) -> Result<(), Error> {
    format_duration(Duration::from_millis(millis))
        .map(|_| ())
        .map_err(|e| {
            Error::bad_request(format!("'{target}': duration {millis}ms is too large: {e}"))
        })
}

/// What a `RestateDeploymentEndpoint`'s status stores of a manifest: the lookup projection.
#[derive(Debug, Clone, PartialEq)]
pub struct Projection {
    pub protocol_type: crd::ProtocolType,
    pub http_version: crd::HttpVersion,
    pub min_protocol_version: i32,
    pub max_protocol_version: i32,
    /// In the manifest's order. The revisions are 0: the caller assigns them.
    pub services: Vec<crd::ProjectedService>,
}

/// Parses a `/discover` response body and projects it, with the registry's checks (`build`):
/// `protocolMode` set, a protocol range intersecting 5..=7, no duplicate service or handler,
/// handler types allowed by their service type, durations Restate can print, valid input/output
/// content-types, plus at least one service. `address` and `additional_headers` are validated like
/// the registry's add request (an absolute http(s) URI, valid header names and values).
pub fn project(
    manifest_json: &[u8],
    address: &str,
    additional_headers: &BTreeMap<String, String>,
    use_http11: bool,
) -> Result<Projection, ManifestError> {
    let endpoint: Endpoint = serde_json::from_slice(manifest_json)?;
    if endpoint.services.is_empty() {
        return Err(Error::bad_request("the manifest has no services"));
    }
    let deployment = build(address, additional_headers, use_http11, None, endpoint)?;
    Ok(Projection {
        protocol_type: match deployment.protocol_type {
            ProtocolType::BidiStream => crd::ProtocolType::BidiStream,
            ProtocolType::RequestResponse => crd::ProtocolType::RequestResponse,
        },
        http_version: match deployment.http_version {
            HttpVersion::Http11 => crd::HttpVersion::Http11,
            HttpVersion::Http2 => crd::HttpVersion::Http2,
        },
        min_protocol_version: deployment.min_protocol_version,
        max_protocol_version: deployment.max_protocol_version,
        services: deployment
            .services
            .into_iter()
            .map(project_service)
            .collect(),
    })
}

fn project_service(service: NewService) -> crd::ProjectedService {
    crd::ProjectedService {
        name: service.name,
        ty: match service.ty {
            ServiceType::Service => crd::ServiceType::Service,
            ServiceType::VirtualObject => crd::ServiceType::VirtualObject,
            ServiceType::Workflow => crd::ServiceType::Workflow,
        },
        revision: 0,
        public: service.public,
        options: project_options(service.options),
        handlers: service
            .handlers
            .into_iter()
            .map(|handler| crd::ProjectedHandler {
                name: handler.name,
                target_ty: match handler.target_ty {
                    TargetType::Service => crd::TargetType::Service,
                    TargetType::VirtualObjectExclusive => crd::TargetType::VirtualObjectExclusive,
                    TargetType::VirtualObjectShared => crd::TargetType::VirtualObjectShared,
                    TargetType::Workflow => crd::TargetType::Workflow,
                    TargetType::WorkflowShared => crd::TargetType::WorkflowShared,
                },
                public: handler.public,
                workflow_completion_retention: handler.workflow_completion_retention,
                options: project_options(handler.options),
                input: handler.input.map(|input| crd::InputRules {
                    required: input.required,
                    content_type: input.content_type,
                    json: input.json,
                }),
                output: handler.output.map(|output| crd::OutputRules {
                    content_type: output.content_type,
                    set_content_type_if_empty: output.set_content_type_if_empty,
                }),
            })
            .collect(),
    }
}

fn project_options(options: Options) -> crd::ProjectedOptions {
    crd::ProjectedOptions {
        idempotency_retention: options.idempotency_retention,
        journal_retention: options.journal_retention,
        inactivity_timeout: options.inactivity_timeout,
        abort_timeout: options.abort_timeout,
        state_preload: options.state_preload_policy.map(|policy| match policy {
            StatePreloadPolicy::All => crd::StatePreload {
                lazy: false,
                keys: vec![],
            },
            StatePreloadPolicy::Partial(keys) => crd::StatePreload { lazy: true, keys },
        }),
        retry_initial_interval: options.retry_initial_interval,
        retry_exponentiation_factor: options.retry_exponentiation_factor,
        retry_max_attempts: options.retry_max_attempts,
        retry_max_interval: options.retry_max_interval,
        retry_on_max_attempts: options.retry_on_max_attempts.map(|on| {
            match on {
                OnMaxAttempts::Pause => crd::ON_MAX_ATTEMPTS_PAUSE,
                OnMaxAttempts::Kill => crd::ON_MAX_ATTEMPTS_KILL,
            }
            .to_owned()
        }),
    }
}

/// What the per-scope read API shows and the projection doesn't store, out of a manifest: the docs
/// and metadata of the services and handlers, and the handlers' JSON schemas.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ManifestDetails {
    pub services: HashMap<String, ServiceDetails>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ServiceDetails {
    pub documentation: Option<String>,
    pub metadata: BTreeMap<String, String>,
    pub handlers: HashMap<String, HandlerDetails>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct HandlerDetails {
    pub documentation: Option<String>,
    pub metadata: BTreeMap<String, String>,
    /// Like in the runtime (and `build`), only with an input content-type.
    pub input_json_schema: Option<serde_json::Value>,
    pub output_json_schema: Option<serde_json::Value>,
}

/// The [`ManifestDetails`] of a `/discover` response body. Not validated beyond parsing: the
/// endpoint was validated when it was discovered, these are display values.
pub fn details(manifest_json: &[u8]) -> Result<ManifestDetails, ManifestError> {
    let endpoint: Endpoint = serde_json::from_slice(manifest_json)?;
    Ok(ManifestDetails {
        services: endpoint
            .services
            .into_iter()
            .map(|service| {
                let handlers = service
                    .handlers
                    .into_iter()
                    .map(|handler| {
                        let input_json_schema = handler
                            .input
                            .filter(|input| input.content_type.is_some())
                            .and_then(|input| input.json_schema);
                        let output_json_schema =
                            handler.output.and_then(|output| output.json_schema);
                        (
                            handler.name,
                            HandlerDetails {
                                documentation: handler.documentation,
                                metadata: handler.metadata,
                                input_json_schema,
                                output_json_schema,
                            },
                        )
                    })
                    .collect();
                (
                    service.name,
                    ServiceDetails {
                        documentation: service.documentation,
                        metadata: service.metadata,
                        handlers,
                    },
                )
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::{Value, json};

    fn endpoint(services: Value) -> Endpoint {
        serde_json::from_value(json!({
            "protocolMode": "BIDI_STREAM",
            "minProtocolVersion": 5,
            "maxProtocolVersion": 7,
            "services": services,
        }))
        .unwrap()
    }

    fn build_endpoint(endpoint: Endpoint) -> std::result::Result<NewDeployment, Error> {
        build(
            "http://localhost:9080",
            &BTreeMap::new(),
            false,
            None,
            endpoint,
        )
    }

    fn build_handler_json(
        service_ty: &str,
        handler: Value,
    ) -> std::result::Result<NewHandler, Error> {
        let deployment = build_endpoint(endpoint(json!([
            {"name": "Svc", "ty": service_ty, "handlers": [handler]}
        ])))?;
        Ok(deployment.services[0].handlers[0].clone())
    }

    fn target_ty(
        service_ty: &str,
        handler_ty: Option<&str>,
    ) -> std::result::Result<TargetType, Error> {
        let mut handler = json!({"name": "h"});
        if let Some(handler_ty) = handler_ty {
            handler["ty"] = json!(handler_ty);
        }
        build_handler_json(service_ty, handler).map(|handler| handler.target_ty)
    }

    fn service_options(service: Value) -> Options {
        build_endpoint(endpoint(json!([service]))).unwrap().services[0]
            .options
            .clone()
    }

    fn manifest_versions(min: u64, max: u64) -> std::result::Result<NewDeployment, Error> {
        build_endpoint(
            serde_json::from_value(json!({
                "protocolMode": "REQUEST_RESPONSE",
                "minProtocolVersion": min,
                "maxProtocolVersion": max,
                "services": [],
            }))
            .unwrap(),
        )
    }

    #[test]
    fn service_handler_is_service() {
        assert_eq!(target_ty("SERVICE", None).unwrap(), TargetType::Service);
    }

    #[test]
    fn virtual_object_handler_defaults_to_exclusive() {
        assert_eq!(
            target_ty("VIRTUAL_OBJECT", None).unwrap(),
            TargetType::VirtualObjectExclusive
        );
    }

    #[test]
    fn virtual_object_shared_handler() {
        assert_eq!(
            target_ty("VIRTUAL_OBJECT", Some("SHARED")).unwrap(),
            TargetType::VirtualObjectShared
        );
    }

    #[test]
    fn workflow_handler_defaults_to_shared() {
        assert_eq!(
            target_ty("WORKFLOW", None).unwrap(),
            TargetType::WorkflowShared
        );
    }

    #[test]
    fn workflow_run_handler() {
        assert_eq!(
            target_ty("WORKFLOW", Some("WORKFLOW")).unwrap(),
            TargetType::Workflow
        );
    }

    #[test]
    fn exclusive_handler_in_service_is_rejected() {
        assert!(matches!(
            target_ty("SERVICE", Some("EXCLUSIVE")),
            Err(ManifestError::Invalid(_))
        ));
    }

    #[test]
    fn no_input_accepts_anything() {
        assert_eq!(
            build_handler_json("SERVICE", json!({"name": "h"}))
                .unwrap()
                .input,
            None
        );
    }

    #[test]
    fn json_input() {
        assert_eq!(build_handler_json(
                "SERVICE",
                json!({"name": "h", "input": {
                    "required": true, "contentType": "application/json", "jsonSchema": {"type": "string"}
                }})
            )
            .unwrap()
            .input, Some(InputPayload {
                required: true,
                content_type: Some("application/json".to_owned()),
                json: true,
            }));
    }

    #[test]
    fn json_schema_without_content_type_is_not_json() {
        assert_eq!(
            build_handler_json(
                "SERVICE",
                json!({"name": "h", "input": {"jsonSchema": {"type": "string"}}})
            )
            .unwrap()
            .input,
            Some(InputPayload {
                required: false,
                content_type: None,
                json: false,
            })
        );
    }

    #[test]
    fn wildcard_type_input_content_type_is_rejected() {
        assert!(matches!(
            build_handler_json(
                "SERVICE",
                json!({"name": "h", "input": {"contentType": "*/json"}})
            ),
            Err(ManifestError::Invalid(_))
        ));
    }

    #[test]
    fn output() {
        assert_eq!(build_handler_json(
                "SERVICE",
                json!({"name": "h", "output": {"contentType": "text/plain", "setContentTypeIfEmpty": true}})
            )
            .unwrap()
            .output, Some(OutputPayload {
                content_type: Some("text/plain".to_owned()),
                set_content_type_if_empty: true,
            }));
    }

    #[test]
    fn handler_ingress_private_is_not_public() {
        assert_eq!(
            build_handler_json("SERVICE", json!({"name": "h", "ingressPrivate": true}))
                .unwrap()
                .public,
            Some(false)
        );
    }

    #[test]
    fn eager_state_disables_lazy_state() {
        assert_eq!(
            service_options(json!({"name": "Svc", "ty": "SERVICE", "enableLazyState": false}))
                .state_preload_policy,
            Some(StatePreloadPolicy::All)
        );
    }

    #[test]
    fn lazy_state_keeps_unique_non_empty_keys() {
        assert_eq!(
            service_options(json!({
                "name": "Svc", "ty": "SERVICE", "enableLazyState": true,
                "eagerStateKeysWhitelist": ["a", "", "b", "a"]
            }))
            .state_preload_policy,
            Some(StatePreloadPolicy::Partial(vec![
                "a".to_owned(),
                "b".to_owned()
            ]))
        );
    }

    #[test]
    fn eager_keys_without_flag_are_partial() {
        assert_eq!(
            service_options(
                json!({"name": "Svc", "ty": "SERVICE", "eagerStateKeysWhitelist": ["a"]})
            )
            .state_preload_policy,
            Some(StatePreloadPolicy::Partial(vec!["a".to_owned()]))
        );
    }

    #[test]
    fn no_state_settings_is_unset() {
        assert_eq!(
            service_options(json!({"name": "Svc", "ty": "SERVICE"})).state_preload_policy,
            None
        );
    }

    #[test]
    fn zero_max_attempts_becomes_one() {
        assert_eq!(
            service_options(json!({"name": "Svc", "ty": "SERVICE", "retryPolicyMaxAttempts": 0}))
                .retry_max_attempts,
            Some(1)
        );
    }

    #[test]
    fn too_large_duration_is_rejected() {
        assert!(matches!(
            build_endpoint(endpoint(json!([
                {"name": "Svc", "ty": "SERVICE", "inactivityTimeout": u64::MAX}
            ]))),
            Err(ManifestError::Invalid(_))
        ));
    }

    #[test]
    fn duplicate_services_are_rejected() {
        assert!(matches!(
            build_endpoint(endpoint(json!([
                {"name": "Svc", "ty": "SERVICE"},
                {"name": "Svc", "ty": "SERVICE"}
            ]))),
            Err(ManifestError::Invalid(_))
        ));
    }

    #[test]
    fn duplicate_handlers_are_rejected() {
        assert!(matches!(
            build_endpoint(endpoint(json!([
                {"name": "Svc", "ty": "SERVICE", "handlers": [{"name": "h"}, {"name": "h"}]}
            ]))),
            Err(ManifestError::Invalid(_))
        ));
    }

    #[test]
    fn missing_protocol_mode_is_rejected() {
        assert!(matches!(
            build_endpoint(
                serde_json::from_value(json!({
                    "minProtocolVersion": 5, "maxProtocolVersion": 7, "services": []
                }))
                .unwrap()
            ),
            Err(ManifestError::Invalid(_))
        ));
    }

    #[test]
    fn old_protocol_versions_are_rejected() {
        assert!(matches!(
            manifest_versions(1, 4),
            Err(ManifestError::Invalid(_))
        ));
    }

    #[test]
    fn min_above_max_is_rejected() {
        assert!(matches!(
            manifest_versions(6, 5),
            Err(ManifestError::Invalid(_))
        ));
    }

    #[test]
    fn overlapping_protocol_versions_are_accepted() {
        assert!(manifest_versions(5, 9).is_ok());
    }

    #[test]
    fn address_is_normalized() {
        assert_eq!(
            normalize_address("http://localhost:9080").unwrap(),
            "http://localhost:9080/"
        );
    }

    #[test]
    fn relative_uri_is_rejected() {
        assert!(matches!(
            normalize_address("/foo"),
            Err(ManifestError::Invalid(_))
        ));
    }

    #[test]
    fn header_names_are_lowercased() {
        assert_eq!(
            validate_header("X-Foo", "bar").unwrap(),
            ("x-foo".to_owned(), "bar".to_owned())
        );
    }

    fn project_json(manifest: Value) -> std::result::Result<Projection, ManifestError> {
        project(
            manifest.to_string().as_bytes(),
            "http://localhost:9080",
            &BTreeMap::new(),
            false,
        )
    }

    #[test]
    fn projection() {
        let projection = project_json(json!({
            "protocolMode": "REQUEST_RESPONSE",
            "minProtocolVersion": 5,
            "maxProtocolVersion": 7,
            "services": [{
                "name": "Counter",
                "ty": "VIRTUAL_OBJECT",
                "documentation": "not projected",
                "enableLazyState": true,
                "eagerStateKeysWhitelist": ["a"],
                "handlers": [
                    {"name": "add", "ingressPrivate": true, "retryPolicyOnMaxAttempts": "KILL",
                     "input": {"contentType": "application/json", "jsonSchema": {}}},
                    {"name": "get", "ty": "SHARED", "enableLazyState": false},
                ],
            }],
        }))
        .unwrap();

        assert_eq!(projection.protocol_type, crd::ProtocolType::RequestResponse);
        assert_eq!(projection.http_version, crd::HttpVersion::Http2);
        assert_eq!(
            serde_json::to_value(&projection.services).unwrap(),
            json!([{
                "name": "Counter",
                "ty": "VirtualObject",
                "revision": 0,
                "public": true,
                "options": {"statePreload": {"lazy": true, "keys": ["a"]}},
                "handlers": [
                    {
                        "name": "add",
                        "targetTy": "VirtualObjectExclusive",
                        "public": false,
                        "options": {"retryOnMaxAttempts": "Kill"},
                        "input": {"required": false, "contentType": "application/json", "json": true},
                    },
                    {
                        "name": "get",
                        "targetTy": "VirtualObjectShared",
                        "options": {"statePreload": {}},
                    },
                ],
            }])
        );
    }

    #[test]
    fn a_manifest_without_services_is_rejected() {
        assert!(matches!(
            project_json(json!({
                "protocolMode": "BIDI_STREAM", "minProtocolVersion": 5, "maxProtocolVersion": 7,
                "services": []
            })),
            Err(ManifestError::Invalid(_))
        ));
    }

    #[test]
    fn a_body_that_isnt_a_manifest_is_rejected() {
        assert!(matches!(
            project(
                b"not json",
                "http://localhost:9080",
                &BTreeMap::new(),
                false
            ),
            Err(ManifestError::Parse(_))
        ));
    }

    #[test]
    fn a_bad_address_is_rejected() {
        assert!(matches!(
            project(
                manifest_bytes().as_slice(),
                "localhost:9080",
                &BTreeMap::new(),
                false
            ),
            Err(ManifestError::Invalid(_))
        ));
    }

    fn manifest_bytes() -> Vec<u8> {
        json!({
            "protocolMode": "BIDI_STREAM", "minProtocolVersion": 5, "maxProtocolVersion": 7,
            "services": [{"name": "Svc", "ty": "SERVICE"}]
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn http11_projection() {
        let projection = project(
            manifest_bytes().as_slice(),
            "http://localhost:9080",
            &BTreeMap::new(),
            true,
        )
        .unwrap();

        assert_eq!(projection.http_version, crd::HttpVersion::Http11);
    }

    #[test]
    fn details_have_the_docs_and_the_schemas() {
        let details = details(
            json!({
                "protocolMode": "BIDI_STREAM", "minProtocolVersion": 5, "maxProtocolVersion": 7,
                "services": [{
                    "name": "Svc", "ty": "SERVICE", "documentation": "A service",
                    "metadata": {"restate.dev/integration": "tanstack-workflow"},
                    "handlers": [
                        {"name": "run", "documentation": "Runs",
                         "input": {"contentType": "application/json", "jsonSchema": {"type": "object"}},
                         "output": {"jsonSchema": {"type": "string"}}},
                        {"name": "untyped", "input": {"jsonSchema": {"type": "object"}}},
                    ],
                }],
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap();

        let service = &details.services["Svc"];
        assert_eq!(service.documentation.as_deref(), Some("A service"));
        assert_eq!(
            service.metadata["restate.dev/integration"],
            "tanstack-workflow"
        );
        assert_eq!(
            service.handlers["run"].documentation.as_deref(),
            Some("Runs")
        );
        assert_eq!(
            service.handlers["run"].input_json_schema,
            Some(json!({"type": "object"}))
        );
        assert_eq!(
            service.handlers["run"].output_json_schema,
            Some(json!({"type": "string"}))
        );
        // Like the runtime: no input schema without a content-type.
        assert_eq!(service.handlers["untyped"].input_json_schema, None);
    }

    #[test]
    fn discover_accepts_the_runtimes_versions_highest_first() {
        let versions: Vec<&str> = DISCOVER_ACCEPT.split(", ").collect();
        assert_eq!(versions.len(), 5);
        assert_eq!(
            versions[0],
            "application/vnd.restate.endpointmanifest.v5+json"
        );
        assert_eq!(
            versions[4],
            "application/vnd.restate.endpointmanifest.v1+json"
        );
    }
}
