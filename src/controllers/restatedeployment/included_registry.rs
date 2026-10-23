//! RestateDeployment against a cluster whose workflow registry is the relay's
//! (`workflowRegistry.mode: included`, `59-registry-replication.md`): each version is registered
//! in the registry API on the relay (`svc/relay :8090` in the cluster's namespace), for the tenant
//! named by the RestateDeployment's `restate.dev/tenant` label, instead of in Restate's own
//! registry.
//!
//! The rest of the flow is unchanged, as in operator mode (`operator_registry.rs`): the versions,
//! their ReplicaSets and Services, the readiness gate, the drain. Registering is the registry's
//! registration through discovery (`POST /v1/scopes/{tenant}/deployments`), "latest" comes from
//! the tenant's services, deregistering is `DELETE /deployments/{id}`. The drain still asks
//! Restate, which holds the invocations pinned to the deployment ids.
//!
//! The registry's ids are deterministic (D31): registering a version again returns its id, and a
//! deleted version registered again gets its id back with the next revisions.

use std::collections::{BTreeMap, HashMap};

use kube::ResourceExt;
use kube::runtime::reflector::ObjectRef;
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use tracing::info;

use crate::controllers::restatedeployment::cleanup::{
    CleanupMode, DeploymentUsage, DeploymentUsageMap,
};
use crate::controllers::restatedeployment::controller::{
    Context, RESTATE_DEPLOYMENT_ID_ANNOTATION, check_admin_response,
};
use crate::controllers::restatedeployment::operator_registry::{
    pinned_invocations, unpinned_invocations,
};
use crate::controllers::restatedeployment::registration::{
    Overwrite, RegisteredDeployment, post_deployment,
};
use crate::resources::restateclusters::RegistryMode;
use crate::resources::restatedeployments::RestateDeployment;
use crate::{Error, Result};

/// The label on a RestateDeployment naming the tenant (the Restate scope) its versions belong to.
pub const TENANT_LABEL: &str = "restate.dev/tenant";

/// The relay's registry API port (`[registry] api_listen`, on the `relay` Service).
const REGISTRY_API_PORT: u16 = 8090;

/// A RestateDeployment registering with an `included`-mode cluster.
#[derive(Debug, Clone)]
pub(super) struct Included {
    pub cluster: String,
    pub tenant: String,
    /// The registry API: `http://relay.<cluster namespace>.svc.<dns>:8090/`.
    api: url::Url,
}

/// The included-mode cluster this RestateDeployment registers with, if it does; an error if it
/// does but has no valid tenant label.
pub(super) fn included_cluster(ctx: &Context, rsd: &RestateDeployment) -> Result<Option<Included>> {
    let Some(name) = rsd.spec.restate.register.cluster.as_ref() else {
        return Ok(None);
    };
    let Some(cluster) = ctx.clusters_store.get(&ObjectRef::new(name)) else {
        return Ok(None);
    };
    if cluster.registry_mode() != Some(RegistryMode::Included) {
        return Ok(None);
    }
    let tenant = tenant(rsd)?;
    // the cluster's namespace is its name
    let api = url::Url::parse(&format!(
        "http://relay.{name}.svc.{}:{REGISTRY_API_PORT}/",
        ctx.cluster_dns
    ))?;
    Ok(Some(Included {
        cluster: name.clone(),
        tenant,
        api,
    }))
}

/// The tenant from the `restate.dev/tenant` label: a Restate scope (1–36 of `[A-Za-z0-9._-]`).
fn tenant(rsd: &RestateDeployment) -> Result<String> {
    let cluster = rsd
        .spec
        .restate
        .register
        .cluster
        .as_deref()
        .unwrap_or_default();
    let tenant = rsd.labels().get(TENANT_LABEL).ok_or_else(|| {
        Error::InvalidRestateConfig(format!(
            "RestateCluster {cluster} keeps its workflow registry in the relay \
             (workflowRegistry.mode: included): label the RestateDeployment with \
             {TENANT_LABEL}: <tenant> to say whose deployment it is"
        ))
    })?;
    if !is_scope(tenant) {
        return Err(Error::InvalidRestateConfig(format!(
            "label {TENANT_LABEL}: {tenant:?} is not a valid tenant: 1 to 36 letters, digits, \
             '.', '_' or '-'"
        )));
    }
    Ok(tenant.clone())
}

