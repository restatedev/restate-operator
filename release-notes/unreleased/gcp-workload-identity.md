# Release Notes: GCP Workload Identity automation via Config Connector

## New Feature

### What Changed
The operator now automatically creates Config Connector `IAMPolicyMember`
resources to bind Kubernetes service accounts to GCP service accounts via
Workload Identity. This is triggered when a RestateCluster has the
`iam.gke.io/gcp-service-account` annotation in `serviceAccountAnnotations`.

The GCP project ID is extracted from the service account email
(`name@PROJECT.iam.gserviceaccount.com`), so no additional configuration
is needed beyond the annotation that the control plane already sets.

A canary job validates that Workload Identity credentials are available
before allowing the StatefulSet to proceed, preventing Restate from starting
without GCS access.

This mirrors the existing AWS Pod Identity Association pattern.

### Why This Matters
Previously, the IAM binding for Workload Identity had to be created manually
via `gcloud` commands each time a new environment was provisioned. This
automates that step, bringing GCP to parity with the existing AWS automation.

### Impact on Users
- **Existing deployments**: No impact. The feature only activates when
  `iam.gke.io/gcp-service-account` is present in `serviceAccountAnnotations`.
- **GCP deployments using this annotation**: Config Connector must be installed
  on the GKE cluster. If the `IAMPolicyMember` CRD is not available, the
  operator sets a `NotReady` status condition with a clear message rather
  than crashing.
- **AWS deployments**: No impact. The canary job infrastructure was refactored
  to share code between AWS and GCP, but behavior is unchanged.

### Migration Guidance
No migration needed. To use this feature:

1. Install Config Connector on the GKE cluster
2. Ensure the Config Connector service account has `roles/iam.serviceAccountAdmin`
   on the target GCP service account
3. Set `serviceAccountAnnotations` on the RestateCluster:

```yaml
spec:
  security:
    serviceAccountAnnotations:
      iam.gke.io/gcp-service-account: restate@my-project.iam.gserviceaccount.com
```
