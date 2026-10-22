//! The schema registry's API tests (`schema-registry/tests/api.rs`, `scoped.rs`), ported: the same
//! deployments, added as `RestateDeploymentEndpoint` watch events instead of `POST /deployments`,
//! answer the same JSON. The registry's shared deployments don't exist here: what its tests add as
//! shared is added to a tenant, and the unscoped cases have their own tests.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use actix_web::body::MessageBody;
use actix_web::dev::{Service, ServiceResponse};
use actix_web::http::StatusCode;
use actix_web::http::header::HeaderMap;
use actix_web::test::{self, TestRequest};
use actix_web::{App, HttpRequest, HttpResponse, HttpServer, web};
use chrono::{TimeZone, Utc};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::runtime::watcher::Event;
use serde_json::{Value, json};

use super::details::DetailsCache;
use super::manifest::{self, DISCOVER_ACCEPT};
use super::server::{configure_lookups, configure_read_api};
use super::{Registry, encode_deployment_id};
use crate::resources::restatedeploymentendpoints::{
    DiscoveredDeployment, EndpointPhase, RestateDeploymentEndpoint, RestateDeploymentEndpointSpec,
    RestateDeploymentEndpointStatus,
};

const CLUSTER: &str = "restate";
const COUNTER_URI: &str = "http://counter:9080";

/// Adds endpoints the way the endpoint controller will: project the manifest, reserve the
/// revisions, write a `Ready` status, and the watch event lands.
struct Harness {
    registry: Registry,
    next_uid: u128,
    objects: HashMap<String, RestateDeploymentEndpoint>,
}

impl Harness {
    /// A registry whose first relist is done (empty).
    fn new() -> Self {
        let registry = Registry::new();
        registry.apply(&Event::Init);
        registry.apply(&Event::InitDone);
        Self::unsynced(registry)
    }

    fn unsynced(registry: Registry) -> Self {
        Self {
            registry,
            next_uid: 0x0194_41f1_be4f_1515_34e9_1b9a_2173_0000,
            objects: HashMap::new(),
        }
    }

    /// The registry's `POST /deployments` body (`uri`, `tenant`, `additional_headers`,
    /// `use_http_11`, `sdk_version`, `manifest`) as an endpoint; returns its deployment id.
    fn add(&mut self, request: Value) -> String {
        let object = self.object(&request);
        let id = object
            .status
            .as_ref()
            .unwrap()
            .deployment_id
            .clone()
            .unwrap();
        self.registry.apply(&Event::Apply(object.clone()));
        self.objects.insert(id.clone(), object);
        id
    }

    fn object(&mut self, request: &Value) -> RestateDeploymentEndpoint {
        self.next_uid += 1;
        let uid = self.next_uid;
        let tenant = request["tenant"].as_str().unwrap_or("acme").to_owned();
        let cluster = request["cluster"].as_str().unwrap_or(CLUSTER).to_owned();
        let address = request["uri"].as_str().unwrap().to_owned();
        let headers: BTreeMap<String, String> =
            serde_json::from_value(request["additional_headers"].clone()).unwrap_or_default();
        let use_http11 = request["use_http_11"].as_bool().unwrap_or(false);
        let projection = manifest::project(
            request["manifest"].to_string().as_bytes(),
            &address,
            &headers,
            use_http11,
        )
        .unwrap();
        let id = encode_deployment_id(uid);
        let names: Vec<String> = projection.services.iter().map(|s| s.name.clone()).collect();
        let revisions = self
            .registry
            .reserve_revisions(&cluster, &tenant, &id, &names);
        let mut services = projection.services;
        for (service, revision) in services.iter_mut().zip(revisions) {
            service.revision = revision;
        }

        let mut object = RestateDeploymentEndpoint::new(
            &format!("endpoint-{}", uid & 0xffff),
            RestateDeploymentEndpointSpec {
                cluster,
                address,
                use_http11: Some(use_http11),
                additional_headers: headers,
            },
        );
        object.metadata.namespace = Some(format!("ns-{tenant}"));
        object.metadata.uid = Some(uuid(uid));
        let created = Utc.with_ymd_and_hms(2026, 10, 21, 10, 0, 0).unwrap()
            + chrono::Duration::seconds((uid & 0xffff) as i64);
        object.metadata.creation_timestamp = Some(Time(created));
        object.status = Some(RestateDeploymentEndpointStatus {
            phase: Some(EndpointPhase::Ready),
            tenant: Some(tenant),
            deployment_id: Some(id),
            discovered: Some(DiscoveredDeployment {
                discovered_at: Time(created),
                protocol_type: projection.protocol_type,
                http_version: projection.http_version,
                min_protocol_version: projection.min_protocol_version,
                max_protocol_version: projection.max_protocol_version,
                sdk_version: request["sdk_version"].as_str().map(str::to_owned),
                services,
            }),
            ..Default::default()
        });
        object
    }

    fn delete(&mut self, id: &str) {
        let object = self.objects.remove(id).unwrap();
        self.registry.apply(&Event::Delete(object));
    }
}

async fn lookups_app(
    registry: Registry,
) -> impl Service<
    actix_http::Request,
    Response = ServiceResponse<impl MessageBody>,
    Error = actix_web::Error,
> {
    test::init_service(App::new().configure(configure_lookups(registry))).await
}

async fn read_api_app(
    registry: Registry,
    details: Arc<DetailsCache>,
) -> impl Service<
    actix_http::Request,
    Response = ServiceResponse<impl MessageBody>,
    Error = actix_web::Error,
> {
    test::init_service(App::new().configure(configure_read_api(registry, details))).await
}

