# Release Notes for Issue #127: Expose tolerations and nodeSelector in chart values

## New Feature

### What Changed
`tolerations` and `nodeSelector` are now declared in
`charts/restate-operator-helm/values.yaml`. The `tolerations` template was
already wired in `templates/deployment.yaml`; this just makes it discoverable
via the values file. A matching `nodeSelector` template block was added.

### Why This Matters
On clusters with node taints — per-workload taint pools, dedicated controller
node groups, Karpenter-style provisioners — the operator pod previously had no
way to schedule through Helm values alone. Users had to fork the chart or layer
a kustomize+helm post-render patch. Declaring these standard pod scheduling
fields in `values.yaml` brings the chart in line with the typical operator
chart convention (cert-manager, external-secrets, AWS Load Balancer Controller,
etc.).

### Impact on Users
- Existing deployments: No impact, both fields are optional
- New deployments: Can pin the operator pod to specific nodes or tolerate
  taints through Helm values, instead of forking the chart or post-rendering
  with kustomize

### Migration Guidance
No migration required. To use the new fields:


```yaml
tolerations:
  - key: "service"
    operator: "Exists"
    effect: "NoSchedule"

nodeSelector:
  workload: controllers
```


### Related Issues
- Issue #127: Expose tolerations / nodeSelector in restate-operator-helm chart
