//! The registry's two listeners (`56-operator-registry.md`, *The lookup server*):
//!
//! * **lookups** (cluster-internal, the relay's and the mux's `registry_url` =
//!   `http://<service>:8090/clusters/<cluster>`): `GET /clusters/{cluster}/v1/exec_lookup` and
//!   `/v1/ingestion_lookup` (`02-contract.md` §4), `503` until the index has synced; `GET /health`.
//! * **read API** (the web UI, CORS any origin): `GET /clusters/{cluster}/v1/scopes/{scope}/…`
//!   (§4.1) in the admin API's shapes, with the live details (docs, metadata, JSON schemas).
//!
//! Status codes, bodies and scope validation are the schema registry's (`schema-registry/src/
//! routes.rs`, `error.rs`).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use actix_web::body::MessageBody;
use actix_web::dev::{ServiceRequest, ServiceResponse};
use actix_web::http::header::{self, HeaderValue};
use actix_web::http::{Method, StatusCode};
use actix_web::middleware::{Next, from_fn};
use actix_web::web::{self, Data, Path, ServiceConfig};
use actix_web::{App, HttpRequest, HttpResponse, HttpServer};
use serde::Deserialize;
use serde_json::json;
use tracing::{debug, info};

use super::Registry;
use super::admin::{
    ErrorDescriptionResponse, ListDeploymentsResponse, ListServiceHandlersResponse,
    ListServicesResponse,
};
use super::api::{ErrorCode, ErrorResponse};
use super::catalog;
use super::details::{self, DetailsCache};
use crate::resources::restatedeploymentendpoints::{MAX_SCOPE_LENGTH, is_valid_scope};

#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// How many deployments' live details the read API keeps (0: none, every answer fetches).
    pub details_cache_entries: usize,
    /// The timeout of a details fetch.
    pub details_timeout: Duration,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            details_cache_entries: details::DEFAULT_CAPACITY,
            details_timeout: details::DEFAULT_TIMEOUT,
        }
    }
}

/// Serves the lookups on `lookups` and the read API on `read_api` until shutdown (SIGTERM/SIGINT,
/// like the operator's other server).
pub async fn run(
    registry: Registry,
    lookups: SocketAddr,
    read_api: SocketAddr,
) -> std::io::Result<()> {
    run_with(registry, lookups, read_api, ServerConfig::default()).await
}

pub async fn run_with(
    registry: Registry,
    lookups: SocketAddr,
    read_api: SocketAddr,
    config: ServerConfig,
) -> std::io::Result<()> {
    let details = Arc::new(DetailsCache::new(
        config.details_cache_entries,
        config.details_timeout,
    ));

    let lookup_registry = registry.clone();
    let lookup_server =
        HttpServer::new(move || App::new().configure(configure_lookups(lookup_registry.clone())))
            .bind(lookups)?
            .shutdown_timeout(5)
            .run();
    info!(%lookups, "registry lookups listening");

    let read_api_server = HttpServer::new(move || {
        App::new().configure(configure_read_api(registry.clone(), Arc::clone(&details)))
    })
    .workers(2)
    .bind(read_api)?
    .shutdown_timeout(5)
    .run();
    info!(%read_api, "registry read API listening");

    let (lookups, read_api) = tokio::join!(lookup_server, read_api_server);
    lookups.and(read_api)
}

/// The lookup listener's routes.
pub fn configure_lookups(registry: Registry) -> impl FnOnce(&mut ServiceConfig) {
    move |config| {
        config
            .app_data(Data::new(registry))
            .route("/health", web::get().to(health))
            .service(
                web::scope("/clusters/{cluster}")
                    .route("/v1/exec_lookup", web::get().to(exec_lookup))
                    .route("/v1/ingestion_lookup", web::get().to(ingestion_lookup)),
            );
    }
}

/// The read API listener's routes.
pub fn configure_read_api(
    registry: Registry,
    details: Arc<DetailsCache>,
) -> impl FnOnce(&mut ServiceConfig) {
    move |config| {
        config
            .app_data(Data::new(registry))
            .app_data(Data::from(details))
            .service(
                web::scope("/clusters/{cluster}/v1/scopes/{scope}")
                    .wrap(from_fn(cors))
                    .route("/health", web::get().to(scoped_health))
                    .route("/deployments", web::get().to(scoped_deployments))
                    .route("/deployments/{id}", web::get().to(scoped_deployment))
                    .route("/services", web::get().to(scoped_services))
                    .route("/services/{name}", web::get().to(scoped_service))
                    .route("/services/{name}/handlers", web::get().to(scoped_handlers))
                    .route(
                        "/services/{name}/handlers/{handler}",
                        web::get().to(scoped_handler),
                    ),
            );
    }
}

// --- Lookups ---------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LookupQuery {
    service_name: String,
    handler_name: String,
    deployment_id: Option<String>,
    /// The scope of the invocation, the tenant. When not set, nothing is visible (no shared
    /// deployments in the operator).
    scope: Option<String>,
}