fn uuid(uid: u128) -> String {
    let hex = format!("{uid:032x}");
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn details_cache() -> Arc<DetailsCache> {
    Arc::new(DetailsCache::new(100, std::time::Duration::from_secs(2)))
}

async fn send<S, B>(app: &S, request: TestRequest) -> (StatusCode, HeaderMap, Value)
where
    S: Service<actix_http::Request, Response = ServiceResponse<B>, Error = actix_web::Error>,
    B: MessageBody,
{
    let response = test::call_service(app, request.to_request()).await;
    let status = response.status();
    let headers = response.headers().clone();
    let body = test::read_body(response).await;
    (
        status,
        headers,
        serde_json::from_slice(&body).unwrap_or(Value::Null),
    )
}

/// `GET /clusters/restate<path>`.
async fn get<S, B>(app: &S, path: &str) -> (StatusCode, Value)
where
    S: Service<actix_http::Request, Response = ServiceResponse<B>, Error = actix_web::Error>,
    B: MessageBody,
{
    let (status, _, body) = send(
        app,
        TestRequest::get().uri(&format!("/clusters/{CLUSTER}{path}")),
    )
    .await;
    (status, body)
}

fn manifest(services: Value) -> Value {
    json!({
        "protocolMode": "BIDI_STREAM",
        "minProtocolVersion": 5,
        "maxProtocolVersion": 7,
        "services": services,
    })
}

fn counter() -> Value {
    json!({
        "name": "Counter",
        "ty": "VIRTUAL_OBJECT",
        "handlers": [{"name": "add"}, {"name": "get", "ty": "SHARED"}],
    })
}

fn greeter() -> Value {
    json!({"name": "Greeter", "ty": "SERVICE", "handlers": [{"name": "greet"}]})
}

fn registration(uri: &str, services: Value) -> Value {
    json!({"uri": uri, "manifest": manifest(services)})
}

fn tenant_registration(tenant: &str, uri: &str, services: Value) -> Value {
    json!({"uri": uri, "tenant": tenant, "manifest": manifest(services)})
}

async fn exec_deployment<S, B>(app: &S, query: &str) -> Value
where
    S: Service<actix_http::Request, Response = ServiceResponse<B>, Error = actix_web::Error>,
    B: MessageBody,
{
    let (_, body) = get(
        app,
        &format!("/v1/exec_lookup?serviceName=Counter&handlerName=add&scope=acme{query}"),
    )
    .await;
    body["deployment"]["id"].clone()
}

// --- Lookups (`schema-registry/tests/api.rs`) ------------------------------------------------

#[actix_web::test]
async fn ingestion_lookup() {
    let mut h = Harness::new();
    h.add(registration(
        COUNTER_URI,
        json!([{
            "name": "Counter",
            "ty": "VIRTUAL_OBJECT",
            "idempotencyRetention": 86400000,
            "handlers": [{
                "name": "add",
                "journalRetention": 3600000,
                "input": {"required": true, "contentType": "application/json", "jsonSchema": {}},
                "output": {"contentType": "application/json"},
            }],
        }]),
    ));
    let app = lookups_app(h.registry.clone()).await;

    let (status, body) = get(
        &app,
        "/v1/ingestion_lookup?serviceName=Counter&handlerName=add&scope=acme",
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({
            "version": 1,
            "target_ty": "virtual_object_exclusive",
            "public": true,
            "idempotency_retention": "1d",
            "journal_retention": "1h",
            "input": {"required": true, "content_type": "application/json", "json": true},
            "output": {"content_type": "application/json", "set_content_type_if_empty": false},
        })
    );
}

#[actix_web::test]
async fn exec_lookup() {
    let mut h = Harness::new();
    let id = h.add(json!({
        "uri": COUNTER_URI,
        "use_http_11": true,
        "additional_headers": {"X-Env": "demo"},
        "manifest": manifest(json!([{
            "name": "Counter",
            "ty": "VIRTUAL_OBJECT",
            "abortTimeout": 600000,
            "enableLazyState": false,
            "handlers": [{"name": "add", "retryPolicyOnMaxAttempts": "KILL"}],
        }])),
    }));
    let app = lookups_app(h.registry.clone()).await;

    let (_, body) = get(
        &app,
        "/v1/exec_lookup?serviceName=Counter&handlerName=add&scope=acme",
    )
    .await;

    assert_eq!(
        body,
        json!({
            "version": 1,
            "deployment": {
                "id": id,
                "address": "http://counter:9080/",
                "protocol_type": "bidi_stream",
                "http_version": "1.1",
                "additional_headers": {"x-env": "demo"},
                "min_protocol_version": 5,
                "max_protocol_version": 7,
            },
            "abort_timeout": "10m",
            "state_preload_policy": "all",
            "retry_policy": {"on_max_attempts": "kill"},
        })
    );
}

#[actix_web::test]
async fn exec_lookup_is_byte_identical() {
    let mut h = Harness::new();
    let id = h.add(registration(COUNTER_URI, json!([counter()])));
    let app = lookups_app(h.registry.clone()).await;

    let response = test::call_service(
        &app,
        TestRequest::get()
            .uri("/clusters/restate/v1/exec_lookup?serviceName=Counter&handlerName=add&scope=acme")
            .to_request(),
    )
    .await;
    let body = test::read_body(response).await;

    assert_eq!(
        std::str::from_utf8(&body).unwrap(),
        format!(
            concat!(
                r#"{{"version":1,"deployment":{{"id":"{}","#,
                r#""address":"http://counter:9080/","protocol_type":"bidi_stream","#,
                r#""http_version":"2","min_protocol_version":5,"max_protocol_version":7}},"#,
                r#""retry_policy":{{}}}}"#
            ),
            id
        )
    );
}

#[actix_web::test]
async fn exec_lookup_pinned() {
    let mut h = Harness::new();
    let id = h.add(registration(COUNTER_URI, json!([counter()])));
    let app = lookups_app(h.registry.clone()).await;

    let deployment = exec_deployment(&app, &format!("&deploymentId={id}")).await;

    assert_eq!(deployment, json!(id));
}

#[actix_web::test]
async fn exec_lookup_returns_sdk_version() {
    let mut h = Harness::new();
    h.add(json!({
        "uri": COUNTER_URI,
        "sdk_version": "restate-sdk-typescript/1.9.0",
        "manifest": manifest(json!([counter()])),
    }));
    let app = lookups_app(h.registry.clone()).await;

    let (_, body) = get(
        &app,
        "/v1/exec_lookup?serviceName=Counter&handlerName=add&scope=acme",
    )
    .await;

    assert_eq!(
        body["deployment"]["sdk_version"],
        json!("restate-sdk-typescript/1.9.0")
    );
}

#[actix_web::test]
async fn exec_lookup_omits_missing_sdk_version() {
    let mut h = Harness::new();
    h.add(registration(COUNTER_URI, json!([counter()])));
    let app = lookups_app(h.registry.clone()).await;

    let (_, body) = get(
        &app,
        "/v1/exec_lookup?serviceName=Counter&handlerName=add&scope=acme",
    )
    .await;

    assert_eq!(body["deployment"].get("sdk_version"), None);
}

#[actix_web::test]
async fn exec_lookup_pinned_to_older_deployment() {
    let mut h = Harness::new();
    let v1 = h.add(registration(COUNTER_URI, json!([counter()])));
    h.add(registration("http://counter-v2:9080", json!([counter()])));
    let app = lookups_app(h.registry.clone()).await;

    let deployment = exec_deployment(&app, &format!("&deploymentId={v1}")).await;

    assert_eq!(deployment, json!(v1));
}

#[actix_web::test]
async fn unknown_service_is_not_found() {
    let h = Harness::new();
    let app = lookups_app(h.registry.clone()).await;

    let response = get(
        &app,
        "/v1/ingestion_lookup?serviceName=Nope&handlerName=add&scope=acme",
    )
    .await;

    assert_eq!(
        response,
        (StatusCode::NOT_FOUND, json!({"error": "service_not_found"}))
    );
}

#[actix_web::test]
async fn unknown_handler_is_not_found() {
    let mut h = Harness::new();
    h.add(registration(COUNTER_URI, json!([counter()])));
    let app = lookups_app(h.registry.clone()).await;

    let response = get(
        &app,
        "/v1/ingestion_lookup?serviceName=Counter&handlerName=nope&scope=acme",
    )
    .await;

    assert_eq!(
        response,
        (StatusCode::NOT_FOUND, json!({"error": "handler_not_found"}))
    );
}

/// Like the registry: `exec_lookup` doesn't check the handler, the service's settings apply.
#[actix_web::test]
async fn exec_lookup_of_unknown_handler_answers_the_service() {
    let mut h = Harness::new();
    let id = h.add(registration(
        COUNTER_URI,
        json!([{"name": "Counter", "ty": "VIRTUAL_OBJECT", "abortTimeout": 60000, "handlers": [{"name": "add"}]}]),
    ));
    let app = lookups_app(h.registry.clone()).await;

    let (status, body) = get(
        &app,
        "/v1/exec_lookup?serviceName=Counter&handlerName=nope&scope=acme",
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["deployment"]["id"], json!(id));
    assert_eq!(body["abort_timeout"], json!("1m"));
}

#[actix_web::test]
async fn unknown_deployment_is_not_found() {
    let mut h = Harness::new();
    h.add(registration(COUNTER_URI, json!([counter()])));
    let app = lookups_app(h.registry.clone()).await;

    for id in ["dp_10000000000000000000000", "not-an-id"] {
        let response = get(
            &app,
            &format!(
                "/v1/exec_lookup?serviceName=Counter&handlerName=add&scope=acme&deploymentId={id}"
            ),
        )
        .await;

        assert_eq!(
            response,
            (
                StatusCode::NOT_FOUND,
                json!({"error": "deployment_not_found"})
            )
        );
    }
}

#[actix_web::test]
async fn missing_handler_name_is_bad_request() {
    let h = Harness::new();
    let app = lookups_app(h.registry.clone()).await;

    let (status, body) = get(&app, "/v1/ingestion_lookup?serviceName=Counter").await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["message"].as_str().unwrap().contains("handlerName"));
}

