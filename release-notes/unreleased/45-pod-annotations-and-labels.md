# Release Notes for Issue #45: Add support for custom pod annotations and labels

## New Feature

### What Changed
Added `spec.compute.annotations` and `spec.compute.labels` fields to the
RestateCluster CRD, allowing users to set custom annotations and labels on the
Restate StatefulSet pod template.

User-specified annotations and labels are merged with any that the operator sets
internally (e.g. for workload identity, trusted CA certs). In case of conflict,
operator-managed values take precedence.

### Why This Matters
Enables integrations that require pod-level metadata, such as GKE ComputeClass
scheduling (`cloud.google.com/compute-class`), Vault agent injection, Datadog,
Prometheus scraping, and custom scheduling constraints.

### Impact on Users
- Existing deployments: No impact, both fields are optional
- New deployments: Can now set annotations and labels for integrations that
  require them on the pod template

### Migration Guidance
No migration required. To use the new fields:

```yaml
spec:
  compute:
    annotations:
      cloud.google.com/compute-class: "restate-workload"
    labels:
      team: "platform"
```

### Related Issues
- Issue #45: Add support for custom annotations on StatefulSet
