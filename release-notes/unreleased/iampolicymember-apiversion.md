# Release Notes: Fix IAMPolicyMember server-side apply wedge

## Bug Fix

### What Changed
The `restate-workload-identity` IAMPolicyMember CR is now applied with
`spec.resourceRef.apiVersion` set to `iam.cnrm.cloud.google.com/v1beta1`,
matching the value Config Connector defaults at creation time.

### Why This Matters
The operator previously omitted `resourceRef.apiVersion` from its
server-side apply request. Config Connector defaults the field on the
initial create, but SSA's atomic-struct semantics then stripped that
defaulted value on every subsequent reconcile. Config Connector's
`deny-immutable-field-updates` admission webhook rejected the change
with:

```
admission webhook "deny-immutable-field-updates.cnrm.cloud.google.com"
denied the request: the IAMPolicyMember's spec is immutable
```

Reconcile looped on the failure and blocked unrelated `RestateCluster`
updates (image overrides, resource changes, etc.) from being applied.

### Impact on Users
- Affects clusters using GCP workload identity binding via the operator
  (BYOC installs on GKE / Config Connector).
- Existing installs already wedged on this error are unblocked once the
  upgraded operator runs a reconcile.
- The webhook only fires on changes to immutable fields, and the value
  the operator now emits matches what Config Connector defaulted at
  create time, so the apply becomes a no-op for that field.

### Migration Guidance
No user action required beyond upgrading the operator. Existing BYOC
installs need an operator chart version bump and a Nuon roll to pick
up the fix.

### Related Issues
- restatedev/restate-cloud#833 (per-environment GCP service account
  identity, where this bug was surfaced during dev canary validation)