#[actix_web::test]
async fn bad_scope_is_bad_request() {
    let h = Harness::new();
    let app = lookups_app(h.registry.clone()).await;

    for scope in ["a%2Fb", "", &"a".repeat(37)] {
        let (status, _) = get(
            &app,
            &format!("/v1/ingestion_lookup?serviceName=Counter&handlerName=add&scope={scope}"),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST, "scope {scope:?}");
    }
}

#[actix_web::test]
async fn same_uri_again_is_a_new_deployment_and_the_next_revision() {
    let mut h = Harness::new();
    let first = h.add(registration(COUNTER_URI, json!([counter()])));

    let second = h.add(registration(COUNTER_URI, json!([counter()])));

    assert_ne!(first, second);
    let endpoint = h.registry.endpoint(CLUSTER, &second).unwrap();
    assert_eq!(endpoint.services[0].revision, 2);
    assert!(endpoint.services[0].latest);
    assert!(!h.registry.endpoint(CLUSTER, &first).unwrap().services[0].latest);
}

#[actix_web::test]
async fn service_type_can_change() {
    let mut h = Harness::new();
    h.add(registration(COUNTER_URI, json!([counter()])));
    h.add(registration(
        "http://counter-v2:9080",
        json!([{"name": "Counter", "ty": "SERVICE", "handlers": [{"name": "add"}]}]),
    ));
    let app = lookups_app(h.registry.clone()).await;

    let (_, body) = get(
        &app,
        "/v1/ingestion_lookup?serviceName=Counter&handlerName=add&scope=acme",
    )
    .await;

    assert_eq!(body["target_ty"], json!("service"));
}

#[actix_web::test]
async fn deleting_the_only_deployment_removes_the_service() {
    let mut h = Harness::new();
    let id = h.add(registration(COUNTER_URI, json!([counter()])));
    h.delete(&id);
    let app = lookups_app(h.registry.clone()).await;

    let response = get(
        &app,
        "/v1/ingestion_lookup?serviceName=Counter&handlerName=add&scope=acme",
    )
    .await;

    assert_eq!(
        response,
        (StatusCode::NOT_FOUND, json!({"error": "service_not_found"}))
    );
}

#[actix_web::test]
async fn deleting_the_latest_deployment_falls_back_to_the_previous_one() {
    let mut h = Harness::new();
    let v1 = h.add(registration(COUNTER_URI, json!([counter()])));
    let v2 = h.add(registration("http://counter-v2:9080", json!([counter()])));
    h.delete(&v2);
    let app = lookups_app(h.registry.clone()).await;

    let deployment = exec_deployment(&app, "").await;

    assert_eq!(deployment, json!(v1));
}

#[actix_web::test]
async fn newer_deployment_gets_new_invocations() {
    let mut h = Harness::new();
    h.add(registration(COUNTER_URI, json!([counter()])));
    let v2 = h.add(registration("http://counter-v2:9080", json!([counter()])));
    let app = lookups_app(h.registry.clone()).await;

    let deployment = exec_deployment(&app, "").await;

    assert_eq!(deployment, json!(v2));
}

#[actix_web::test]
async fn pinned_older_deployment_keeps_its_settings() {
    let mut h = Harness::new();
    let v1 = h.add(registration(
        COUNTER_URI,
        json!([{"name": "Counter", "ty": "VIRTUAL_OBJECT", "inactivityTimeout": 60000, "handlers": [{"name": "add"}]}]),
    ));
    h.add(registration(
        "http://counter-v2:9080",
        json!([{"name": "Counter", "ty": "VIRTUAL_OBJECT", "inactivityTimeout": 120000, "handlers": [{"name": "add"}]}]),
    ));
    let app = lookups_app(h.registry.clone()).await;

    let (_, pinned) = get(
        &app,
        &format!(
            "/v1/exec_lookup?serviceName=Counter&handlerName=add&scope=acme&deploymentId={v1}"
        ),
    )
    .await;
    let (_, latest) = get(
        &app,
        "/v1/exec_lookup?serviceName=Counter&handlerName=add&scope=acme",
    )
    .await;

    assert_eq!(pinned["inactivity_timeout"], json!("1m"));
    assert_eq!(latest["inactivity_timeout"], json!("2m"));
}

#[actix_web::test]
async fn handler_settings_override_service_settings() {
    let mut h = Harness::new();
    h.add(registration(
        COUNTER_URI,
        json!([{
            "name": "Counter",
            "ty": "VIRTUAL_OBJECT",
            "inactivityTimeout": 60000,
            "retryPolicyInitialInterval": 100,
            "retryPolicyMaxAttempts": 10,
            "handlers": [{"name": "add", "inactivityTimeout": 1000, "retryPolicyMaxAttempts": 3}],
        }]),
    ));
    let app = lookups_app(h.registry.clone()).await;

    let (_, body) = get(
        &app,
        "/v1/exec_lookup?serviceName=Counter&handlerName=add&scope=acme",
    )
    .await;

    assert_eq!(
        (&body["inactivity_timeout"], &body["retry_policy"]),
        (
            &json!("1s"),
            &json!({"initial_interval": "100ms", "max_attempts": 3})
        )
    );
}

#[actix_web::test]
async fn other_tenant_deployment_is_not_found() {
    let mut h = Harness::new();
    let acme = h.add(tenant_registration(
        "acme",
        "http://acme:9080",
        json!([counter()]),
    ));
    let app = lookups_app(h.registry.clone()).await;

    let response = get(
        &app,
        &format!(
            "/v1/exec_lookup?serviceName=Counter&handlerName=add&deploymentId={acme}&scope=other"
        ),
    )
    .await;

    assert_eq!(
        response,
        (
            StatusCode::NOT_FOUND,
            json!({"error": "deployment_not_found"})
        )
    );
}

