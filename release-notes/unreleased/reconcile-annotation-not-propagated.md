# Release Notes: `restate.dev/reconcile` is no longer copied to owned objects

## Bug Fix

### What Changed
The operator copies a resource's annotations onto the objects it creates, including the RestateCluster's pod template. The break-glass annotation `restate.dev/reconcile` is no longer copied.

### Why This Matters
Setting or removing the annotation changed the pod template, which rolled the pods. Pausing a cluster under an operator that does not know the annotation (2.x) restarted it, and resuming a cluster whose template carried the annotation restarted it again.

### Impact on Users
- Resources without the annotation: no change.
- Clusters whose pod template carries the annotation roll once, on their first reconcile with this version: those paused while an older operator reconciled them (after resuming), and those carrying the annotation with any value other than `disabled`, which an older operator copied into the template.
- A ReplicaSet created while its RestateDeployment carried the annotation keeps it on its own metadata. It has no effect there.

### Migration Guidance
None.
