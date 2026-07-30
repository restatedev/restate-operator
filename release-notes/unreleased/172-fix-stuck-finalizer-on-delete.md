# Release Notes for Issue #172: Fix RestateDeployment finalizer stuck in CleanupFailed loop on delete

## Bug Fix

### What Changed

When deleting a `RestateDeployment`, the finalizer no longer gets permanently
stuck with `CleanupFailed(DeploymentInUse)`. Previously, `cleanup_old_replicasets`
treated the latest ReplicaSet's Restate deployment as an unconditional blocker
because it is always `active = true` in `sys_service`. Since no new version
ever registers during deletion, the `active_count > 0` check caused an infinite
retry loop with no way out.

When `rsd.deletion_timestamp.is_some()`, active deployments are now scheduled
for drain (respecting `spec.restate.drainDelaySeconds`) rather than treated as
a permanent blocker. After the drain period they proceed through the existing
force-delete path.

### Why This Matters

This is the common case in **ephemeral PR/preview environments**: a service is
deployed, registered with Restate, but no workflows are ever invoked before the
environment is torn down. Without this fix, deleting the `RestateDeployment`
would stall forever and require manual intervention to remove the finalizer.

### Impact on Users

- **Existing deployments being deleted:** Stuck `RestateDeployment` objects will
  make progress on the next reconcile after upgrading.
- **New deletions:** Behave as expected — drain delay is respected, then the
  Restate deployment is force-deleted and the finalizer released.
- **No migration required.**

### Related Issues

- Issue #172: RestateDeployment finalizer stuck in CleanupFailed(DeploymentInUse) loop when no invocations ran
