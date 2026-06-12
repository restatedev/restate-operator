# Design: Per-version autoscaling for RestateDeployment (ReplicaSet mode)

Status: draft / proposed
Mode: ReplicaSet only (Knative mode already autoscales the tail natively)

## Problem

A `RestateDeployment` (RD) keeps a separate `ReplicaSet` (RS) per immutable
service version. Old versions must stay alive until Restate reports their
`deployment-id` has no active invocations (the drain window), which can last
hours for long-running or stuck workflows.

Today every draining RS is held at the **full** `spec.replicas` for that entire
window. The compute cost multiplies across concurrently-draining versions, and
users have no good lever to reduce it:

1. An HPA pointed at the RD scale subresource only ever scales the **latest**
   version (`spec.replicas` only flows to the newest RS).
2. Users can't easily HPA the old RSs themselves — they'd have to build a
   controller to discover new RSs and prune HPAs for retired ones.
3. So draining versions sit at full replicas the whole time, with no way to
   shed compute as their load falls.

### Options considered and rejected

- **Static drain floor** (scale old versions to a fixed `drainReplicas`): too
  static — only works for predictably-short workflows.
- **Operator computes replicas from invocation count**: the signal is bad.
  Pinned invocations are mostly *suspended* (parked server-side in Restate,
  consuming zero pod resources), so the count wildly overestimates needed
  compute, and the real load is bursty. No clean `f(count) → replicas`.
- **Adopt Knative for ReplicaSet-mode users**: Knative is a platform
  (controller + webhook + autoscaler + activator + a networking layer +
  per-pod queue-proxy sidecar + a release treadmill). Adopting/operating it to
  save compute on draining versions is disproportionate. Knative mode stays
  **bring-your-own**, for users who already run it.
- **Users declare their own per-version Deployments; operator blocks deletion
  with finalizers**: externalizes the runtime-vs-declarative conflict into the
  user's GitOps pipeline. A finalizer that blocks Argo/Flux pruning of a
  retired version (for the full multi-hour drain) produces OutOfSync/stuck-
  deleting noise, and operator-vs-Argo fights over `metadata.finalizers`.

## Approach

The operator **generates one HPA per draining (non-latest) version**, opt-in,
owned and garbage-collected by the operator. The RD stays the single
declarative object in git; the HPA is just one more operator-owned child,
exactly like the per-version RS and Service already are. The hard scaling
decision is delegated to the HPA on a user-chosen metric — the operator does
not compute replica counts itself.

