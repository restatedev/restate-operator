//! The in-memory index of the `Ready` `RestateDeploymentEndpoint`s, and the lookups over it.
//!
//! The index is the only copy of the endpoints the operator keeps (no reflector store): each watch
//! event is applied and dropped. It holds what the lookups and the per-scope read API answer from,
//! interned ([`Pool`]): repeated strings once, and the **service shapes** (type, options, handlers)
//! once across every version and tenant that shares them.
//!
//! The registry's semantics (`schema-registry/src/db.rs`, `catalog.rs`), minus the shared
//! deployments: every endpoint belongs to one tenant, an unscoped lookup sees nothing.
//!
//! # Size
//!
//! Per endpoint, roughly: the [`Endpoint`] struct (~130 B) + its name and address on the heap
//! (~20 + ~70 B) + 12 B per service + ~70 B of map entries (`endpoints`, `by_tenant`, `latest`),
//! so **~300 B for a one-service endpoint**; a service shape is ~100 B + ~80 B per handler, once
//! per distinct shape (shared by the versions and the tenants running the same code). Compare
//! ~5–10 KB per object for a kube-rs store of the full objects. `58-operator-scale.md` works out
//! the totals per scale tier.

use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kube::ResourceExt;
use kube::runtime::watcher;
use tracing::warn;

use super::api::{
    DeploymentInfo, ErrorCode, ExecLookupResponse, Friendly, HttpVersion, IngestionLookupResponse,
    InputPayload, OnMaxAttempts, OutputPayload, ProtocolType, RetryPolicyOverrides,
    StatePreloadPolicy, TargetType,
};
use super::manifest::{ServiceType, normalize_address, validate_header};
use super::pool::{Id, Pool};
use crate::resources::restatedeploymentendpoints::{
    self as crd, EndpointPhase, ON_MAX_ATTEMPTS_KILL, ON_MAX_ATTEMPTS_PAUSE,
    RestateDeploymentEndpoint, is_valid_scope,
};

/// How long a revision reservation is kept if neither its endpoint's watch event nor a release
/// comes: a safety net against a controller that never releases.
const RESERVATION_TTL: Duration = Duration::from_secs(10 * 60);

/// A deployment id (`dp_1…`) as a number: the endpoint's `metadata.uid`, 128 bits.
pub type DeploymentKey = u128;

/// One indexed endpoint. Its strings are interned in [`Index::strings`], its service shapes in
/// [`Index::shapes`].
#[derive(Debug, Clone, PartialEq)]
pub struct Endpoint {
    pub id: DeploymentKey,
    pub cluster: Id,
    pub tenant: Id,
    pub namespace: Id,
    pub name: Box<str>,
    /// Normalized like the registry's (`http://host:9080` ⇒ `http://host:9080/`).
    pub address: Box<str>,
    /// Lowercased names, sorted.
    pub headers: Box<[(Id, Id)]>,
    pub sdk_version: Option<Id>,
    pub protocol_type: ProtocolType,
    pub http_version: HttpVersion,
    pub min_protocol_version: i32,
    pub max_protocol_version: i32,
    /// `metadata.creationTimestamp`, in milliseconds.
    pub created_at: i64,
    /// Sorted by name, unique.
    pub services: Box<[EndpointService]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EndpointService {
    pub name: Id,
    pub revision: u32,
    pub shape: Id,
}

/// What a service is, apart from its name and revision: shared by every endpoint serving the same.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ServiceShape {
    pub ty: ServiceType,
    pub public: bool,
    pub options: Option<Box<Opts>>,
    /// Sorted by name, unique.
    pub handlers: Box<[HandlerShape]>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HandlerShape {
    pub name: Box<str>,
    pub target_ty: TargetType,
    /// When not set, the service's.
    pub public: Option<bool>,
    pub workflow_completion_retention: Option<u64>,
    pub options: Option<Box<Opts>>,
    pub input: Option<Box<InputPayload>>,
    pub output: Option<Box<OutputPayload>>,
}

/// The settings of a service or a handler (the registry's `manifest::Options`). Durations are in
/// milliseconds, `None` means not set. Boxed and absent when nothing is set, the usual case.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Opts {
    pub idempotency_retention: Option<u64>,
    pub journal_retention: Option<u64>,
    pub inactivity_timeout: Option<u64>,
    pub abort_timeout: Option<u64>,
    pub state_preload_policy: Option<StatePreloadPolicy>,
    pub retry_initial_interval: Option<u64>,
    pub retry_exponentiation_factor: Option<F64>,
    pub retry_max_attempts: Option<u64>,
    pub retry_max_interval: Option<u64>,
    pub retry_on_max_attempts: Option<OnMaxAttempts>,
}