#[actix_web::test]
async fn other_tenant_service_is_not_found() {
    let mut h = Harness::new();
    h.add(tenant_registration(
        "acme",
        "http://acme:9080",
        json!([counter()]),
    ));
    let app = lookups_app(h.registry.clone()).await;

    let exec = get(
        &app,
        "/v1/exec_lookup?serviceName=Counter&handlerName=add&scope=other",
    )
    .await;
    let ingestion = get(
        &app,
        "/v1/ingestion_lookup?serviceName=Counter&handlerName=add&scope=other",
    )
    .await;

    for response in [exec, ingestion] {
        assert_eq!(
            response,
            (StatusCode::NOT_FOUND, json!({"error": "service_not_found"}))
        );
    }
}

#[actix_web::test]
async fn same_uri_in_two_tenants_are_two_deployments_with_their_own_revisions() {
    let mut h = Harness::new();
    let acme = h.add(tenant_registration("acme", COUNTER_URI, json!([counter()])));
    let other = h.add(tenant_registration(
        "other",
        COUNTER_URI,
        json!([counter()]),
    ));

    assert_ne!(acme, other);
    assert_eq!(h.registry.max_revision(CLUSTER, "acme", "Counter"), Some(1));
    assert_eq!(
        h.registry.max_revision(CLUSTER, "other", "Counter"),
        Some(1)
    );
}

/// No shared deployments in the operator: an unscoped lookup finds nothing, like the registry's
/// unscoped lookup of a tenant-only service.
#[actix_web::test]
async fn unscoped_lookups_find_nothing() {
    let mut h = Harness::new();
    let id = h.add(registration(COUNTER_URI, json!([counter()])));
    let app = lookups_app(h.registry.clone()).await;

    let exec = get(&app, "/v1/exec_lookup?serviceName=Counter&handlerName=add").await;
    let ingestion = get(
        &app,
        "/v1/ingestion_lookup?serviceName=Counter&handlerName=add",
    )
    .await;
    let pinned = get(
        &app,
        &format!("/v1/exec_lookup?serviceName=Counter&handlerName=add&deploymentId={id}"),
    )
    .await;

    assert_eq!(
        exec,
        (StatusCode::NOT_FOUND, json!({"error": "service_not_found"}))
    );
    assert_eq!(
        ingestion,
        (StatusCode::NOT_FOUND, json!({"error": "service_not_found"}))
    );
    assert_eq!(
        pinned,
        (
            StatusCode::NOT_FOUND,
            json!({"error": "deployment_not_found"})
        )
    );
}

#[actix_web::test]
async fn clusters_are_separate_registries() {
    let mut h = Harness::new();
    let id = h.add(
        json!({"uri": COUNTER_URI, "cluster": "other", "manifest": manifest(json!([counter()]))}),
    );
    let app = lookups_app(h.registry.clone()).await;

    let here = get(
        &app,
        &format!(
            "/v1/exec_lookup?serviceName=Counter&handlerName=add&scope=acme&deploymentId={id}"
        ),
    )
    .await;
    let (status, there, _) = send(
        &app,
        TestRequest::get()
            .uri("/clusters/other/v1/exec_lookup?serviceName=Counter&handlerName=add&scope=acme"),
    )
    .await;

    assert_eq!(here.0, StatusCode::NOT_FOUND);
    assert_eq!(status, StatusCode::OK);
    let _ = there;
}

#[actix_web::test]
async fn every_change_bumps_the_version() {
    let mut h = Harness::new();
    let lookup = "/v1/ingestion_lookup?serviceName=Counter&handlerName=add&scope=acme";
    let v1 = h.add(registration(COUNTER_URI, json!([counter()])));
    let app = lookups_app(h.registry.clone()).await;
    let (_, first) = get(&app, lookup).await;
    h.add(registration("http://counter-v2:9080", json!([counter()])));
    let (_, second) = get(&app, lookup).await;
    h.delete(&v1);
    let (_, third) = get(&app, lookup).await;

    assert_eq!(first["version"], json!(1));
    assert_eq!(second["version"], json!(2));
    assert_eq!(third["version"], json!(3));
}

#[test]
fn an_unchanged_object_doesnt_bump_the_version() {
    let mut h = Harness::new();
    let id = h.add(registration(COUNTER_URI, json!([counter()])));
    let mut object = h.objects[&id].clone();
    // A status change the lookups don't see.
    object.status.as_mut().unwrap().summary = Some("Counter@1".to_owned());

    h.registry.apply(&Event::Apply(object));

    assert_eq!(h.registry.version(), 1);
}

/// The README's demo inserts (`demo/cron.json`, `checkout.json`), here in a tenant.
#[actix_web::test]
async fn demo_inserts() {
    let mut h = Harness::new();
    h.add(json!({
        "uri": "http://cron.restate-demo.svc.cluster.local:9080/",
        "tenant": "alice",
        "sdk_version": "restate-sdk-typescript/1.17.0",
        "manifest": {
            "protocolMode": "BIDI_STREAM", "minProtocolVersion": 5, "maxProtocolVersion": 7,
            "services": [{ "name": "cron", "ty": "VIRTUAL_OBJECT", "journalRetention": 600000, "handlers": [
                { "name": "create", "journalRetention": 0 }, { "name": "delete", "journalRetention": 0 },
                { "name": "get", "ty": "SHARED", "journalRetention": 0 },
                { "name": "list", "ty": "SHARED", "journalRetention": 0 },
                { "name": "wake", "ingressPrivate": true } ] }]
        }
    }));
    h.add(json!({
        "uri": "http://relay.restate-demo.svc.cluster.local:9080/demo/sample/http/in-process/9080/",
        "tenant": "alice",
        "additional_headers": { "x-restate-cluster-name": "env_demo" },
        "sdk_version": "restate-sdk-typescript/1.17.3",
        "manifest": {
            "protocolMode": "BIDI_STREAM", "minProtocolVersion": 5, "maxProtocolVersion": 8,
            "services": [{ "name": "checkout", "ty": "SERVICE", "handlers": [{ "name": "run" }] }]
        }
    }));
    let app = lookups_app(h.registry.clone()).await;

    let (_, wake) = get(
        &app,
        "/v1/ingestion_lookup?serviceName=cron&handlerName=wake&scope=alice",
    )
    .await;
    assert_eq!(
        wake,
        json!({
            "version": 2,
            "target_ty": "virtual_object_exclusive",
            "public": false,
            "journal_retention": "10m",
        })
    );
    let (_, list) = get(
        &app,
        "/v1/ingestion_lookup?serviceName=cron&handlerName=list&scope=alice",
    )
    .await;
    assert_eq!(list["target_ty"], json!("virtual_object_shared"));
    assert_eq!(list["journal_retention"], json!("0s"));

    let (_, exec) = get(
        &app,
        "/v1/exec_lookup?serviceName=checkout&handlerName=run&scope=alice",
    )
    .await;
    assert_eq!(
        exec["deployment"]["address"],
        json!("http://relay.restate-demo.svc.cluster.local:9080/demo/sample/http/in-process/9080/")
    );
    assert_eq!(
        exec["deployment"]["additional_headers"],
        json!({"x-restate-cluster-name": "env_demo"})
    );
    assert_eq!(exec["deployment"]["max_protocol_version"], json!(8));
    assert_eq!(
        exec["deployment"]["sdk_version"],
        json!("restate-sdk-typescript/1.17.3")
    );
}

