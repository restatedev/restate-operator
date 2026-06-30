# Release Notes: RestateCluster pod lifecycle, sidecars, and termination grace period

## New Feature

### What Changed

Five new optional fields on `RestateCluster` `spec.compute` are passed through to
the underlying StatefulSet pod template:

- `lifecycle` — container lifecycle hooks (`postStart` / `preStop`) for the Restate
  container.
- `sidecars` — native sidecar containers. Each runs alongside the Restate container
  (started before it, terminated after it); the operator forces `restartPolicy: Always`
  so an entry behaves as a sidecar rather than a startup-blocking init container.
- `terminationGracePeriodSeconds` — overrides the pod termination grace period
  (previously hardcoded to 60 seconds).
- `extraVolumes` — additional pod volumes alongside the operator-managed ones.
- `extraVolumeMounts` — additional volume mounts for the Restate container; sidecars
  carry their own mounts and share these pod-level `extraVolumes`.

### Why This Matters

These let operators wire in graceful-shutdown hooks, run companion containers (log
shippers, proxies, metrics exporters) next to Restate with correct start/stop
ordering, and tune the shutdown window for their workload.

### Impact on Users

- Existing deployments: no impact — all five fields are optional and omitting them
  preserves today's behaviour (no lifecycle hooks, no sidecars, no extra volumes,
  60s grace period).
- New deployments: opt in via `spec.compute`.

### Usage

```yaml
spec:
  compute:
    image: docker.restate.dev/restatedev/restate:latest
    terminationGracePeriodSeconds: 120
    extraVolumes:
      - name: hooks
        configMap:
          name: node-state-control
          defaultMode: 0755
    extraVolumeMounts:
      - name: hooks
        mountPath: /node-state-control
        readOnly: true
    lifecycle:
      preStop:
        exec:
          command: ["/bin/sh", "/node-state-control/pre-stop.sh"]
    sidecars:
      - name: log-shipper
        image: log-shipper:latest
        volumeMounts:
          - name: hooks
            mountPath: /node-state-control
```
