//! Policy decisions shared by the ReplicaSet and Knative cleanup reconcilers: what
//! Restate still needs a registered deployment for, how we ask it, how long we keep a
//! drained version around, and the drain deadline that records it.

use std::collections::HashMap;
use std::fmt::Debug;
use std::time::Duration;

use kube::api::{Api, Patch, PatchParams};
use kube::{Resource, ResourceExt};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use tracing::{debug, info};

use crate::Result;

use crate::resources::restatedeployments::{DeletePolicy, OnTimeout, RestateDeployment};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CleanupMode {
    Rollout,
    Deleting,
    /// Deleting without waiting: `deletePolicy: force`, or a drain with
    /// `onTimeout: force` whose timeout has run out.
    ForceDeleting,
}

impl CleanupMode {
    pub(crate) fn for_rsd(rsd: &RestateDeployment) -> Self {
        if rsd.metadata.deletion_timestamp.is_none() {
            return Self::Rollout;
        }

        match rsd.spec.restate.delete_policy() {
            DeletePolicy::Force => Self::ForceDeleting,
            DeletePolicy::Drain => match rsd.spec.restate.drain_on_timeout() {
                OnTimeout::Hold => Self::Deleting,
                OnTimeout::Force => match drain_deadline(rsd) {
                    Some(deadline) if deadline <= chrono::Utc::now() => Self::ForceDeleting,
                    _ => Self::Deleting,
                },
            },
        }
    }

    pub(crate) fn is_deleting(self) -> bool {
        !matches!(self, Self::Rollout)
    }

    /// Whether a version is due for removal the moment it stops being needed, rather
    /// than after `drainDelaySeconds`.
    pub(crate) fn skips_drain_delay(self) -> bool {
        matches!(self, Self::ForceDeleting)
    }

    /// Whether the usage query has to attribute unpinned invocations. Only a drain
    /// blocks on them; see [`deployment_usage_query`].
    fn needs_unpinned_count(self) -> bool {
        matches!(self, Self::Deleting)
    }
}

/// When a drain stops waiting: under `onTimeout: force` it force-deregisters, under
/// `onTimeout: hold` it keeps waiting but reports itself overdue.
pub(crate) fn drain_deadline(rsd: &RestateDeployment) -> Option<chrono::DateTime<chrono::Utc>> {
    let deleted_at = rsd.metadata.deletion_timestamp.as_ref()?;
    deleted_at.0.checked_add_signed(chrono::TimeDelta::seconds(
        rsd.spec.restate.drain_timeout_seconds(),
    ))
}

/// What Restate believes about one registered deployment. Kept as separate facts
/// because cleanup weighs them differently during a rollout and during a deletion.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct DeploymentUsage {
    /// At least one service still points here.
    pub latest_for_service: bool,
    /// Unfinished invocations already bound to this deployment.
    pub pinned_invocations: u64,
    /// Unfinished invocations not yet bound to any deployment
    pub unpinned_invocations: u64,
}

pub(crate) type DeploymentUsageMap = HashMap<String, DeploymentUsage>;

impl DeploymentUsage {
    /// Keep whichever value argues more strongly for holding on to the deployment.
    pub(crate) fn merge(&mut self, other: Self) {
        self.latest_for_service |= other.latest_for_service;
        self.pinned_invocations = self.pinned_invocations.max(other.pinned_invocations);
        self.unpinned_invocations = self.unpinned_invocations.max(other.unpinned_invocations);
    }

    /// Work that would be stranded by removing this deployment. Unlike being a service's
    /// endpoint, this falls to zero on its own, so it is safe to wait on.
    pub(crate) fn in_flight_invocations(&self) -> u64 {
        self.pinned_invocations + self.unpinned_invocations
    }

    /// Whether Restate still needs this deployment.
    pub(crate) fn is_active(&self, mode: CleanupMode) -> bool {
        match mode {
            CleanupMode::ForceDeleting => false,
            CleanupMode::Deleting => self.in_flight_invocations() > 0,
            CleanupMode::Rollout => self.latest_for_service || self.in_flight_invocations() > 0,
        }
    }
}

