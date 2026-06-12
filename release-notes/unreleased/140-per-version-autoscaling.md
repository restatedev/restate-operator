# Release Notes for Issue #140: Per-version autoscaling for draining RestateDeployment versions

## New Feature

### What Changed
A new optional `spec.autoscaling` field on `RestateDeployment` (ReplicaSet mode)
lets the operator manage a `HorizontalPodAutoscaler` per **non-latest** version.
The field is a pass-through `HorizontalPodAutoscaler` `.spec` (`minReplicas`,
`maxReplicas`, `metrics`, `behavior`); the operator injects `scaleTargetRef` per
version and owns the HPA, so it is garbage-collected with the
`RestateDeployment`.

A non-latest version gets an HPA while Restate still reports it active (has
invocations); the HPA is removed when the version goes inactive — before the
operator scales the ReplicaSet to zero.

### Why This Matters
Previously, every old version was held at its full `replicas` count for the
entire drain window — which can last hours for long-running or stuck workflows —
multiplying compute cost across concurrently-draining versions, with no good
lever to reduce it (an HPA on the `RestateDeployment` scale subresource only
ever scales the latest version). Now old versions shed compute as their load
falls, while remaining available for in-flight invocations.

### Impact on Users
- Existing deployments: no impact — `autoscaling` is optional, and omitting it
  preserves today's behaviour (draining versions stay at full `replicas`).
- New deployments: opt in to autoscale draining versions.
- Knative mode: unaffected — Knative's autoscaler already handles this.

### Migration Guidance
No migration is required to keep current behaviour. To opt in, add an
`autoscaling` block to your `RestateDeployment`:

```yaml
spec:
  autoscaling:
    minReplicas: 1
    maxReplicas: 10
    metrics:
      - type: Resource
        resource:
          name: cpu
          target:
            type: Utilization
            averageUtilization: 70
```

Notes:
- The **latest** version is not covered here — autoscale it with your own HPA
  targeting the `RestateDeployment` scale subresource.
- `minReplicas` is floored at 1 (there is no scale-to-zero in ReplicaSet mode).
- CPU/memory metrics require container resource `requests`; prefer CPU (memory
  does not scale back down).
- **RBAC**: the bundled Helm chart now grants the operator permissions on
  `horizontalpodautoscalers` (the `autoscaling` API group). If you deploy the
  operator with your own RBAC, add `get,list,watch,create,patch,delete` on that
  resource, or per-version HPAs will silently fail to be created.

### Related Issues
- Issue #140: Per-version autoscaling for RestateDeployment (ReplicaSet mode)
- Issue #139: status `labelSelector` scoping (prerequisite — makes latest-version
  autoscaling correct)
