//! Deciding what to tell Restate about the current version's endpoint, and confirming it
//! was heard.
//!
//! Registration and cleanup ask different questions of the same data. Cleanup asks "may I
//! remove this deployment?", for which in-flight invocations matter as much as being a
//! service's endpoint — that is [`DeploymentUsage::is_active`]. Registration asks "does new
//! work go here?", for which only the endpoint matters. Conflating the two is what let a
//! rollback onto a version that still held a pinned invocation read as "already handled"
//! and leave the newer version serving traffic.

use std::collections::{BTreeSet, HashMap};

use kube::runtime::reflector::Store;
use kube::{Resource, ResourceExt};
use reqwest::Method;
use serde::Deserialize;

use crate::controllers::restatedeployment::cleanup::DeploymentUsageMap;
use crate::controllers::restatedeployment::controller::{
    Context, RESTATE_DEPLOYMENT_ID_ANNOTATION, check_admin_response,
};
use crate::resources::restatedeployments::{RestateAdminEndpoint, RestateDeployment};
use crate::{Error, Result};

/// Whether a registration may replace an existing deployment at the same address.
///
/// Restate maps this onto its `force` flag, which is the only mechanism currently able to
/// move a service's latest revision onto an already-registered deployment without minting
/// a new deployment id. `PUT /deployments/{id}` deliberately preserves the revision number
/// and so cannot promote. See restatedev/restate#5157 for the intended replacement, which
/// would let `Yes` become an explicit "select this deployment" call instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Overwrite {
    No,
    Yes,
}

impl Overwrite {
    /// Restate's `force` also implies `breaking`: there is no overwrite-without-breaking
    /// flag. That is tolerable for a rollback — reversing a breaking change is what the
    /// user asked for — but it is why this is never sent on a first registration.
    fn force(self) -> bool {
        self == Self::Yes
    }
}

/// What the operator must do to make this version the one Restate routes new invocations to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RegistrationAction {
    /// Nothing recorded, or Restate has never heard of the recorded id.
    Register,
    /// Restate knows this id but another of *our own* versions is serving new invocations —
    /// a rollback. Re-register the same endpoint with overwrite so its service revisions are
    /// bumped past the current latest, keeping the deployment id, and everything pinned to
    /// it, intact. Carries the id that currently holds latest, for the event the operator
    /// emits afterwards.
    Promote { superseded_by: String },
    /// Already serving new invocations. Leave it alone. Carries the recorded id, which this
    /// variant can only be reached with — holding it here is what saves the caller from
    /// re-deriving it from an `Option` it has already proven to be `Some`.
    AlreadyLatest { deployment_id: String },
    /// Our deployment is registered but superseded, and no version of this RestateDeployment
    /// is serving these services either — so something outside it is. Forcing here would
    /// start a promotion war between two controllers, each bumping revisions to take the
    /// service back, so refuse to write anything and report instead.
    Conflict,
}

impl RegistrationAction {
    pub(super) fn overwrite(&self) -> Overwrite {
        match self {
            Self::Promote { .. } => Overwrite::Yes,
            _ => Overwrite::No,
        }
    }
}

/// Decide from what Restate already told us, without any further admin calls.
///
/// `owned_ids` yields the deployment ids recorded on the versioned objects belonging to this
/// RestateDeployment — the evidence that distinguishes a rollback (one of ours holds latest)
/// from a foreign controller having taken the service over. It is a closure because
/// collecting it means walking a cluster-wide reflector cache, and the steady state
/// (`AlreadyLatest`) never needs the answer.
///
/// Known limitation: `latest_for_service` is true when a deployment is latest for *any* of
/// its services, so a deployment that is latest for some and superseded for others reads as
/// `AlreadyLatest` here. That state needs a version exposing a strict subset of another's
/// services to arise. [`confirm_latest`] catches it after a write; catching it on the
/// untouched path needs per-service data the usage query does not carry, and is deliberately
/// left out of this change.
pub(super) fn plan_registration(
    recorded_id: Option<&str>,
    deployments: &DeploymentUsageMap,
    owned_ids: impl FnOnce() -> BTreeSet<String>,
) -> RegistrationAction {
    let Some(recorded_id) = recorded_id else {
        return RegistrationAction::Register;
    };

    let Some(usage) = deployments.get(recorded_id) else {
        // Registered under an id Restate no longer has — deregistered out of band, or a
        // brand new endpoint. Either way a plain registration is what's wanted.
        return RegistrationAction::Register;
    };

    if usage.latest_for_service {
        return RegistrationAction::AlreadyLatest {
            deployment_id: recorded_id.to_owned(),
        };
    }

    // Our deployment exists but is superseded. It can only have been superseded by whoever
    // registered these services next; if that is one of our own versions this is a rollback.
    // Ordered, so a (pathological) tie between two of our versions still names the same one
    // every reconcile rather than flapping in what the operator reports.
    let superseded_by = owned_ids()
        .into_iter()
        .filter(|id| id != recorded_id)
        .find(|id| {
            deployments
                .get(id)
                .is_some_and(|usage| usage.latest_for_service)
        });

    match superseded_by {
        Some(superseded_by) => RegistrationAction::Promote { superseded_by },
        None => RegistrationAction::Conflict,
    }
}

