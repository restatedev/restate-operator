//! Policy decisions shared by the ReplicaSet and Knative cleanup reconcilers: what
//! Restate still needs a registered deployment for, and how long we keep a drained
//! version around.

use std::collections::HashMap;

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

/// A version that cleanup could not remove because Restate still needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BlockingVersion {
    /// The ReplicaSet or Configuration name.
    pub name: String,
    /// The Restate deployment the version is registered as, so status readers can go
    /// straight to `restate invocations list --deployment <id>`.
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
    fn blocking_versions_name_the_work_holding_the_deletion() {
        let described = describe_blocking_versions(&[
            BlockingVersion {
                name: "greeter-abc123".into(),
                deployment_id: Some("dp_abc".into()),
                usage: usage(true, 2, 0),
            },
            BlockingVersion {
                name: "greeter-def456".into(),
                deployment_id: Some("dp_def".into()),
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
