# Release Notes for Issue #170: Do not propagate ApplySet bookkeeping labels

## Bug Fix

### What Changed

`RestateDeployment` no longer copies labels in the
`applyset.kubernetes.io/` namespace onto its operator-owned Services, Knative
Configurations, or Knative Routes. Other user labels continue to propagate.

### Why This Matters

kubectl uses `applyset.kubernetes.io/part-of` to decide which resources belong
to an ApplySet. Copying that label made operator-owned child resources look
like direct members of the ApplySet that contained the `RestateDeployment`.
A later `kubectl apply --prune --applyset=...` could consequently delete a
versioned Service before Restate had drained the corresponding deployment.

### Impact on Users

- Existing `RestateDeployment` resources need no manifest changes.
- The operator removes the bookkeeping labels from children on reconciliation.
- kubectl continues to manage and prune the `RestateDeployment` itself.

### Migration Guidance

Upgrade the operator. No CRD or workload configuration changes are required.

### Related Issues

- Issue #170: RestateDeployment propagates ApplySet membership to owned
  resources