/// The deployment ids recorded on the versioned objects owned by this RestateDeployment
/// (ReplicaSets in ReplicaSet mode, Configurations in Knative mode).
pub(super) fn owned_deployment_ids<K>(
    store: &Store<K>,
    namespace: &str,
    rsd_uid: &str,
) -> BTreeSet<String>
where
    K: Resource + Clone + 'static,
    K::DynamicType: std::hash::Hash + Eq + Clone + Default,
{
    store
        .state()
        .into_iter()
        .filter(|obj| {
            let obj_namespace = match obj.meta().namespace.as_deref() {
                Some("") | None => "default",
                Some(ns) => ns,
            };
            obj_namespace == namespace
                && obj.owner_references().iter().any(|reference| {
                    reference.uid == rsd_uid
                        && reference.kind == <RestateDeployment as Resource>::kind(&())
                })
        })
        .filter_map(|obj| {
            obj.annotations()
                .get(RESTATE_DEPLOYMENT_ID_ANNOTATION)
                .cloned()
        })
        .collect()
}

/// Whether this RestateDeployment owns any versioned object other than `except_name` in the
/// namespace — i.e. whether a previous version exists that cleanup might still need to drain.
/// Read from the already-synced reflector cache, so it costs no admin call and mirrors the
/// filter [`cleanup_old_replicasets`](super::reconcilers::replicaset::cleanup_old_replicasets)
/// applies when deciding what to drain.
pub(super) fn has_other_owned<K>(
    store: &Store<K>,
    namespace: &str,
    rsd_uid: &str,
    except_name: &str,
) -> bool
where
    K: Resource + Clone + 'static,
    K::DynamicType: std::hash::Hash + Eq + Clone + Default,
{
    store.state().into_iter().any(|obj| {
        let obj_namespace = match obj.meta().namespace.as_deref() {
            Some("") | None => "default",
            Some(ns) => ns,
        };
        obj_namespace == namespace
            && obj.name_any() != except_name
            && obj.owner_references().iter().any(|reference| {
                reference.uid == rsd_uid
                    && reference.kind == <RestateDeployment as Resource>::kind(&())
            })
    })
}

/// A deployment as Restate returned it from registration.
#[derive(Debug, Clone)]
pub(super) struct RegisteredDeployment {
    pub id: String,
    /// The services Restate discovered at this endpoint — what [`confirm_latest`] checks.
    pub services: Vec<String>,
}

#[derive(Deserialize)]
struct RegisterDeploymentResponse {
    id: String,
    #[serde(default)]
    services: Vec<ServiceRef>,
}

#[derive(Deserialize)]
struct ServiceRef {
    name: String,
}

#[derive(Deserialize)]
struct ListServicesResponse {
    #[serde(default)]
    services: Vec<ServiceLatest>,
}

#[derive(Deserialize)]
struct ServiceLatest {
    name: String,
    deployment_id: String,
}

