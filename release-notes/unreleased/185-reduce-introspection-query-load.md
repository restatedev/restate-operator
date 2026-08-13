# Reduce RestateDeployment introspection query load

## Bug Fix

### What Changed

The RestateDeployment controller no longer issues a Restate introspection query
(`POST /query` over `sys_invocation_status`) on every reconcile, and no longer reconciles on
every HorizontalPodAutoscaler status update. Two changes:

- **Owned HPAs are watched for spec changes only.** The Kubernetes HPA controller re-writes an
  HPA's status on a ~15s timer; with per-version autoscaling that is one such write per version
  (including every draining one), and each previously woke the owning RestateDeployment. The
  watch now filters to generation (spec) changes, so those heartbeats no longer trigger
  reconciles. The operator's HPA cache still sees every update.
- **The deployment-usage query runs only when it is needed.** The query answers two questions:
  is this version the one Restate routes new invocations to (registration), and have older
  versions drained (cleanup). In the common case — this version already latest, with no older
  version to drain — the operator determines "already latest" from the cheaper `GET /services`
  and skips the invocation-status query entirely. Registration, promotion, rollback,
  foreign-takeover, and drain paths run the full query exactly as before.

### Why This Matters

With per-version autoscaling enabled, a deployment carrying several draining versions produced
a steady stream of reconciles — roughly one HPA heartbeat every 15 seconds per version — each
issuing an invocation-status query. Across many deployments sharing one Restate admin endpoint
this multiplied into sustained query load that could overwhelm Restate's query engine,
surfacing as `500 Internal Server Error` ("No such scanner") responses from `/query`.

### Impact on Users

- No configuration change, and no change to registration, rollback, or drain behaviour.
- Restate admin/query load from the operator drops sharply in the steady state: introspection
  queries are issued during rollouts and drains, not continuously.
- HorizontalPodAutoscaler status changes no longer trigger RestateDeployment reconciles; spec
  changes and (re)creation still do.

### Migration Guidance

None. No CRD, Helm value, or CLI flag changes.

### Related Issues

- #185: reducing expensive RestateDeployment usage-query load (also approached by #186 and #188).
