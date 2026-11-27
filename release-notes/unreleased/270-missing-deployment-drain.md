# Release Notes for Issue #270: a version is only removed once a second answer confirms it

## Bug Fix

### What Changed

The operator no longer removes an old version because one `/query` answer left its deployment
out. A Restate node that has not loaded its schema answers with no rows at all
(restatedev/restate#6063), so during a rollout every previous version could be scaled to zero
while its pinned invocations were still running, and those invocations then stalled until the
deployment was scheduled again.

Now cleanup plans each version from Restate's answer, and if any planned action rests on a
recorded deployment id being absent (scheduling a removal, scaling a version down, or deleting
it for the revision history), it asks Restate again 3 seconds later and plans again from the
merged answer. An id counts as absent only if both answers lack it; rows come from the second
answer, and an id only the first answer had keeps its first-answer row. A pass where nothing
rests on an absence costs no extra query.

When registering a version returns the id it already had
recorded while the answer omitted that id, the answer was stale: the operator emits a
`StaleDeploymentUsage` Warning event, skips cleanup for that pass, and retries after 10 seconds.

### Why This Matters

A single stale answer used to be indistinguishable from a real deregistration, and acting on it
stranded pinned invocations for as long as the old version stayed at zero. A version that is
wrongly scheduled is now unscheduled on the next pass that lists it, with no persisted state.

### Impact on Users

- A version whose deployment really was deregistered out of band waits `drainDelaySeconds`
  before its pods go, instead of going immediately. `deletePolicy: force` and
  `onTimeout: force` are unchanged.
- The steady state costs no additional admin query; only a pass that would act on an absence
  pays for one more.
- No CRD, status, annotation or Helm change. `StaleDeploymentUsage` is a Kubernetes event only.

### Migration Guidance

None required.

### Related Issues

- restatedev/restate#6063: a node without a loaded schema answers `/query` with no rows.