/// Register (or re-register) an endpoint.
pub(super) async fn register_deployment(
    ctx: &Context,
    rsd: &RestateDeployment,
    service_endpoint: &url::Url,
    use_http11: Option<bool>,
    overwrite: Overwrite,
) -> Result<RegisteredDeployment> {
    let endpoint = &rsd.spec.restate.register;

    tracing::debug!(
        force = overwrite.force(),
        "Registering endpoint '{service_endpoint}' to Restate at '{endpoint}'",
    );

    let mut payload = serde_json::json!({
        "uri": service_endpoint,
        // Always explicit. This flag's default depends on the admin API version the request
        // resolves to, and it has already changed once (restatedev/restate#3859) — silently
        // turning every re-registration into a no-op, which is the bug this path exists to
        // fix. Stating it keeps the operator's intent independent of that default.
        "force": overwrite.force(),
    });

    if let Some(use_http11) = use_http11 {
        payload["use_http_11"] = serde_json::Value::Bool(use_http11);
    }

    let resp = ctx
        .request(Method::POST, endpoint, "/deployments")?
        .json(&payload)
        .send()
        .await
        .map_err(Error::AdminCallFailed)?;
    let resp: RegisterDeploymentResponse = check_admin_response(resp)
        .await?
        .json()
        .await
        .map_err(Error::AdminCallFailed)?;

    tracing::info!(
        deployment_id = %resp.id,
        url = %service_endpoint,
        force = overwrite.force(),
        "Successfully registered Restate deployment"
    );

    Ok(RegisteredDeployment {
        id: resp.id,
        services: resp.services.into_iter().map(|svc| svc.name).collect(),
    })
}

/// Assert that Restate now routes every one of this deployment's services to it.
///
/// Registration's own response cannot answer this. A non-overwriting registration of an
/// endpoint Restate already knows returns 200 with the existing deployment id and its own
/// services attached, which is indistinguishable from success by inspection — the operator
/// used to take exactly that as proof and report `Ready=True` while the previous version
/// kept serving. `GET /services` is the authoritative view and is a metadata read, so it
/// costs nothing like the usage query.
pub(super) async fn confirm_latest(
    ctx: &Context,
    endpoint: &RestateAdminEndpoint,
    registered: &RegisteredDeployment,
) -> Result<()> {
    if registered.services.is_empty() {
        // Nothing discovered at the endpoint. Not something this function can adjudicate;
        // the deployment is registered but serves nothing, which cleanup will handle.
        return Ok(());
    }

    let latest = latest_deployment_by_service(ctx, endpoint).await?;

    let superseded: Vec<&str> = registered
        .services
        .iter()
        .filter(|service| latest.get(*service).map(String::as_str) != Some(registered.id.as_str()))
        .map(String::as_str)
        .collect();

    if superseded.is_empty() {
        return Ok(());
    }

    let detail = superseded
        .iter()
        .map(|service| match latest.get(*service) {
            Some(other) => format!("{service} -> {other}"),
            None => format!("{service} -> (unregistered)"),
        })
        .collect::<Vec<_>>()
        .join(", ");

    Err(Error::DeploymentNotLatest {
        message: format!(
            "Restate registered deployment {} but still routes new invocations elsewhere: {detail}",
            registered.id
        ),
        reason: "NotLatest".into(),
        requeue_after: None,
    })
}

async fn latest_deployment_by_service(
    ctx: &Context,
    endpoint: &RestateAdminEndpoint,
) -> Result<HashMap<String, String>> {
    let resp = ctx
        .request(Method::GET, endpoint, "/services")?
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(Error::AdminCallFailed)?;

    let resp: ListServicesResponse = check_admin_response(resp)
        .await?
        .json()
        .await
        .map_err(Error::AdminCallFailed)?;

    Ok(resp
        .services
        .into_iter()
        .map(|svc| (svc.name, svc.deployment_id))
        .collect())
}

