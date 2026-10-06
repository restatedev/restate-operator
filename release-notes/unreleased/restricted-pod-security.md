# Release Notes: Operator-managed pods satisfy the `restricted` Pod Security Standard

## Bug Fix

### What Changed
Every pod the operator creates now sets `runAsNonRoot: true` and drops all Linux capabilities, in addition to the
existing non-root user, seccomp `RuntimeDefault`, no privilege escalation and read-only root filesystem. This covers:

- the Restate StatefulSet container
- the `combine-ca-certs` init container (`spec.security.trustedCaCerts`)
- the PodIdentityAssociation and GCP Workload Identity canary Jobs, which previously had no security context and ran
  as the image's default user (root for `alpine`); they now run as uid 1000 / gid 3000
- the Restate Cloud tunnel Deployment

The Helm chart's own operator pod now defaults to `podSecurityContext.seccompProfile.type: RuntimeDefault`, the one
`restricted` requirement it was missing.

### Why This Matters
In namespaces enforcing `pod-security.kubernetes.io/enforce=restricted` (or an equivalent cluster-wide default),
the pods were rejected with `violates PodSecurity "restricted:latest"`. The StatefulSet was created but `restate-0`
never was, so with a `WaitForFirstConsumer` StorageClass the cluster reported `PersistentVolumeClaimNotBound`.

### Impact on Users
- Upgrading the operator changes the pod template, so existing RestateCluster StatefulSets and tunnel Deployments
  roll once.
- A custom `canaryImage` must be able to run `cat`, `grep` and `wget` as a non-root user.
- `spec.compute.sidecars` and RestateDeployment pods come from your own spec and are unchanged.