/// The Restate SQL behind [`DeploymentUsageMap`]: one row per registered deployment.
///
/// The unpinned count is the expensive half. Attributing an invocation that carries no
/// `pinned_deployment_id` needs `target_service_name`, which is nested inside the encoded
/// invocation target and so costs a decode per scanned row, plus a join against
/// `sys_service` — on top of a second pass over `sys_invocation_status` across every
/// partition. Only a drain needs the answer -- a force deletion blocks on nothing, and
/// unpinned work is attributed through
/// `sys_service.deployment_id`, the same column that sets `latest_for_service`, so during a
/// rollout a non-zero count could only ever name a deployment that flag is already holding.
/// The rollout query therefore doesn't ask, and selects a constant so both modes return one
/// row shape.
///
/// DataFusion does fold a shared query guarded by `WHERE 1 = 0` down to the same plan
/// (checked with `EXPLAIN` against Restate 1.7.2 — the `unpinned` scan disappears
/// entirely), but building the two strings here says what we mean rather than leaving the
/// cost of the reconcile path resting on an optimiser pass across server versions we don't
/// pin.
pub(crate) fn deployment_usage_query(mode: CleanupMode) -> String {
    let (unpinned_cte, unpinned_count, unpinned_join) = if mode.needs_unpinned_count() {
        (
            r#",
            unpinned AS (
                SELECT s.deployment_id AS id, COUNT(*) AS n
                FROM sys_invocation_status i
                JOIN sys_service s ON s.name = i.target_service_name
                WHERE i.pinned_deployment_id IS NULL
                  AND i.status != 'completed'
                GROUP BY s.deployment_id
            )"#,
            "COALESCE(u.n, 0)",
            "\n            LEFT JOIN unpinned u ON d.id = u.id",
        )
    } else {
        ("", "0", "")
    };

    // The counts are LEFT JOINed onto `sys_deployment` so that a deployment with no
    // matching row still appears; COALESCE turns that missing row back into a zero count
    // rather than a NULL the row struct can't hold.
    format!(
        r#"
            WITH latest AS (
                SELECT DISTINCT deployment_id AS id
                FROM sys_service
                WHERE deployment_id IS NOT NULL
            ),
            pinned AS (
                SELECT pinned_deployment_id AS id, COUNT(*) AS n
                FROM sys_invocation_status
                WHERE pinned_deployment_id IS NOT NULL AND status != 'completed'
                GROUP BY pinned_deployment_id
            ){unpinned_cte}
            SELECT d.id AS deployment_id,
                   l.id IS NOT NULL AS latest_for_service,
                   COALESCE(p.n, 0) AS pinned_invocations,
                   {unpinned_count} AS unpinned_invocations
            FROM sys_deployment d
            LEFT JOIN latest l ON d.id = l.id
            LEFT JOIN pinned p ON d.id = p.id{unpinned_join}
        "#
    )
}

/// One row of [`deployment_usage_query`], as Restate's `/query` endpoint returns it.
#[derive(Debug, Deserialize)]
pub(crate) struct DeploymentUsageRow {
    pub deployment_id: String,
    pub latest_for_service: bool,
    // DataFusion counts are signed; the domain isn't, so these are clamped on the way in.
    pub pinned_invocations: i64,
    pub unpinned_invocations: i64,
}

#[derive(Debug, Deserialize)]
pub(crate) struct DeploymentUsageRows {
    pub rows: Vec<DeploymentUsageRow>,
}

impl DeploymentUsageRows {
    pub(crate) fn into_map(self) -> DeploymentUsageMap {
        let mut usage_by_deployment: DeploymentUsageMap = HashMap::with_capacity(self.rows.len());

        for row in self.rows {
            let usage = DeploymentUsage {
                latest_for_service: row.latest_for_service,
                pinned_invocations: row.pinned_invocations.max(0) as u64,
                unpinned_invocations: row.unpinned_invocations.max(0) as u64,
            };

            usage_by_deployment
                .entry(row.deployment_id)
                // two rows for one deployment id shouldnt happen: `latest` is DISTINCT,
                // `pinned`/`unpinned` are grouped, sys_deployment holds one row per id, and
                // all three are LEFT JOINed. we take the most conservative view of each
                // fact if a future query shape ever does produce duplicates.
                .and_modify(|existing| existing.merge(usage))
                .or_insert(usage);
        }

        usage_by_deployment
    }
}