This is the same pattern Temporal's
[worker-controller](https://github.com/temporalio/temporal-worker-controller/blob/main/docs/worker-resource-templates.md)
uses (`WorkerResourceTemplate` renders one HPA per running version, injecting
the versioned target and version-scoped metric labels).

### Division of responsibility

| Version state            | Scaled by                              | Owns `spec.replicas` |
| ------------------------ | -------------------------------------- | -------------------- |
| **Latest**               | user's HPA → RD scale subresource      | operator (conduit)   |
| **Non-latest, active**   | operator-stamped per-version HPA       | the HPA              |
| **Non-latest, inactive** | operator (scale to 0, delete)          | operator             |

The operator only ever stamps HPAs for **non-latest** versions. The latest
version keeps its existing path (user HPA on the RD scale subresource). The two
mechanisms target different RSs and never contend for the same object.

## Prerequisite — issue #139 (must land first)

The RD's `.status.labelSelector` is written verbatim from `spec.selector`
(`controller.rs:776-781`), with no `pod-template-hash` filter. An HPA on the RD
scale subresource therefore averages metrics across **every** RS the RD owns —
latest plus all draining ones — for the entire drain window, dragging the
average down and under-provisioning (or scaling down) a genuinely-busy latest
version.

[#139](https://github.com/restatedev/restate-operator/issues/139) fixes this by
appending `pod-template-hash=<latest>` to the status selector so the latest-
version HPA reads only latest-version pods.

This is a hard prerequisite: it makes the latest-version autoscaling path
correct, which is what lets us scope the operator-stamped HPAs to non-latest
versions only — so the two scaling mechanisms never contend over the same RS's
`spec.replicas`.

## API

Add an optional `autoscaling` block to `RestateDeploymentSpec`. It is a
**pass-through HPA spec template** (`x-kubernetes-preserve-unknown-fields`,
mirroring how `spec.template.spec` already passes the pod spec through), so we
do not re-declare and then chase the `autoscaling/v2` schema as it evolves.

```yaml
spec:
  autoscaling:
    # Pass-through HorizontalPodAutoscaler .spec, minus the fields the operator
    # injects per version (see below). Opt-in: absent => current behaviour
    # (draining versions held at full replicas).
    minReplicas: 1            # floor per draining version; see "no scale-to-zero"
    maxReplicas: 10
    metrics:                  # opaque, passed through to the HPA
      - type: Resource
        resource:
          name: cpu
          target:
            type: Utilization
            averageUtilization: 70
    behavior: { ... }         # optional, passed through
```

### Operator-injected fields

The operator fills these per version so the user never has to know the
versioned RS name:

- `scaleTargetRef` → `{ apiVersion: apps/v1, kind: ReplicaSet, name: <rd>-<hash> }`
  — the HPA targets the version's ReplicaSet directly (a ReplicaSet exposes a
  scale subresource).
The operator injects **only** `scaleTargetRef` — it does not modify the metric
spec. Whether a metric is therefore per-version depends on its type:

- **Resource / ContainerResource / Pods** metrics are read from the pods of the
  `scaleTargetRef` (this version's ReplicaSet, whose pods carry the
  `pod-template-hash`), so they are inherently per-version. This is the CPU case;
  nothing extra is needed.
- **Object / External** metrics reference an external series by their own
  metadata, not the target's pods. The same template is applied to every draining
  version, so each version's HPA would read the **same** value — these are **not**
  per-version. Don't put an Object/External metric in the template expecting
  per-version scaling: the operator does not inject a version-scoping selector.

### Defaults / validation

- Absent `autoscaling` ⇒ today's behaviour unchanged (draining versions stay at
  full replicas). Fully backwards-compatible.
- `minReplicas` cannot be 0 in practice (see below); validate/clamp to ≥ 1.
- CPU/memory metrics require container resource `requests` to be set, or the HPA
  silently does nothing; validate or warn.

## Scaling signal

The initial implementation uses CPU. For a draining version, CPU is a reasonable
proxy because of Restate's execution model:

- A draining version consumes CPU **only when an invocation pinned to it is
  actively executing**. Suspended invocations (sleeps, awaiting calls/
  awakeables — the majority over a long drain) are parked in Restate and use no
  CPU and no pod slot.
- So: an idle draining version sits at baseline CPU and the HPA scales it to the
  floor. When a batch of pinned invocations resumes, CPU rises and the HPA
  scales up; Restate's invocation retry/backoff covers the reaction lag.

Known gap: a handler that is **concurrency-bound but low-CPU** (e.g. blocking on
a synchronous external HTTP/DB call *inside* the handler, not a Restate await)
needs capacity while showing little CPU, so CPU-HPA under-provisions it. This is
secondary (Restate idiom pushes waits through suspending awaits) and bounded
(load on a draining version only falls over time).

