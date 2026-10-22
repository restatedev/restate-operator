//! The workflow registry in the operator (`workflowRegistry.mode: operator`,
//! `56-operator-registry.md`): an in-memory index of the `Ready` `RestateDeploymentEndpoint`s, fed
//! by a watch, answering the schema registry's lookup API (`02-contract.md` §4) and per-scope read
//! API (§4.1) per RestateCluster (`/clusters/{cluster}/…`, [`server`]).
//!
//! The models and their semantics are the schema registry's (`schema-registry/src`), so the
//! `included` and `operator` modes answer the same, minus the shared deployments: here every
//! deployment belongs to one tenant and an unscoped lookup finds nothing.
//!
//! [`Registry`] is the handle the controllers and the server share: [`Registry::apply`] is fed the
//! endpoint watch's events, the revision assignment and the queries serve the endpoint and
//! RestateDeployment controllers.

pub mod admin;
pub mod api;
mod catalog;
pub mod details;
mod index;
pub mod manifest;
mod pool;
pub mod server;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use kube::runtime::watcher;
use parking_lot::RwLock;

use index::{Endpoint, Index};
pub use index::{decode_deployment_id, encode_deployment_id};

use crate::resources::restatedeploymentendpoints::RestateDeploymentEndpoint;

/// The registry's index. Cheap to clone: every clone is the same index.
///
/// See the `index` module for what's kept per endpoint (~300 B for a one-service endpoint, the
/// service shapes shared across versions and tenants).
#[derive(Clone, Default)]
pub struct Registry {
    index: Arc<RwLock<Index>>,
}

/// An indexed endpoint, for the controllers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointInfo {
    pub namespace: String,
    pub name: String,
    pub deployment_id: String,
    pub tenant: String,
    /// Its services, by name.
    pub services: Vec<ServiceRevision>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceRevision {
    pub name: String,
    pub revision: u32,
    /// Whether this endpoint is the latest of the service in its tenant.
    pub latest: bool,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies a watch event of the `RestateDeploymentEndpoint` watcher (all namespaces).
    ///
    /// Only endpoints with `status.phase: Ready`, a valid `status.tenant`, a `status.deploymentId`
    /// (the one derived from their uid) and `status.discovered` are indexed, under `spec.cluster`.
    /// `Apply` upserts (or removes what's no longer indexable), `Delete` removes. A relist
    /// (`Init`, `InitApply`…, `InitDone`) is a mark-and-sweep in place: the entries keep answering
    /// until `InitDone`, which removes the ones not seen again.
    pub fn apply(&self, event: &watcher::Event<RestateDeploymentEndpoint>) {
        self.index.write().apply(event);
    }

    /// Whether the index has been fully listed once (the first `InitDone`). Until then the lookups
    /// answer `503`: never a false "not found" after a restart.
    pub fn is_synced(&self) -> bool {
        self.index.read().synced
    }

    /// The lookups' `version`: bumped by every change of the index (per process).
    pub fn version(&self) -> u64 {
        self.index.read().version
    }

    /// The number of indexed endpoints.
    pub fn len(&self) -> usize {
        self.index.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Reserves the next revision of each of `services` in `(cluster, tenant)` for the endpoint
    /// `deployment_id`, and returns them (in `services` order): one more than the highest of the
    /// indexed endpoints' revisions and of the outstanding reservations.
    ///
    /// The reservation is kept until `apply` indexes `deployment_id` with (at least) these
    /// revisions, the endpoint is deleted, [`Registry::release_reservation`] is called, or 10
    /// minutes pass. Reserving again for the same `deployment_id` replaces its reservation.
    pub fn reserve_revisions(
        &self,
        cluster: &str,
        tenant: &str,
        deployment_id: &str,
        services: &[String],
    ) -> Vec<u32> {
        self.index
            .write()
            .reserve_revisions(cluster, tenant, deployment_id, services)
    }

    /// Drops the reservation of `deployment_id` (e.g. its status patch failed).
    pub fn release_reservation(&self, cluster: &str, deployment_id: &str) {
        self.index
            .write()
            .release_reservation(cluster, deployment_id);
    }

    /// The deployment id of the latest endpoint of `service` in `(cluster, tenant)`: the highest
    /// `(revision, creationTimestamp, uid)`.
    pub fn latest(&self, cluster: &str, tenant: &str, service: &str) -> Option<String> {
        self.index
            .read()
            .latest(cluster, tenant, service)
            .map(encode_deployment_id)
    }

    /// Whether the endpoint `deployment_id` is indexed in `cluster` and the latest of every one of
    /// its services.
    pub fn is_latest_for_all_services(&self, cluster: &str, deployment_id: &str) -> bool {
        let index = self.index.read();
        let Some(endpoint) =
            decode_deployment_id(deployment_id).and_then(|id| index.endpoint(cluster, id))
        else {
            return false;
        };
        endpoint
            .services
            .iter()
            .all(|service| index.is_latest(endpoint, service.name))
    }

    /// The highest revision of `service` among the indexed endpoints of `(cluster, tenant)`.
    pub fn max_revision(&self, cluster: &str, tenant: &str, service: &str) -> Option<u32> {
        self.index.read().max_revision(cluster, tenant, service)
    }

    /// The indexed endpoint `deployment_id` of `cluster`.
    pub fn endpoint(&self, cluster: &str, deployment_id: &str) -> Option<EndpointInfo> {
        let index = self.index.read();
        decode_deployment_id(deployment_id)
            .and_then(|id| index.endpoint(cluster, id))
            .map(|endpoint| info(&index, endpoint))
    }

    /// The indexed endpoints of `(cluster, tenant)`, by creation time then deployment id.
    pub fn tenant_endpoints(&self, cluster: &str, tenant: &str) -> Vec<EndpointInfo> {
        let index = self.index.read();
        index
            .tenant_endpoints(cluster, tenant)
            .into_iter()
            .map(|endpoint| info(&index, endpoint))
            .collect()
    }

    fn read(&self) -> parking_lot::RwLockReadGuard<'_, Index> {
        self.index.read()
    }
}

fn info(index: &Index, endpoint: &Endpoint) -> EndpointInfo {
    EndpointInfo {
        namespace: index.strings.get(endpoint.namespace).to_owned(),
        name: endpoint.name.to_string(),
        deployment_id: encode_deployment_id(endpoint.id),
        tenant: index.strings.get(endpoint.tenant).to_owned(),
        services: endpoint
            .services
            .iter()
            .map(|service| ServiceRevision {
                name: index.strings.get(service.name).to_owned(),
                revision: service.revision,
                latest: index.is_latest(endpoint, service.name),
            })
            .collect(),
    }
}