fn is_scope(value: &str) -> bool {
    (1..=36).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

impl Included {
    fn url(&self, path: &str) -> Result<url::Url> {
        Ok(self.api.join(path)?)
    }

    fn scoped(&self, path: &str) -> Result<url::Url> {
        self.url(&format!("v1/scopes/{}/{path}", self.tenant))
    }
}

#[derive(Deserialize)]
struct ListServices {
    #[serde(default)]
    services: Vec<ServiceLatest>,
}

#[derive(Deserialize)]
struct ServiceLatest {
    name: String,
    deployment_id: String,
}

#[derive(Deserialize)]
struct ListDeployments {
    #[serde(default)]
    deployments: Vec<Deployment>,
}

#[derive(Deserialize)]
struct Deployment {
    id: String,
    #[serde(default)]
    services: Vec<ServiceRevision>,
}

#[derive(Deserialize)]
struct ServiceRevision {
    name: String,
    #[serde(default)]
    revision: u32,
}

async fn get_json<T: for<'de> Deserialize<'de>>(ctx: &Context, url: url::Url) -> Result<T> {
    let resp = ctx
        .http_client
        .get(url)
        .send()
        .await
        .map_err(Error::AdminCallFailed)?;
    check_admin_response(resp)
        .await?
        .json()
        .await
        .map_err(Error::AdminCallFailed)
}

/// Service name -> the deployment its new invocations start on, in the tenant's view.
async fn latest_by_service(ctx: &Context, included: &Included) -> Result<HashMap<String, String>> {
    let services: ListServices = get_json(ctx, included.scoped("services")?).await?;
    Ok(services
        .services
        .into_iter()
        .map(|s| (s.name, s.deployment_id))
        .collect())
}

/// The tenant's deployments in the registry, by id.
async fn tenant_deployments(
    ctx: &Context,
    included: &Included,
) -> Result<HashMap<String, Deployment>> {
    let mut url = included.url("deployments")?;
    url.query_pairs_mut()
        .append_pair("tenant", &included.tenant);
    let list: ListDeployments = get_json(ctx, url).await?;
    Ok(list
        .deployments
        .into_iter()
        .map(|d| (d.id.clone(), d))
        .collect())
}

/// Every service of `services` starts its new invocations on `id`.
fn latest_for_all(latest: &HashMap<String, String>, id: &str, services: &[String]) -> bool {
    !services.is_empty()
        && services
            .iter()
            .all(|service| latest.get(service).map(String::as_str) == Some(id))
}

/// The fast path: the recorded deployment is the latest of all its services.
pub(super) async fn is_latest(ctx: &Context, included: &Included, id: &str) -> Result<bool> {
    let deployments = tenant_deployments(ctx, included).await?;
    let Some(deployment) = deployments.get(id) else {
        return Ok(false);
    };
    let services: Vec<String> = deployment.services.iter().map(|s| s.name.clone()).collect();
    Ok(latest_for_all(
        &latest_by_service(ctx, included).await?,
        id,
        &services,
    ))
}

/// Register a version: the registry discovers it and stores it as the tenant's. Already
/// registered ⇒ the same id back (D31). A promotion (rollback onto a version the registry still
/// has) can't bump its revisions in place, so it's deleted and registered again: same id, the
/// next revisions. In between, a lookup of it misses and the relay retries the attempt.
pub(super) async fn register(
    ctx: &Context,
    included: &Included,
    service_endpoint: &url::Url,
    use_http11: Option<bool>,
    overwrite: Overwrite,
    recorded_id: Option<&str>,
) -> Result<RegisteredDeployment> {
    if overwrite == Overwrite::Yes
        && let Some(id) = recorded_id
    {
        info!(
            deployment_id = id,
            tenant = %included.tenant,
            "Promoting a deployment in the relay's registry: deleting it to register it again"
        );
        delete(ctx, included, id).await?;
    }

    let mut payload = serde_json::json!({ "uri": service_endpoint });
    if let Some(use_http11) = use_http11 {
        payload["use_http_11"] = serde_json::Value::Bool(use_http11);
    }
    let registered = post_deployment(
        ctx.http_client
            .request(Method::POST, included.scoped("deployments")?),
        &payload,
    )
    .await?;
    info!(
        deployment_id = %registered.id,
        tenant = %included.tenant,
        url = %service_endpoint,
        "Registered the deployment in the relay's registry"
    );
    Ok(registered)
}

/// Every service of the registered deployment now starts its new invocations on it. A relay pod
/// other than the writer may still be catching up (~1 s): that's a requeue.
pub(super) async fn confirm_latest(
    ctx: &Context,
    included: &Included,
    registered: &RegisteredDeployment,
) -> Result<()> {
    if registered.services.is_empty()
        || latest_for_all(
            &latest_by_service(ctx, included).await?,
            &registered.id,
            &registered.services,
        )
    {
        return Ok(());
    }
    Err(Error::DeploymentNotLatest {
        message: format!(
            "Deployment {} is registered but the relay's registry doesn't route all of its \
             services ({}) to it yet",
            registered.id,
            registered.services.join(", ")
        ),
        reason: "NotLatest".into(),
        requeue_after: Some(std::time::Duration::from_secs(2)),
    })
}

/// Deregister a version (idempotent).
pub(super) async fn delete(ctx: &Context, included: &Included, id: &str) -> Result<()> {
    let resp = ctx
        .http_client
        .delete(included.url(&format!("deployments/{id}"))?)
        .send()
        .await
        .map_err(Error::AdminCallFailed)?;
    if resp.status() != StatusCode::NOT_FOUND {
        check_admin_response(resp).await?;
    }
    Ok(())
}

/// The deployments of this RestateDeployment that the registry holds, and what's still running
/// on them: the included-mode equivalent of the usage query over `sys_deployment`. A version
/// the registry no longer has reads as "removed by other means", like a deployment missing from
/// Restate's registry.
pub(super) async fn list_deployments(
    ctx: &Context,
    rsd: &RestateDeployment,
    included: &Included,
    mode: CleanupMode,
) -> Result<DeploymentUsageMap> {
    let namespace = rsd.namespace().unwrap_or_default();
    let uid = rsd.uid().unwrap_or_default();

    // the deployment ids recorded on this RestateDeployment's versions
    let ids: Vec<String> = ctx
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
            rs.annotations()
                .get(RESTATE_DEPLOYMENT_ID_ANNOTATION)
                .cloned()
        })
        .collect();
    if ids.is_empty() {
        return Ok(DeploymentUsageMap::new());
    }

    let deployments = tenant_deployments(ctx, included).await?;
    let latest = latest_by_service(ctx, included).await?;

    let mut usage = DeploymentUsageMap::new();
    // service -> (revision, deployment id) of our deployments serving it, for attributing the
    // unpinned invocations
    let mut serving: BTreeMap<String, Vec<(u32, String)>> = BTreeMap::new();
    for id in &ids {
        let Some(deployment) = deployments.get(id) else {
            continue;
        };
        let latest_for_service = deployment
            .services
            .iter()
            .any(|s| latest.get(&s.name).map(String::as_str) == Some(id.as_str()));
        for service in &deployment.services {
            serving
                .entry(service.name.clone())
                .or_default()
                .push((service.revision, id.clone()));
        }
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

    if mode.is_deleting() {
        // attributed to the deployment holding the service's latest revision among ours: the
        // one a not yet started invocation would run on
        for (service, unpinned) in
            unpinned_invocations(ctx, rsd, &included.tenant, serving.keys()).await?
        {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::restatedeployments::RestateDeploymentSpec;

    fn rsd(labels: &[(&str, &str)]) -> RestateDeployment {
        let mut rsd = RestateDeployment::new(
            "checkout",
            serde_json::from_value::<RestateDeploymentSpec>(serde_json::json!({
                "replicas": 1,
                "revisionHistoryLimit": 10,
                "template": { "spec": { "containers": [{ "name": "main", "image": "checkout:v1" }] } },
                "restate": { "register": { "cluster": "restate" } },
            }))
            .unwrap(),
        );
        rsd.metadata.labels = Some(
            labels
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        );
        rsd
    }

    #[test]
    fn the_tenant_comes_from_the_label() {
        assert_eq!(tenant(&rsd(&[(TENANT_LABEL, "t1")])).unwrap(), "t1");
        assert_eq!(
            tenant(&rsd(&[(
                TENANT_LABEL,
                "0b1c2d3e-aaaa-bbbb-cccc-0123456789ab"
            )]))
            .unwrap(),
            "0b1c2d3e-aaaa-bbbb-cccc-0123456789ab"
        );
        for bad in [
            rsd(&[]),
            rsd(&[(TENANT_LABEL, "")]),
            rsd(&[(TENANT_LABEL, "a/b")]),
            rsd(&[(TENANT_LABEL, &"x".repeat(37))]),
        ] {
            assert!(matches!(tenant(&bad), Err(Error::InvalidRestateConfig(_))));
        }
    }

    #[test]
    fn latest_for_all_needs_every_service() {
        let latest = HashMap::from([
            ("checkout".to_owned(), "dp_1a".to_owned()),
            ("refunds".to_owned(), "dp_1b".to_owned()),
        ]);
        let one = ["checkout".to_owned()];
        let both = ["checkout".to_owned(), "refunds".to_owned()];
        assert!(latest_for_all(&latest, "dp_1a", &one));
        assert!(!latest_for_all(&latest, "dp_1a", &both));
        assert!(!latest_for_all(&latest, "dp_1a", &[]));
        assert!(!latest_for_all(&latest, "dp_1c", &["unknown".to_owned()]));
    }

    #[test]
    fn urls() {
        let included = Included {
            cluster: "restate".into(),
            tenant: "t1".into(),
            api: url::Url::parse("http://relay.restate.svc.cluster.local:8090/").unwrap(),
        };
        assert_eq!(
            included.scoped("deployments").unwrap().as_str(),
            "http://relay.restate.svc.cluster.local:8090/v1/scopes/t1/deployments"
        );
        assert_eq!(
            included.url("deployments/dp_1x").unwrap().as_str(),
            "http://relay.restate.svc.cluster.local:8090/deployments/dp_1x"
        );
    }
}