// --- The index: sync, relists, readiness of objects ------------------------------------------

#[actix_web::test]
async fn lookups_are_unavailable_until_the_first_relist_is_done() {
    let registry = Registry::new();
    let mut h = Harness::unsynced(registry.clone());
    registry.apply(&Event::Init);
    let object = h.object(&registration(COUNTER_URI, json!([counter()])));
    registry.apply(&Event::InitApply(object));
    let app = lookups_app(h.registry.clone()).await;

    let (before, before_body) = get(
        &app,
        "/v1/exec_lookup?serviceName=Counter&handlerName=add&scope=acme",
    )
    .await;
    let (health_before, _, _) = send(&app, TestRequest::get().uri("/health")).await;
    assert!(!registry.is_synced());
    registry.apply(&Event::InitDone);
    let (after, _) = get(
        &app,
        "/v1/exec_lookup?serviceName=Counter&handlerName=add&scope=acme",
    )
    .await;
    let (health_after, _, health) = send(&app, TestRequest::get().uri("/health")).await;

    assert_eq!(before, StatusCode::SERVICE_UNAVAILABLE);
    assert!(before_body["message"].is_string());
    assert_eq!(health_before, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(after, StatusCode::OK);
    assert_eq!(health_after, StatusCode::OK);
    assert_eq!(health["synced"], json!(true));
    assert_eq!(health["endpoints"], json!(1));
}

#[test]
fn a_relist_sweeps_what_it_didnt_see() {
    let mut h = Harness::new();
    let kept = h.add(registration(COUNTER_URI, json!([counter()])));
    let gone = h.add(registration("http://greeter:9080", json!([greeter()])));

    h.registry.apply(&Event::Init);
    h.registry
        .apply(&Event::InitApply(h.objects[&kept].clone()));
    // Until the relist is done, the old entries keep answering.
    assert_eq!(
        h.registry.latest(CLUSTER, "acme", "Greeter").as_deref(),
        Some(gone.as_str())
    );
    h.registry.apply(&Event::InitDone);

    assert_eq!(h.registry.latest(CLUSTER, "acme", "Greeter"), None);
    assert_eq!(
        h.registry.latest(CLUSTER, "acme", "Counter").as_deref(),
        Some(kept.as_str())
    );
    assert_eq!(h.registry.len(), 1);
}

#[test]
fn only_ready_endpoints_with_a_tenant_are_indexed() {
    let mut h = Harness::new();
    let request = registration(COUNTER_URI, json!([counter()]));
    let mut pending = h.object(&request);
    pending.status.as_mut().unwrap().phase = Some(EndpointPhase::Pending);
    let mut no_tenant = h.object(&request);
    no_tenant.status.as_mut().unwrap().tenant = None;
    let mut bad_tenant = h.object(&request);
    bad_tenant.status.as_mut().unwrap().tenant = Some("a".repeat(37));
    let mut foreign_id = h.object(&request);
    foreign_id.status.as_mut().unwrap().deployment_id = Some(encode_deployment_id(1));
    let mut undiscovered = h.object(&request);
    undiscovered.status.as_mut().unwrap().discovered = None;

    for object in [pending, no_tenant, bad_tenant, foreign_id, undiscovered] {
        h.registry.apply(&Event::Apply(object));
    }

    assert!(h.registry.is_empty());
}

#[test]
fn an_endpoint_that_fails_leaves_the_index() {
    let mut h = Harness::new();
    let id = h.add(registration(COUNTER_URI, json!([counter()])));
    let mut failed = h.objects[&id].clone();
    failed.status.as_mut().unwrap().phase = Some(EndpointPhase::Failed);

    h.registry.apply(&Event::Apply(failed));

    assert_eq!(h.registry.latest(CLUSTER, "acme", "Counter"), None);
    assert!(h.registry.is_empty());
}

#[test]
fn a_relabelled_namespace_moves_the_endpoint() {
    let mut h = Harness::new();
    let id = h.add(registration(COUNTER_URI, json!([counter()])));
    let mut moved = h.objects[&id].clone();
    moved.status.as_mut().unwrap().tenant = Some("other".to_owned());

    h.registry.apply(&Event::Apply(moved));

    assert_eq!(h.registry.latest(CLUSTER, "acme", "Counter"), None);
    assert_eq!(
        h.registry.latest(CLUSTER, "other", "Counter").as_deref(),
        Some(id.as_str())
    );
}

#[test]
fn latest_ties_go_to_the_newest_then_the_highest_uid() {
    let mut h = Harness::new();
    let request = registration(COUNTER_URI, json!([counter()]));
    // Three endpoints given the same revision (e.g. by two leaders).
    let mut objects: Vec<_> = (0..3).map(|_| h.object(&request)).collect();
    for object in &mut objects {
        let status = object.status.as_mut().unwrap();
        status.discovered.as_mut().unwrap().services[0].revision = 1;
    }
    let same_time = objects[0].metadata.creation_timestamp.clone();
    objects[2].metadata.creation_timestamp = same_time;
    let ids: Vec<String> = objects
        .iter()
        .map(|o| o.status.as_ref().unwrap().deployment_id.clone().unwrap())
        .collect();
    for object in &objects {
        h.registry.apply(&Event::Apply(object.clone()));
    }

    // objects[1] is the newest.
    assert_eq!(
        h.registry.latest(CLUSTER, "acme", "Counter").as_deref(),
        Some(ids[1].as_str())
    );
    h.registry.apply(&Event::Delete(objects[1].clone()));
    // objects[0] and [2] were created at the same time: the higher uid wins.
    assert_eq!(
        h.registry.latest(CLUSTER, "acme", "Counter").as_deref(),
        Some(ids[2].as_str())
    );
}

// --- Revision reservations and the controllers' queries ---------------------------------------

#[test]
fn reservations_hand_out_increasing_revisions() {
    let h = Harness::new();
    let services = ["Counter".to_owned(), "Greeter".to_owned()];

    let first = h
        .registry
        .reserve_revisions(CLUSTER, "acme", "dp_a", &services);
    let second = h
        .registry
        .reserve_revisions(CLUSTER, "acme", "dp_b", &services[..1]);
    let other_tenant = h
        .registry
        .reserve_revisions(CLUSTER, "other", "dp_c", &services[..1]);
    // Reserving again for the same deployment replaces its reservation.
    let again = h
        .registry
        .reserve_revisions(CLUSTER, "acme", "dp_b", &services[..1]);

    assert_eq!(first, vec![1, 1]);
    assert_eq!(second, vec![2]);
    assert_eq!(other_tenant, vec![1]);
    assert_eq!(again, vec![2]);
}

#[test]
fn reservations_count_the_indexed_endpoints_and_clear_with_their_event() {
    let mut h = Harness::new();
    // Revisions 1 and 2, indexed: their reservations are gone.
    h.add(registration(COUNTER_URI, json!([counter()])));
    let v2 = h.add(registration(COUNTER_URI, json!([counter()])));

    let next = h
        .registry
        .reserve_revisions(CLUSTER, "acme", "dp_x", &["Counter".to_owned()]);
    h.registry.release_reservation(CLUSTER, "dp_x");
    let after_release =
        h.registry
            .reserve_revisions(CLUSTER, "acme", "dp_y", &["Counter".to_owned()]);
    h.registry.release_reservation(CLUSTER, "dp_y");
    h.delete(&v2);
    let after_delete =
        h.registry
            .reserve_revisions(CLUSTER, "acme", "dp_z", &["Counter".to_owned()]);

    assert_eq!(next, vec![3]);
    assert_eq!(after_release, vec![3]);
    // The highest live one is 1 again.
    assert_eq!(after_delete, vec![2]);
}

#[test]
fn nothing_is_kept_for_services_without_endpoints_or_reservations() {
    let mut h = Harness::new();
    let id = h.add(registration(COUNTER_URI, json!([counter()])));
    h.delete(&id);

    assert_eq!(
        h.registry
            .reserve_revisions(CLUSTER, "acme", "dp_x", &["Counter".to_owned()]),
        vec![1]
    );
}

#[test]
fn a_pending_reservation_survives_unrelated_events() {
    let mut h = Harness::new();
    let id = h.add(registration(COUNTER_URI, json!([counter()])));
    // A rollback: the endpoint gets max + 1 reserved, then an event with the old revision lands.
    let promoted = h
        .registry
        .reserve_revisions(CLUSTER, "acme", &id, &["Counter".to_owned()]);
    let mut object = h.objects[&id].clone();
    object.status.as_mut().unwrap().summary = Some("Counter@1".to_owned());
    h.registry.apply(&Event::Apply(object.clone()));

    let other = h
        .registry
        .reserve_revisions(CLUSTER, "acme", "dp_x", &["Counter".to_owned()]);
    h.registry.release_reservation(CLUSTER, "dp_x");
    // The patch with the reserved revision lands: the reservation is fulfilled.
    object
        .status
        .as_mut()
        .unwrap()
        .discovered
        .as_mut()
        .unwrap()
        .services[0]
        .revision = promoted[0];
    h.registry.apply(&Event::Apply(object));
    let after = h
        .registry
        .reserve_revisions(CLUSTER, "acme", "dp_y", &["Counter".to_owned()]);

    assert_eq!(promoted, vec![2]);
    assert_eq!(other, vec![3]);
    assert_eq!(after, vec![3]);
    assert!(h.registry.is_latest_for_all_services(CLUSTER, &id));
}

#[test]
fn controller_queries() {
    let mut h = Harness::new();
    let v1 = h.add(registration(COUNTER_URI, json!([counter(), greeter()])));
    let v2 = h.add(registration("http://counter-v2:9080", json!([counter()])));

    assert_eq!(
        h.registry.latest(CLUSTER, "acme", "Counter").as_deref(),
        Some(v2.as_str())
    );
    assert_eq!(
        h.registry.latest(CLUSTER, "acme", "Greeter").as_deref(),
        Some(v1.as_str())
    );
    assert!(h.registry.is_latest_for_all_services(CLUSTER, &v2));
    assert!(!h.registry.is_latest_for_all_services(CLUSTER, &v1));
    assert!(!h.registry.is_latest_for_all_services("other", &v2));
    assert!(!h.registry.is_latest_for_all_services(CLUSTER, "dp_nope"));
    assert_eq!(h.registry.max_revision(CLUSTER, "acme", "Counter"), Some(2));
    assert_eq!(h.registry.max_revision(CLUSTER, "acme", "Nope"), None);

    let endpoints = h.registry.tenant_endpoints(CLUSTER, "acme");
    assert_eq!(endpoints.len(), 2);
    assert_eq!(endpoints[0].deployment_id, v1);
    assert_eq!(endpoints[0].namespace, "ns-acme");
    assert_eq!(
        endpoints[0]
            .services
            .iter()
            .map(|s| (s.name.as_str(), s.revision, s.latest))
            .collect::<Vec<_>>(),
        vec![("Counter", 1, false), ("Greeter", 1, true)]
    );
    assert_eq!(endpoints[1].deployment_id, v2);
    assert!(h.registry.tenant_endpoints(CLUSTER, "other").is_empty());
}

// --- Memory: interning --------------------------------------------------------------------------

#[test]
fn versions_and_tenants_share_service_shapes_and_names() {
    let mut h = Harness::new();
    let mut ids = vec![];
    for tenant in ["t1", "t2", "t3"] {
        for version in 0..2 {
            ids.push(h.add(tenant_registration(
                tenant,
                &format!("http://counter-{version}:9080"),
                json!([counter()]),
            )));
        }
    }
    // A different shape.
    ids.push(h.add(tenant_registration(
        "t1",
        COUNTER_URI,
        json!([{"name": "Counter", "ty": "SERVICE", "handlers": [{"name": "add"}]}]),
    )));

    {
        let index = h.registry.read();
        assert_eq!(index.len(), 7);
        assert_eq!(index.shapes.len(), 2);
        // restate, t1..t3, ns-t1..ns-t3, Counter.
        assert_eq!(index.strings.len(), 8);
    }

    for id in ids {
        h.delete(&id);
    }
    let index = h.registry.read();
    assert_eq!(index.shapes.len(), 0);
    assert_eq!(index.strings.len(), 0);
}

// --- Read API (`schema-registry/tests/scoped.rs`) ----------------------------------------------

#[actix_web::test]
async fn deployments_are_only_the_scopes_own() {
    let mut h = Harness::new();
    let alice = h.add(tenant_registration(
        "alice",
        "http://greeter:9080",
        json!([greeter()]),
    ));
    let bob = h.add(tenant_registration(
        "bob",
        "http://greeter:9080",
        json!([greeter()]),
    ));
    let app = read_api_app(h.registry.clone(), details_cache()).await;

    let (status, body) = get(&app, "/v1/scopes/alice/deployments").await;
    let (bob_status, _) = get(&app, &format!("/v1/scopes/alice/deployments/{bob}")).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({"deployments": [{
            "type": "http",
            "id": alice,
            "uri": "http://greeter:9080/",
            "protocol_type": "BidiStream",
            "http_version": "HTTP/2.0",
            "metadata": {"tenant": "alice"},
            "created_at": "2026-10-21T10:00:01Z",
            "min_protocol_version": 5,
            "max_protocol_version": 7,
            "services": [{"name": "Greeter", "revision": 1}],
        }]})
    );
    assert_eq!(bob_status, StatusCode::NOT_FOUND);
}

