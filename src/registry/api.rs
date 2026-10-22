//! Wire models of the lookup API (v1): what the relay (at preflight, and for callees) and the ingress
//! mux read (`02-contract.md` §4 in the design docs).
//!
//! Copied from the schema registry (`schema-registry/src/api.rs`), itself copied from the runtime
//! (`crates/types/src/schema/lookup/api.rs`) and pruned, plus `DeploymentInfo::sdk_version`: the
//! field names, serde renames and skipped fields must stay as they are there.
//!
//! Served under `/clusters/{cluster}` (one registry per RestateCluster):
//!
//! * `GET /v1/ingestion_lookup?serviceName=&handlerName=[&scope=]` returns
//!   [`IngestionLookupResponse`].
//! * `GET /v1/exec_lookup?serviceName=&handlerName=[&deploymentId=][&scope=]` returns
//!   [`ExecLookupResponse`].
//!
//! A not found answer is a `404` with an [`ErrorResponse`] body. An absent setting means not
//! configured: Restate's server defaults and limits apply.

use std::collections::BTreeMap;
use std::time::Duration;

use jiff::fmt::friendly::{Designator, SpanPrinter};
use jiff::{Span, SpanRound, Unit};
use serde::{Deserialize, Serialize, Serializer};

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct IngestionLookupResponse {
    /// Monotonic version of the registry content.
    pub version: u64,
    pub target_ty: TargetType,
    pub public: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotency_retention: Option<Friendly>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow_completion_retention: Option<Friendly>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub journal_retention: Option<Friendly>,
    /// When not set, any input is accepted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<InputPayload>,
    /// When not set, a non-empty output is returned as `application/json`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<OutputPayload>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetType {
    Service,
    VirtualObjectExclusive,
    VirtualObjectShared,
    Workflow,
    WorkflowShared,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct InputPayload {
    /// If false, an empty body without content-type is accepted too.
    pub required: bool,
    /// Accepted content-type, in the `Accept` header format. When not set, only an empty body is
    /// accepted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// If true, the body must be a non-empty JSON value.
    pub json: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct OutputPayload {
    /// Content-type of the response. When not set, no content-type is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// If true, the content-type is set even when the output is empty.
    pub set_content_type_if_empty: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ExecLookupResponse {
    /// Monotonic version of the registry content.
    pub version: u64,
    pub deployment: DeploymentInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inactivity_timeout: Option<Friendly>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub abort_timeout: Option<Friendly>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_preload_policy: Option<StatePreloadPolicy>,
    pub retry_policy: RetryPolicyOverrides,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct DeploymentInfo {
    pub id: String,
    pub address: String,
    pub protocol_type: ProtocolType,
    pub http_version: HttpVersion,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub additional_headers: BTreeMap<String, String>,
    pub min_protocol_version: i32,
    pub max_protocol_version: i32,
    /// The deployment's SDK version, as inserted. The relay returns it to the runtime for
    /// introspection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sdk_version: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolType {
    BidiStream,
    RequestResponse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HttpVersion {
    #[serde(rename = "1.1")]
    Http11,
    #[serde(rename = "2")]
    Http2,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatePreloadPolicy {
    /// Preload all the state eagerly.
    All,
    /// Preload only these state keys, everything else is fetched lazily.
    Partial(Vec<String>),
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct RetryPolicyOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initial_interval: Option<Friendly>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exponentiation_factor: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_interval: Option<Friendly>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_max_attempts: Option<OnMaxAttempts>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnMaxAttempts {
    Pause,
    Kill,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ErrorResponse {
    pub error: ErrorCode,
}

// The variant names are the wire error codes.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    ServiceNotFound,
    HandlerNotFound,
    DeploymentNotFound,
}

/// A duration serialized like the runtime's `FriendlyDuration` (`restate_util_time`), e.g.
/// `1d 12h 4m 2s`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Friendly(pub Duration);

impl Serialize for Friendly {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let formatted = format_duration(self.0).map_err(serde::ser::Error::custom)?;
        serializer.serialize_str(&formatted)
    }
}

/// Fails for durations too large for a jiff `Span`, which the runtime can't parse either.
pub fn format_duration(duration: Duration) -> Result<String, jiff::Error> {
    static PRINTER: SpanPrinter = SpanPrinter::new().designator(Designator::Compact);

    let span =
        Span::try_from(duration)?.round(SpanRound::new().largest(Unit::Day).days_are_24_hours())?;
    Ok(PRINTER.span_to_string(&span))
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

    #[test]
    fn format_minutes() {
        assert_eq!(
            format_duration(Duration::from_secs(23 * 60)).unwrap(),
            "23m"
        );
    }

    #[test]
    fn format_days_to_seconds() {
        assert_eq!(
            format_duration(Duration::from_secs(129842)).unwrap(),
            "1d 12h 4m 2s"
        );
    }

    #[test]
    fn format_millis() {
        assert_eq!(
            format_duration(Duration::from_millis(1500)).unwrap(),
            "1s 500ms"
        );
    }

    #[test]
    fn format_zero() {
        assert_eq!(format_duration(Duration::ZERO).unwrap(), "0s");
    }

    #[test]
    fn format_too_large_fails() {
        assert!(format_duration(Duration::from_secs(u64::MAX)).is_err());
    }

    #[test]
    fn preload_all_encoding() {
        assert_eq!(
            serde_json::to_value(StatePreloadPolicy::All).unwrap(),
            json!("all")
        );
    }

    #[test]
    fn preload_partial_encoding() {
        assert_eq!(
            serde_json::to_value(StatePreloadPolicy::Partial(vec!["a".to_owned()])).unwrap(),
            json!({"partial": ["a"]})
        );
    }

    #[test]
    fn http_version_encoding() {
        assert_eq!(
            serde_json::to_value(HttpVersion::Http11).unwrap(),
            json!("1.1")
        );
    }

    /// The runtime's sample answer in `crates/types/src/schema/lookup/resolver.rs`.
    #[test]
    fn ingestion_golden() {
        let response = IngestionLookupResponse {
            version: 1,
            target_ty: TargetType::VirtualObjectShared,
            public: true,
            idempotency_retention: None,
            workflow_completion_retention: None,
            journal_retention: None,
            input: None,
            output: None,
        };

        assert_eq!(
            serde_json::to_string(&response).unwrap(),
            r#"{"version":1,"target_ty":"virtual_object_shared","public":true}"#
        );
    }

    /// The runtime's sample answer in `crates/types/src/schema/lookup/resolver.rs`.
    #[test]
    fn exec_golden() {
        let response = ExecLookupResponse {
            version: 1,
            deployment: DeploymentInfo {
                id: "dp_101SZwviYzes2mjOamuMJWw".to_owned(),
                address: "http://localhost:9080/".to_owned(),
                protocol_type: ProtocolType::BidiStream,
                http_version: HttpVersion::Http2,
                additional_headers: BTreeMap::new(),
                min_protocol_version: 5,
                max_protocol_version: 7,
                sdk_version: None,
            },
            inactivity_timeout: None,
            abort_timeout: None,
            state_preload_policy: None,
            retry_policy: RetryPolicyOverrides::default(),
        };

        assert_eq!(
            serde_json::to_string(&response).unwrap(),
            concat!(
                r#"{"version":1,"deployment":{"id":"dp_101SZwviYzes2mjOamuMJWw","#,
                r#""address":"http://localhost:9080/","protocol_type":"bidi_stream","#,
                r#""http_version":"2","min_protocol_version":5,"max_protocol_version":7},"#,
                r#""retry_policy":{}}"#
            )
        );
    }
}
