# Release Notes for Issue #103: Fix IAMPolicyMember 403 on non-GCP clusters

## Bug Fix

### What Changed
The operator now requires the `gcpWorkloadIdentity` Helm value to be explicitly
set before it will attempt to create or delete IAMPolicyMember resources. Previously,
the operator unconditionally attempted to clean up IAMPolicyMember resources on every
reconciliation, even on non-GCP clusters where the RBAC rules were not granted, causing
a 403 Forbidden error loop.

The `iam.gke.io/gcp-service-account` annotation in `serviceAccountAnnotations` is now
ignored with a warning unless `gcpWorkloadIdentity` is enabled.

### Impact on Users
- **Non-GCP clusters**: The 403 reconcile loop is fixed. No action needed if you were
  not using the `gcpWorkloadIdentity` Helm value.
- **GCP clusters using Workload Identity**: You must now set `gcpWorkloadIdentity: true`
  in your Helm values for the operator to manage IAMPolicyMember resources.

### Migration Guidance
If you are using GCP Workload Identity with Config Connector, add to your Helm values:

```yaml
gcpWorkloadIdentity: true
```

### Related Issues
- Issue #103: IAMPolicyMember cleanup causes 403 on non-GCP clusters
