//! RestateDeployment against a cluster whose workflow registry is the operator
//! (`workflowRegistry.mode: operator`): each version becomes a RestateDeploymentEndpoint
//! instead of a registration in Restate's own registry.
//!
//! Everything else about a RestateDeployment stays: the versions, their ReplicaSets and
//! Services, the readiness gate, the drain. What changes is every admin API call the flow makes
//! about deployments (`56-operator-registry.md`, "RestateDeployment in operator mode"):
//! registering is creating the version's endpoint and waiting for its discovery, "latest" comes
//! from the registry's index, and deregistering is deleting the endpoint. The drain still asks
//! Restate, which still holds the invocations pinned to the deployment ids.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use k8s_openapi::api::apps::v1::ReplicaSetStatus;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{DeleteParams, PostParams};
use kube::runtime::reflector::ObjectRef;
use kube::{Api, Resource, ResourceExt};
use reqwest::Method;
use serde::Deserialize;
use tracing::{debug, info};

use crate::controllers::restatedeployment::cleanup::{
    CleanupMode, DeploymentUsage, DeploymentUsageMap,
};
use crate::controllers::restatedeployment::controller::{
    APP_MANAGED_BY_LABEL, Context, OWNED_BY_LABEL, RESTATE_DEPLOYMENT_ID_ANNOTATION,
    check_admin_response,
};
use crate::controllers::restatedeployment::registration::{Overwrite, RegisteredDeployment};
use crate::resources::restateclusters::RegistryMode;
use crate::resources::restatedeploymentendpoints::{
    CLUSTER_LABEL, EndpointPhase, RestateDeploymentEndpoint, RestateDeploymentEndpointSpec,
};
use crate::resources::restatedeployments::RestateDeployment;
use crate::{Error, Result};

/// How soon to look again while a version's endpoint is being discovered or indexed.
const DISCOVERY_POLL: Duration = Duration::from_secs(3);

/// The name of the operator-mode cluster this RestateDeployment registers with, if it does.
pub(super) fn operator_cluster(ctx: &Context, rsd: &RestateDeployment) -> Option<String> {
    let name = rsd.spec.restate.register.cluster.as_ref()?;
    let cluster = ctx.clusters_store.get(&ObjectRef::new(name))?;
    (cluster.registry_mode() == Some(RegistryMode::Operator)).then(|| name.clone())
}

/// Operator and included modes serve ReplicaSet versions only.
pub(super) fn reject_knative(ctx: &Context, rsd: &RestateDeployment) -> Result<()> {
    if let Some(cluster) = operator_cluster(ctx, rsd) {
        return Err(Error::InvalidRestateConfig(format!(
            "RestateCluster {cluster} serves its workflow registry from the operator \
             (workflowRegistry.mode: operator), which supports replicaset deployments only"
        )));
    }
    if let Some(included) = super::included_registry::included_cluster(ctx, rsd)? {
        return Err(Error::InvalidRestateConfig(format!(
            "RestateCluster {} keeps its workflow registry in the relay \
             (workflowRegistry.mode: included), which supports replicaset deployments only",
            included.cluster
        )));
    }
    Ok(())
}

fn not_indexed_yet() -> Error {
    Error::DeploymentNotReady {
        message: "Waiting for the operator's workflow registry to sync".into(),
        reason: "RegistryNotSynced".into(),
        requeue_after: Some(Duration::from_secs(1)),
        replica_set_status: None,
    }
}

