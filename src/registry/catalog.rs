//! The per-scope read API's answers (`02-contract.md` §4.1): the index's views in the admin API's
//! shapes (`admin.rs`), plus the docs, metadata and JSON schemas of the live details when they
//! could be fetched. The rendering of the schema registry's `catalog.rs`, over the index.

use std::collections::BTreeMap;
use std::time::Duration;

use super::admin::{
    self, DEFAULT_ABORT_TIMEOUT, DEFAULT_IDEMPOTENCY_RETENTION, DEFAULT_INACTIVITY_TIMEOUT,
    DEFAULT_RETRY_EXPONENTIATION_FACTOR, DEFAULT_RETRY_INITIAL_INTERVAL, DeploymentResponse,
    DeploymentType, HandlerMetadata, HandlerRetryPolicyMetadata, HandlerType, ServiceMetadata,
    ServiceNameRevPair, ServiceRetryPolicyMetadata, ServiceType,
};
use super::api::{
    self, Friendly, HttpVersion, InputPayload, OutputPayload, ProtocolType, TargetType,
};
use super::index::{DeploymentView, HandlerShape, Opts, ServiceView, encode_deployment_id};
use super::manifest::{self, HandlerDetails, ManifestDetails, ServiceDetails};

/// A deployment in the admin API's shape, with `services` already rendered.
pub fn deployment<S>(deployment: &DeploymentView, services: Vec<S>) -> DeploymentResponse<S> {
    DeploymentResponse {
        ty: DeploymentType::Http,
        id: encode_deployment_id(deployment.id),
        uri: deployment.address.clone(),
        protocol_type: protocol_type(deployment.protocol_type),
        http_version: match deployment.http_version {
            HttpVersion::Http11 => "HTTP/1.1",
            HttpVersion::Http2 => "HTTP/2.0",
        },
        additional_headers: deployment.headers.clone(),
        // Every deployment is a tenant's own here (nothing is shared).
        metadata: BTreeMap::from([("tenant".to_owned(), deployment.tenant.clone())]),
        created_at: jiff::Timestamp::from_millisecond(deployment.created_at)
            .unwrap_or(jiff::Timestamp::UNIX_EPOCH)
            .to_string(),
        min_protocol_version: deployment.min_protocol_version,
        max_protocol_version: deployment.max_protocol_version,
        sdk_version: deployment.sdk_version.clone(),
        services,
    }
}

pub fn name_revision(service: &ServiceView) -> ServiceNameRevPair {
    ServiceNameRevPair {
        name: service.name.clone(),
        revision: service.revision,
    }
}

/// `details`: the deployment's live details, `None` if they couldn't be fetched.
pub fn service(
    deployment: &DeploymentView,
    service: &ServiceView,
    details: Option<&ManifestDetails>,
) -> ServiceMetadata {
    let shape = &service.shape;
    let details: Option<&ServiceDetails> =
        details.and_then(|details| details.services.get(&service.name));
    let request_response = deployment.protocol_type == ProtocolType::RequestResponse;
    let options = shape.options.as_deref().cloned().unwrap_or_default();
    let workflow_completion_retention = shape
        .handlers
        .iter()
        .find(|handler| handler.target_ty == TargetType::Workflow)
        .and_then(|handler| friendly(handler.workflow_completion_retention));
    let state_preload_policy = options
        .state_preload_policy
        .clone()
        .map(state_preload_policy);

    ServiceMetadata {
        name: service.name.clone(),
        ty: match shape.ty {
            manifest::ServiceType::Service => ServiceType::Service,
            manifest::ServiceType::VirtualObject => ServiceType::VirtualObject,
            manifest::ServiceType::Workflow => ServiceType::Workflow,
        },
        handlers: shape
            .handlers
            .iter()
            .map(|handler| {
                handler_metadata(
                    handler,
                    shape.public,
                    request_response,
                    details.and_then(|details| details.handlers.get(&*handler.name)),
                )
            })
            .collect(),
        documentation: details.and_then(|details| details.documentation.clone()),
        metadata: details
            .map(|details| details.metadata.clone())
            .unwrap_or_default(),
        deployment_id: encode_deployment_id(deployment.id),
        revision: service.revision,
        public: shape.public,
        idempotency_retention: Friendly(
            options
                .idempotency_retention
                .map_or(DEFAULT_IDEMPOTENCY_RETENTION, Duration::from_millis),
        ),
        workflow_completion_retention,
        journal_retention: friendly(options.journal_retention),
        inactivity_timeout: Friendly(
            options
                .inactivity_timeout
                .map_or(DEFAULT_INACTIVITY_TIMEOUT, Duration::from_millis),
        ),
        abort_timeout: Friendly(
            options
                .abort_timeout
                .map_or(DEFAULT_ABORT_TIMEOUT, Duration::from_millis),
        ),
        enable_lazy_state: is_lazy(state_preload_policy.as_ref()),
        state_preload_policy: state_preload_policy.unwrap_or_default(),
        retry_policy: ServiceRetryPolicyMetadata {
            initial_interval: Friendly(
                options
                    .retry_initial_interval
                    .map_or(DEFAULT_RETRY_INITIAL_INTERVAL, Duration::from_millis),
            ),
            exponentiation_factor: options
                .retry_exponentiation_factor
                .map_or(DEFAULT_RETRY_EXPONENTIATION_FACTOR, |f| f.0 as f32),
            max_attempts: options.retry_max_attempts,
            max_interval: friendly(options.retry_max_interval),
            on_max_attempts: options
                .retry_on_max_attempts
                .map(on_max_attempts)
                .unwrap_or_default(),
        },
    }
}