#[actix_web::test]
async fn a_deployment_has_its_services_if_the_scope_sees_it() {
    let mut h = Harness::new();
    let alice = h.add(tenant_registration(
        "alice",
        "http://127.0.0.1:1/",
        json!([counter()]),
    ));
    let app = read_api_app(h.registry.clone(), details_cache()).await;

    let (status, body) = get(&app, &format!("/v1/scopes/alice/deployments/{alice}")).await;
    let (other_status, other_body) =
        get(&app, &format!("/v1/scopes/bob/deployments/{alice}")).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["services"][0]["name"], json!("Counter"));
    assert_eq!(body["services"][0]["ty"], json!("VirtualObject"));
    assert_eq!(body["services"][0]["deployment_id"], json!(alice));
    assert_eq!(other_status, StatusCode::NOT_FOUND);
    assert_eq!(
        other_body,
        json!({"message": format!("deployment '{alice}' not found")})
    );
}

#[actix_web::test]
async fn services_are_the_latest_revision_of_the_scopes_own() {
    let mut h = Harness::new();
    h.add(tenant_registration(
        "alice",
        "http://127.0.0.1:1/",
        json!([counter(), greeter()]),
    ));
    let alice_v2 = h.add(tenant_registration(
        "alice",
        "http://127.0.0.1:2/",
        json!([counter()]),
    ));
    let app = read_api_app(h.registry.clone(), details_cache()).await;

    let (_, alice_services) = get(&app, "/v1/scopes/alice/services").await;
    let (_, bob_services) = get(&app, "/v1/scopes/bob/services").await;
    let (bob_counter, _) = get(&app, "/v1/scopes/bob/services/Counter").await;
    let (_, alice_counter) = get(&app, "/v1/scopes/alice/services/Counter").await;

    let summary = |services: &Value| -> Vec<(Value, Value)> {
        services["services"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| (s["name"].clone(), s["revision"].clone()))
            .collect()
    };
    assert_eq!(
        summary(&alice_services),
        vec![(json!("Counter"), json!(2)), (json!("Greeter"), json!(1))]
    );
    assert_eq!(alice_counter["deployment_id"], json!(alice_v2));
    assert!(summary(&bob_services).is_empty());
    assert_eq!(bob_counter, StatusCode::NOT_FOUND);
}