/// The deployments of this RestateDeployment that the registry serves, and what's still
/// running on them: the operator-mode equivalent of the usage query over `sys_deployment`.
///
/// A deployment appears here iff its version's endpoint is Ready (so it's in the index): a
/// version whose endpoint is gone reads as "removed by other means", exactly like a
/// deployment missing from Restate's registry today.
pub(super) async fn list_deployments(
    ctx: &Context,
    rsd: &RestateDeployment,
    cluster: &str,
    mode: CleanupMode,
) -> Result<DeploymentUsageMap> {
    if !ctx.registry.is_synced() {
        return Err(not_indexed_yet());
    }
    let namespace = rsd.namespace().unwrap_or_default();
    let uid = rsd.uid().unwrap_or_default();
    let endpoints_api: Api<RestateDeploymentEndpoint> =
        Api::namespaced(ctx.client.clone(), &namespace);

    // the versions of this RestateDeployment that have a deployment id recorded
    let versions: Vec<(String, String)> = ctx
        .replicasets_store
        .state()
        .iter()
        .filter(|rs| rs.namespace().as_deref() == Some(namespace.as_str()))
        .filter(|rs| {
            rs.owner_references()
                .iter()
                .any(|owner| owner.uid == uid && owner.controller == Some(true))
        })
        .filter_map(|rs| {
            let id = rs.annotations().get(RESTATE_DEPLOYMENT_ID_ANNOTATION)?;
            Some((rs.name_any(), id.clone()))
        })
        .collect();

    let mut usage = DeploymentUsageMap::new();
    let mut tenant = None;
    // service -> (revision, deployment id) of our endpoints serving it, for attributing the
    // unpinned invocations
    let mut serving: HashMap<String, Vec<(u32, String)>> = HashMap::new();
    for (name, id) in &versions {
        let Some(endpoint) = endpoints_api.get_opt(name).await? else {
            continue;
        };
        let Some(status) = endpoint.status.as_ref() else {
            continue;
        };
        if status.phase != Some(EndpointPhase::Ready)
            || status.deployment_id.as_deref() != Some(id.as_str())
            || ctx.registry.endpoint(cluster, id).is_none()
        {
            continue;
        }
        let (Some(endpoint_tenant), Some(discovered)) = (&status.tenant, &status.discovered) else {
            continue;
        };
        let latest_for_service = discovered.services.iter().any(|service| {
            ctx.registry
                .latest(cluster, endpoint_tenant, &service.name)
                .as_deref()
                == Some(id.as_str())
        });
        for service in &discovered.services {
            serving
                .entry(service.name.clone())
                .or_default()
                .push((service.revision, id.clone()));
        }
        tenant = Some(endpoint_tenant.clone());
        usage.insert(
            id.clone(),
            DeploymentUsage {
                latest_for_service,
                ..Default::default()
            },
        );
    }

    if usage.is_empty() || mode == CleanupMode::ForceDeleting {
        return Ok(usage);
    }

    for (id, pinned) in pinned_invocations(ctx, rsd, usage.keys()).await? {
        if let Some(entry) = usage.get_mut(&id) {
            entry.pinned_invocations = pinned;
        }
    }

    if mode.is_deleting()
        && let Some(tenant) = tenant
    {
        // attributed to the endpoint holding the service's latest revision among ours: the
        // one a not yet started invocation would run on
        for (service, unpinned) in unpinned_invocations(ctx, rsd, &tenant, serving.keys()).await? {
            let Some((_, id)) = serving.get(&service).and_then(|v| v.iter().max()) else {
                continue;
            };
            if let Some(entry) = usage.get_mut(id) {
                entry.unpinned_invocations += unpinned;
            }
        }
    }

    Ok(usage)
}

#[derive(Deserialize)]
struct Rows<T> {
    rows: Vec<T>,
}

#[derive(Deserialize)]
struct PinnedRow {
    deployment_id: String,
    n: i64,
}

#[derive(Deserialize)]
struct UnpinnedRow {
    service: String,
    n: i64,
}

/// A SQL string literal. The values here are deployment ids, scopes and service names, which
/// can't hold quotes anyway; doubling them is the belt to that.
fn sql_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn sql_list<'a>(values: impl Iterator<Item = &'a String>) -> String {
    values
        .map(|v| sql_literal(v))
        .collect::<Vec<_>>()
        .join(", ")
}

pub(super) fn pinned_query<'a>(ids: impl Iterator<Item = &'a String>) -> String {
    format!(
        "SELECT pinned_deployment_id AS deployment_id, COUNT(*) AS n \
         FROM sys_invocation_status \
         WHERE pinned_deployment_id IN ({}) AND status != 'completed' \
         GROUP BY pinned_deployment_id",
        sql_list(ids)
    )
}

pub(super) fn unpinned_query<'a>(
    tenant: &str,
    services: impl Iterator<Item = &'a String>,
) -> String {
    format!(
        "SELECT target_service_name AS service, COUNT(*) AS n \
         FROM sys_invocation_status \
         WHERE pinned_deployment_id IS NULL AND status != 'completed' \
           AND scope = {} AND target_service_name IN ({}) \
         GROUP BY target_service_name",
        sql_literal(tenant),
        sql_list(services)
    )
}

