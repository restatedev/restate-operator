# Release Notes for Issue #194: Retry stalled credential canaries

## Bug Fix

### What Changed

The Pod Identity pod shortcut only accelerates success. Missing credentials on an
individual pod no longer delete the Job before its retry can run. Terminally failed
Jobs are deleted with their pods and retried on the next reconcile.

Both Pod Identity and Workload Identity canary Jobs now have a five-minute active
deadline, allowing stalled Jobs to fail and be retried. Successful Jobs are retained.

### Why This Matters

Credential propagation and scheduling delays no longer cause premature Job deletion
or leave a canary waiting indefinitely. Pod checks are scoped to the current Job.

### Impact on Users

New and existing clusters receive the deadline on reconciliation. Previously orphaned
canary pods are not removed by this fix and may still need separate cleanup.

### Migration Guidance

No configuration changes are required.

### Related Issues

- #194: Pod Identity canary lifecycle