**Follow-up:** add a per-`deployment-id` demand metric in the Restate runtime
(in-flight / concurrency, the Knative-equivalent signal). Note this is **not**
zero-operator-change: such a signal is an External/Object metric (see "Operator-
injected fields"), so consuming it *per version* requires the operator to inject
a version-scoped selector into each stamped HPA — which it does not do today —
in addition to the runtime metric source itself.

Recommend **CPU, not memory**: memory is sticky and won't scale *down*,
defeating the goal.

## No scale-to-zero

HPA cannot scale below `minReplicas`, and `minReplicas: 0` requires the alpha
`HPAScaleToZero` gate (off in most clusters). So each *active* draining version
floors at `minReplicas ≥ 1`. Steady-state cost is
`minReplicas × (concurrently-draining-and-active versions)` — far below
`fullReplicas × N`, but not zero. This is inherent to ReplicaSet mode: there is
no activator to revive a version from zero on demand, so users who need true
zero use Knative mode. The min=1 floor degrades gracefully on a burst — the warm
pod absorbs the initial hit while the HPA and Restate retry cover the ramp.

## Lifecycle

Invariant:

> A non-latest version has an operator-stamped HPA **iff** Restate still reports
> its `deployment-id` active (has invocations).

```
version goes non-latest, still active
    → stamp HPA (autoscaling template, scaleTargetRef → this RS)
    → operator stops propagating spec.replicas to this RS (the HPA owns it)

version still draining
    → HPA owns scale between minReplicas and load

Restate reports deployment-id INACTIVE
    → delete the HPA
    → scale RS to 0
    → retain at 0 for revision_history_limit
    → delete RS (+ force-delete Restate deployment) as today
```

The HPA is deleted at the **inactive → scale-to-0** transition, before the
ReplicaSet is scaled down. This matters because the existing teardown retains
the ReplicaSet at 0 replicas for `revision_history_limit` before deleting it:
with `minReplicas ≥ 1`, an HPA still in place during that retention window would
keep scaling the drained version back up to its floor, holding a pod for a
version that has no work. Deleting the HPA the moment the deployment goes
inactive lets the operator alone drive the version down to 0 and out.

The operator owns the HPA via an ownerReference on the RestateDeployment, so a
leaked HPA is garbage-collected if the RD is deleted; routine teardown deletes
it explicitly at the inactive transition as above. The same teardown applies on
the RD finalizer/cleanup path.

## Design decisions

- **Pass-through HPA template**, not a curated field subset — avoids coupling to
  `autoscaling/v2` evolution.
- **Embed `autoscaling` in the RD spec**, not a separate CRD — keeps the
  single-declarative-object (GitOps) model. (Revisit a separate
  per-version-resource CRD only if we later want to template additional kinds
  per version, e.g. PodDisruptionBudget.)
- **Latest version uses the existing RD scale subresource path**, made correct
  by #139; it is out of scope for operator-stamped HPAs.

## Acknowledged limitations

- Not zero-cost: `minReplicas × active-draining versions`.
- CPU under-scales concurrency-bound low-CPU handlers until the runtime-metric
  follow-up lands.
- Requires container resource `requests` for CPU/memory metrics to function.

## Work breakdown

1. **Prerequisite:** land #139 (hash-filter `.status.labelSelector`).
2. Add pass-through `autoscaling` field to `RestateDeploymentSpec`; `just generate`.
3. HPA reconciler: stamp/update an HPA for each non-latest **active** version,
   injecting `scaleTargetRef` (Resource metrics are per-version via the target's
   pod selector; the operator does not inject metric-label scoping); own by the RD.
4. Stop propagating `spec.replicas` to an RS once it has an operator HPA.
5. Teardown: delete the HPA at the inactive→scale-to-0 transition (before
   scaling to 0), per the invariant above; ensure the RD finalizer/cleanup path
   covers it too.
6. Validation/docs: `minReplicas ≥ 1`, requires resource `requests`, recommend
   CPU over memory.
7. Follow-up (separate): per-`deployment-id` demand metric in the runtime.

## Test plan

The feature's risk is concentrated in lifecycle correctness (when an HPA is
created, when it is deleted, who owns `spec.replicas` at each moment), not in
the HPA's own scaling math — that is Kubernetes' job. So most coverage should be
fast, deterministic tests of the reconciler's behaviour, with a small number of
real-load end-to-end tests to confirm pods actually move. Three layers:

### 1. Reconciler unit tests (fast, deterministic, the bulk of coverage)

Given an RD spec plus a synthetic set of versions and a faked Restate
active/inactive map, assert the exact set of HPA create/update/delete actions
and replica handling:

- **Stamping:** a non-latest, active version with `autoscaling` set gets exactly
  one HPA, with `scaleTargetRef` pointing at that version's RS.
- **Latest is excluded:** the latest version never gets an operator HPA.
- **Replicas handoff:** once a version has an HPA, the operator stops writing
  `spec.replicas` to its RS (no reconcile should re-assert a replica count).
- **Template pass-through:** user-supplied `minReplicas`/`maxReplicas`/`metrics`/
  `behavior` land verbatim on the generated HPA; only the injected fields differ.
- **Opt-in / backwards-compat:** with no `autoscaling` block, no HPA is ever
  created and draining versions stay at full replicas (regression guard).