/// An `f64` compared and hashed by its bits (manifests carry no NaN).
#[derive(Debug, Clone, Copy)]
pub struct F64(pub f64);

impl PartialEq for F64 {
    fn eq(&self, other: &Self) -> bool {
        self.0.to_bits() == other.0.to_bits()
    }
}

impl Eq for F64 {}

impl Hash for F64 {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.to_bits().hash(state);
    }
}

#[derive(Debug)]
struct Entry {
    endpoint: Endpoint,
    /// The relist it was last seen in (mark-and-sweep).
    epoch: u32,
}

#[derive(Debug)]
struct Reservation {
    cluster: Box<str>,
    tenant: Box<str>,
    services: Vec<(Box<str>, u32)>,
    at: Instant,
}

type TenantKey = (Id, Id);
type ServiceKey = (Id, Id, Id);

#[derive(Debug, Default)]
pub struct Index {
    pub strings: Pool<str>,
    pub shapes: Pool<ServiceShape>,
    endpoints: HashMap<DeploymentKey, Entry>,
    /// (cluster, tenant) ⇒ its endpoints.
    by_tenant: HashMap<TenantKey, Vec<DeploymentKey>>,
    /// (cluster, tenant, service) ⇒ the endpoint serving its latest revision.
    latest: HashMap<ServiceKey, DeploymentKey>,
    /// By deployment id (as written in the endpoint's status).
    reservations: HashMap<Box<str>, Reservation>,
    /// Bumped by every change of what the lookups answer (the lookups' `version`).
    pub version: u64,
    epoch: u32,
    pub synced: bool,
}

/// An endpoint about to be indexed, before interning.
struct Candidate {
    id: DeploymentKey,
    deployment_id: String,
    cluster: String,
    tenant: String,
    namespace: String,
    name: String,
    address: String,
    headers: Vec<(String, String)>,
    sdk_version: Option<String>,
    protocol_type: ProtocolType,
    http_version: HttpVersion,
    min_protocol_version: i32,
    max_protocol_version: i32,
    created_at: i64,
    services: Vec<(String, u32, ServiceShape)>,
}

impl Index {
    pub fn apply(&mut self, event: &watcher::Event<RestateDeploymentEndpoint>) {
        match event {
            watcher::Event::Init => {
                self.epoch = self.epoch.wrapping_add(1);
            }
            watcher::Event::InitApply(object) | watcher::Event::Apply(object) => {
                self.upsert(object)
            }
            watcher::Event::Delete(object) => {
                if let Some(uid) = object.uid().as_deref().and_then(parse_uid) {
                    self.remove(uid);
                }
                if let Some(id) = object
                    .status
                    .as_ref()
                    .and_then(|status| status.deployment_id.as_deref())
                {
                    self.reservations.remove(id);
                }
            }
            watcher::Event::InitDone => {
                let epoch = self.epoch;
                let stale: Vec<_> = self
                    .endpoints
                    .iter()
                    .filter(|(_, entry)| entry.epoch != epoch)
                    .map(|(id, _)| *id)
                    .collect();
                for id in stale {
                    self.remove(id);
                }
                self.synced = true;
            }
        }
    }

    fn upsert(&mut self, object: &RestateDeploymentEndpoint) {
        let Some(uid) = object.uid().as_deref().and_then(parse_uid) else {
            warn!(
                namespace = ?object.namespace(),
                name = %object.name_any(),
                "RestateDeploymentEndpoint without a valid uid, ignored"
            );
            return;
        };
        let Some(candidate) = candidate(uid, object) else {
            self.remove(uid);
            return;
        };
        let deployment_id = candidate.deployment_id.clone();
        let endpoint = self.intern(candidate);
        let epoch = self.epoch;

        match self.endpoints.get_mut(&uid) {
            Some(entry) if entry.endpoint == endpoint => {
                entry.epoch = epoch;
                self.release_endpoint(&endpoint);
            }
            _ => {
                self.remove(uid);
                self.insert(endpoint, epoch);
                self.version += 1;
            }
        }
        self.clear_fulfilled_reservation(&deployment_id, uid);
    }

