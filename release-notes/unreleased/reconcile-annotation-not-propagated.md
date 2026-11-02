# Release Notes: `restate.dev/reconcile` is no longer copied to owned objects

## Bug Fix

### What Changed
The operator copies a resource's annotations onto the objects it creates, including the RestateCluster's pod template. The break-glass annotation `restate.dev/reconcile` is no longer copied.

### Why This Matters
Setting or removing the annotation changed the pod template, which rolled the pods. Pausing a cluster under an operator that does not know the annotation (2.x) restarted it, and resuming a cluster whose template carried the annotation restarted it again.

### Impact on Users
- Clusters that were never paused: no change.
- Clusters whose pod template carries the annotation (paused while an older operator reconciled them): the annotation leaves the template on their next reconcile after resuming, which rolls them once.

### Migration Guidance
None.