fn checkout_manifest() -> Value {
    json!({
        "protocolMode": "REQUEST_RESPONSE",
        "minProtocolVersion": 5,
        "maxProtocolVersion": 7,
        "services": [{
            "name": "Checkout",
            "ty": "WORKFLOW",
            "documentation": "Checks out a cart",
            "metadata": {"team": "shop"},
            "journalRetention": 60000,
            "enableLazyState": true,
            "retryPolicyMaxAttempts": 5,
            "handlers": [
                {
                    "name": "run",
                    "ty": "WORKFLOW",
                    "workflowCompletionRetention": 86400000,
                    "inactivityTimeout": 5000,
                    "documentation": "The run",
                    "input": {
                        "contentType": "application/json",
                        "jsonSchema": {"title": "Cart", "type": "object"},
                    },
                    "output": {"contentType": "application/json", "jsonSchema": {"type": "string"}},
                },
                {"name": "status", "ingressPrivate": true, "input": {}, "output": {}},
            ],
        }],
    })
}

/// A deployment's `/discover`, in process (h2c and HTTP/1.1): counts the requests, records their
/// headers.
struct DiscoverServer {
    address: String,
    requests: Arc<AtomicUsize>,
    headers: Arc<parking_lot::Mutex<Vec<(String, String)>>>,
}

async fn discover_server(manifest: Value) -> DiscoverServer {
    let requests = Arc::new(AtomicUsize::new(0));
    let headers = Arc::new(parking_lot::Mutex::new(vec![]));
    let body = manifest.to_string();
    let (count, seen) = (requests.clone(), headers.clone());
    let server = HttpServer::new(move || {
        let (count, seen, body) = (count.clone(), seen.clone(), body.clone());
        App::new().route(
            "/discover",
            web::get().to(move |request: HttpRequest| {
                let (count, seen, body) = (count.clone(), seen.clone(), body.clone());
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    *seen.lock() = request
                        .headers()
                        .iter()
                        .map(|(n, v)| (n.to_string(), v.to_str().unwrap().to_owned()))
                        .collect();
                    HttpResponse::Ok()
                        .content_type("application/vnd.restate.endpointmanifest.v4+json")
                        .body(body)
                }
            }),
        )
    })
    .workers(1)
    .disable_signals()
    .bind_auto_h2c("127.0.0.1:0")
    .unwrap();
    let address = format!("http://{}/", server.addrs()[0]);
    actix_web::rt::spawn(server.run());
    DiscoverServer {
        address,
        requests,
        headers,
    }
}

#[actix_web::test]
async fn service_has_the_admin_api_shape_with_the_live_details() {
    let discover = discover_server(checkout_manifest()).await;
    let mut h = Harness::new();
    h.add(json!({
        "uri": discover.address,
        "tenant": "alice",
        "additional_headers": {"x-api-key": "secret"},
        "manifest": checkout_manifest(),
    }));
    let app = read_api_app(h.registry.clone(), details_cache()).await;

    let (status, service) = get(&app, "/v1/scopes/alice/services/Checkout").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(service["ty"], json!("Workflow"));
    assert_eq!(service["documentation"], json!("Checks out a cart"));
    assert_eq!(service["metadata"], json!({"team": "shop"}));
    assert_eq!(service["public"], json!(true));
    assert_eq!(service["journal_retention"], json!("1m"));
    assert_eq!(service["workflow_completion_retention"], json!("1d"));
    assert_eq!(service["idempotency_retention"], json!("1d"));
    assert_eq!(service["inactivity_timeout"], json!("1m"));
    assert_eq!(service["abort_timeout"], json!("10m"));
    assert_eq!(service["enable_lazy_state"], json!(true));
    assert_eq!(service["state_preload_policy"], json!({"Partial": []}));
    assert_eq!(
        service["retry_policy"],
        json!({
            "initial_interval": "100ms",
            "exponentiation_factor": 2.0,
            "max_attempts": 5,
            "max_interval": null,
            "on_max_attempts": "Pause",
        })
    );
    assert_eq!(
        service["handlers"][0],
        json!({
            "name": "run",
            "ty": "Workflow",
            "documentation": "The run",
            "public": true,
            "input_description": "JSON 'Cart'",
            "output_description": "JSON string",
            "input_json_schema": {"title": "Cart", "type": "object"},
            "output_json_schema": {"type": "string"},
            "retry_policy": {"max_attempts": null},
        })
    );
    assert_eq!(service["handlers"][1]["name"], json!("status"));
    assert_eq!(service["handlers"][1]["ty"], json!("Shared"));
    assert_eq!(service["handlers"][1]["public"], json!(false));
    assert_eq!(service["handlers"][1]["input_description"], json!("none"));
    assert_eq!(service["handlers"][1]["output_description"], json!("none"));

    // The discovery request: the endpoint's headers, the manifest versions.
    let headers = discover.headers.lock().clone();
    assert!(headers.contains(&("x-api-key".to_owned(), "secret".to_owned())));
    assert!(headers.contains(&("accept".to_owned(), DISCOVER_ACCEPT.to_owned())));
}

