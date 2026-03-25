# Release Notes for Issue #94: Configurable canary image

## New Feature

### What Changed
The container image used for PIA and Workload Identity canary jobs is now
configurable via the `canaryImage` Helm value, `CANARY_IMAGE` environment
variable, or `--canary-image` CLI flag. Previously `busybox:uclibc` was
hardcoded, which fails in environments that cannot pull from Docker Hub.

### Why This Matters
Air-gapped or restricted environments require all images to be pulled from
a private registry. The hardcoded image caused canary pods to enter
ImagePullBackOff, blocking RestateCluster reconciliation.

### Impact on Users
- **Existing deployments**: No impact. The default remains `busybox:uclibc`.
- **Restricted environments**: Can now point to a private registry mirror.

### Migration Guidance
If your nodes cannot pull from Docker Hub, set the canary image in your
Helm values:

```yaml
canaryImage: my-registry.example.com/busybox:uclibc
```

The simplest approach is to mirror the default image to your private registry:

```bash
docker pull busybox:uclibc
docker tag busybox:uclibc my-registry.example.com/busybox:uclibc
docker push my-registry.example.com/busybox:uclibc
```

If using a different image, it must provide `grep` and `wget` (used by the
AWS PIA and GCP Workload Identity canary jobs respectively).

### Related Issues
- Issue #94: Cannot configure image URI for PIA canary pods
