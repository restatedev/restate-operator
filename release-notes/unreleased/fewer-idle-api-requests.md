# Release Notes: Far fewer Kubernetes API requests when nothing has changed

## Behavioral Change

### What Changed
- Idle reconciles no longer write to the Kubernetes API. The operator compares what it would
  apply with its watch caches and skips server-side applies, deletes of objects that are already
  gone, and status patches that would change nothing. RestateDeployment no longer creates the
  current ReplicaSet only to get a `409 AlreadyExists` back.
- Each RestateCluster and RestateCloudEnvironment is now reconciled once per event or resync.
  Previously every namespaced child queued a second reconcile of its cluster-scoped owner, so
  each RestateCluster was reconciled twice.
- The steady-state resync is configurable and defaults to **30 minutes** (was 5): CLI
  `--resync-interval-seconds`, env `RESYNC_INTERVAL_SECONDS`, Helm `resyncIntervalSeconds`.
  Each resync is jittered by ±10%. The shorter requeues used while a resource is not ready, while
  an old version drains, or after an error are unchanged.
- Out-of-band edits to the per-version Services of a RestateDeployment, and to the tunnel
  Deployment and Service of a RestateCloudEnvironment, are now reverted as soon as the watch
  reports them rather than at the next resync.

### Why This Matters
On a local kind cluster with audit logging, an idle RestateCluster with three replicas cost 21
API requests per reconcile, twice every 5 minutes: about 12,000 requests (about 18 MB of
audit log) per cluster per day. On EKS each one is an audit record billed by CloudWatch. With
this change an idle reconcile makes no requests at all.

### Impact on Users
- Drift is still repaired: every object the operator writes is watched, and a write to it by
  anyone else is undone when the watch delivers it, as before.
- After an operator restart, the first reconcile of each resource applies everything once.
- The resync is now a backstop for state the watches cannot see (for example a deployment
  removed from Restate by hand), so such changes take up to 30 minutes to be noticed. Lower
  `resyncIntervalSeconds` if that is too long.
