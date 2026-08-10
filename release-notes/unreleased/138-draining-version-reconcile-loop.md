# Release Notes for Issue #138: Hot reconcile loop while an old version is draining

## Bug Fix

### What Changed

A `RestateDeployment` with more than one active version — a new version deployed while the old one
still has invocations durably pinned to it — reconciled in a tight self-sustaining loop, at roughly
8 reconciles per second and ~30k ReplicaSet writes per hour, indefinitely, with no spec or status
actually changing.

Two things combined to close the loop:

- The owned-ReplicaSet watch had no change predicate, so any ReplicaSet update re-enqueued its
  owner — including the operator's own writes, and including bare `resourceVersion` bumps with no
  observable field change.
- The path that adopts an already-existing ReplicaSet re-applied the scale subresource
  (`propagate-replicas`) and the propagated annotations (`propagate-annotations`) on every
  reconcile, without first checking whether either would change anything.

The fixes:

- Every stream feeding the RestateDeployment controller now has a change predicate, matching what
  the RestateCluster controller already did. The ReplicaSet stream reacts to its generation,
  labels, annotations and status — not to a bare `resourceVersion` bump; HorizontalPodAutoscalers
  ignore their (continuously churning) status; Services hash their spec.
- The per-reconcile applies onto an existing ReplicaSet, the per-version Service, and the
  RestateDeployment's own status are skipped when they would change nothing. Annotation and label
  pruning still happens: the operator reads back what it owns from `managedFields`, so a key
  removed from the RestateDeployment is still removed downstream.
- The poll that detects a still-active old version having drained now scales with
  `drainDelaySeconds` (a tenth of it, clamped to 10–60s, so 30s at the default 300) instead of
  running every 10s forever. Noticing the transition is followed by the deliberate
  `drainDelaySeconds` wait regardless, so polling much finer than that only cost apiserver traffic.
- The controller debounces reconcile requests by 500ms, as a backstop against any future write
  reintroducing a feedback edge.

### Why This Matters

On clusters with apiserver audit logging this produced a large and continuous write/audit volume —
the issue was found while investigating an increased cloud bill from EKS audit logs. The retention
of the old version was correct and remains unchanged; only the continuous rewriting of unchanged
objects is gone.

### Impact on Users

- Existing and new deployments both benefit; no configuration change is needed.
- A settled `RestateDeployment` with a draining version now reconciles on the drain poll interval
  (~30s at the default `drainDelaySeconds`) and writes nothing while nothing changes.
- Detection of a version having finished draining can now take up to `drainDelaySeconds / 10`
  longer than before (at most 60s). Removal itself is unaffected: it was already delayed by
  `drainDelaySeconds` after the version went inactive.

### Migration Guidance

None.

### Related Issues

- Issue #138: bug: owned ReplicaSet re-triggers reconciliation (~8/sec) while an old version is
  draining, causing a hot reconcile loop
