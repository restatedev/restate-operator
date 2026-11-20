# Release Notes: Operator-managed pods satisfy the `restricted` Pod Security Standard

## Bug Fix

### What Changed
Every pod the operator creates now sets `runAsNonRoot: true` at pod level, and every container it creates drops all
Linux capabilities, in addition to the existing non-root user, seccomp `RuntimeDefault`, no privilege escalation and
read-only root filesystem. This covers:

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
- Clusters using PodIdentityAssociation (PIA) or GCP Workload Identity (WI) recreate their canary Job once on
  upgrade, because a Job's pod template is immutable. Until the new Job succeeds, the cluster briefly reports
  `Ready=False` with reason `PodIdentityAssociationCanaryPending` or `WorkloadIdentityCanaryPending`.
- A custom `canaryImage` must be able to run `cat`, `grep` and `wget` as a non-root user.
- `runAsNonRoot: true` at pod level also applies to containers in `spec.compute.sidecars`. A sidecar that sets
  `runAsUser: 0` fails to start after the upgrade (`container's runAsUser breaks non-root policy`). Sidecars without
  their own `runAsUser` already ran as uid 1000 from the pod security context and are unaffected.
- RestateDeployment pods come from your own spec and are unchanged.

### Migration Guidance
If a sidecar has to run as root, set `runAsNonRoot: false` in that container's `securityContext`; the container-level
value overrides the pod-level one. Such a pod is not admitted under `restricted` Pod Security.