/// `200` once the index has synced, `503` before.
async fn health(registry: Data<Registry>) -> HttpResponse {
    let index = registry.read();
    let body = json!({"synced": index.synced, "version": index.version, "endpoints": index.len()});
    if index.synced {
        HttpResponse::Ok().json(body)
    } else {
        HttpResponse::ServiceUnavailable().json(body)
    }
}

fn not_synced() -> HttpResponse {
    HttpResponse::ServiceUnavailable().json(ErrorDescriptionResponse {
        message: "the registry's index isn't synced yet".to_owned(),
    })
}

fn bad_request(message: String) -> HttpResponse {
    HttpResponse::BadRequest().json(json!({ "message": message }))
}

fn not_found(error: ErrorCode) -> HttpResponse {
    HttpResponse::NotFound().json(ErrorResponse { error })
}

fn parse_lookup(request: &HttpRequest) -> Result<LookupQuery, HttpResponse> {
    let query = web::Query::<LookupQuery>::from_query(request.query_string())
        .map_err(|e| bad_request(format!("Failed to deserialize query string: {e}")))?
        .into_inner();
    validate_scope("scope", query.scope.as_deref())?;
    Ok(query)
}

async fn exec_lookup(
    registry: Data<Registry>,
    cluster: Path<String>,
    request: HttpRequest,
) -> HttpResponse {
    let query = match parse_lookup(&request) {
        Ok(query) => query,
        Err(response) => return response,
    };
    let index = registry.read();
    if !index.synced {
        return not_synced();
    }
    let response = index.exec_lookup(
        &cluster,
        query.scope.as_deref(),
        &query.service_name,
        &query.handler_name,
        query.deployment_id.as_deref(),
    );
    drop(index);
    debug!(
        cluster = %cluster,
        scope = ?query.scope,
        service = %query.service_name,
        handler = %query.handler_name,
        pinned = ?query.deployment_id,
        deployment = ?response.as_ref().map(|response| &response.deployment.id).ok(),
        "exec lookup"
    );
    match response {
        Ok(response) => HttpResponse::Ok().json(response),
        Err(error) => not_found(error),
    }
}

async fn ingestion_lookup(
    registry: Data<Registry>,
    cluster: Path<String>,
    request: HttpRequest,
) -> HttpResponse {
    let query = match parse_lookup(&request) {
        Ok(query) => query,
        Err(response) => return response,
    };
    debug!(cluster = %cluster, scope = ?query.scope, service = %query.service_name, handler = %query.handler_name, "ingestion lookup");
    let index = registry.read();
    if !index.synced {
        return not_synced();
    }
    match index.ingestion_lookup(
        &cluster,
        query.scope.as_deref(),
        &query.service_name,
        &query.handler_name,
    ) {
        Ok(response) => HttpResponse::Ok().json(response),
        Err(error) => not_found(error),
    }
}

/// The rules of a Restate scope (`util/string/src/restricted_value.rs` in the runtime).
fn validate_scope(what: &str, scope: Option<&str>) -> Result<(), HttpResponse> {
    match scope {
        Some(scope) if !is_valid_scope(scope) => Err(bad_request(format!(
            "bad {what} '{scope}': must be 1 to {MAX_SCOPE_LENGTH} characters in [a-zA-Z0-9_.-]"
        ))),
        _ => Ok(()),
    }
}

// --- Read API --------------------------------------------------------------------------------