    fn intern(&mut self, candidate: Candidate) -> Endpoint {
        let strings = &mut self.strings;
        let mut services: Vec<EndpointService> = candidate
            .services
            .into_iter()
            .map(|(name, revision, shape)| EndpointService {
                name: strings.intern(name.as_str()),
                revision,
                shape: self.shapes.intern_owned(shape),
            })
            .collect();
        services.sort_by(|a, b| strings.get(a.name).cmp(strings.get(b.name)));
        Endpoint {
            id: candidate.id,
            cluster: strings.intern(candidate.cluster.as_str()),
            tenant: strings.intern(candidate.tenant.as_str()),
            namespace: strings.intern(candidate.namespace.as_str()),
            name: candidate.name.into(),
            address: candidate.address.into(),
            headers: candidate
                .headers
                .iter()
                .map(|(name, value)| {
                    (
                        strings.intern(name.as_str()),
                        strings.intern(value.as_str()),
                    )
                })
                .collect(),
            sdk_version: candidate
                .sdk_version
                .as_deref()
                .map(|sdk_version| strings.intern(sdk_version)),
            protocol_type: candidate.protocol_type,
            http_version: candidate.http_version,
            min_protocol_version: candidate.min_protocol_version,
            max_protocol_version: candidate.max_protocol_version,
            created_at: candidate.created_at,
            services: services.into(),
        }
    }

    fn release_endpoint(&mut self, endpoint: &Endpoint) {
        let strings = &mut self.strings;
        strings.release(endpoint.cluster);
        strings.release(endpoint.tenant);
        strings.release(endpoint.namespace);
        for (name, value) in endpoint.headers.iter() {
            strings.release(*name);
            strings.release(*value);
        }
        if let Some(sdk_version) = endpoint.sdk_version {
            strings.release(sdk_version);
        }
        for service in endpoint.services.iter() {
            strings.release(service.name);
            self.shapes.release(service.shape);
        }
    }

    fn insert(&mut self, endpoint: Endpoint, epoch: u32) {
        let id = endpoint.id;
        self.by_tenant
            .entry((endpoint.cluster, endpoint.tenant))
            .or_default()
            .push(id);
        for service in endpoint.services.iter() {
            let key = (endpoint.cluster, endpoint.tenant, service.name);
            let newer = match self.latest.get(&key) {
                None => true,
                Some(current) => {
                    let current = &self.endpoints[current].endpoint;
                    rank(&endpoint, service.name) > rank(current, service.name)
                }
            };
            if newer {
                self.latest.insert(key, id);
            }
        }
        self.endpoints.insert(id, Entry { endpoint, epoch });
    }

    fn remove(&mut self, id: DeploymentKey) {
        let Some(Entry { endpoint, .. }) = self.endpoints.remove(&id) else {
            return;
        };
        let tenant_key = (endpoint.cluster, endpoint.tenant);
        if let Some(ids) = self.by_tenant.get_mut(&tenant_key) {
            ids.retain(|other| *other != id);
            if ids.is_empty() {
                self.by_tenant.remove(&tenant_key);
            }
        }
        for service in endpoint.services.iter() {
            let key = (endpoint.cluster, endpoint.tenant, service.name);
            if self.latest.get(&key) == Some(&id) {
                // Recomputed from the tenant's few live endpoints.
                let next = self
                    .by_tenant
                    .get(&tenant_key)
                    .into_iter()
                    .flatten()
                    .map(|other| &self.endpoints[other].endpoint)
                    .filter(|other| service_of(other, service.name).is_some())
                    .max_by_key(|other| rank(other, service.name))
                    .map(|other| other.id);
                match next {
                    Some(next) => self.latest.insert(key, next),
                    None => self.latest.remove(&key),
                };
            }
        }
        self.release_endpoint(&endpoint);
        self.version += 1;
    }

    // --- Revisions -------------------------------------------------------------------------

    fn clear_fulfilled_reservation(&mut self, deployment_id: &str, id: DeploymentKey) {
        let Some(reservation) = self.reservations.get(deployment_id) else {
            return;
        };
        let Some(entry) = self.endpoints.get(&id) else {
            return;
        };
        let strings = &self.strings;
        let fulfilled = reservation.services.iter().all(|(name, revision)| {
            strings
                .find(&**name)
                .and_then(|name| service_of(&entry.endpoint, name))
                .is_some_and(|service| service.revision >= *revision)
        });
        if fulfilled {
            self.reservations.remove(deployment_id);
        }
    }

