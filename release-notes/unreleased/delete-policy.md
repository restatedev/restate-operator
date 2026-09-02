# Release Notes: `deletePolicy` for RestateDeployment

## New Feature

### What Changed

Deleting a `RestateDeployment` is now governed by `spec.restate.deletePolicy`, whose terms
are set by `spec.restate.drain`:

- `deletePolicy: drain` (default): the existing behaviour — wait for in-flight invocations
  to finish before deregistering. `drain.timeoutSeconds` (default `3600`) sets the deadline
  and `drain.onTimeout` decides what happens there:
  - `hold` (default): keep waiting, and report the drain as overdue.
  - `force`: deregister anyway, abandoning whatever is left.
- `deletePolicy: force`: deregister and tear down immediately, skipping the drain,
  `drainDelaySeconds` and the revision history limit. `drain` is ignored entirely.

Progress is reported on `.status.deletion`, which names the versions holding the deletion
and their pinned/unpinned invocation counts, alongside the phase (`Draining`, `Overdue`,
`Forcing`), the deadline, what happens at it, and a total. `kubectl get rsd -o wide` shows
the phase and total.

A force deletion that walked over unfinished invocations raises a `ForcedDeletion` warning
event naming what it abandoned.

See `docs/delete-policy.md`.

### Why This Matters

A deletion blocked by a scheduled invocation days out, or by a workflow that will never
complete, previously had no deadline and no visible reason: the only signal was a warning
event and the operator log, and the only way out was to find and cancel the invocations.
There is now a bounded option, and a status field that answers "what is this waiting for".

### Impact on Users

- Existing deployments keep the current behaviour: no `deletePolicy` means `drain`, which
  still waits indefinitely for invocations to finish.
- After one hour, a blocked `drain` deletion now reports `phase: Overdue` and raises
  `DeletionDrainOverdue` instead of `DeploymentInUse`. The deletion itself is still held,
  and the Kubernetes event reason is unchanged (`FailedReconcile`) -- only its message
  changes, to name the timeout and point at the ways out of it.
- **Metric label change.** `restate_operator_reconciliation_errors_total` now labels
  RestateDeployment failures with the error the reconciler actually raised
  (`DeletionDrainOverdue`, `DeploymentInUse`, `AdminCallFailed`, ...) instead of
  collapsing all of them into `FinalizerError`. Alerts or dashboards matching
  `error="FinalizerError"` for this controller need updating.
- `deletePolicy: force` skips the wait, not the deregistration: it still needs the Restate
  admin API to be reachable, so it will not unstick a deletion that is blocked because
  Restate is down. See `docs/delete-policy.md`.
- `spec.restate.deletePolicy` and `spec.restate.drain` are new optional fields; the CRD
  must be updated before they can be set.
- A paused RestateDeployment (`restate.dev/reconcile: disabled`) still deletes normally,
  and now drops the `Disabled` reconciliation state and its `Reconciling` condition when
  the deletion starts, rather than reporting itself suspended throughout the teardown.

### Migration Guidance

Apply the updated CRDs (`helm upgrade` of `restate-operator-crds`, or
`kubectl apply --server-side -f crd/restatedeployments.yaml`). No changes are required to
existing `RestateDeployment` resources.

To unstick a deletion that is already blocked, patch the terminating object:

```bash
kubectl patch rsd greeter --type=merge \
  -p '{"spec":{"restate":{"deletePolicy":"force"}}}'
```