async fn query<T: for<'de> Deserialize<'de>>(
    ctx: &Context,
    rsd: &RestateDeployment,
    sql: String,
) -> Result<Vec<T>> {
    let resp = ctx
        .request(Method::POST, &rsd.spec.restate.register, "/query")?
        .header(reqwest::header::ACCEPT, "application/json")
        .json(&serde_json::json!({ "query": sql }))
        .send()
        .await
        .map_err(Error::AdminCallFailed)?;
    let rows: Rows<T> = check_admin_response(resp)
        .await?
        .json()
        .await
        .map_err(Error::AdminCallFailed)?;
    Ok(rows.rows)
}

pub(super) async fn pinned_invocations<'a>(
    ctx: &Context,
    rsd: &RestateDeployment,
    ids: impl Iterator<Item = &'a String>,
) -> Result<Vec<(String, u64)>> {
    let rows: Vec<PinnedRow> = query(ctx, rsd, pinned_query(ids)).await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.deployment_id, row.n.max(0) as u64))
        .collect())
}

pub(super) async fn unpinned_invocations<'a>(
    ctx: &Context,
    rsd: &RestateDeployment,
    tenant: &str,
    services: impl Iterator<Item = &'a String>,
) -> Result<Vec<(String, u64)>> {
    let rows: Vec<UnpinnedRow> = query(ctx, rsd, unpinned_query(tenant, services)).await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.service, row.n.max(0) as u64))
        .collect())
}

/// The operator-mode fast path: the recorded deployment is the latest of all its services.
pub(super) fn is_latest(ctx: &Context, cluster: &str, deployment_id: &str) -> bool {
    ctx.registry.is_synced()
        && ctx
            .registry
            .is_latest_for_all_services(cluster, deployment_id)
}

/// "Register" a version: make sure its RestateDeploymentEndpoint exists, and once the operator
/// has discovered it, hand back its deployment id and services. A promotion (rollback) bumps
/// the endpoint's revisions past everything else.
#[allow(clippy::too_many_arguments)]
pub(super) async fn register(
    ctx: &Context,
    rsd: &RestateDeployment,
    cluster: &str,
    versioned_name: &str,
    service_endpoint: &url::Url,
    use_http11: Option<bool>,
    overwrite: Overwrite,
    replica_set_status: Option<&ReplicaSetStatus>,
) -> Result<RegisteredDeployment> {
    let namespace = rsd.namespace().unwrap_or_default();
    let api: Api<RestateDeploymentEndpoint> = Api::namespaced(ctx.client.clone(), &namespace);
    let waiting = |message: String, reason: &str| Error::DeploymentNotReady {
        message,
        reason: reason.into(),
        requeue_after: Some(DISCOVERY_POLL),
        replica_set_status: replica_set_status.cloned().map(Box::new),
    };

    let address = service_endpoint.to_string();
    let endpoint = match api.get_opt(versioned_name).await? {
        Some(endpoint) if endpoint.spec.address != address => {
            // can't happen while the address is derived from the versioned name; an endpoint
            // is immutable, so a different one is replaced
            info!(
                "Replacing RestateDeploymentEndpoint {namespace}/{versioned_name}: its address {} is not {address}",
                endpoint.spec.address
            );
            api.delete(versioned_name, &DeleteParams::default()).await?;
            return Err(waiting(
                format!("Replacing the endpoint {versioned_name}"),
                "Registering",
            ));
        }
        Some(endpoint) => endpoint,
        None => {
            let endpoint = new_endpoint(rsd, cluster, versioned_name, address, use_http11);
            debug!("Creating RestateDeploymentEndpoint {namespace}/{versioned_name}");
            api.create(&PostParams::default(), &endpoint).await?;
            return Err(waiting(
                format!("Waiting for the operator to discover {versioned_name}"),
                "Discovering",
            ));
        }
    };

    let status = endpoint.status.clone().unwrap_or_default();
    let condition = status
        .conditions
        .iter()
        .flatten()
        .find(|c| c.type_ == "Ready")
        .cloned();
    match status.phase {
        Some(EndpointPhase::Ready) => {}
        Some(EndpointPhase::Failed) => {
            let (reason, message) = condition
                .map(|c| (c.reason, c.message))
                .unwrap_or_else(|| ("Failed".into(), "discovery failed".into()));
            return Err(Error::DeploymentNotReady {
                message: format!("Endpoint {versioned_name}: {message}"),
                reason,
                requeue_after: Some(Duration::from_secs(30)),
                replica_set_status: replica_set_status.cloned().map(Box::new),
            });
        }
        _ => {
            let message = condition
                .map(|c| format!("Endpoint {versioned_name}: {}", c.message))
                .unwrap_or_else(|| {
                    format!("Waiting for the operator to discover {versioned_name}")
                });
            return Err(waiting(message, "Discovering"));
        }
    }

    let (Some(id), Some(discovered)) = (status.deployment_id.clone(), status.discovered.as_ref())
    else {
        return Err(waiting(
            format!("Waiting for the operator to discover {versioned_name}"),
            "Discovering",
        ));
    };
    let mut services: Vec<String> = discovered.services.iter().map(|s| s.name.clone()).collect();

    if overwrite == Overwrite::Yes {
        info!("Promoting {id} ({namespace}/{versioned_name}) back to the latest revision");
        services = crate::controllers::restatedeploymentendpoint::promote(
            &ctx.client,
            &ctx.registry,
            &endpoint,
        )
        .await?;
    }

    Ok(RegisteredDeployment { id, services })
}

