# Release Notes: Configurable tunnel proxy URL for Restate Cloud

## New Feature

### What Changed
`RestateCloudEnvironment` has a new optional field, `spec.proxyBaseUrl`. When it is set, deployments registered against that environment use its host and port instead of `tunnel.<region>.restate.cloud:9080`. The port defaults to 9080, as it does for the `restate` CLI. This applies to both `tunnelMode: external` and `tunnelMode: in-process`.

### Why This Matters
Some BYOC regions serve the tunnel proxy under a different name, such as `proxy.<region>.restate.cloud`. On those regions the default URL times out, and registration fails with `META0003`.

### Impact on Users
- Nothing changes when `proxyBaseUrl` is not set.
- With `tunnelMode: in-process`, setting or changing `proxyBaseUrl` creates a new revision, because the registered URL changes with it.
- With `tunnelMode: external`, a deployment that is already registered keeps its URL. The new URL is used for the next registration, for example on the next rollout.

### Migration Guidance
Copy the `proxyBaseUrl` that describing your environment returns, for example with the Restate Cloud API:

```yaml
spec:
  region: abc123xyz.byoc.us
  proxyBaseUrl: https://proxy.abc123xyz.byoc.us.restate.cloud:9080
```
