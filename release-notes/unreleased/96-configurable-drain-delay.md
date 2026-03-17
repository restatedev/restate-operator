# Release Notes for PR #96: Configurable drain delay

## New Feature

### What Changed
Added a new optional `drainDelaySeconds` field to the RestateDeployment CRD's
`spec.restate` section. This controls how long the operator waits after a
deployment is drained before removing the old version. Previously hardcoded to
5 minutes (300 seconds).

### Why This Matters
The default 5-minute safety buffer isn't always appropriate. Some environments
may want a longer window before old versions are cleaned up, while others may
want a shorter one.

### Impact on Users
- **Existing deployments**: No impact. The default remains 300 seconds (5 minutes).
- **New deployments**: Can now configure the drain delay per RestateDeployment.

### Migration Guidance
No migration needed. To configure a custom drain delay:

```yaml
spec:
  restate:
    drainDelaySeconds: 600  # 10 minutes
```

### Related Issues
- PR #96: Make drain delay configurable via drainDelaySeconds field
