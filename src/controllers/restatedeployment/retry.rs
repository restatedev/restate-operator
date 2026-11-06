//! How often a RestateDeployment goes back to the Restate admin API.
//!
//! Every reconcile can query the admin API, and many RestateDeployments can share one
//! Restate cluster. The intervals here keep a struggling cluster from being retried at a
//! fixed rate, and keep resources that fail or poll together from doing so in lockstep.

use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::resources::restatedeployments::RestateDeploymentCondition;

/// The `Ready` reasons for a failed admin API call. Other reasons don't back off: pods
/// that are still starting are progress, and an error raised before any request is sent,
/// such as a missing Secret, puts no load on the admin API.
const ADMIN_FAILURE_REASONS: &[&str] = &["AdminCallFailed", "AdminCallRejected"];

/// How often a healthy deployment is re-checked when nothing else triggers a reconcile.
const HEALTHY_POLL: Duration = Duration::from_secs(5 * 60);

/// How long to wait before trying again, given how long the wait has already lasted.
///
/// Waits start at 30 seconds and grow to a quarter of the time already waited, up to
/// 5 minutes. Most failures clear within the first few retries; a long one settles at
/// the same rate as the healthy poll.
pub(crate) fn backoff(waited: Duration) -> Duration {
    const FLOOR: Duration = Duration::from_secs(30);
    const CEILING: Duration = HEALTHY_POLL;

    (waited / 4).clamp(FLOOR, CEILING)
}

/// When to retry a failed admin API call, given how long the calls have been failing.
pub(crate) fn admin_retry_after(failing_for: Duration) -> Duration {
    jitter(backoff(failing_for))
}

/// When to retry an admin API call that just failed while reconciling a deployment.
///
/// `ready` is the condition the previous reconcile wrote, so the interval grows with how
/// long the calls have been failing without the operator keeping any state, and it
/// carries over an operator restart. `lastTransitionTime` moves only when the status
/// changes, so a deployment that was already `False` for another reason when the
/// failures began backs off from that earlier time. The 5-minute ceiling bounds that
/// overestimate.
pub(crate) fn admin_failure_retry_after(
    ready: Option<&RestateDeploymentCondition>,
    now: DateTime<Utc>,
) -> Duration {
    let failing_for = ready
        .filter(|ready| {
            ready
                .reason
                .as_deref()
                .is_some_and(|reason| ADMIN_FAILURE_REASONS.contains(&reason))
        })
        .and_then(|ready| ready.last_transition_time.as_ref())
        .and_then(|since| (now - since.0).to_std().ok())
        .unwrap_or_default();

    admin_retry_after(failing_for)
}

/// When to retry a reconcile that failed for a reason other than the admin API.
pub(crate) fn other_failure_retry_after() -> Duration {
    jitter(backoff(Duration::ZERO))
}

/// When to re-check a healthy deployment: when its next drained version is due for
/// removal, or after the routine poll otherwise.
pub(crate) fn healthy_requeue_after(
    next_removal: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Duration {
    let poll = jitter(HEALTHY_POLL);

    match next_removal {
        // A removal already due converts to a negative duration, which `to_std` rejects.
        Some(due) => (due - now).to_std().unwrap_or_default().min(poll),
        None => poll,
    }
}

/// Lengthen `interval` by a random 0–20%, so that resources whose timers started
/// together, such as every deployment on a cluster that just went down, spread their
/// admin calls out instead of arriving at the same instant.
fn jitter(interval: Duration) -> Duration {
    interval.mul_f64(1.0 + rand::random_range(0.0..0.2))
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;

    fn ready(reason: &str, since: DateTime<Utc>) -> RestateDeploymentCondition {
        RestateDeploymentCondition {
            last_transition_time: Some(Time(since)),
            message: None,
            reason: Some(reason.into()),
            status: "False".into(),
            r#type: "Ready".into(),
        }
    }

    fn within_jitter(actual: Duration, base: Duration) -> bool {
        actual >= base && actual <= base.mul_f64(1.2)
    }

    #[test]
    fn retries_slow_down_the_longer_a_failure_lasts() {
        let now = Utc::now();
        let retry_after = |reason, failing_for_secs| {
            let since = now - chrono::Duration::seconds(failing_for_secs);
            admin_failure_retry_after(Some(&ready(reason, since)), now)
        };

        assert!(within_jitter(
            admin_failure_retry_after(None, now),
            Duration::from_secs(30)
        ));
        assert!(within_jitter(
            retry_after("AdminCallFailed", 0),
            Duration::from_secs(30)
        ));
        assert!(within_jitter(
            retry_after("AdminCallRejected", 480),
            Duration::from_secs(120)
        ));
        assert!(within_jitter(
            retry_after("AdminCallFailed", 86_400),
            Duration::from_secs(300)
        ));
    }

    /// Only time spent failing against the admin API counts. Pods that take a while to
    /// start are progress, and a reconcile that failed before sending a request (a
    /// missing Secret, say) put no load on the admin API.
    #[test]
    fn other_reasons_do_not_count_as_failing() {
        let now = Utc::now();
        let since = now - chrono::Duration::seconds(3600);

        for reason in ["ReplicaSetPodNotReady", "FailedReconcile"] {
            assert!(within_jitter(
                admin_failure_retry_after(Some(&ready(reason, since)), now),
                Duration::from_secs(30)
            ));
        }
    }

    #[test]
    fn healthy_poll_waits_for_the_next_removal_but_no_longer_than_the_poll() {
        let now = Utc::now();
        let removal_in =
            |secs| healthy_requeue_after(Some(now + chrono::Duration::seconds(secs)), now);

        assert_eq!(removal_in(-10), Duration::ZERO);
        assert_eq!(removal_in(45), Duration::from_secs(45));
        assert!(within_jitter(removal_in(3600), HEALTHY_POLL));
        assert!(within_jitter(
            healthy_requeue_after(None, now),
            HEALTHY_POLL
        ));
    }
}