/// How long to wait before re-checking a deletion that in-flight invocations are holding.
///
/// Every retry re-runs [`deployment_usage_query`] in its deleting flavour, so a deletion
/// parked behind a scheduled invocation days out would otherwise pay that query's two
/// all-partition scans every 30 seconds for as long as it waits. Most drains finish in the
/// first minutes, so poll at the floor early and stretch towards a cap the longer the wait
/// has already run.
/// The backoff is capped by `until_deadline` where there is one, so a drain that has
/// already settled on the ceiling doesn't sleep through its own timeout.
pub(crate) fn blocked_deletion_requeue(
    blocked_for: Duration,
    until_deadline: Option<Duration>,
) -> Duration {
    const FLOOR: Duration = Duration::from_secs(30);
    const CEILING: Duration = Duration::from_secs(300);

    let backoff = (blocked_for / 4).clamp(FLOOR, CEILING);

    match until_deadline {
        Some(until_deadline) => backoff.min(until_deadline.max(Duration::from_secs(1))),
        None => backoff,
    }
}

/// What one pass of cleanup did and did not manage to remove.
#[derive(Debug, Default)]
pub(crate) struct CleanupOutcome {
    /// Versions Restate still needs, which cleanup therefore left alone.
    pub blocking: Vec<BlockingVersion>,
    /// When the soonest drained-but-not-yet-due version comes up for removal.
    pub next_removal: Option<chrono::DateTime<chrono::Utc>>,
    /// Versions torn down while invocations were still in flight. Only a force deletion
    /// produces these.
    pub abandoned: Vec<BlockingVersion>,
}

/// A version that cleanup could not remove because Restate still needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BlockingVersion {
    /// The ReplicaSet or Configuration name.
    pub name: String,
    /// The Restate deployment it is registered as.
    pub deployment_id: Option<String>,
    pub usage: DeploymentUsage,
}

