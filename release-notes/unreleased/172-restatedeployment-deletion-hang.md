# Release Notes for Issue #172: RestateDeployment deletion no longer hangs forever

## Bug Fix

### What Changed

Deleting a `RestateDeployment` no longer blocks indefinitely on its own latest version.

Cleanup previously asked Restate a single question — "is this deployment active?" — where
"active" meant *either* that a service still pointed at the deployment *or* that it had
unfinished invocations. During a rollout that conflation is harmless: a newer version
eventually takes over as the service's endpoint and the old one goes inactive. During a
deletion it was fatal. Nothing is coming to supersede the endpoint, so the latest version
stayed "active" forever, cleanup never deregistered it, `active_count` never reached zero,
and the finalizer requeued every 30 seconds without end.

The operator now tracks the two facts separately and weighs them by why cleanup is
running. Being a service's current endpoint holds a version through a rollout but is
ignored during a deletion; only unfinished invocations — which drain on their own — can
hold a deletion. The version then goes through the normal drain, deregistration and
teardown path.

Alongside that:

- **Unfinished invocations not yet bound to a deployment now count.** Paused, queued and
  scheduled work carries no `pinned_deployment_id`, so the old query scored it as zero and
  a deletion could tear the endpoint out from under it. It is now attributed through the
  target service. Only a deletion asks: that attribution costs a second scan of
  `sys_invocation_status` and a per-row decode of the invocation target, and during a
  rollout it can only ever name the deployment that is already the service's endpoint. The
  reconcile path's query is unchanged.
- **A blocked deletion says what is blocking it.** The `DeploymentInUse` event now names
  each version and its pinned/unpinned invocation counts instead of a generic message.
- **A blocked deletion backs off.** Retries start at the usual 30 seconds and stretch to
  five minutes the longer the wait runs, so a deletion parked behind a scheduled invocation
  days out stops re-running that query twice a minute for the duration.
- **Drain deadlines are now honoured.** The requeue interval derived from a version's
  remove-at time was being discarded, because errors reach the controller's error policy
  wrapped by the finalizer machinery; a short `drainDelaySeconds` cost up to 30 seconds per
  version regardless. It is now unwrapped, and floored at one second so a sub-second
  deadline cannot spin the reconciler.
- **A draining version keeps its autoscaler.** Removal of an inactive version's
  operator-managed HPA moved to the point where the version is actually scaled to zero.
  Previously a deletion stripped the HPA from every version on the first reconcile, while
  those versions were still serving traffic for the whole drain window.

### Why This Matters

Before this fix, `kubectl delete restatedeployment` never returned for a
`RestateDeployment` whose services were still registered — which is the normal state of any
healthy deployment. The only workaround was to remove the finalizer by hand, which left the
deployment registered in Restate with no pods behind it. Namespace deletion inherited the
same hang.

### Impact on Users

- **Existing deployments:** no configuration change. Deletions that were previously wedged
  will proceed the next time the operator reconciles them; ones whose finalizer was removed
  by hand may have left a stale registration behind in Restate.
- **Deletion now takes at least `spec.restate.drainDelaySeconds` (default 300s).** The
  latest version is put through the same drain as any superseded version, so teardown waits
  out the drain window even when the deployment never served an invocation. This is the
  interval that was previously unbounded.
- **Deletion still waits on unfinished invocations, and that wait has no upper bound.**
  Scheduled invocations are the sharp edge: a delayed call whose execution time is days out
  counts as unfinished and holds the deletion until it fires. The `DeploymentInUse` event
  reports the counts so this is diagnosable. Bounding or skipping the wait is the job of the
  planned `deletionPolicy` field, not of manual intervention.
- **New deployments:** no impact.

### Migration Guidance

None required. If a `RestateDeployment` is currently stuck deleting, upgrading the operator
is sufficient — no manual finalizer edits.

To check what is holding a deletion:

```bash
kubectl describe restatedeployment <name> -n <namespace>
# Warning  FailedReconcile  ... This RestateDeployment is backing active versions in
# Restate: greeter-7f9c4d (0 pinned, 3 unpinned invocations). ...
```

### Related Issues

- Issue #172: RestateDeployment finalizer never completes
