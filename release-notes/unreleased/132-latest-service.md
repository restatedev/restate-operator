# Release Notes for Issue #132: Stable Service for the latest RestateDeployment version

## New Feature

### What Changed

A `RestateDeployment` in ReplicaSet mode can set `spec.latestService.enabled: true`. The
operator then manages a `ClusterIP` Service named `<name>-latest` that selects only the pods of
the latest version and exposes all of their container ports. Its selector switches to a new
version once that version's ReplicaSet is ready and Restate has confirmed it as the latest
deployment, and switches back on a rollback.

### Why This Matters

Applications that also serve traffic outside Restate (for example, HTTP endpoints behind an
Ingress) had no stable way to send it only to the newest version: old versions keep running
while their invocations drain, and a Service selecting the application's pods kept routing
traffic to them.

### Impact on Users

- Off by default; existing deployments are unchanged.
- The field is rejected in Knative mode.
- If a Service named `<name>-latest` already exists and is not owned by the `RestateDeployment`,
  the operator does not touch it and reports a `LatestServiceFailed` event.
- The operator's ClusterRole gains `delete` on `services`, used to remove the Service when
  `latestService` is disabled. The bundled Helm chart includes it; if you manage the operator's
  RBAC yourself, add it.

### Migration Guidance

Upgrade the CRDs (the `restate-operator-crds` chart, or `crd/restatedeployments.yaml`) to use
the new field.

### Related Issues

- Issue #132: Label or stable name for the latest RestateDeployment version
