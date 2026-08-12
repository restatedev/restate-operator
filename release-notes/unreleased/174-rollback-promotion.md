# Release Notes for Issue #174: Rolling back now moves Restate's routing

## Bug Fix

### What Changed

Rolling a `RestateDeployment` back to a previously registered revision now makes that
revision the one Restate sends new invocations to, and `Ready=True` no longer claims
otherwise.

Because ReplicaSets and Services are named by a content hash of the pod template,
`v1 -> v2 -> v1` re-adopts the original ReplicaSet, Service URL and Restate deployment id
rather than creating new ones. Restoring the Kubernetes revision was therefore never enough
on its own — Restate also has to be told that the restored deployment is latest again — and
two separate faults stopped that from happening:

- **The decision asked the wrong question.** The operator skipped registration whenever the
  recorded deployment was "active", where active meant *either* that a service pointed at it
  *or* that it had unfinished invocations. A rolled-back version holding a single pinned
  invocation satisfied the second half, so the operator did nothing at all and the newer
  version kept serving.
- **When it did register, it could not promote.** Registration is a `POST /deployments`
  against the unversioned admin path, which resolves to `AdminApiVersion::Unknown`, where
  `force` defaults to *false* (changed in Restate v1.6.0). Re-registering an unchanged
  endpoint on such a server returns `200` with the existing deployment id and changes
  nothing. The operator read that as success.

The operator now decides purely on whether Restate routes new invocations to the recorded
deployment, and re-registers with overwrite when it does not — which bumps that deployment's
service revisions past the current latest while keeping its deployment id, so invocations
already pinned to it are undisturbed. It then asks Restate what it actually routes before
reporting `Ready`.

Alongside that:

- **`force` is now always sent explicitly.** Its default depends on the admin API version a
  request resolves to, and that default has already changed once underneath the operator.
- **Registration is verified, not assumed.** After registering, the operator checks
  `GET /services` and confirms every service discovered at the endpoint resolves to this
  deployment. A registration that made no routing change is no longer treated as a success.
- **The deployment id is recorded before the routing is confirmed.** The id is true either
  way — it is what Restate holds for that endpoint — and recording it is what lets the next
  reconcile plan a promotion. Confirming first would strand a registration that landed on an
  endpoint Restate already knew but was not routing to, with nothing written down and every
  subsequent reconcile planning the same plain registration again.
- **A promotion emits a `Promoted` event.** Overwriting is how Restate is asked to move
  routing, and it also permits breaking schema changes and resets the deployment's
  registration timestamp, so it leaves an audit trail rather than only a log line.
- **A foreign owner is reported, not fought.** If the desired deployment is superseded and
  no version of this `RestateDeployment` holds the service either, something outside it has
  registered those services. The operator refuses to force in that case and reports
  `Ready=False` with reason `ForeignDeployment`. Forcing would start a promotion war, with
  two controllers bumping revisions to take the service back indefinitely.
- **The ReplicaSet cache is prewarmed at startup.** That cache is how a rollback is told
  apart from a foreign owner, and an unsynced one would answer "nothing of ours is latest",
  which would park a healthy rollback at `Ready=False` until the cache filled.
- **Knative mode promotes too.** It used to return on the recorded deployment id alone, so
  it could never promote on a rollback; it now plans against Restate's routing exactly as
  ReplicaSet mode does, using its Configurations as the owned-version evidence.

### Known limitation

`latest_for_service`, the flag this decides on, is true when a deployment is latest for *any*
of its services. A deployment that is latest for some and superseded for others therefore
reads as current, and a rollback onto it does nothing. Reaching that state needs one version
to expose a strict subset of another's services. Resolving routing per service needs admin
reads this change deliberately does not add; it is tracked separately.

### Why This Matters

A rollback that restored the pods but not the routing looked entirely healthy: the
ReplicaSet scaled back up, `Ready` went `True`, and the deployment id in status was the
expected one. Only the traffic was still going to the version being rolled back *from* —
which, during an incident, is the version being rolled back away from.

### Impact on Users

- **`Ready=True` is stricter.** It now means Restate routes new invocations to the desired
  revision, not merely that the pods are up. Deployments sitting in an inconsistent state —
  including ones that have been inconsistent for a while — will report `Ready=False` with a
  reason until it resolves.
- **On upgrade, inconsistent deployments are promoted on the next reconcile.** If a
  `RestateDeployment`'s desired revision is not currently latest in Restate, the operator
  will move routing to it. This is the fix working, but it is a routing change that happens
  without being asked for, so upgrade at a time when that is acceptable.
- **An endpoint registered outside its `RestateDeployment` now stalls rather than being
  silently tolerated.** Services that were pointed at a manually registered deployment, or at
  one left behind by a decommissioned controller, report `Ready=False` with reason
  `ForeignDeployment` on the first reconcile after the upgrade — the operator declines to
  take them by force. Nothing is written and no routing changes; resolve it by removing the
  other registrant so the `RestateDeployment`'s own endpoint becomes latest again.
- **Rollback requires the restored version's pods to serve discovery.** Overwriting
  re-runs discovery against the endpoint, so a rollback to a revision whose image can no
  longer be pulled now blocks at `Ready=False` instead of silently half-completing.
- **A promoted deployment's registration time is reset in Restate.** Overwriting replaces
  `sys_deployment.created_at`, so a rolled-back deployment appears freshly registered in the
  UI and in `sys_deployment`. The operator keeps no separate record of the original time.
- **`force: false` is now sent explicitly where the field was previously omitted.** No
  behaviour change. Restate defaults the omitted flag by admin API version — *true* on `/v1`
  and `/v2`, *false* on the unversioned router and `/v3` — but the operator only ever reaches
  the unversioned router, where the default was already false: it builds each request as
  `admin_url.join("/deployments")`, and a leading-slash path replaces the base path outright,
  so a version prefix on `restate.register` could not survive into the request even if one
  were configured. Sending the flag makes the operator's intent independent of that default
  rather than changing what any current deployment does.
- **New deployments:** no impact beyond the above.

### Migration Guidance

None required. No CRD change, and no new annotation or status field.

To check whether a deployment is in the inconsistent state this fixes, compare the id the
operator recorded against the one Restate routes to:

```bash
kubectl get restatedeployment <name> -n <namespace> -o jsonpath='{.status.deploymentId}'
curl -s "$RESTATE_ADMIN/services/<ServiceName>" | jq -r .deployment_id
```

If they differ, upgrading the operator resolves it on the next reconcile.

### Related Issues

- Issue #174: Support rolling RestateDeployments back to a previously registered revision
- Issue #146: content-hash RS naming doesn't handle niche rollback edge case
- restatedev/restate#5157: decouple deployment registration from selecting the latest
  revision — the intended replacement for forced re-registration
