# Release Notes: Upgrade to kube 4 and k8s-openapi 0.28

## Behavioral Change

### What Changed
The operator now builds against kube 4.2 and k8s-openapi 0.28, which describe the Kubernetes 1.36 API.
The regenerated CRDs mark optional fields as `nullable: true`, and the descriptions of embedded
Kubernetes types (tolerations, volume claims and others) follow the 1.36 API documentation.

The `restate.dev/remove-version-at` and `restate.dev/registered-at` annotations are now written with
a `Z` suffix (for example `2026-01-01T00:00:00Z`) where they used a `+00:00` offset before. Both
forms are RFC 3339 timestamps, and both this release and earlier ones read either form.

### Why This Matters
kube 2 depended on `serde_yaml`, which is deprecated, and on `unsafe-libyaml`, which is unmaintained
(RUSTSEC-2024-0449). kube 4 removes both from the operator.

### Impact on Users
- Existing installations keep their current CRDs until the new ones are applied. The current CRDs
  keep working with this release.
- Tools that compare these annotations as strings, rather than parsing them as timestamps, will
  see the new suffix on versions that are scheduled for removal after the upgrade.

### Migration Guidance
- Apply the CRDs from `crd/` to pick up the `nullable` markers.
