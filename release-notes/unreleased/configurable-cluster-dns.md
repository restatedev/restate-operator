# Release Notes: Configurable cluster DNS suffix

## New Feature

### What Changed
The operator now supports configuring the Kubernetes cluster DNS suffix via a
new `--cluster-dns` CLI flag, `CLUSTER_DNS` environment variable, or Helm
`clusterDns` value. Previously, the DNS suffix `cluster.local` was hardcoded in
all internal service URLs (admin API, gRPC provisioning, service discovery).

### Why This Matters
While `cluster.local` is the Kubernetes default, clusters can be configured with
a different DNS suffix via the kubelet `--cluster-domain` flag or CoreDNS
configuration. This is common in multi-cluster setups, federated environments,
and enterprises with custom naming conventions. Without this change, the operator
could not be used on such clusters — all internal service-to-service communication
would fail with DNS resolution errors.

### Impact on Users
- **Existing deployments**: No impact. The default remains `cluster.local`.
- **New deployments on non-standard DNS clusters**: Can now configure the correct
  DNS suffix and the operator will work correctly.

### Migration Guidance
No migration needed for existing users. For clusters with a custom DNS suffix,
configure the operator using one of:

**Helm** (recommended):
```yaml
# values.yaml
clusterDns: "my.custom.domain"
```

**Environment variable**:
```yaml
env:
  - name: CLUSTER_DNS
    value: "my.custom.domain"
```

**CLI flag**:
```bash
restate-operator --cluster-dns my.custom.domain
```