- **`minReplicas` validation:** `0` is clamped/rejected (no `HPAScaleToZero`
  assumption).
- **Knative mode:** the `autoscaling` block produces no HPAs in Knative mode.

### 2. Integration tests against a real API server (envtest or kind, faked Restate)

Validate behaviour against a real apiserver and GC, with Restate admin responses
stubbed so active/inactive is fully controllable:

- **Deletion ordering (critical):** flip a version inactive and assert the HPA is
  deleted *before* the RS is scaled to 0, the RS then reaches 0, and it **stays
  at 0** through the `revision_history_limit` retention window — i.e. it does not
  bounce back to `minReplicas`. Sample the RS replica count over time and assert
  it is stable at 0 with no HPA present.
- **No contention while draining:** for a non-latest active version, sample its
  RS replicas over time and assert the operator never fights the HPA (no
  operator-driven reset / flap).
- **Active → inactive → active:** purge to flip a version inactive (HPA deleted),
  then make it active again; assert the HPA is re-stamped. The invariant must
  hold bidirectionally.
- **Adopt / release:** adding `autoscaling` to an RD that already has draining
  active versions retroactively stamps their HPAs; removing it deletes the HPAs
  and reverts those versions to full replicas.
- **Garbage collection:** deleting the RD removes all owned HPAs (ownerReference
  on the RD); the finalizer/cleanup path leaves no orphaned HPAs.
- **Idempotency across restarts:** kill and restart the operator mid-lifecycle
  (mid-drain, and between HPA-delete and scale-to-0) and assert it converges to
  the correct state with no duplicate or orphaned HPAs.

### 3. End-to-end with real load (kind + real Restate + operator)

The happy path and the few scenarios that need a real HPA + metrics pipeline.
Use a metric you can drive deterministically (a custom/external metric, or CPU
with a load generator and generous timeouts) so scale changes are observable:

- **Happy path:** kind cluster with Restate, operator, and `autoscaling` set;
  register v1, send it load, register v2 (v1 becomes non-latest), and observe an
  HPA appear for v1. Drive load on v1 up and down and observe its HPA scale the
  v1 RS up, then back to the floor as load falls.
- **Per-version metric isolation:** with v1 and v2 both present and HPA'd, drive
  load onto v1 only and assert v1's RS scales while v2's stays at its floor —
  load on one version must not move another (the per-version analogue of #139).
- **Drain to floor, then out:** hold a long invocation pinned to v1 so it stays
  active at the floor, then complete/purge it (`restate invocations purge <id>`)
  to flip v1 inactive; observe the HPA removed, v1 scaled to 0, and eventually
  deleted.
- **Burst from floor:** with v1 idle at `minReplicas: 1`, resume a batch of
  pinned invocations and confirm the floor pod absorbs the initial load and the
  HPA scales up without dropped invocations (Restate retries cover the ramp).

### Failure injection / robustness

- **Restate admin unavailable:** while the active/inactive query is failing, the
  operator must take no destructive action — it must not delete HPAs or scale
  down versions on a missing/empty query result. Assert versions are left intact
  and reconciliation requeues.
- **Metrics source unavailable:** with metrics-server / the custom-metrics
  adapter down, the HPA reports an unknown metric; assert the operator does not
  crash and the version holds at its current/floor replicas (no scale to zero).
- **Missing resource requests:** a CPU-metric HPA on pods with no CPU `requests`
  cannot compute a target; assert the validation/warning fires and document that
  scaling will not occur.
- **Rapid releases / many concurrent drains:** roll v1→v2→…→vN quickly with all
  versions still active; assert each non-latest version gets exactly one HPA,
  none leak, and steady-state cost equals `Σ minReplicas`.
- **Hash collision:** exercise the `collision_count` path and confirm the HPA
  still targets the correct (re-hashed) RS.

### What to assert

- HPA existence and `scaleTargetRef`/selector correctness per version.
- RS replica counts sampled **over time** (to catch flapping and bounce-back,
  not just end state).
- `ownerReferences` on generated HPAs point at the RD.
- RD status/conditions and emitted events reflect the autoscaling lifecycle.
- Negative assertions: no HPA for the latest version; no HPA after teardown; no
  destructive action during admin-API failures.