    pub fn reserve_revisions(
        &mut self,
        cluster: &str,
        tenant: &str,
        deployment_id: &str,
        services: &[String],
    ) -> Vec<u32> {
        let now = Instant::now();
        self.reservations.remove(deployment_id);
        self.reservations
            .retain(|_, reservation| now.duration_since(reservation.at) < RESERVATION_TTL);

        let revisions: Vec<u32> = services
            .iter()
            .map(|service| {
                let live = self.max_revision(cluster, tenant, service).unwrap_or(0);
                let reserved = self
                    .reservations
                    .values()
                    .filter(|r| &*r.cluster == cluster && &*r.tenant == tenant)
                    .flat_map(|r| r.services.iter())
                    .filter(|(name, _)| **name == **service)
                    .map(|(_, revision)| *revision)
                    .max()
                    .unwrap_or(0);
                live.max(reserved) + 1
            })
            .collect();
        self.reservations.insert(
            deployment_id.into(),
            Reservation {
                cluster: cluster.into(),
                tenant: tenant.into(),
                services: services
                    .iter()
                    .map(|service| service.as_str().into())
                    .zip(revisions.iter().copied())
                    .collect(),
                at: now,
            },
        );
        revisions
    }

    pub fn release_reservation(&mut self, cluster: &str, deployment_id: &str) {
        if self
            .reservations
            .get(deployment_id)
            .is_some_and(|reservation| &*reservation.cluster == cluster)
        {
            self.reservations.remove(deployment_id);
        }
    }

    // --- Queries for the controllers --------------------------------------------------------

    fn service_key(&self, cluster: &str, tenant: &str, service: &str) -> Option<ServiceKey> {
        Some((
            self.strings.find(cluster)?,
            self.strings.find(tenant)?,
            self.strings.find(service)?,
        ))
    }

    fn latest_endpoint(&self, cluster: &str, tenant: &str, service: &str) -> Option<&Endpoint> {
        let key = self.service_key(cluster, tenant, service)?;
        self.latest.get(&key).map(|id| &self.endpoints[id].endpoint)
    }

    pub fn latest(&self, cluster: &str, tenant: &str, service: &str) -> Option<DeploymentKey> {
        self.latest_endpoint(cluster, tenant, service)
            .map(|endpoint| endpoint.id)
    }

    pub fn max_revision(&self, cluster: &str, tenant: &str, service: &str) -> Option<u32> {
        // The latest has the highest revision: it's the first of the ranking.
        let endpoint = self.latest_endpoint(cluster, tenant, service)?;
        let name = self.strings.find(service)?;
        service_of(endpoint, name).map(|service| service.revision)
    }

    /// The endpoint `id`, if it's indexed in `cluster`.
    pub fn endpoint(&self, cluster: &str, id: DeploymentKey) -> Option<&Endpoint> {
        let endpoint = &self.endpoints.get(&id)?.endpoint;
        (self.strings.get(endpoint.cluster) == cluster).then_some(endpoint)
    }

    pub fn is_latest(&self, endpoint: &Endpoint, service: Id) -> bool {
        self.latest
            .get(&(endpoint.cluster, endpoint.tenant, service))
            == Some(&endpoint.id)
    }

    /// The endpoints of `tenant` in `cluster`, by creation time then id.
    pub fn tenant_endpoints(&self, cluster: &str, tenant: &str) -> Vec<&Endpoint> {
        let (Some(cluster), Some(tenant)) = (self.strings.find(cluster), self.strings.find(tenant))
        else {
            return vec![];
        };
        let mut endpoints: Vec<&Endpoint> = self
            .by_tenant
            .get(&(cluster, tenant))
            .into_iter()
            .flatten()
            .map(|id| &self.endpoints[id].endpoint)
            .collect();
        endpoints.sort_by_cached_key(|endpoint| {
            (endpoint.created_at, encode_deployment_id(endpoint.id))
        });
        endpoints
    }

    pub fn len(&self) -> usize {
        self.endpoints.len()
    }

    // --- The lookups (`02-contract.md` §4) --------------------------------------------------

