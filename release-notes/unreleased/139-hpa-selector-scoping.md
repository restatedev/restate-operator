# Release Notes for Issue #139: Scope RestateDeployment status labelSelector to the latest version

## Bug Fix

### What Changed
The `RestateDeployment` scale subresource's `.status.labelSelector` is now scoped
to the latest version's pods by appending the `pod-template-hash`. Previously it
was written verbatim from `spec.selector` with no version filter.

### Why This Matters
A `HorizontalPodAutoscaler` targeting a `RestateDeployment` reads
`.status.labelSelector` to decide which pods' metrics to aggregate. Without the
version filter it matched pods across **every** ReplicaSet the deployment had
ever owned — the latest plus any old ones still draining pinned invocations.
Over the (potentially multi-hour) drain window this polluted the averaged
metric, dragging it down and under-provisioning — or even scaling down — the
genuinely-busy latest version.

### Impact on Users
- Deployments with an HPA targeting a `RestateDeployment`: autoscaling decisions
  for the latest version are now based only on latest-version pods, fixing
  under-provisioning during drains.
- Deployments without an HPA: no behavioural impact.

### Migration Guidance
No action required.

### Related Issues
- Issue #139: scale subresource label selector points to pods for all versions