/// Render blocking versions for the `DeploymentInUse` error, so a stuck deletion says
/// which version is holding it and what kind of work to go looking for.
pub(crate) fn describe_blocking_versions(blocking: &[BlockingVersion]) -> String {
    blocking
        .iter()
        .map(|BlockingVersion { name, usage, .. }| {
            format!(
                "{name} ({} pinned, {} unpinned invocations)",
                usage.pinned_invocations, usage.unpinned_invocations
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Render the versions a force deletion tore down with work still in flight.
///
/// Usually pinned-only, because the query a force deletion runs doesn't attribute unpinned
/// work -- so the count is labelled "pinned" rather than "unfinished", which would read as
/// a total it isn't. A pass whose `onTimeout: force` deadline expired after the query ran
/// does have the unpinned count, and reports it.
pub(crate) fn describe_abandoned_versions(abandoned: &[BlockingVersion]) -> String {
    abandoned
        .iter()
        .map(|BlockingVersion { name, usage, .. }| {
            if usage.unpinned_invocations > 0 {
                format!(
                    "{name} ({} pinned, {} unpinned invocations)",
                    usage.pinned_invocations, usage.unpinned_invocations
                )
            } else {
                format!("{name} ({} pinned invocations)", usage.pinned_invocations)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether a drained, zero-scaled version should be kept for rollback.
pub(crate) fn retain_for_rollback(
    mode: CleanupMode,
    historic_count: i32,
    revision_history_limit: i32,
) -> bool {
    !mode.is_deleting() && historic_count < revision_history_limit
}

/// When a superseded version may be torn down.
pub(crate) const RESTATE_REMOVE_VERSION_AT_ANNOTATION: &str = "restate.dev/remove-version-at";

/// Every write to the deadline goes out under this one manager.
const REMOVE_VERSION_AT_FIELD_MANAGER: &str = "restate-operator/remove-version-at";

fn stamp_patch<K>(
    remove_at: chrono::DateTime<chrono::Utc>,
) -> (PatchParams, Patch<serde_json::Value>)
where
    K: Resource<DynamicType = ()>,
{
    let patch = json!({
        "apiVersion": K::api_version(&()),
        "kind": K::kind(&()),
        "metadata": { "annotations": {
            RESTATE_REMOVE_VERSION_AT_ANNOTATION: remove_at.to_rfc3339(),
        } },
    });

    (
        PatchParams::apply(REMOVE_VERSION_AT_FIELD_MANAGER).force(),
        Patch::Apply(patch),
    )
}

/// Giving up the claim is what removes the annotation; setting it to `null` leaves an
/// empty string behind. Only a deadline this manager owns can go this way, so a hand-set
/// one survives.
fn clear_patch<K>() -> (PatchParams, Patch<serde_json::Value>)
where
    K: Resource<DynamicType = ()>,
{
    let patch = json!({
        "apiVersion": K::api_version(&()),
        "kind": K::kind(&()),
        "metadata": { "annotations": {} },
    });

    (
        PatchParams::apply(REMOVE_VERSION_AT_FIELD_MANAGER).force(),
        Patch::Apply(patch),
    )
}

pub(crate) async fn schedule_version_removal<K>(
    api: &Api<K>,
    namespace: &str,
    name: &str,
    drain_delay_seconds: i64,
) -> Result<chrono::DateTime<chrono::Utc>>
where
    K: Resource<DynamicType = ()> + Clone + DeserializeOwned + Debug,
{
    let remove_at = chrono::Utc::now()
        .checked_add_signed(chrono::TimeDelta::seconds(drain_delay_seconds))
        .expect("remove_version_at in bounds");

    info!(
        kind = %K::kind(&()),
        version = %name,
        namespace = %namespace,
        drain_delay_seconds,
        remove_at = %remove_at.to_rfc3339(),
        "Scheduling removal of old version (after drain delay)"
    );

    let (params, patch) = stamp_patch::<K>(remove_at);
    api.patch_metadata(name, &params, &patch).await?;

    Ok(remove_at)
}

/// For a version that is staying. A leftover deadline would tear it down with no drain
/// delay the next time it is superseded. No API call if nothing is scheduled.
pub(crate) async fn unschedule_version_removal<K>(api: &Api<K>, version: &K) -> Result<()>
where
    K: Resource<DynamicType = ()> + Clone + DeserializeOwned + Debug,
{
    if !version
        .annotations()
        .contains_key(RESTATE_REMOVE_VERSION_AT_ANNOTATION)
    {
        return Ok(());
    }

    let name = version.name_any();
    debug!(
        kind = %K::kind(&()),
        version = %name,
        namespace = %version.namespace().unwrap_or_default(),
        "Unscheduling removal of version that is staying"
    );

    let (params, patch) = clear_patch::<K>();
    api.patch_metadata(&name, &params, &patch).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::knative::Configuration;
    use k8s_openapi::api::apps::v1::ReplicaSet;

    /// Both deployment modes write the same annotation under the same field manager.
    #[test]
    fn the_deadline_is_stamped_and_cleared_under_one_field_manager() {
        let remove_at = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00+00:00")
            .expect("test deadline parses")
            .to_utc();

        let (params, patch) = stamp_patch::<ReplicaSet>(remove_at);
        assert_eq!(
            params.field_manager.as_deref(),
            Some("restate-operator/remove-version-at"),
        );
        assert_eq!(
            patch,
            Patch::Apply(json!({
                "apiVersion": "apps/v1",
                "kind": "ReplicaSet",
                "metadata": { "annotations": {
                    "restate.dev/remove-version-at": "2026-01-01T00:00:00+00:00",
                } },
            })),
        );

        assert_eq!(
            stamp_patch::<Configuration>(remove_at).1,
            Patch::Apply(json!({
                "apiVersion": "serving.knative.dev/v1",
                "kind": "Configuration",
                "metadata": { "annotations": {
                    "restate.dev/remove-version-at": "2026-01-01T00:00:00+00:00",
                } },
            })),
        );

        let (params, patch) = clear_patch::<ReplicaSet>();
        assert_eq!(
            params.field_manager.as_deref(),
            Some("restate-operator/remove-version-at"),
        );
        assert_eq!(
            patch,
            Patch::Apply(json!({
                "apiVersion": "apps/v1",
                "kind": "ReplicaSet",
                "metadata": { "annotations": {} },
            })),
        );

        assert_eq!(
            clear_patch::<Configuration>().1,
            Patch::Apply(json!({
                "apiVersion": "serving.knative.dev/v1",
                "kind": "Configuration",
                "metadata": { "annotations": {} },
            })),
        );
    }

    /// Stamp, clear, stamp again, as a rollback does it. Only a real apiserver can say
    /// whether the clear removes the annotation and the next stamp lands.
    ///
    /// Creates and deletes the namespace `restate-operator-drain-deadline` in the current
    /// kube context.
    ///
    ///     cargo test --lib -- --ignored the_cleared_deadline
    mod live {
        use super::*;
        use k8s_openapi::api::apps::v1::ReplicaSet;
        use k8s_openapi::api::core::v1::Namespace;
        use kube::Client;
        use kube::api::{DeleteParams, PostParams};

        const NAMESPACE: &str = "restate-operator-drain-deadline";
        const VERSION: &str = "greeter-abc123";

        /// The deadline annotation, and every manager claiming it.
        async fn deadline(rs_api: &Api<ReplicaSet>) -> (Option<String>, Vec<String>) {
            let rs = rs_api
                .get(VERSION)
                .await
                .expect("the ReplicaSet is readable");

            let claimants = rs
                .managed_fields()
                .iter()
                .filter(|entry| {
                    let claimed = serde_json::to_string(&entry.fields_v1).unwrap_or_default();
                    claimed.contains(RESTATE_REMOVE_VERSION_AT_ANNOTATION)
                })
                .map(|entry| entry.manager.clone().unwrap_or_default())
                .collect();

            (
                rs.annotations()
                    .get(RESTATE_REMOVE_VERSION_AT_ANNOTATION)
                    .cloned(),
                claimants,
            )
        }

        #[tokio::test]
        #[ignore = "needs a Kubernetes apiserver; run with --ignored"]
        async fn the_cleared_deadline_can_be_stamped_again() {
            let client = Client::try_default()
                .await
                .expect("a kube context to run against");

            let ns_api: Api<Namespace> = Api::all(client.clone());
            let _ = ns_api
                .create(
                    &PostParams::default(),
                    &serde_json::from_value(json!({
                        "apiVersion": "v1",
                        "kind": "Namespace",
                        "metadata": { "name": NAMESPACE },
                    }))
                    .expect("the scratch namespace deserializes"),
                )
                .await;

            let rs_api: Api<ReplicaSet> = Api::namespaced(client, NAMESPACE);
            let _ = rs_api.delete(VERSION, &DeleteParams::default()).await;

            // as the operator creates it
            rs_api
                .create(
                    &PostParams {
                        dry_run: false,
                        field_manager: Some("restate-operator".to_owned()),
                    },
                    &serde_json::from_value(json!({
                        "apiVersion": "apps/v1",
                        "kind": "ReplicaSet",
                        "metadata": {
                            "name": VERSION,
                            "annotations": { "restate.dev/pod-template": "{}" },
                        },
                        "spec": {
                            "replicas": 0,
                            "selector": { "matchLabels": { "pod-template-hash": "abc123" } },
                            "template": {
                                "metadata": { "labels": { "pod-template-hash": "abc123" } },
                                "spec": { "containers": [
                                    { "name": "app", "image": "registry.k8s.io/pause:3.9" },
                                ] },
                            },
                        },
                    }))
                    .expect("the test ReplicaSet deserializes"),
                )
                .await
                .expect("the ReplicaSet is created");

            let first = schedule_version_removal(&rs_api, NAMESPACE, VERSION, 300)
                .await
                .expect("the deadline is stamped");
            assert_eq!(
                deadline(&rs_api).await,
                (
                    Some(first.to_rfc3339()),
                    vec![REMOVE_VERSION_AT_FIELD_MANAGER.to_owned()],
                ),
            );

            let stamped = rs_api
                .get(VERSION)
                .await
                .expect("the ReplicaSet is readable");
            unschedule_version_removal(&rs_api, &stamped)
                .await
                .expect("the deadline is cleared");
            assert_eq!(
                deadline(&rs_api).await,
                (None, vec![]),
                "the annotation is gone, and nothing is left claiming it",
            );

            let second = schedule_version_removal(&rs_api, NAMESPACE, VERSION, 300)
                .await
                .expect("the cleared deadline is stamped again");
            assert_ne!(second.to_rfc3339(), first.to_rfc3339());
            assert_eq!(
                deadline(&rs_api).await,
                (
                    Some(second.to_rfc3339()),
                    vec![REMOVE_VERSION_AT_FIELD_MANAGER.to_owned()],
                ),
                "the re-stamped deadline landed, under the one manager",
            );

            let no_deadline = rs_api
                .get(VERSION)
                .await
                .expect("the ReplicaSet is readable");
            unschedule_version_removal(&rs_api, &no_deadline)
                .await
                .expect("clearing an unscheduled version is a no-op");

            // a deadline this manager never owned survives the clear
            rs_api
                .patch_metadata(
                    VERSION,
                    &PatchParams::apply("someone-else").force(),
                    &Patch::Apply(json!({
                        "apiVersion": "apps/v1",
                        "kind": "ReplicaSet",
                        "metadata": { "annotations": {
                            RESTATE_REMOVE_VERSION_AT_ANNOTATION: "2026-01-01T00:00:00+00:00",
                        } },
                    })),
                )
                .await
                .expect("the foreign deadline is set");
            let foreign = rs_api
                .get(VERSION)
                .await
                .expect("the ReplicaSet is readable");
            unschedule_version_removal(&rs_api, &foreign)
                .await
                .expect("the clear is applied");
            assert_eq!(
                deadline(&rs_api).await.0,
                Some("2026-01-01T00:00:00+00:00".to_owned()),
                "a deadline this manager never owned survives the clear",
            );

            let _ = ns_api.delete(NAMESPACE, &DeleteParams::default()).await;
        }
    }

    fn blocking(name: &str, usage: DeploymentUsage) -> BlockingVersion {
        BlockingVersion {
            name: name.into(),
            deployment_id: None,
            usage,
        }
    }

    fn usage(latest: bool, pinned: u64, unpinned: u64) -> DeploymentUsage {
        DeploymentUsage {
            latest_for_service: latest,
            pinned_invocations: pinned,
            unpinned_invocations: unpinned,
        }
    }

    /// `on_timeout` of `None` leaves the drain block off entirely, so the defaults apply.
    fn deleting_rsd(
        policy: &str,
        on_timeout: Option<&str>,
        timeout_seconds: i64,
        deleted_secs_ago: i64,
    ) -> RestateDeployment {
        let spec = serde_json::from_value(serde_json::json!({
            "replicas": 1,
            "revisionHistoryLimit": 10,
            "template": { "metadata": null, "spec": {} },
            "restate": {
                "register": { "url": "http://restate:9070/" },
                "deletePolicy": policy,
                "drain": on_timeout.map(|on_timeout| serde_json::json!({
                    "timeoutSeconds": timeout_seconds,
                    "onTimeout": on_timeout,
                })),
            },
        }))
        .expect("test RestateDeploymentSpec deserializes");

        let mut rsd = RestateDeployment::new("greeter", spec);
        rsd.metadata.deletion_timestamp =
            Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                chrono::Utc::now() - chrono::TimeDelta::seconds(deleted_secs_ago),
            ));
        rsd
    }

    #[test]
    fn force_deletion_waits_for_nothing() {
        for u in [usage(true, 0, 0), usage(true, 3, 9), usage(false, 1, 0)] {
            assert!(!u.is_active(CleanupMode::ForceDeleting));
        }
    }

    #[test]
    fn mode_follows_the_delete_policy() {
        let mut not_deleting = deleting_rsd("force", None, 3600, 0);
        not_deleting.metadata.deletion_timestamp = None;
        assert_eq!(CleanupMode::for_rsd(&not_deleting), CleanupMode::Rollout);

        assert_eq!(
            CleanupMode::for_rsd(&deleting_rsd("force", None, 3600, 0)),
            CleanupMode::ForceDeleting
        );

        // a drain waits past its deadline; only the status changes there
        assert_eq!(
            CleanupMode::for_rsd(&deleting_rsd("drain", Some("hold"), 3600, 0)),
            CleanupMode::Deleting
        );
        assert_eq!(
            CleanupMode::for_rsd(&deleting_rsd("drain", Some("hold"), 3600, 7200)),
            CleanupMode::Deleting
        );

        assert_eq!(
            CleanupMode::for_rsd(&deleting_rsd("drain", Some("force"), 3600, 60)),
            CleanupMode::Deleting
        );
        assert_eq!(
            CleanupMode::for_rsd(&deleting_rsd("drain", Some("force"), 3600, 3601)),
            CleanupMode::ForceDeleting
        );

        // no drain block at all is a `hold`, so it holds however far past the deadline
        // it is -- the one case that must not start forcing by omission
        assert_eq!(
            CleanupMode::for_rsd(&deleting_rsd("drain", None, 3600, 7200)),
            CleanupMode::Deleting
        );
    }

    #[test]
    fn default_delete_policy_is_a_one_hour_drain_that_holds() {
        let mut rsd = deleting_rsd("drain", Some("force"), 3600, 0);
        rsd.spec.restate.delete_policy = None;
        rsd.spec.restate.drain = None;

        assert_eq!(CleanupMode::for_rsd(&rsd), CleanupMode::Deleting);
        assert_eq!(rsd.spec.restate.drain_timeout_seconds(), 3600);
        assert_eq!(rsd.spec.restate.drain_on_timeout(), OnTimeout::Hold);
        assert_eq!(
            drain_deadline(&rsd),
            Some(rsd.metadata.deletion_timestamp.as_ref().unwrap().0 + chrono::TimeDelta::hours(1))
        );
    }

    #[test]
    fn latest_endpoint_is_active_during_rollout_but_not_during_deletion() {
        let u = usage(true, 0, 0);
        assert!(u.is_active(CleanupMode::Rollout));
        assert!(!u.is_active(CleanupMode::Deleting));
    }

    #[test]
    fn pinned_invocations_block_deletion() {
        let u = usage(true, 3, 0);
        assert!(u.is_active(CleanupMode::Rollout));
        assert!(u.is_active(CleanupMode::Deleting));
    }

    #[test]
    fn unpinned_invocations_block_deletion() {
        // a pinned-only count reads paused/queued work as zero and deletes out from under it
        let u = usage(true, 0, 80);
        assert!(u.is_active(CleanupMode::Rollout));
        assert!(u.is_active(CleanupMode::Deleting));
    }

    #[test]
    fn fully_drained_version_is_inactive_either_way() {
        let u = usage(false, 0, 0);
        assert!(!u.is_active(CleanupMode::Rollout));
        assert!(!u.is_active(CleanupMode::Deleting));
    }

    #[test]
    fn superseded_version_with_in_flight_work_stays_active() {
        let u = usage(false, 1, 0);
        assert!(u.is_active(CleanupMode::Rollout));
        assert!(u.is_active(CleanupMode::Deleting));
    }

    #[test]
    fn merge_takes_the_conservative_view() {
        let mut u = usage(false, 1, 0);
        u.merge(usage(true, 0, 5));
        assert_eq!(u, usage(true, 1, 5));
    }

    #[test]
    fn rollback_retention_applies_during_normal_reconcile() {
        assert!(retain_for_rollback(CleanupMode::Rollout, 0, 10));
        assert!(retain_for_rollback(CleanupMode::Rollout, 9, 10));
        assert!(!retain_for_rollback(CleanupMode::Rollout, 10, 10));
    }

    #[test]
    fn rollback_retention_is_disabled_while_deleting() {
        // retaining here would skip the deregistration below it and leak the registration
        assert!(!retain_for_rollback(CleanupMode::Deleting, 0, 10));
        assert!(!retain_for_rollback(CleanupMode::Deleting, 9, 10));
    }

    #[test]
    fn rollout_query_does_not_pay_for_the_unpinned_count() {
        let query = deployment_usage_query(CleanupMode::Rollout);

        // no second scan of sys_invocation_status, and no decode of the nested
        // target_service_name to join it against sys_service
        assert_eq!(query.matches("sys_invocation_status").count(), 1);
        assert!(!query.contains("target_service_name"));
        assert!(!query.contains("unpinned AS"));

        // ...but the column is still projected, so one row struct parses both modes
        assert!(query.contains("AS unpinned_invocations"));
        assert!(query.contains("AS pinned_invocations"));
        assert!(query.contains("AS latest_for_service"));
    }

    #[test]
    fn force_deletion_does_not_pay_for_the_unpinned_count_either() {
        // nothing blocks a force deletion, so there is nothing to attribute
        let query = deployment_usage_query(CleanupMode::ForceDeleting);
        assert_eq!(query.matches("sys_invocation_status").count(), 1);
        assert!(!query.contains("unpinned AS"));
    }

    #[test]
    fn force_deletion_still_removes_drained_versions_immediately() {
        assert!(CleanupMode::ForceDeleting.skips_drain_delay());
        assert!(!CleanupMode::Deleting.skips_drain_delay());
        assert!(!CleanupMode::Rollout.skips_drain_delay());

        // the revision history limit must not hold a version back from either deletion
        assert!(!retain_for_rollback(CleanupMode::ForceDeleting, 0, 10));
    }

    #[test]
    fn abandoned_versions_report_what_was_walked_over() {
        // the force query doesn't attribute unpinned work, so the count says "pinned"
        // rather than claiming to be every unfinished invocation
        assert_eq!(
            describe_abandoned_versions(&[blocking("greeter-abc123", usage(false, 4, 0))]),
            "greeter-abc123 (4 pinned invocations)"
        );

        // ...but an `onTimeout: force` drain that crossed its deadline after the query ran
        // does have the unpinned count, and must not drop it
        assert_eq!(
            describe_abandoned_versions(&[blocking("greeter-abc123", usage(false, 4, 7))]),
            "greeter-abc123 (4 pinned, 7 unpinned invocations)"
        );
    }

    #[test]
    fn blocked_deletion_does_not_sleep_through_the_deadline() {
        let requeue = |secs, until| {
            blocked_deletion_requeue(Duration::from_secs(secs), Some(Duration::from_secs(until)))
        };

        // an `onTimeout: force` drain waiting an hour is on the 300s ceiling, but
        // its deadline is 45s away
        assert_eq!(requeue(3600, 45), Duration::from_secs(45));
        assert_eq!(requeue(3600, 600), Duration::from_secs(300));

        // never zero, or the reconciler spins on the deadline
        assert_eq!(requeue(3600, 0), Duration::from_secs(1));
    }

    #[test]
    fn deleting_query_counts_unpinned_work_through_the_service() {
        let query = deployment_usage_query(CleanupMode::Deleting);

        assert_eq!(query.matches("sys_invocation_status").count(), 2);
        assert!(query.contains("JOIN sys_service s ON s.name = i.target_service_name"));
        assert!(query.contains("COALESCE(u.n, 0) AS unpinned_invocations"));
        assert!(query.contains("LEFT JOIN unpinned u ON d.id = u.id"));
    }

    #[test]
    fn both_query_flavours_exclude_completed_invocations() {
        // completed invocations linger in sys_invocation_status for the retention window;
        // counting them would hold a deletion open for a full day of drained history
        for mode in [CleanupMode::Rollout, CleanupMode::Deleting] {
            let query = deployment_usage_query(mode);
            assert_eq!(
                query.matches("status != 'completed'").count(),
                query.matches("sys_invocation_status").count(),
                "{mode:?}"
            );
        }
    }

    #[test]
    fn blocked_deletion_polls_hard_early_and_backs_off_later() {
        let requeue = |secs| blocked_deletion_requeue(Duration::from_secs(secs), None);

        // the common case -- a drain that finishes in the first minutes -- keeps the
        // 30s cadence it had before the backoff
        assert_eq!(requeue(0), Duration::from_secs(30));
        assert_eq!(requeue(120), Duration::from_secs(30));

        assert_eq!(requeue(240), Duration::from_secs(60));

        // a deletion held by a scheduled invocation days out settles on the cap
        assert_eq!(requeue(1200), Duration::from_secs(300));
        assert_eq!(requeue(86_400), Duration::from_secs(300));
    }

    #[test]
    fn blocking_versions_name_the_work_holding_the_deletion() {
        let described = describe_blocking_versions(&[
            blocking("greeter-abc123", usage(true, 2, 0)),
            blocking("greeter-def456", usage(false, 0, 7)),
        ]);

        assert_eq!(
            described,
            "greeter-abc123 (2 pinned, 0 unpinned invocations), \
             greeter-def456 (0 pinned, 7 unpinned invocations)"
        );
    }
}