    pub fn exec_lookup(
        &self,
        cluster: &str,
        scope: Option<&str>,
        service: &str,
        handler: &str,
        deployment_id: Option<&str>,
    ) -> Result<ExecLookupResponse, ErrorCode> {
        let not_found = if deployment_id.is_some() {
            ErrorCode::DeploymentNotFound
        } else {
            ErrorCode::ServiceNotFound
        };
        // Nothing is shared: an unscoped lookup sees nothing.
        let scope = scope.ok_or(not_found)?;

        let (endpoint, shape) = match deployment_id {
            // That deployment, if the scope sees it. When it doesn't serve the service, no settings.
            Some(deployment_id) => {
                let endpoint = decode_deployment_id(deployment_id)
                    .and_then(|id| self.endpoint(cluster, id))
                    .filter(|endpoint| self.strings.get(endpoint.tenant) == scope)
                    .ok_or(ErrorCode::DeploymentNotFound)?;
                let shape = self
                    .strings
                    .find(service)
                    .and_then(|name| service_of(endpoint, name))
                    .map(|service| self.shapes.get(service.shape));
                (endpoint, shape)
            }
            // The latest revision of the service in the scope.
            None => {
                let endpoint = self
                    .latest_endpoint(cluster, scope, service)
                    .ok_or(ErrorCode::ServiceNotFound)?;
                let shape = self
                    .strings
                    .find(service)
                    .and_then(|name| service_of(endpoint, name))
                    .map(|service| self.shapes.get(service.shape));
                (endpoint, shape)
            }
        };

        let service_options = shape.and_then(|shape| shape.options.as_deref());
        let handler_options = shape
            .and_then(|shape| find_handler(shape, handler))
            .and_then(|handler| handler.options.as_deref());
        let setting = |f: &dyn Fn(&Opts) -> Option<u64>| {
            handler_options
                .and_then(f)
                .or_else(|| service_options.and_then(f))
        };

        Ok(ExecLookupResponse {
            version: self.version,
            deployment: DeploymentInfo {
                id: encode_deployment_id(endpoint.id),
                address: endpoint.address.to_string(),
                protocol_type: endpoint.protocol_type,
                http_version: endpoint.http_version,
                additional_headers: self.headers(endpoint),
                min_protocol_version: endpoint.min_protocol_version,
                max_protocol_version: endpoint.max_protocol_version,
                sdk_version: endpoint
                    .sdk_version
                    .map(|sdk_version| self.strings.get(sdk_version).to_owned()),
            },
            inactivity_timeout: friendly(setting(&|o| o.inactivity_timeout)),
            abort_timeout: friendly(setting(&|o| o.abort_timeout)),
            state_preload_policy: handler_options
                .and_then(|o| o.state_preload_policy.clone())
                .or_else(|| service_options.and_then(|o| o.state_preload_policy.clone())),
            retry_policy: RetryPolicyOverrides {
                initial_interval: friendly(setting(&|o| o.retry_initial_interval)),
                exponentiation_factor: handler_options
                    .and_then(|o| o.retry_exponentiation_factor)
                    .or_else(|| service_options.and_then(|o| o.retry_exponentiation_factor))
                    .map(|f| f.0 as f32),
                max_attempts: setting(&|o| o.retry_max_attempts),
                max_interval: friendly(setting(&|o| o.retry_max_interval)),
                on_max_attempts: handler_options
                    .and_then(|o| o.retry_on_max_attempts)
                    .or_else(|| service_options.and_then(|o| o.retry_on_max_attempts)),
            },
        })
    }

    pub fn ingestion_lookup(
        &self,
        cluster: &str,
        scope: Option<&str>,
        service: &str,
        handler: &str,
    ) -> Result<IngestionLookupResponse, ErrorCode> {
        let scope = scope.ok_or(ErrorCode::ServiceNotFound)?;
        let endpoint = self
            .latest_endpoint(cluster, scope, service)
            .ok_or(ErrorCode::ServiceNotFound)?;
        let shape = self
            .strings
            .find(service)
            .and_then(|name| service_of(endpoint, name))
            .map(|service| self.shapes.get(service.shape))
            .ok_or(ErrorCode::ServiceNotFound)?;
        let found = find_handler(shape, handler).ok_or(ErrorCode::HandlerNotFound)?;

        let service_options = shape.options.as_deref();
        let handler_options = found.options.as_deref();
        let setting = |f: &dyn Fn(&Opts) -> Option<u64>| {
            handler_options
                .and_then(f)
                .or_else(|| service_options.and_then(f))
        };
        Ok(IngestionLookupResponse {
            version: self.version,
            target_ty: found.target_ty,
            public: found.public.unwrap_or(shape.public),
            idempotency_retention: friendly(setting(&|o| o.idempotency_retention)),
            workflow_completion_retention: friendly(found.workflow_completion_retention),
            journal_retention: friendly(setting(&|o| o.journal_retention)),
            input: found.input.as_deref().cloned(),
            output: found.output.as_deref().cloned(),
        })
    }

