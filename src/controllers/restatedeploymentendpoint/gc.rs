//! The revision GC: what bounds the endpoints nobody deletes (`56-operator-registry.md`, "Who
//! holds what, when").
//!
//! A RestateDeployment drains and deletes its own versions' endpoints. A hand-made endpoint is
//! its creator's to delete, and a creator that never does would grow the catalog (and the
//! operator's memory) forever. So per tenant and service the operator keeps the latest
//! endpoint and the `MAX_REVISIONS_PER_SERVICE` newest before it; an older one that is the
//! latest of none of its services and has nothing pinned to it in Restate is deleted.

use std::collections::HashSet;

use kube::api::DeleteParams;
use kube::{Api, Client, Resource, ResourceExt};
use serde::Deserialize;
use tracing::{debug, info, warn};

use crate::registry::{EndpointInfo, Registry};
use crate::resources::restatedeploymentendpoints::RestateDeploymentEndpoint;
use crate::resources::restatedeployments::RestateDeployment;
use crate::{Error, Result};

/// The non-latest revisions of a service kept per tenant.
pub const MAX_REVISIONS_PER_SERVICE: usize = 3;

/// The endpoints of a tenant (all of one cluster) that the GC may delete, provided nothing is
/// pinned to them: the latest of none of their services, and for each of them at least `keep`
/// newer non-latest revisions exist.
pub fn candidates(endpoints: &[EndpointInfo], keep: usize) -> Vec<&EndpointInfo> {
    endpoints
        .iter()
        .filter(|endpoint| {
            !endpoint.services.is_empty()
                && endpoint.services.iter().all(|service| {
                    !service.latest
                        && endpoints
                            .iter()
                            .filter(|other| other.deployment_id != endpoint.deployment_id)
                            .flat_map(|other| &other.services)
                            .filter(|s| {
                                s.name == service.name && !s.latest && s.revision > service.revision
                            })
                            .count()
                            >= keep
                })
        })
        .collect()
}

#[derive(Deserialize)]
struct Rows {
    rows: Vec<PinnedRow>,
}

#[derive(Deserialize)]
struct PinnedRow {
    deployment_id: String,
}

/// Runs the GC for one tenant of an operator-mode cluster. Errors talking to Restate leave
/// everything in place (the next run tries again).
pub async fn collect(
    client: &Client,
    http: &reqwest::Client,
    registry: &Registry,
    cluster: &str,
    tenant: &str,
    cluster_dns: &str,
) -> Result<Collected> {
    let endpoints = registry.tenant_endpoints(cluster, tenant);
    let candidates = candidates(&endpoints, MAX_REVISIONS_PER_SERVICE);
    if candidates.is_empty() {
        return Ok(Collected::All);
    }

    // RestateDeployments drain their own: only hand-made endpoints are collected
    let mut unowned = Vec::new();
    for endpoint in candidates {
        let api: Api<RestateDeploymentEndpoint> =
            Api::namespaced(client.clone(), &endpoint.namespace);
        let Some(object) = api.get_opt(&endpoint.name).await? else {
            continue;
        };
        let owned = object
            .owner_references()
            .iter()
            .any(|owner| owner.kind == RestateDeployment::kind(&()));
        if !owned {
            unowned.push(endpoint);
        }
    }
    if unowned.is_empty() {
        return Ok(Collected::All);
    }

    let pinned = pinned_deployments(http, cluster, cluster_dns, &unowned).await?;
    let mut collected = Collected::All;
    for endpoint in unowned {
        if pinned.contains(&endpoint.deployment_id) {
            collected = Collected::SomePinned;
            debug!(
                "Keeping {} ({}/{}): invocations are still pinned to it",
                endpoint.deployment_id, endpoint.namespace, endpoint.name
            );
            continue;
        }
        info!(
            "Deleting RestateDeploymentEndpoint {}/{} ({}): more than {MAX_REVISIONS_PER_SERVICE} \
             newer revisions of its services, nothing pinned to it",
            endpoint.namespace, endpoint.name, endpoint.deployment_id
        );
        let api: Api<RestateDeploymentEndpoint> =
            Api::namespaced(client.clone(), &endpoint.namespace);
        match api.delete(&endpoint.name, &DeleteParams::default()).await {
            Ok(_) => {}
            Err(kube::Error::Api(e)) if e.code == 404 => {}
            Err(e) => warn!(
                "Could not delete RestateDeploymentEndpoint {}/{}: {e}",
                endpoint.namespace, endpoint.name
            ),
        }
    }
    Ok(collected)
}

