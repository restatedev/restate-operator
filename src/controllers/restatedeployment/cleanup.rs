//! Policy decisions shared by the ReplicaSet and Knative cleanup reconcilers: what
//! Restate still needs a registered deployment for, how we ask it, and how long we keep a
//! drained version around.

use std::collections::HashMap;
use std::time::Duration;

use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CleanupMode {
    Rollout,
    Deleting,
}

impl CleanupMode {
    pub(crate) fn for_rsd(rsd: &crate::resources::restatedeployments::RestateDeployment) -> Self {
        if rsd.metadata.deletion_timestamp.is_some() {
            Self::Deleting
        } else {
            Self::Rollout
        }
    }

    pub(crate) fn is_deleting(self) -> bool {
        matches!(self, Self::Deleting)
    }
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
/// partition. Only a deletion needs the answer: unpinned work is attributed through
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
    let (unpinned_cte, unpinned_count, unpinned_join) = if mode.is_deleting() {
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
pub(crate) fn blocked_deletion_requeue(blocked_for: Duration) -> Duration {
    const FLOOR: Duration = Duration::from_secs(30);
    const CEILING: Duration = Duration::from_secs(300);

    (blocked_for / 4).clamp(FLOOR, CEILING)
}

/// A version that cleanup could not remove because Restate still needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BlockingVersion {
    /// The ReplicaSet or Configuration name.
    pub name: String,
    pub usage: DeploymentUsage,
}

/// Render blocking versions for the `DeploymentInUse` error, so a stuck deletion says
/// which version is holding it and what kind of work to go looking for.
pub(crate) fn describe_blocking_versions(blocking: &[BlockingVersion]) -> String {
    blocking
        .iter()
        .map(|BlockingVersion { name, usage }| {
            format!(
                "{name} ({} pinned, {} unpinned invocations)",
                usage.pinned_invocations, usage.unpinned_invocations
            )
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

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(latest: bool, pinned: u64, unpinned: u64) -> DeploymentUsage {
        DeploymentUsage {
            latest_for_service: latest,
            pinned_invocations: pinned,
            unpinned_invocations: unpinned,
        }
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
        let requeue = |secs| blocked_deletion_requeue(Duration::from_secs(secs));

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
            BlockingVersion {
                name: "greeter-abc123".into(),
                usage: usage(true, 2, 0),
            },
            BlockingVersion {
                name: "greeter-def456".into(),
                usage: usage(false, 0, 7),
            },
        ]);

        assert_eq!(
            described,
            "greeter-abc123 (2 pinned, 0 unpinned invocations), \
             greeter-def456 (0 pinned, 7 unpinned invocations)"
        );
    }
}