fn handler_metadata(
    handler: &HandlerShape,
    service_public: bool,
    request_response: bool,
    details: Option<&HandlerDetails>,
) -> HandlerMetadata {
    let options: Opts = handler.options.as_deref().cloned().unwrap_or_default();
    let input_json_schema = details.and_then(|details| details.input_json_schema.clone());
    let output_json_schema = details.and_then(|details| details.output_json_schema.clone());
    HandlerMetadata {
        name: handler.name.to_string(),
        ty: match handler.target_ty {
            TargetType::Service => None,
            TargetType::VirtualObjectExclusive => Some(HandlerType::Exclusive),
            TargetType::VirtualObjectShared | TargetType::WorkflowShared => {
                Some(HandlerType::Shared)
            }
            TargetType::Workflow => Some(HandlerType::Workflow),
        },
        documentation: details.and_then(|details| details.documentation.clone()),
        metadata: details
            .map(|details| details.metadata.clone())
            .unwrap_or_default(),
        idempotency_retention: friendly(options.idempotency_retention),
        journal_retention: friendly(options.journal_retention),
        // Like the runtime: no inactivity timeout for request/response deployments.
        inactivity_timeout: friendly(options.inactivity_timeout).filter(|_| !request_response),
        abort_timeout: friendly(options.abort_timeout),
        state_preload_policy: options
            .state_preload_policy
            .clone()
            .map(state_preload_policy),
        public: handler.public.unwrap_or(service_public),
        input_description: input_description(handler.input.as_deref(), input_json_schema.as_ref()),
        output_description: output_description(
            handler.output.as_deref(),
            output_json_schema.as_ref(),
        ),
        input_json_schema,
        output_json_schema,
        retry_policy: HandlerRetryPolicyMetadata {
            initial_interval: friendly(options.retry_initial_interval),
            exponentiation_factor: options.retry_exponentiation_factor.map(|f| f.0 as f32),
            max_attempts: options.retry_max_attempts,
            max_interval: friendly(options.retry_max_interval),
            on_max_attempts: options.retry_on_max_attempts.map(on_max_attempts),
        },
    }
}

fn friendly(millis: Option<u64>) -> Option<Friendly> {
    millis.map(|millis| Friendly(Duration::from_millis(millis)))
}

fn protocol_type(protocol_type: ProtocolType) -> admin::ProtocolType {
    match protocol_type {
        ProtocolType::BidiStream => admin::ProtocolType::BidiStream,
        ProtocolType::RequestResponse => admin::ProtocolType::RequestResponse,
    }
}

fn state_preload_policy(policy: api::StatePreloadPolicy) -> admin::StatePreloadPolicy {
    match policy {
        api::StatePreloadPolicy::All => admin::StatePreloadPolicy::All,
        api::StatePreloadPolicy::Partial(keys) => admin::StatePreloadPolicy::Partial(keys),
    }
}

fn on_max_attempts(on: api::OnMaxAttempts) -> admin::OnMaxAttempts {
    match on {
        api::OnMaxAttempts::Pause => admin::OnMaxAttempts::Pause,
        api::OnMaxAttempts::Kill => admin::OnMaxAttempts::Kill,
    }
}

/// Fully lazy state: what `enableLazyState` in the manifest turns into.
fn is_lazy(policy: Option<&admin::StatePreloadPolicy>) -> bool {
    matches!(policy, Some(admin::StatePreloadPolicy::Partial(keys)) if keys.is_empty())
}

/// The runtime's `InputRules` display, from what the registry kept of them. No input in the
/// manifest is the runtime's default (any content-type, or none); an input without a content-type
/// accepts only an empty body.
fn input_description(input: Option<&InputPayload>, schema: Option<&serde_json::Value>) -> String {
    match input {
        None => "*/*".to_owned(),
        Some(InputPayload {
            content_type: None, ..
        }) => "none".to_owned(),
        Some(InputPayload {
            content_type: Some(content_type),
            json,
            ..
        }) => {
            let content_type = content_type.trim();
            if *json {
                json_description(schema).unwrap_or_else(|| content_type.to_owned())
            } else {
                content_type.to_owned()
            }
        }
    }
}

/// The runtime's `OutputRules` display. No output in the manifest is the runtime's default
/// (`application/json`).
fn output_description(
    output: Option<&OutputPayload>,
    schema: Option<&serde_json::Value>,
) -> String {
    if let Some(description) = json_description(schema) {
        return description;
    }
    match output {
        None => "application/json".to_owned(),
        Some(OutputPayload {
            content_type: None, ..
        }) => "none".to_owned(),
        Some(OutputPayload {
            content_type: Some(content_type),
            set_content_type_if_empty,
        }) => {
            if *set_content_type_if_empty {
                format!("optional {content_type}")
            } else {
                content_type.clone()
            }
        }
    }
}

/// The runtime's `try_display_json_detailed_info`: the schema's title, else its type.
fn json_description(schema: Option<&serde_json::Value>) -> Option<String> {
    let schema = schema?;
    if let Some(serde_json::Value::String(title)) = schema.get("title") {
        Some(format!("JSON '{title}'"))
    } else if let Some(serde_json::Value::String(ty)) = schema.get("type") {
        Some(format!("JSON {ty}"))
    } else {
        None
    }
}