    pub fn headers(&self, endpoint: &Endpoint) -> BTreeMap<String, String> {
        endpoint
            .headers
            .iter()
            .map(|(name, value)| {
                (
                    self.strings.get(*name).to_owned(),
                    self.strings.get(*value).to_owned(),
                )
            })
            .collect()
    }
}

/// Latest wins: the highest revision, then the newest, then the highest uid.
fn rank(endpoint: &Endpoint, service: Id) -> (u32, i64, DeploymentKey) {
    let revision = service_of(endpoint, service).map_or(0, |service| service.revision);
    (revision, endpoint.created_at, endpoint.id)
}

pub fn service_of(endpoint: &Endpoint, name: Id) -> Option<&EndpointService> {
    // A handful of services per endpoint: a scan beats a search through the interner.
    endpoint
        .services
        .iter()
        .find(|service| service.name == name)
}

pub fn find_handler<'a>(shape: &'a ServiceShape, name: &str) -> Option<&'a HandlerShape> {
    shape
        .handlers
        .binary_search_by(|handler| (*handler.name).cmp(name))
        .ok()
        .map(|index| &shape.handlers[index])
}

fn friendly(millis: Option<u64>) -> Option<Friendly> {
    millis.map(|millis| Friendly(Duration::from_millis(millis)))
}

/// What an object contributes to the index: `None` unless it's `Ready`, with a valid tenant, its
/// deployment id (the one derived from its uid) and what discovery found.
fn candidate(uid: DeploymentKey, object: &RestateDeploymentEndpoint) -> Option<Candidate> {
    let status = object.status.as_ref()?;
    if status.phase != Some(EndpointPhase::Ready) {
        return None;
    }
    let tenant = status.tenant.as_deref().filter(|t| is_valid_scope(t))?;
    let deployment_id = status.deployment_id.as_deref()?;
    let discovered = status.discovered.as_ref()?;
    let skip = |why: &str| {
        warn!(
            namespace = ?object.namespace(),
            name = %object.name_any(),
            "RestateDeploymentEndpoint not served: {why}"
        );
    };
    if decode_deployment_id(deployment_id) != Some(uid) {
        skip("its status.deploymentId isn't the one of its uid");
        return None;
    }
    let Ok(address) = normalize_address(&object.spec.address) else {
        skip("bad spec.address");
        return None;
    };
    let mut headers = Vec::with_capacity(object.spec.additional_headers.len());
    for (name, value) in &object.spec.additional_headers {
        let Ok(header) = validate_header(name, value) else {
            skip("bad spec.additionalHeaders");
            return None;
        };
        headers.push(header);
    }
    headers.sort();

    let mut services: Vec<(String, u32, ServiceShape)> = discovered
        .services
        .iter()
        .map(|service| (service.name.clone(), service.revision, shape(service)))
        .collect();
    services.sort_by(|a, b| a.0.cmp(&b.0));
    services.dedup_by(|a, b| a.0 == b.0);

    Some(Candidate {
        id: uid,
        deployment_id: deployment_id.to_owned(),
        cluster: object.spec.cluster.clone(),
        tenant: tenant.to_owned(),
        namespace: object.namespace().unwrap_or_default(),
        name: object.name_any(),
        address,
        headers,
        sdk_version: discovered.sdk_version.clone(),
        protocol_type: match discovered.protocol_type {
            crd::ProtocolType::BidiStream => ProtocolType::BidiStream,
            crd::ProtocolType::RequestResponse => ProtocolType::RequestResponse,
        },
        http_version: match discovered.http_version {
            crd::HttpVersion::Http11 => HttpVersion::Http11,
            crd::HttpVersion::Http2 => HttpVersion::Http2,
        },
        min_protocol_version: discovered.min_protocol_version,
        max_protocol_version: discovered.max_protocol_version,
        created_at: object
            .metadata
            .creation_timestamp
            .as_ref()
            .map_or(0, |time| time.0.timestamp_millis()),
        services,
    })
}

