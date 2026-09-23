//! Process-local admission control and retry coordination for expensive Restate admin queries.

use std::collections::HashMap;
use std::hash::{BuildHasher, Hash, RandomState};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const FLOOR: Duration = Duration::from_secs(30);
const CEILING: Duration = Duration::from_secs(5 * 60);
const MAX_JITTER: Duration = Duration::from_secs(60);
const ENDPOINT_SPACING: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct RetryKey {
    endpoint: String,
    resource: String,
}

#[derive(Debug, Default)]
struct RetryState {
    failures: u32,
    not_before: Option<Instant>,
}

#[derive(Debug, Default)]
struct EndpointState {
    in_flight: bool,
    next_admission: Option<Instant>,
}

#[derive(Default)]
struct Inner {
    retries: HashMap<RetryKey, RetryState>,
    endpoints: HashMap<String, EndpointState>,
}

/// Coordinates expensive deployment-usage queries for one operator process.
///
/// A permit is held until the HTTP operation completes, rather than for a fixed time, so a slow
/// legacy query cannot overlap a later reconcile. The endpoint spacing is enforced when a caller
/// actually obtains that permit. Delayed callers do not reserve queue positions: a watch event
/// may cause an earlier healthy caller to run first, but cannot starve a caller on a stale slot.
pub(super) struct ExpensiveOperationRetries {
    inner: Mutex<Inner>,
}

impl ExpensiveOperationRetries {
    pub(super) fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Acquires the endpoint permit or returns a controller requeue delay.
    pub(super) fn admit(&self, endpoint: String, resource: String) -> Result<(), Duration> {
        let now = Instant::now();
        let key = RetryKey {
            endpoint: endpoint.clone(),
            resource,
        };
        let mut inner = self.inner.lock().expect("retry coordinator lock poisoned");
        let not_before = inner.retries.entry(key.clone()).or_default().not_before;
        if let Some(not_before) = not_before.filter(|time| *time > now) {
            return Err(not_before.saturating_duration_since(now));
        }

        {
            let endpoint_state = inner.endpoints.entry(endpoint).or_default();
            if endpoint_state.in_flight {
                return Err(ENDPOINT_SPACING);
            }
            if let Some(next_admission) = endpoint_state.next_admission.filter(|time| *time > now) {
                return Err(next_admission.saturating_duration_since(now));
            }
            endpoint_state.in_flight = true;
            endpoint_state.next_admission = Some(now + ENDPOINT_SPACING);
        }

        inner.retries.entry(key).or_default().not_before = None;
        Ok(())
    }

    /// Releases the endpoint permit after the HTTP request completes. It intentionally keeps the
    /// resource's retry history; the whole reconcile must succeed before that state is reset.
    pub(super) fn finish(&self, endpoint: &str) {
        if let Some(state) = self
            .inner
            .lock()
            .expect("retry coordinator lock poisoned")
            .endpoints
            .get_mut(endpoint)
        {
            state.in_flight = false;
        }
    }

    /// Records one failed query-bearing reconcile and schedules a fresh exponential retry.
    pub(super) fn failure(&self, endpoint: String, resource: String) -> Duration {
        let now = Instant::now();
        let key = RetryKey { endpoint, resource };
        let mut inner = self.inner.lock().expect("retry coordinator lock poisoned");
        let retry = inner.retries.entry(key.clone()).or_default();
        retry.failures = retry.failures.saturating_add(1);
        let delay = backoff_with_positive_jitter(&key, retry.failures);
        retry.not_before = Some(now + delay);
        delay
    }

    /// A fully successful reconcile clears only its own retry history.
    pub(super) fn reset_resource(&self, endpoint: &str, resource: &str) {
        self.inner
            .lock()
            .expect("retry coordinator lock poisoned")
            .retries
            .remove(&RetryKey {
                endpoint: endpoint.into(),
                resource: resource.into(),
            });
    }
}

fn backoff_with_positive_jitter(key: &RetryKey, failures: u32) -> Duration {
    let multiplier = 1_u32 << failures.saturating_sub(1).min(4);
    let base = FLOOR.saturating_mul(multiplier).min(CEILING);
    let jitter_cap = (base / 5).min(MAX_JITTER);
    let jitter = RandomState::new().hash_one(key) % (jitter_cap.as_nanos() as u64 + 1);
    base + Duration::from_nanos(jitter)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_keeps_jitter_at_the_cap() {
        let key = RetryKey {
            endpoint: "https://admin.example.test".into(),
            resource: "uid-a".into(),
        };
        for (failures, floor) in [(1, 30), (2, 60), (3, 120), (4, 240), (5, 300), (6, 300)] {
            let delay = backoff_with_positive_jitter(&key, failures);
            assert!(delay >= Duration::from_secs(floor));
            assert!(delay <= CEILING + MAX_JITTER);
        }
    }

    #[test]
    fn endpoint_permit_remains_held_until_the_query_finishes() {
        let retries = ExpensiveOperationRetries::new();
        let endpoint = "https://admin.example.test";
        assert!(retries.admit(endpoint.into(), "uid-a".into()).is_ok());
        // This is still deferred even after the normal spacing period would have elapsed,
        // because the only release path is `finish`.
        assert!(retries.admit(endpoint.into(), "uid-b".into()).is_err());
        retries.finish(endpoint);
        // Spacing is now the only remaining limiter.
        assert!(retries.admit(endpoint.into(), "uid-b".into()).is_err());
    }

    #[test]
    fn failure_history_is_keyed_by_the_expensive_operation_not_the_reason() {
        let retries = ExpensiveOperationRetries::new();
        let endpoint = "https://admin.example.test";
        let first = retries.failure(endpoint.into(), "uid-a".into());
        retries.reset_resource(endpoint, "uid-a");
        let reset = retries.failure(endpoint.into(), "uid-a".into());
        assert!(first >= FLOOR);
        assert!(reset >= FLOOR);
    }
}