fn new_endpoint(
    rsd: &RestateDeployment,
    cluster: &str,
    versioned_name: &str,
    address: String,
    use_http11: Option<bool>,
) -> RestateDeploymentEndpoint {
    let labels = BTreeMap::from([
        (OWNED_BY_LABEL.to_owned(), rsd.name_any()),
        (
            APP_MANAGED_BY_LABEL.to_owned(),
            "restate-operator".to_owned(),
        ),
        (CLUSTER_LABEL.to_owned(), cluster.to_owned()),
    ]);
    RestateDeploymentEndpoint {
        metadata: ObjectMeta {
            name: Some(versioned_name.to_owned()),
            namespace: rsd.namespace(),
            labels: Some(labels),
            owner_references: rsd.controller_owner_ref(&()).map(|owner| vec![owner]),
            ..Default::default()
        },
        spec: RestateDeploymentEndpointSpec {
            cluster: cluster.to_owned(),
            address,
            use_http11: use_http11.filter(|v| *v),
            additional_headers: BTreeMap::new(),
        },
        status: None,
    }
}

/// Every service of the registered deployment now routes new invocations to it.
pub(super) fn confirm_latest(
    ctx: &Context,
    cluster: &str,
    registered: &RegisteredDeployment,
) -> Result<()> {
    if registered.services.is_empty() || is_latest(ctx, cluster, &registered.id) {
        return Ok(());
    }
    Err(Error::DeploymentNotLatest {
        message: format!(
            "Deployment {} is discovered but the workflow registry doesn't route all of its \
             services ({}) to it yet",
            registered.id,
            registered.services.join(", ")
        ),
        reason: "NotLatest".into(),
        // usually the index catching up with the status write that just happened
        requeue_after: Some(Duration::from_secs(2)),
    })
}

/// Deregister a version: delete its endpoint (the registry stops serving it at once).
pub(super) async fn remove_endpoint(
    ctx: &Context,
    namespace: &str,
    versioned_name: &str,
) -> Result<()> {
    let api: Api<RestateDeploymentEndpoint> = Api::namespaced(ctx.client.clone(), namespace);
    debug!("Deleting RestateDeploymentEndpoint {namespace}/{versioned_name}");
    match api.delete(versioned_name, &DeleteParams::default()).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(e)) if e.code == 404 => Ok(()),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pinned_query_lists_the_ids() {
        let ids = ["dp_1a".to_owned(), "dp_1b".to_owned()];
        let sql = pinned_query(ids.iter());
        assert!(sql.contains("pinned_deployment_id IN ('dp_1a', 'dp_1b')"));
        assert!(sql.contains("status != 'completed'"));
    }

    #[test]
    fn the_unpinned_query_is_scoped_to_the_tenant() {
        let services = ["checkout".to_owned()];
        let sql = unpinned_query("t1", services.iter());
        assert!(sql.contains("scope = 't1'"));
        assert!(sql.contains("target_service_name IN ('checkout')"));
        assert!(sql.contains("pinned_deployment_id IS NULL"));
    }

    #[test]
    fn literals_are_quoted() {
        assert_eq!(sql_literal("a'b"), "'a''b'");
    }
}