/// CORS for the browsers (any origin, `GET`), like the registry's `CorsLayer`: preflights are
/// answered here, every other answer gets `access-control-allow-origin: *`.
async fn cors(
    request: ServiceRequest,
    next: Next<impl MessageBody + 'static>,
) -> Result<ServiceResponse<impl MessageBody>, actix_web::Error> {
    const VARY: &str = "origin, access-control-request-method, access-control-request-headers";
    if request.method() == Method::OPTIONS
        && request
            .headers()
            .contains_key(header::ACCESS_CONTROL_REQUEST_METHOD)
    {
        let mut response = HttpResponse::Ok();
        response
            .insert_header((header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"))
            .insert_header((header::ACCESS_CONTROL_ALLOW_METHODS, "GET"))
            .insert_header((header::VARY, VARY));
        // Not `*`: browsers don't let it cover `authorization`.
        if let Some(headers) = request
            .headers()
            .get(header::ACCESS_CONTROL_REQUEST_HEADERS)
        {
            response.insert_header((header::ACCESS_CONTROL_ALLOW_HEADERS, headers.clone()));
        }
        return Ok(request
            .into_response(response.finish())
            .map_into_right_body());
    }
    let mut response = next.call(request).await?;
    let headers = response.headers_mut();
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(header::VARY, HeaderValue::from_static(VARY));
    Ok(response.map_into_left_body())
}

fn scoped_error(status: StatusCode, message: String) -> HttpResponse {
    HttpResponse::build(status).json(ErrorDescriptionResponse { message })
}

/// The scope checks of every read API route but `/health`.
fn check_scope(registry: &Registry, scope: &str) -> Result<(), HttpResponse> {
    validate_scope("scope", Some(scope))?;
    if !registry.is_synced() {
        return Err(not_synced());
    }
    Ok(())
}

macro_rules! check {
    ($check:expr) => {
        if let Err(response) = $check {
            return response;
        }
    };
}

async fn scoped_health(path: Path<(String, String)>) -> HttpResponse {
    let (_, scope) = path.into_inner();
    check!(validate_scope("scope", Some(&scope)));
    HttpResponse::Ok().finish()
}

async fn scoped_deployments(
    registry: Data<Registry>,
    path: Path<(String, String)>,
) -> HttpResponse {
    let (cluster, scope) = path.into_inner();
    check!(check_scope(&registry, &scope));
    let deployments = registry
        .read()
        .scope_deployments(&cluster, &scope)
        .into_iter()
        .map(|(deployment, services)| {
            catalog::deployment(
                &deployment,
                services.iter().map(catalog::name_revision).collect(),
            )
        })
        .collect();
    HttpResponse::Ok().json(ListDeploymentsResponse { deployments })
}

async fn scoped_deployment(
    registry: Data<Registry>,
    details: Data<DetailsCache>,
    path: Path<(String, String, String)>,
) -> HttpResponse {
    let (cluster, scope, id) = path.into_inner();
    check!(check_scope(&registry, &scope));
    let Some((deployment, services)) = registry.read().scope_deployment(&cluster, &scope, &id)
    else {
        return scoped_error(
            StatusCode::NOT_FOUND,
            format!("deployment '{id}' not found"),
        );
    };
    let details = details.get(&deployment).await;
    let services = services
        .iter()
        .map(|service| catalog::service(&deployment, service, details.as_deref()))
        .collect();
    HttpResponse::Ok().json(catalog::deployment(&deployment, services))
}

async fn scoped_services(
    registry: Data<Registry>,
    details: Data<DetailsCache>,
    path: Path<(String, String)>,
) -> HttpResponse {
    let (cluster, scope) = path.into_inner();
    check!(check_scope(&registry, &scope));
    let services = registry.read().scope_services(&cluster, &scope);
    let details = details
        .get_all(services.iter().map(|(deployment, _)| &**deployment))
        .await;
    let services = services
        .iter()
        .map(|(deployment, service)| {
            catalog::service(
                deployment,
                service,
                details.get(&deployment.id).map(|details| &**details),
            )
        })
        .collect();
    HttpResponse::Ok().json(ListServicesResponse { services })
}

/// The scope's service `name`, rendered with its details.
async fn find_service(
    registry: &Registry,
    details: &DetailsCache,
    cluster: &str,
    scope: &str,
    name: &str,
) -> Result<super::admin::ServiceMetadata, HttpResponse> {
    check_scope(registry, scope)?;
    let Some((deployment, service)) = registry.read().scope_service(cluster, scope, name) else {
        return Err(scoped_error(
            StatusCode::NOT_FOUND,
            format!("service '{name}' not found"),
        ));
    };
    let details = details.get(&deployment).await;
    Ok(catalog::service(&deployment, &service, details.as_deref()))
}

async fn scoped_service(
    registry: Data<Registry>,
    details: Data<DetailsCache>,
    path: Path<(String, String, String)>,
) -> HttpResponse {
    let (cluster, scope, name) = path.into_inner();
    match find_service(&registry, &details, &cluster, &scope, &name).await {
        Ok(service) => HttpResponse::Ok().json(service),
        Err(response) => response,
    }
}

async fn scoped_handlers(
    registry: Data<Registry>,
    details: Data<DetailsCache>,
    path: Path<(String, String, String)>,
) -> HttpResponse {
    let (cluster, scope, name) = path.into_inner();
    match find_service(&registry, &details, &cluster, &scope, &name).await {
        Ok(service) => HttpResponse::Ok().json(ListServiceHandlersResponse {
            handlers: service.handlers,
        }),
        Err(response) => response,
    }
}

async fn scoped_handler(
    registry: Data<Registry>,
    details: Data<DetailsCache>,
    path: Path<(String, String, String, String)>,
) -> HttpResponse {
    let (cluster, scope, name, handler) = path.into_inner();
    match find_service(&registry, &details, &cluster, &scope, &name).await {
        Ok(service) => match service.handlers.into_iter().find(|h| h.name == handler) {
            Some(handler) => HttpResponse::Ok().json(handler),
            None => scoped_error(
                StatusCode::NOT_FOUND,
                format!("handler '{name}/{handler}' not found"),
            ),
        },
        Err(response) => response,
    }
}
