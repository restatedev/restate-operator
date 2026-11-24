# Release Notes: Custom registration endpoint for RestateDeployment

## New Feature

### What Changed

- New optional field `spec.restate.endpoint` on `RestateDeployment` (ReplicaSet mode only).
  - `url`: a URL template registered with Restate instead of the per-version Service URL.
  - `headers`: extra headers Restate sends to the version (`additional_headers` on registration).
- `url` and header values support `{{ revision }}`, `{{ name }}` and `{{ namespace }}`.
- When `url` is set it must reference `{{ revision }}`, so versions stay distinct. If Restate
  still returns another version's deployment id (a shared URL whose revision header is not in
  the server's `admin.deployment-routing-headers`), the reconcile fails with
  `InvalidRestateConfig` instead of recording the shared id.
- Changing `endpoint` creates a new version (new ReplicaSet and Restate deployment).

### Why This Matters

A ClusterIP Service balances per connection, and Restate keeps long-lived HTTP/2 connections,
so invocations can concentrate on a few pods of a version. Registering through an L7 proxy
(e.g. Envoy Gateway) allows per-request balancing while the operator keeps managing versions,
draining and deregistration.

### Impact on Users

- No change when `endpoint` is unset: same URL, same registration request, same ReplicaSet names.
- Not supported with `deploymentMode: knative` or `tunnelMode: in-process`, or together with
  `servicePath` when `url` is set (rejected by the CRD).

### Migration Guidance

None required. To use it, set `spec.restate.endpoint` and route the rendered URL/header to the
per-version Service (named like `{{ revision }}`) in your proxy. For header-based routing,
add the header to the Restate server's `admin.deployment-routing-headers`.