#[actix_web::test]
async fn without_the_live_details_the_service_is_shown_from_the_index() {
    let mut h = Harness::new();
    // Nothing listens there (an old version scaled down).
    h.add(
        json!({"uri": "http://127.0.0.1:1/", "tenant": "alice", "manifest": checkout_manifest()}),
    );
    let app = read_api_app(h.registry.clone(), details_cache()).await;

    let (status, service) = get(&app, "/v1/scopes/alice/services/Checkout").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(service.get("documentation"), None);
    assert_eq!(service.get("metadata"), None);
    assert_eq!(
        service["handlers"][0],
        json!({
            "name": "run",
            "ty": "Workflow",
            "public": true,
            "input_description": "application/json",
            "output_description": "application/json",
            "retry_policy": {"max_attempts": null},
        })
    );
}

#[actix_web::test]
async fn the_details_are_cached_per_deployment() {
    let discover = discover_server(manifest(json!([counter()]))).await;
    let mut h = Harness::new();
    let v1 = h.add(tenant_registration(
        "alice",
        &discover.address,
        json!([counter()]),
    ));
    let v2 = h.add(json!({
        "uri": discover.address, "tenant": "alice", "use_http_11": true,
        "manifest": manifest(json!([greeter()])),
    }));
    let details = Arc::new(DetailsCache::new(1, std::time::Duration::from_secs(2)));
    let app = read_api_app(h.registry.clone(), details.clone()).await;

    get(&app, &format!("/v1/scopes/alice/deployments/{v1}")).await;
    get(&app, &format!("/v1/scopes/alice/deployments/{v1}")).await;
    get(&app, "/v1/scopes/alice/services/Counter/handlers").await;
    assert_eq!(discover.requests.load(Ordering::SeqCst), 1);
    // HTTP/1.1, and the capacity is 1: v1 is evicted.
    let (status, _) = get(&app, &format!("/v1/scopes/alice/deployments/{v2}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(discover.requests.load(Ordering::SeqCst), 2);
    get(&app, &format!("/v1/scopes/alice/deployments/{v1}")).await;

    assert_eq!(discover.requests.load(Ordering::SeqCst), 3);
    assert_eq!(details.cached(), 1);
}

#[actix_web::test]
async fn handlers_of_a_service() {
    let mut h = Harness::new();
    h.add(tenant_registration(
        "alice",
        "http://127.0.0.1:1/",
        json!([counter()]),
    ));
    let app = read_api_app(h.registry.clone(), details_cache()).await;

    let (_, handlers) = get(&app, "/v1/scopes/alice/services/Counter/handlers").await;
    let (status, handler) = get(&app, "/v1/scopes/alice/services/Counter/handlers/get").await;
    let (missing_status, missing) =
        get(&app, "/v1/scopes/alice/services/Counter/handlers/nope").await;
    let (missing_service, missing_service_body) =
        get(&app, "/v1/scopes/alice/services/Nope/handlers").await;

    let names: Vec<_> = handlers["handlers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["name"].clone())
        .collect();
    assert_eq!(names, vec![json!("add"), json!("get")]);
    assert_eq!(status, StatusCode::OK);
    assert_eq!(handler["ty"], json!("Shared"));
    assert_eq!(handler["input_description"], json!("*/*"));
    assert_eq!(handler["output_description"], json!("application/json"));
    assert_eq!(missing_status, StatusCode::NOT_FOUND);
    assert_eq!(
        missing,
        json!({"message": "handler 'Counter/nope' not found"})
    );
    assert_eq!(missing_service, StatusCode::NOT_FOUND);
    assert_eq!(
        missing_service_body,
        json!({"message": "service 'Nope' not found"})
    );
}

#[actix_web::test]
async fn health_and_bad_scopes() {
    let h = Harness::new();
    let app = read_api_app(h.registry.clone(), details_cache()).await;

    let (health, _) = get(&app, "/v1/scopes/alice/health").await;
    let (bad, body) = get(&app, "/v1/scopes/a%20b/services").await;
    let (too_long, _) = get(&app, &format!("/v1/scopes/{}/health", "a".repeat(37))).await;

    assert_eq!(health, StatusCode::OK);
    assert_eq!(bad, StatusCode::BAD_REQUEST);
    assert!(body["message"].as_str().unwrap().contains("scope"));
    assert_eq!(too_long, StatusCode::BAD_REQUEST);
}

#[actix_web::test]
async fn the_read_api_is_unavailable_until_synced() {
    let h = Harness::unsynced(Registry::new());
    let app = read_api_app(h.registry.clone(), details_cache()).await;

    let (health, _) = get(&app, "/v1/scopes/alice/health").await;
    let (services, body) = get(&app, "/v1/scopes/alice/services").await;

    assert_eq!(health, StatusCode::OK);
    assert_eq!(services, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body["message"].is_string());
}

#[actix_web::test]
async fn cors_on_the_read_api_only() {
    let h = Harness::new();
    let read_api = read_api_app(h.registry.clone(), details_cache()).await;
    let lookups = lookups_app(h.registry.clone()).await;

    let (preflight, headers, _) = send(
        &read_api,
        TestRequest::default()
            .method(actix_web::http::Method::OPTIONS)
            .uri("/clusters/restate/v1/scopes/alice/services")
            .insert_header(("origin", "http://localhost:4300"))
            .insert_header(("access-control-request-method", "GET"))
            .insert_header(("access-control-request-headers", "authorization")),
    )
    .await;
    let (not_found, not_found_headers, _) = send(
        &read_api,
        TestRequest::get()
            .uri("/clusters/restate/v1/scopes/alice/services/nope")
            .insert_header(("origin", "http://localhost:4300")),
    )
    .await;
    let (_, lookup_headers, _) = send(
        &lookups,
        TestRequest::get()
            .uri("/clusters/restate/v1/exec_lookup?serviceName=a&handlerName=b&scope=alice")
            .insert_header(("origin", "http://localhost:4300")),
    )
    .await;

    assert_eq!(preflight, StatusCode::OK);
    assert_eq!(headers.get("access-control-allow-origin").unwrap(), "*");
    assert_eq!(headers.get("access-control-allow-methods").unwrap(), "GET");
    assert_eq!(
        headers.get("access-control-allow-headers").unwrap(),
        "authorization"
    );
    assert_eq!(not_found, StatusCode::NOT_FOUND);
    assert_eq!(
        not_found_headers
            .get("access-control-allow-origin")
            .unwrap(),
        "*"
    );
    assert!(lookup_headers.get("access-control-allow-origin").is_none());
}

#[actix_web::test]
async fn each_listener_serves_only_its_api() {
    let mut h = Harness::new();
    h.add(tenant_registration(
        "alice",
        COUNTER_URI,
        json!([counter()]),
    ));
    let read_api = read_api_app(h.registry.clone(), details_cache()).await;
    let lookups = lookups_app(h.registry.clone()).await;

    let (lookup_on_read_api, _) = get(
        &read_api,
        "/v1/ingestion_lookup?serviceName=Counter&handlerName=add&scope=alice",
    )
    .await;
    let (read_api_on_lookups, _) = get(&lookups, "/v1/scopes/alice/deployments").await;

    assert_eq!(lookup_on_read_api, StatusCode::NOT_FOUND);
    assert_eq!(read_api_on_lookups, StatusCode::NOT_FOUND);
}
