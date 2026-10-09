# Release Notes for Issue #185: Rate limit RestateDeployment cleanup queries

## Behavioral Change

### What Changed

The operator now rate-limits expensive RestateDeployment deployment-usage queries per
Restate Admin API endpoint. Only one such query may run for an endpoint at a time, and
retries are spaced with exponential backoff and jitter.

### Why This Matters

When a deployment remains in use, repeated reconciles can otherwise repeatedly issue a
costly usage query. Coordinating these retries protects the Restate environment while
preserving normal cleanup once the deployment can be removed.

### Impact on Users

- Existing and new deployments may take longer to retry cleanup after a query-related
  failure or while another deployment targeting the same endpoint is being checked.
- No manifest, Helm value, or migration change is required.

### Related Issues

- Issue #185: Use vqueues for pinned RestateDeployment accounting when capability is proven