/// What a GC run left behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Collected {
    /// Everything it could collect is gone.
    All,
    /// Some candidates still have invocations pinned to them: worth another run later.
    SomePinned,
}

/// The deployment ids, among `endpoints`, with unfinished invocations pinned to them.
async fn pinned_deployments(
    http: &reqwest::Client,
    cluster: &str,
    cluster_dns: &str,
    endpoints: &[&EndpointInfo],
) -> Result<HashSet<String>> {
    let ids = endpoints
        .iter()
        .map(|e| format!("'{}'", e.deployment_id.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(", ");
    let query = format!(
        "SELECT DISTINCT pinned_deployment_id AS deployment_id FROM sys_invocation_status \
         WHERE pinned_deployment_id IN ({ids}) AND status != 'completed'"
    );
    // the cluster's namespace is its name (see the RestateCluster controller)
    let url = format!("http://restate.{cluster}.svc.{cluster_dns}:9070/query");
    let resp = http
        .post(&url)
        .header(reqwest::header::ACCEPT, "application/json")
        .json(&serde_json::json!({ "query": query }))
        .send()
        .await
        .map_err(Error::AdminCallFailed)?;
    let rows: Rows = crate::controllers::restatedeployment::controller::check_admin_response(resp)
        .await?
        .json()
        .await
        .map_err(Error::AdminCallFailed)?;
    Ok(rows.rows.into_iter().map(|r| r.deployment_id).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::ServiceRevision;

    fn endpoint(id: &str, services: &[(&str, u32, bool)]) -> EndpointInfo {
        EndpointInfo {
            namespace: "t1".into(),
            name: id.into(),
            deployment_id: id.into(),
            tenant: "t1".into(),
            services: services
                .iter()
                .map(|(name, revision, latest)| ServiceRevision {
                    name: (*name).into(),
                    revision: *revision,
                    latest: *latest,
                })
                .collect(),
        }
    }

    fn ids(candidates: Vec<&EndpointInfo>) -> Vec<&str> {
        candidates
            .into_iter()
            .map(|e| e.deployment_id.as_str())
            .collect()
    }

    #[test]
    fn keeps_the_latest_and_the_newest_non_latest_revisions() {
        let endpoints: Vec<_> = (1..=6)
            .map(|r| endpoint(&format!("dp{r}"), &[("checkout", r, r == 6)]))
            .collect();
        // latest 6, kept 5 4 3, collected 2 1
        assert_eq!(ids(candidates(&endpoints, 3)), vec!["dp1", "dp2"]);
        // without the two oldest, nothing is past the kept three
        assert!(candidates(&endpoints[2..], 3).is_empty());
    }

    #[test]
    fn an_endpoint_still_latest_for_one_service_is_kept() {
        let mut endpoints: Vec<_> = (1..=5)
            .map(|r| endpoint(&format!("dp{r}"), &[("checkout", r, r == 5)]))
            .collect();
        endpoints[0].services.push(ServiceRevision {
            name: "cron".into(),
            revision: 1,
            latest: true,
        });
        assert_eq!(ids(candidates(&endpoints, 3)), Vec::<&str>::new());
    }

    #[test]
    fn every_service_must_have_enough_newer_revisions() {
        let mut endpoints: Vec<_> = (1..=5)
            .map(|r| endpoint(&format!("dp{r}"), &[("checkout", r, r == 5)]))
            .collect();
        // dp1 also serves a service with only one newer non-latest revision
        endpoints[0].services.push(ServiceRevision {
            name: "billing".into(),
            revision: 1,
            latest: false,
        });
        endpoints.push(endpoint("dpb2", &[("billing", 2, false)]));
        endpoints.push(endpoint("dpb3", &[("billing", 3, true)]));
        assert_eq!(ids(candidates(&endpoints, 3)), Vec::<&str>::new());
    }
}
