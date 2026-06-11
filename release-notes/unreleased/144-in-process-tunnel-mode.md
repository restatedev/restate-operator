# Release Notes for PR #144: `tunnelMode: in-process` for Restate Cloud deployments

## New Feature

### What Changed
`RestateDeployment` gained `spec.restate.tunnelMode` (only takes effect with
`register.cloud`). With `tunnelMode: in-process`, the deployment's pods hold
their own outbound tunnel connections to Restate Cloud — for example with the
`@restatedev/restate-sdk-tunnel` npm package — instead of being reached
through the tunnel-client pods and each version's `Service`.

For such deployments the operator:

- injects `RESTATE_INPROC_TUNNEL_NAME` (the versioned name of the revision),
  `RESTATE_INPROC_ENVIRONMENT_ID`, `RESTATE_INPROC_CLOUD_REGION` and
  `RESTATE_INPROC_SIGNING_PUBLIC_KEY` into every container (and init
  container) of the pod template; declaring one of these yourself is a
  reconcile error. Credentials are never injected —
  `RESTATE_INPROC_AUTH_TOKEN_FILE` is reserved for your own Secret mount
- registers each version under its tunnel URL
  (`https://tunnel.<region>.restate.cloud:9080/<env>/<versioned-name>/http/in-process/9080/`)
  instead of the Service URL
- mints a new version when the referenced `RestateCloudEnvironment`'s
  environment id, region or signing key changes, exactly like a pod template
  change (the values are folded into the revision hash — only when the mode
  is set, so existing deployments keep their hashes)

Versioning, draining, removal and the per-revision `Service` are unchanged.
`tunnelMode: in-process` is rejected in Knative mode.

### Why This Matters
Workloads in private networks can serve a Restate Cloud environment with zero
inbound networking — no Service reachability from the tunnel-client pods
required — while keeping the operator's transparent per-revision registration
and draining. Paired with the npm package's `RESTATE_INPROC_*` fallbacks,
application code needs no tunnel configuration at all beyond a mounted API
key.

### Impact on Users
- Existing deployments: no impact. The field is optional; hashes of
  deployments that don't set it are unchanged, so no ReplicaSets roll on
  upgrade.
- New in-process deployments: set `tunnelMode: in-process` under
  `spec.restate` with `register.cloud`, run an in-process tunnel client in
  your pods, and mount an API key Secret (point
  `RESTATE_INPROC_AUTH_TOKEN_FILE` at it).

### Migration Guidance
No migration required. To adopt the mode on an existing cloud-registered
deployment, add `tunnelMode: in-process` — this creates a new version (new
hash), which is registered through the tunnel while the old Service-routed
version drains as usual. Reapply the CRDs (`kubectl apply --server-side -f
crd/restatedeployments.yaml`) before using the new field.
