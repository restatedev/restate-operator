# Release Notes: Requeue based on drain poll interval when old deployments are still active

## Bug Fix

### What Changed
When old deployment versions still have active invocations (draining), the
operator now requeues every 10 seconds instead of the hardcoded 5-minute
reconcile interval. This applies to both ReplicaSet and Knative deployment
modes.

### Why This Matters
Previously, even with `drainDelaySeconds: 0`, old versions could take up to
5 minutes to be cleaned up because the controller's default requeue interval
was always used when active invocations were still present. The drain delay
setting had no effect on how quickly the operator polled for drain completion.

### Impact on Users
- Cleanup of old versions now happens within ~10 seconds of drain completion
  instead of up to 5 minutes
- No configuration changes needed

### Related Issues
- Follow-up to PR #96 (configurable drain delay)
