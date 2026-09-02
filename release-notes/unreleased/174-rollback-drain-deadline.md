# Release Notes for Issue #174: Rolled-back versions keep their drain delay

## Bug Fix

### What Changed

A version that is rolled back to — or reintroduced with an identical spec — no longer
carries a stale removal deadline into its next rollout.

When a version is superseded, the operator stamps `restate.dev/remove-version-at` on its
ReplicaSet (Knative mode: its Configuration) to schedule teardown after
`spec.restate.drainDelaySeconds`. Because ReplicaSets and Services are named by a content
hash of the pod template, and Configurations by tag, rolling back re-adopts that exact
object rather than creating a new one — and the stamp came with it. Nothing removed it:
the annotation has its own field manager, so the operator's other applies own the fields
they set and not that one, and the cleanup pass skips whichever version is currently
latest.

The next time that version was superseded, its deadline had already passed, so it was
scaled to zero on the first reconcile instead of being given its drain window.

The operator now clears the annotation when it re-adopts a version as the latest one, in
both deployment modes.

Clearing it also works now. In ReplicaSet mode the old clear left the annotation behind
with an empty value instead of removing it, so a version that became active again while
draining kept an empty deadline for good and was re-patched on every reconcile. Knative
mode was unaffected by that part.

### Why This Matters

The drain delay exists so that invocations already pinned to a version can finish on it.
A version that skipped the delay was scaled to zero while that work was still in flight,
which surfaces as invocations retrying against a version with no pods behind it until they
are re-pinned or time out.

The window for this was narrow but not exotic: it needed a rollback (or a re-applied
identical spec) followed by another rollout, which is the ordinary shape of "revert, fix
forward".

### Impact on Users

- **Existing deployments:** no configuration change. A version currently holding a stale
  or empty deadline has it cleared the next time it is reconciled as the latest version.
- **New deployments:** no impact.
- Rollback followed by a further rollout now takes `drainDelaySeconds` longer to tear the
  intermediate version down — that delay is the fix, not a regression.

### Migration Guidance

None required.

### Related Issues

- Issue #174: Support rolling RestateDeployments back to a previously registered revision
