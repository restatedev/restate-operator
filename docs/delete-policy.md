# Deleting a RestateDeployment: `deletePolicy`

A Restate deployment is immutable and may have invocations pinned to it, so deleting a
`RestateDeployment` is not just a matter of removing its ReplicaSets. The operator holds a
finalizer (`deployments.restate.dev`) and, by default, waits for Restate to finish with
every version it registered before it deregisters them and lets the object go.

`spec.restate.deletePolicy` decides whether there is a wait at all, and `spec.restate.drain`
decides its terms: `timeoutSeconds` (default `3600`) sets the deadline, and `onTimeout`
(default `hold`) says what happens when it arrives. The clock starts at
`metadata.deletionTimestamp`, not at the last reconcile.

| `deletePolicy` | `drain.onTimeout` | Waits for invocations | At the deadline                             |
|----------------|-------------------|-----------------------|---------------------------------------------|
| `drain` (default) | `hold` (default) | yes, indefinitely   | keeps waiting, reports `Overdue`            |
| `drain`        | `force`           | yes                   | deregisters anyway, abandoning what is left |
| `force`        | n/a               | no                    | n/a — `drain` is ignored entirely            |

Under `onTimeout: hold` the timeout never lets the deletion through; it only decides when
the drain starts reporting itself `Overdue`, which is the signal that it needs a human.

```yaml
spec:
  restate:
    register:
      cluster: my-cluster
    deletePolicy: drain
    drain:
      timeoutSeconds: 1800
      onTimeout: force
```

## What counts as an invocation still in flight

Per version, the operator asks Restate for two counts:

- **pinned**: unfinished invocations already bound to that deployment id.
- **unpinned**: unfinished invocations not yet bound to any deployment, but targeting a
  service this version is the registered endpoint for. This includes scheduled
  invocations, whose execution time can be arbitrarily far out — a deletion held by one of
  these may never clear on its own.

Completed invocations don't count, even though Restate keeps them in
`sys_invocation_status` for the retention window (24h by default).

A `force` deletion blocks on nothing, so it doesn't run the query that attributes unpinned
work; the counts it reports for what it walked over are labelled pinned-only for that
reason. (An `onTimeout: force` drain whose deadline expires after the query has already
run does have both counts, and reports both.)

## Watching a deletion

`.status.deletion` is written while the deletion is in progress:

```yaml
status:
  deletion:
    policy: drain
    phase: Overdue
    startedAt: "2026-08-31T10:00:00Z"
    deadline: "2026-08-31T11:00:00Z"
    onTimeout: hold
    message: Still waiting on greeter-7d9f4c (3 pinned, 7 unpinned invocations) after the 3600s drain timeout
    totalPendingInvocations: 10
    pendingInvocations:
    - version: greeter-7d9f4c
      deploymentId: dp_abc123
      pinned: 3
      unpinned: 7
```

Phases are `Draining` (waiting on invocations, or on `drainDelaySeconds` once they have
finished), `Overdue` (past `drain.timeoutSeconds` and still waiting) and `Forcing`.
`onTimeout` is echoed back so the `deadline` reads unambiguously: it is when the drain
gives up under `force`, and only when it starts complaining under `hold`.

`kubectl get rsd -o wide` shows the phase and the total. A force deletion that abandoned
work also leaves a `ForcedDeletion` warning event, which is the only lasting record of it —
the object is gone moments later.

## Unsticking a deletion

The deletion is blocked, `phase: Overdue`, and the invocations are not going to finish.
Either deal with the invocations:

```bash
restate invocations cancel <id>     # or: purge, for completed ones
```

or change the terms on the terminating object — Kubernetes allows spec updates while a
finalizer is pending. To give up on what is left:

```bash
kubectl patch rsd greeter --type=merge \
  -p '{"spec":{"restate":{"drain":{"onTimeout":"force"}}}}'
```

The next reconcile deregisters the remaining versions with `force=true` and removes the
finalizer. `{"deletePolicy":"force"}` does the same thing and additionally skips the
drain delay and revision history limit; either works, since the drain is already overdue.

To keep waiting but stop the reporting, raise the timeout instead:

```bash
kubectl patch rsd greeter --type=merge \
  -p '{"spec":{"restate":{"drain":{"timeoutSeconds":86400}}}}'
```

### If Restate itself is unreachable

`force` skips the *wait*, not the *deregistration*: every pass still queries the admin API
to find out which versions are registered, and a version whose registration the operator
can't confirm is never torn down blind. So if the deletion is stuck because Restate is
down or gone, `force` will not move it -- `.status.deletion` reports `phase: Forcing` while
a `FailedReconcile` event and the operator log show the underlying `AdminCallFailed`.

If the Restate cluster is genuinely gone for good, delete the `RestateCluster` -- cleanup
short-circuits on a `spec.restate.register.cluster` that no longer exists and lets the
object go. Failing that (a `register.url` endpoint, say), remove the finalizer by hand,
accepting that the registrations are orphaned:

```bash
kubectl patch rsd greeter --type=json \
  -p '[{"op":"remove","path":"/metadata/finalizers"}]'
```

## What `force` skips

Beyond not waiting for invocations, a force deletion also skips `drainDelaySeconds` (the
grace period between a version being drained and its removal) and the revision history
limit. Every version it owns is deregistered and torn down on the first pass.

It does not change how *rollouts* retire old versions: `drainDelaySeconds` and
`revisionHistoryLimit` still apply there, and old versions with invocations in flight are
still kept alive. `deletePolicy` only takes effect once the `RestateDeployment` itself has
been deleted.
