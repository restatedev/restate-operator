# Release Notes: RestateCluster pod lifecycle, sidecars, and termination grace period

## New Feature

### What Changed

Three new optional fields on `RestateCluster` `spec.compute` are passed through to
the underlying StatefulSet pod template:

- `lifecycle` — container lifecycle hooks (`postStart` / `preStop`) for the Restate
  container.
- `sidecars` — native sidecar containers. Each runs alongside the Restate container
  (started before it, terminated after it); the operator forces `restartPolicy: Always`
  so an entry behaves as a sidecar rather than a startup-blocking init container.
- `terminationGracePeriodSeconds` — overrides the pod termination grace period
  (previously hardcoded to 60 seconds).

### Why This Matters

These let operators wire in graceful-shutdown hooks, run companion containers (log
shippers, proxies, metrics exporters) next to Restate with correct start/stop
ordering, and tune the shutdown window for their workload.

### Impact on Users

- Existing deployments: no impact — all three fields are optional and omitting them
  preserves today's behaviour (no lifecycle hooks, no sidecars, 60s grace period).
- New deployments: opt in via `spec.compute`.

### Usage

```yaml
spec:
  compute:
    image: docker.restate.dev/restatedev/restate:latest
    terminationGracePeriodSeconds: 120
    lifecycle:
      preStop:
        exec:
          command: ["/bin/sh", "-c", "sleep 5"]
    sidecars:
      - name: log-shipper
        image: log-shipper:latest
```
