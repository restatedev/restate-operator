# Release Notes for Issue #166: Poll for CRDs on startup instead of exiting

## Behavioral Change

### What Changed

Previously, if a required CRD (`RestateCluster`, `RestateDeployment`, or `RestateCloudEnvironment`) was not installed in the cluster when the operator started, the operator would log an error and exit immediately. The same applied to the optional `PodIdentityAssociation` CRD when an AWS pod identity association cluster was configured.

Now, on startup the operator polls the apiserver's discovery endpoint for each required CRD and loops until the resource appears, emitting a warning every 10 seconds while it is missing. The operator no longer exits when a required CRD is absent — it waits for it to be applied and then begins reconciling.

Discovery is used rather than listing the resource or reading the `CustomResourceDefinition` object: it needs no RBAC beyond what every authenticated client has, and a resource only shows up there once its CRD is established and the version is served.

All of these now mean "not ready yet, keep waiting": a missing group/version (`404`), a missing resource within a served group/version, the responses that mean the apiserver cannot answer right now (`429` and `5xx`), and a failure to reach the apiserver at all (connection refused, timeouts). Every one of those routinely happens while a cluster is coming up, and exiting fixes none of them — it just turns a wait into a crashloop. This also covers the API-group listing the `RestateCluster` controller uses to detect which *optional* CRDs are installed, which previously exited on any failure and so ran before (and defeated) the CRD wait.

Any other failure — a `401` or `403`, a bad kubeconfig — still logs an error and exits, since those need someone to change something. The corollary is that a permanently unreachable apiserver (a wrong CA, say) is now waited on rather than crashlooped; that is deliberate, because waiting is no longer silent (see below).

Because a CRD that never arrives would otherwise leave the operator idle but healthy, the wait is now surfaced in three ways beyond the log line:

- **A readiness endpoint.** The new `/ready` endpoint returns `200` only once every controller has its CRD and has started reconciling, and `503` otherwise, with the controllers still waiting listed in the body:

  ```json
  {"ready":false,"pendingControllers":["RestateCluster","RestateDeployment"]}
  ```

  `/health` keeps its old always-`200` behaviour and is now used as a liveness probe: restarting the operator does not make a missing CRD appear, so waiting for a CRD must not get the pod killed.
- **A metric.** `restate_operator_crd_missing{crd="<plural>.<group>"}` is `1` while the operator is waiting for that CRD and `0` once it is available, so a CRD that never arrives can be alerted on (e.g. `restate_operator_crd_missing > 0 for 5m`).
- **A Kubernetes event.** A single `Warning` event with reason `WaitingForCRD` is recorded against the operator's own pod, naming every CRD being waited for at once:

  > Waiting for 3 CRDs to be installed before reconciling them: restateclusters.restate.dev, restatecloudenvironments.restate.dev, restatedeployments.restate.dev.

  The controllers wait independently, so this is reported centrally rather than from each of them — one event series for the operator instead of one per CRD. It is re-published every 10 seconds, which the API folds into that series' count, and a `Normal` `CRDsAvailable` event closes it out once everything is reconciling. Nothing is emitted at all if the CRDs land within the first 10 seconds, which is the common case for the GitOps race this change exists to handle.

While waiting, the operator remains responsive to `SIGTERM`/`SIGINT`: the wait is aborted and the process shuts down promptly rather than running until the kubelet's grace period expires. The signal handlers are installed once, before the poll loop, so there is no window in which a signal would terminate the process outright.

The `PodIdentityAssociation` check (when `aws-pod-identity-association-cluster` is set) still exits, since that is a configuration error rather than a missing-CRD race.

### Why This Matters

In GitOps and automated install flows the operator and the CRDs are often applied together, and the operator can come up before the CRDs are admitted. The old behaviour required careful ordering (or restarts) to get the operator running. With this change the operator simply waits for the CRDs to arrive, which matches how most Kubernetes operators behave and removes the need to restart the operator after installing the CRDs.

### Impact on Users

- **New deployments:** The operator can be installed in any order relative to its CRDs. It will start and begin reconciling as soon as the CRDs are available. Until then the pod is `NotReady`, so `kubectl get pods` and `kubectl describe pod` both say what is being waited on.
- **Existing deployments:** No change once the CRDs are present. If the CRDs are removed while the operator is running, the controller for that resource will keep retrying its watches (existing kube-rs behaviour) rather than exiting; readiness is not revoked once a controller has started.
- **Observability:** While waiting for a CRD, the operator logs a `WARN` line every 10 seconds naming the missing CRD, serves `503` on `/ready`, sets `restate_operator_crd_missing` to `1` for that CRD, and records one `WaitingForCRD` event on its own pod listing all of them (`kubectl describe pod -n <operator namespace> <operator pod>`).
- **RBAC:** No new permissions are required. The discovery endpoint is readable by any authenticated client, and the operator already holds `create`/`patch` on `events.k8s.io` for the events it emits on the resources it manages.
- **Chart:**
  - The readiness probe now targets `/ready` and a liveness probe targeting `/health` has been added. **Upgrade note:** the chart's `version` defaults to the chart version, so chart and image move together. If you pin `version` to an operator image older than this release, pin the chart too — an older image has no `/ready`, and the resulting `404` would keep the pod `NotReady` forever.
  - The Deployment passes `OPERATOR_POD_NAME` and `OPERATOR_POD_UID` via the downward API so that operator-level events can be attached to the right pod. If you deploy the operator without the chart, set both (or neither — without them the operator logs and reports the metric as usual, but emits no events).
  - The operator `Service` sets `publishNotReadyAddresses: true`. It exists only to serve `/metrics` and `/`, and a `NotReady` operator is exactly when `restate_operator_crd_missing` needs scraping, so its endpoint must not drop out of the `ServiceMonitor`'s targets.