# Release Notes: Requeue based on drain delay when old deployments are still active

## Bug Fix

### What Changed
When old deployment versions still have active invocations (draining), the
operator now requeues based on `drainDelaySeconds` instead of the hardcoded
5-minute reconcile interval. This applies to both ReplicaSet and Knative
deployment modes.

### Why This Matters
Previously, even with `drainDelaySeconds: 0`, old versions could take up to
5 minutes to be cleaned up because the controller's default requeue interval
was always used when active invocations were still present. The drain delay
setting had no effect on how quickly the operator polled for drain completion.

### Impact on Users
- `drainDelaySeconds: 0` now results in near-immediate cleanup polling
  instead of a 5-minute wait
- Custom drain delay values are now respected for requeue timing during the
  active drain phase
- No change for users relying on the default 300-second drain delay

### Related Issues
- Follow-up to PR #96 (configurable drain delay)