fn shape(service: &crd::ProjectedService) -> ServiceShape {
    let mut handlers: Vec<HandlerShape> = service
        .handlers
        .iter()
        .map(|handler| HandlerShape {
            name: handler.name.as_str().into(),
            target_ty: match handler.target_ty {
                crd::TargetType::Service => TargetType::Service,
                crd::TargetType::VirtualObjectExclusive => TargetType::VirtualObjectExclusive,
                crd::TargetType::VirtualObjectShared => TargetType::VirtualObjectShared,
                crd::TargetType::Workflow => TargetType::Workflow,
                crd::TargetType::WorkflowShared => TargetType::WorkflowShared,
            },
            public: handler.public,
            workflow_completion_retention: handler.workflow_completion_retention,
            options: opts(&handler.options),
            input: handler.input.as_ref().map(|input| {
                Box::new(InputPayload {
                    required: input.required,
                    content_type: input.content_type.clone(),
                    json: input.json,
                })
            }),
            output: handler.output.as_ref().map(|output| {
                Box::new(OutputPayload {
                    content_type: output.content_type.clone(),
                    set_content_type_if_empty: output.set_content_type_if_empty,
                })
            }),
        })
        .collect();
    handlers.sort_by(|a, b| a.name.cmp(&b.name));
    handlers.dedup_by(|a, b| a.name == b.name);
    ServiceShape {
        ty: match service.ty {
            crd::ServiceType::Service => ServiceType::Service,
            crd::ServiceType::VirtualObject => ServiceType::VirtualObject,
            crd::ServiceType::Workflow => ServiceType::Workflow,
        },
        public: service.public,
        options: opts(&service.options),
        handlers: handlers.into(),
    }
}

fn opts(options: &crd::ProjectedOptions) -> Option<Box<Opts>> {
    if options.is_empty() {
        return None;
    }
    Some(Box::new(Opts {
        idempotency_retention: options.idempotency_retention,
        journal_retention: options.journal_retention,
        inactivity_timeout: options.inactivity_timeout,
        abort_timeout: options.abort_timeout,
        state_preload_policy: options.state_preload.as_ref().map(|preload| {
            if preload.lazy {
                StatePreloadPolicy::Partial(preload.keys.clone())
            } else {
                StatePreloadPolicy::All
            }
        }),
        retry_initial_interval: options.retry_initial_interval,
        retry_exponentiation_factor: options.retry_exponentiation_factor.map(F64),
        retry_max_attempts: options.retry_max_attempts,
        retry_max_interval: options.retry_max_interval,
        retry_on_max_attempts: options
            .retry_on_max_attempts
            .as_deref()
            .and_then(|on| match on {
                ON_MAX_ATTEMPTS_PAUSE => Some(OnMaxAttempts::Pause),
                ON_MAX_ATTEMPTS_KILL => Some(OnMaxAttempts::Kill),
                _ => None,
            }),
    }))
}

const ALPHABET: &[u8; 62] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
const ENCODED_LENGTH: usize = 22;

/// A `metadata.uid` (a UUID) as a number.
pub fn parse_uid(uid: &str) -> Option<DeploymentKey> {
    let hex: String = uid.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 {
        return None;
    }
    u128::from_str_radix(&hex, 16).ok()
}

/// The runtime's `DeploymentId` format (see
/// [`crd::deployment_id_from_uid`]): `dp_1` + base62 of the byte-swapped value, 22 digits.
pub fn encode_deployment_id(id: DeploymentKey) -> String {
    let mut n = id.swap_bytes();
    let mut digits = [b'0'; ENCODED_LENGTH];
    for digit in digits.iter_mut().rev() {
        if n == 0 {
            break;
        }
        *digit = ALPHABET[(n % 62) as usize];
        n /= 62;
    }
    format!(
        "dp_1{}",
        std::str::from_utf8(&digits).expect("base62 digits are ascii")
    )
}

/// The inverse of [`encode_deployment_id`]; `None` for anything that isn't one.
pub fn decode_deployment_id(id: &str) -> Option<DeploymentKey> {
    let digits = id.strip_prefix("dp_1")?;
    if digits.len() != ENCODED_LENGTH {
        return None;
    }
    let mut n: u128 = 0;
    for b in digits.bytes() {
        let digit = ALPHABET.iter().position(|a| *a == b)? as u128;
        n = n.checked_mul(62)?.checked_add(digit)?;
    }
    Some(n.swap_bytes())
}

/// Shared handles on what a read API answer needs, taken under the lock and rendered outside it
/// (the details are fetched in between).
#[derive(Debug, Clone)]
pub struct DeploymentView {
    pub id: DeploymentKey,
    pub tenant: String,
    pub address: String,
    pub headers: BTreeMap<String, String>,
    pub protocol_type: ProtocolType,
    pub http_version: HttpVersion,
    pub min_protocol_version: i32,
    pub max_protocol_version: i32,
    pub sdk_version: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone)]