/// The set of deployment ids Restate currently routes at least one service to.
///
/// This is the same `latest_for_service` fact the usage query carries, but read from
/// `GET /services` — a metadata call that never scans `sys_invocation_status`. It answers "is
/// my recorded version still the one taking new invocations?" cheaply enough to run on the hot
/// reconcile path, so the steady state can decide it is already latest without the usage query.
pub(super) async fn latest_deployment_ids(
    ctx: &Context,
    endpoint: &RestateAdminEndpoint,
) -> Result<BTreeSet<String>> {
    Ok(latest_deployment_by_service(ctx, endpoint)
        .await?
        .into_values()
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controllers::restatedeployment::cleanup::DeploymentUsage;

    fn usage(latest: bool, pinned: u64) -> DeploymentUsage {
        DeploymentUsage {
            latest_for_service: latest,
            pinned_invocations: pinned,
            unpinned_invocations: 0,
        }
    }

    fn owned(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|id| id.to_string()).collect()
    }

    fn promote(superseded_by: &str) -> RegistrationAction {
        RegistrationAction::Promote {
            superseded_by: superseded_by.into(),
        }
    }

    fn already_latest(deployment_id: &str) -> RegistrationAction {
        RegistrationAction::AlreadyLatest {
            deployment_id: deployment_id.into(),
        }
    }

    #[test]
    fn nothing_recorded_registers() {
        assert_eq!(
            plan_registration(None, &DeploymentUsageMap::new(), || owned(&[])),
            RegistrationAction::Register
        );
    }

    #[test]
    fn recorded_but_unknown_to_restate_registers() {
        // deregistered out of band; a plain registration re-creates it
        let deployments = DeploymentUsageMap::from([("dp_other".into(), usage(true, 0))]);
        assert_eq!(
            plan_registration(Some("dp_gone"), &deployments, || owned(&["dp_gone"])),
            RegistrationAction::Register
        );
    }

    #[test]
    fn latest_deployment_is_left_alone() {
        let deployments = DeploymentUsageMap::from([("dp_v1".into(), usage(true, 0))]);
        assert_eq!(
            plan_registration(Some("dp_v1"), &deployments, || owned(&["dp_v1"])),
            already_latest("dp_v1")
        );
    }

    /// The id travels with the verdict so the caller never has to assert an invariant the
    /// planner already established. Previously the Knative path recovered it by unwrapping
    /// the same `Option` it had passed in, which put a panic on the reconcile path.
    #[test]
    fn already_latest_carries_the_recorded_id() {
        let deployments = DeploymentUsageMap::from([("dp_v1".into(), usage(true, 3))]);
        let RegistrationAction::AlreadyLatest { deployment_id } =
            plan_registration(Some("dp_v1"), &deployments, || owned(&["dp_v1"]))
        else {
            panic!("expected AlreadyLatest");
        };
        assert_eq!(deployment_id, "dp_v1");
    }

    /// The steady state must not pay for the cluster-wide reflector walk that only a
    /// rollback needs.
    #[test]
    fn the_untouched_path_never_collects_the_owned_ids() {
        let deployments = DeploymentUsageMap::from([("dp_v1".into(), usage(true, 0))]);
        plan_registration(Some("dp_v1"), &deployments, || {
            panic!("owned ids collected on the AlreadyLatest path")
        });
        plan_registration(None, &deployments, || {
            panic!("owned ids collected on the Register path")
        });
    }

    #[test]
    fn rollback_onto_a_drained_version_promotes() {
        let deployments = DeploymentUsageMap::from([
            ("dp_v1".into(), usage(false, 0)),
            ("dp_v2".into(), usage(true, 0)),
        ]);
        assert_eq!(
            plan_registration(Some("dp_v1"), &deployments, || owned(&["dp_v1", "dp_v2"])),
            promote("dp_v2")
        );
    }

    /// The #174 regression: a rolled-back version still holding a pinned invocation used to
    /// read as "active" and skip registration entirely, leaving the newer version latest.
    /// In-flight work must have no bearing on the decision.
    #[test]
    fn rollback_promotes_even_while_pinned_invocations_remain() {
        let deployments = DeploymentUsageMap::from([
            ("dp_v1".into(), usage(false, 7)),
            ("dp_v2".into(), usage(true, 0)),
        ]);
        assert_eq!(
            plan_registration(Some("dp_v1"), &deployments, || owned(&["dp_v1", "dp_v2"])),
            promote("dp_v2")
        );
    }

    /// ...and the same holds when the version that superseded it is itself draining work.
    #[test]
    fn promotion_is_unaffected_by_the_superseding_versions_workload() {
        let deployments = DeploymentUsageMap::from([
            ("dp_v1".into(), usage(false, 0)),
            ("dp_v2".into(), usage(true, 12)),
        ]);
        assert_eq!(
            plan_registration(Some("dp_v1"), &deployments, || owned(&["dp_v1", "dp_v2"])),
            promote("dp_v2")
        );
    }

    /// Forcing here would fight whatever else is registering the service: each controller
    /// would bump revisions to take it back, forever, rediscovering both endpoints each time.
    #[test]
    fn a_foreign_owner_is_a_conflict_not_a_promotion() {
        let deployments = DeploymentUsageMap::from([
            ("dp_mine".into(), usage(false, 0)),
            ("dp_theirs".into(), usage(true, 0)),
        ]);
        assert_eq!(
            plan_registration(Some("dp_mine"), &deployments, || owned(&["dp_mine"])),
            RegistrationAction::Conflict
        );
    }

    /// A superseded deployment whose successor is also superseded (two rollbacks deep) is
    /// still ours to promote, as long as some version of ours holds latest — and the version
    /// named as having superseded it is the one actually serving, not the one in between.
    #[test]
    fn promotion_looks_past_intermediate_superseded_versions() {
        let deployments = DeploymentUsageMap::from([
            ("dp_v1".into(), usage(false, 0)),
            ("dp_v2".into(), usage(false, 0)),
            ("dp_v3".into(), usage(true, 0)),
        ]);
        assert_eq!(
            plan_registration(Some("dp_v1"), &deployments, || owned(&[
                "dp_v1", "dp_v2", "dp_v3"
            ])),
            promote("dp_v3")
        );
    }

    /// No version of ours holds latest and the map names nobody who does. Registration
    /// creates service revisions, so something must be holding them; not being able to see
    /// what — a stale cache, a deployment removed from under us — is exactly when forcing is
    /// least safe. The guard stays shut and reports rather than guessing.
    #[test]
    fn all_of_our_versions_superseded_by_nobody_is_a_conflict() {
        let deployments = DeploymentUsageMap::from([
            ("dp_v1".into(), usage(false, 0)),
            ("dp_v2".into(), usage(false, 0)),
        ]);
        assert_eq!(
            plan_registration(Some("dp_v1"), &deployments, || owned(&["dp_v1", "dp_v2"])),
            RegistrationAction::Conflict
        );
    }

    #[test]
    fn only_promotion_overwrites() {
        assert_eq!(RegistrationAction::Register.overwrite(), Overwrite::No);
        assert_eq!(promote("dp_v2").overwrite(), Overwrite::Yes);
        assert_eq!(already_latest("dp_v1").overwrite(), Overwrite::No);
        assert_eq!(RegistrationAction::Conflict.overwrite(), Overwrite::No);
        assert!(!Overwrite::No.force());
        assert!(Overwrite::Yes.force());
    }

    mod has_other_owned {
        use super::super::has_other_owned;
        use k8s_openapi::api::apps::v1::ReplicaSet;
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
        use kube::runtime::{reflector, watcher};

        const UID: &str = "rd-uid";
        const NS: &str = "app";

        fn replica_set(name: &str, namespace: &str, owner_uid: Option<&str>) -> ReplicaSet {
            ReplicaSet {
                metadata: ObjectMeta {
                    name: Some(name.into()),
                    namespace: Some(namespace.into()),
                    owner_references: owner_uid.map(|uid| {
                        vec![OwnerReference {
                            kind: "RestateDeployment".into(),
                            name: "rd".into(),
                            uid: uid.into(),
                            api_version: "restate.dev/v1beta1".into(),
                            ..Default::default()
                        }]
                    }),
                    ..Default::default()
                },
                ..Default::default()
            }
        }

        // The writer is returned boxed and must be kept alive: the Store reads through the
        // shared handle the writer owns.
        fn store_of(
            objects: Vec<ReplicaSet>,
        ) -> (reflector::Store<ReplicaSet>, Box<dyn std::any::Any>) {
            let (reader, mut writer) = reflector::store::<ReplicaSet>();
            writer.apply_watcher_event(&watcher::Event::Init);
            for object in objects {
                writer.apply_watcher_event(&watcher::Event::InitApply(object));
            }
            writer.apply_watcher_event(&watcher::Event::InitDone);
            (reader, Box::new(writer))
        }

        #[test]
        fn a_lone_version_has_no_others() {
            let (store, _writer) = store_of(vec![replica_set("rd-current", NS, Some(UID))]);
            assert!(!has_other_owned(&store, NS, UID, "rd-current"));
        }

        #[test]
        fn an_older_owned_version_counts() {
            let (store, _writer) = store_of(vec![
                replica_set("rd-current", NS, Some(UID)),
                replica_set("rd-old", NS, Some(UID)),
            ]);
            assert!(has_other_owned(&store, NS, UID, "rd-current"));
        }

        #[test]
        fn foreign_other_namespace_and_orphaned_replicasets_do_not_count() {
            let (store, _writer) = store_of(vec![
                replica_set("rd-current", NS, Some(UID)),
                // ours, but in another namespace
                replica_set("rd-old", "other-ns", Some(UID)),
                // same namespace, owned by a different RestateDeployment
                replica_set("foreign", NS, Some("other-uid")),
                // same namespace, no controller at all
                replica_set("orphan", NS, None),
            ]);
            assert!(!has_other_owned(&store, NS, UID, "rd-current"));
        }
    }
}
