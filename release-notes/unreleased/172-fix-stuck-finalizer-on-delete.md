# Release Notes for Issue #172: Fix RestateDeployment finalizer stuck in CleanupFailed loop on delete

## Bug Fix

### What Changed

When deleting a `RestateDeployment`, the finalizer no longer gets permanently
stuck with `CleanupFailed(DeploymentInUse)`. Previously, cleanup treated the
latest version's Restate deployment as an unconditional blocker because it is
always "active" — it is the latest entry in `sys_service`. Since no new version
ever registers during deletion, the `active_count > 0` check caused an infinite
retry loop with no way out.

The operator now tracks the two liveness signals separately instead of one
`active` flag:

- `latest_endpoint` — serves the latest revision of a service (`sys_service`)
- `has_pinned_invocations` — has a non-completed invocation pinned to it
  (`sys_invocation_status`)

Outside deletion nothing changes: either signal keeps a version alive. During
deletion only pinned invocations are worth waiting for — a version that is
merely the latest endpoint has nothing to drain, so it is deregistered and
removed immediately. A version with live pinned invocations is held for
`spec.restate.drainDelaySeconds` first, then force-deregistered as before.

Cleanup during deletion also bypasses `spec.revisionHistoryLimit`. Retaining
versions for rollback made no sense once the object is going away, and it left
the Restate deployments registered forever with nothing to deregister them
after the finalizer was released.

Both deployment modes are covered — ReplicaSet and Knative (Configurations).

### Why This Matters

This is the common case in **ephemeral PR/preview environments**: a service is
deployed, registered with Restate, but no workflows are ever invoked before the
environment is torn down. Without this fix, deleting the `RestateDeployment`
would stall forever and require manual intervention to remove the finalizer.

### Impact on Users

- **Existing deployments being deleted:** Stuck `RestateDeployment` objects will
  make progress on the next reconcile after upgrading.
- **New deletions with no live invocations:** Complete immediately, without
  waiting out the drain delay.
- **New deletions with live pinned invocations:** Drain delay is respected, then
  the Restate deployment is force-deleted and the finalizer released.
- **No migration required.**

### Related Issues

- Issue #172: RestateDeployment finalizer stuck in CleanupFailed(DeploymentInUse) loop when no invocations ran