pub struct ServiceView {
    pub name: String,
    pub revision: u32,
    pub shape: Arc<ServiceShape>,
}

impl Index {
    pub fn deployment_view(&self, endpoint: &Endpoint) -> DeploymentView {
        DeploymentView {
            id: endpoint.id,
            tenant: self.strings.get(endpoint.tenant).to_owned(),
            address: endpoint.address.to_string(),
            headers: self.headers(endpoint),
            protocol_type: endpoint.protocol_type,
            http_version: endpoint.http_version,
            min_protocol_version: endpoint.min_protocol_version,
            max_protocol_version: endpoint.max_protocol_version,
            sdk_version: endpoint
                .sdk_version
                .map(|sdk_version| self.strings.get(sdk_version).to_owned()),
            created_at: endpoint.created_at,
        }
    }

    pub fn service_view(&self, service: &EndpointService) -> ServiceView {
        ServiceView {
            name: self.strings.get(service.name).to_owned(),
            revision: service.revision,
            shape: self.shapes.get_arc(service.shape),
        }
    }

    /// The scope's own deployments, with their services (by name).
    pub fn scope_deployments(
        &self,
        cluster: &str,
        scope: &str,
    ) -> Vec<(DeploymentView, Vec<ServiceView>)> {
        self.tenant_endpoints(cluster, scope)
            .into_iter()
            .map(|endpoint| {
                (
                    self.deployment_view(endpoint),
                    endpoint
                        .services
                        .iter()
                        .map(|service| self.service_view(service))
                        .collect(),
                )
            })
            .collect()
    }

    /// The deployment `id`, if it's the scope's own.
    pub fn scope_deployment(
        &self,
        cluster: &str,
        scope: &str,
        id: &str,
    ) -> Option<(DeploymentView, Vec<ServiceView>)> {
        let endpoint = decode_deployment_id(id)
            .and_then(|id| self.endpoint(cluster, id))
            .filter(|endpoint| self.strings.get(endpoint.tenant) == scope)?;
        Some((
            self.deployment_view(endpoint),
            endpoint
                .services
                .iter()
                .map(|service| self.service_view(service))
                .collect(),
        ))
    }

    /// The latest revision of every service of the scope, by name.
    pub fn scope_services(
        &self,
        cluster: &str,
        scope: &str,
    ) -> Vec<(Arc<DeploymentView>, ServiceView)> {
        let mut services = vec![];
        for endpoint in self.tenant_endpoints(cluster, scope) {
            let mut view = None;
            for service in endpoint.services.iter() {
                if self.is_latest(endpoint, service.name) {
                    let view = view
                        .get_or_insert_with(|| Arc::new(self.deployment_view(endpoint)))
                        .clone();
                    services.push((view, self.service_view(service)));
                }
            }
        }
        services.sort_by(|a, b| a.1.name.cmp(&b.1.name));
        services
    }

    /// The latest revision of the scope's service `name`.
    pub fn scope_service(
        &self,
        cluster: &str,
        scope: &str,
        name: &str,
    ) -> Option<(DeploymentView, ServiceView)> {
        let endpoint = self.latest_endpoint(cluster, scope, name)?;
        let service = service_of(endpoint, self.strings.find(name)?)?;
        Some((self.deployment_view(endpoint), self.service_view(service)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deployment_ids_round_trip() {
        for uid in [
            "00000000-0000-0000-0000-000000000001",
            "019441f1-be4f-1515-34e9-1b9a2173692d",
            "ffffffff-ffff-ffff-ffff-ffffffffffff",
        ] {
            let id = parse_uid(uid).unwrap();
            let encoded = encode_deployment_id(id);
            assert_eq!(Some(encoded.clone()), crd::deployment_id_from_uid(uid));
            assert_eq!(decode_deployment_id(&encoded), Some(id));
        }
    }

    #[test]
    fn bad_deployment_ids_dont_decode() {
        assert_eq!(decode_deployment_id("dp_1"), None);
        assert_eq!(decode_deployment_id("dp_1zzzzzzzzzzzzzzzzzzzzzz"), None);
        assert_eq!(decode_deployment_id("dp_1000000000000000000000!"), None);
        assert_eq!(decode_deployment_id("xx_10000000000000000000000"), None);
    }
}
