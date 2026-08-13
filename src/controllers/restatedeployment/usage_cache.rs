//! A short-lived, endpoint-scoped cache of the deployment-usage answer.
//!
//! The answer belongs to the Restate environment, not to the RestateDeployment that asked:
//! every resource registered against the same admin endpoint gets the same map back.
//!
//! Staleness is bounded by [`TTL`] and biased safely. Invocation counts fall as work drains,
//! so a stale answer over-states how busy a deployment is and defers a removal rather than
//! bringing one forward. The opposite case needs new work to pin to an already-superseded
//! deployment, which only happens when someone restarts or resumes an invocation onto it by
//! name — a race that a fresh query loses too, since nothing holds still between the query
//! and the decision it feeds.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use super::cleanup::{CleanupMode, DeploymentUsageMap};

/// How long an answer may be reused.
///
/// Longer than the spacing `retry::ENDPOINT_SPACING` puts between queries, so a resource
/// that was deferred and comes back for its turn finds the answer the query it waited on
/// produced, instead of starting another and re-forming the queue behind it.
const TTL: Duration = Duration::from_secs(60);

struct Entry {
    mode: CleanupMode,
    usage: DeploymentUsageMap,
    fetched_at: Instant,
}

impl Entry {
    /// The deleting query counts unpinned invocations for real, where the rollout query
    /// selects a constant zero. So a deleting answer serves either caller, and a rollout
    /// answer serves only a rollout: handing one to a deletion would report queued work as
    /// absent and let the deployment be removed out from under it.
    fn serves(&self, mode: CleanupMode) -> bool {
        self.mode == mode || self.mode.is_deleting()
    }
}

/// Answers for one operator process, bounded by the number of environments it manages.
#[derive(Default)]
pub(super) struct UsageCache {
    entries: Mutex<HashMap<String, Entry>>,
}

impl UsageCache {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn get(&self, endpoint: &str, mode: CleanupMode) -> Option<DeploymentUsageMap> {
        self.get_at(endpoint, mode, Instant::now())
    }

    pub(super) fn insert(&self, endpoint: String, mode: CleanupMode, usage: &DeploymentUsageMap) {
        self.insert_at(endpoint, mode, usage, Instant::now());
    }

    pub(super) fn invalidate(&self, endpoint: &str) {
        self.lock().remove(endpoint);
    }

    fn get_at(
        &self,
        endpoint: &str,
        mode: CleanupMode,
        now: Instant,
    ) -> Option<DeploymentUsageMap> {
        self.lock()
            .get(endpoint)
            .filter(|entry| now.duration_since(entry.fetched_at) < TTL && entry.serves(mode))
            .map(|entry| entry.usage.clone())
    }

    fn insert_at(
        &self,
        endpoint: String,
        mode: CleanupMode,
        usage: &DeploymentUsageMap,
        at: Instant,
    ) {
        self.lock().insert(
            endpoint,
            Entry {
                mode,
                usage: usage.clone(),
                fetched_at: at,
            },
        );
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Entry>> {
        self.entries.lock().expect("usage cache lock poisoned")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::controllers::restatedeployment::cleanup::DeploymentUsage;

    fn usage(pinned: u64, unpinned: u64) -> DeploymentUsageMap {
        HashMap::from([(
            "dp_1".to_string(),
            DeploymentUsage {
                latest_for_service: false,
                pinned_invocations: pinned,
                unpinned_invocations: unpinned,
            },
        )])
    }

    #[test]
    fn a_fresh_answer_is_reused() {
        let cache = UsageCache::new();
        cache.insert("http://restate/".into(), CleanupMode::Rollout, &usage(3, 0));

        assert_eq!(
            cache.get("http://restate/", CleanupMode::Rollout),
            Some(usage(3, 0))
        );
    }

    #[test]
    fn an_answer_older_than_the_ttl_is_not() {
        let cache = UsageCache::new();
        let at = Instant::now();
        cache.insert_at(
            "http://restate/".into(),
            CleanupMode::Rollout,
            &usage(3, 0),
            at,
        );

        assert!(
            cache
                .get_at("http://restate/", CleanupMode::Rollout, at + TTL)
                .is_none()
        );
    }

    #[test]
    fn a_deleting_answer_serves_a_rollout_caller() {
        let cache = UsageCache::new();
        cache.insert(
            "http://restate/".into(),
            CleanupMode::Deleting,
            &usage(1, 7),
        );

        assert_eq!(
            cache.get("http://restate/", CleanupMode::Rollout),
            Some(usage(1, 7))
        );
    }

    #[test]
    fn a_rollout_answer_never_serves_a_deletion() {
        // Reused for a deletion, a rollout's constant-zero unpinned count reads as "nothing
        // queued here" and deletes the deployment out from under work not yet pinned.
        let cache = UsageCache::new();
        cache.insert("http://restate/".into(), CleanupMode::Rollout, &usage(0, 0));

        assert!(
            cache
                .get("http://restate/", CleanupMode::Deleting)
                .is_none()
        );
    }

    #[test]
    fn invalidating_drops_the_answer() {
        let cache = UsageCache::new();
        cache.insert(
            "http://restate/".into(),
            CleanupMode::Deleting,
            &usage(0, 0),
        );
        cache.invalidate("http://restate/");

        assert!(
            cache
                .get("http://restate/", CleanupMode::Deleting)
                .is_none()
        );
    }

    #[test]
    fn endpoints_do_not_share_answers() {
        let cache = UsageCache::new();
        cache.insert("http://one/".into(), CleanupMode::Deleting, &usage(4, 0));

        assert!(cache.get("http://two/", CleanupMode::Deleting).is_none());
        assert_eq!(
            cache.get("http://one/", CleanupMode::Deleting),
            Some(usage(4, 0))
        );
    }
}
