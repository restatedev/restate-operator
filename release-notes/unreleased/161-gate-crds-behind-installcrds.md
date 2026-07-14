# Release Notes for Issue #161: Gate CRDs behind installCrds

## New Feature

### What Changed
The chart's CRDs are now templated and gated behind a new `installCrds` Helm value
(default `true`). The CRD manifests moved from `crds/` to `crd-manifests/` and are
rendered by `templates/crds.yaml`.

### Impact on Users
- Existing deployments: no change with the default `installCrds: true`.
- Because the CRDs are now templated (rather than in the Helm `crds/` directory),
  they are applied on `helm upgrade`, so CRD schema updates ship with chart upgrades.

### Migration Guidance
Set `installCrds=false` when the CRDs are managed outside this chart (e.g. a
separate manifest, as nuon-byoc vendors them) to avoid duplicate server-side-apply
ownership:

```yaml
installCrds: false
```

Caveat: templated CRDs are removed on `helm uninstall`, which cascades to and
deletes any existing custom resources (RestateClusters, RestateDeployments,
RestateCloudEnvironments). Set `installCrds=false` if you need CRDs to survive an
uninstall.

### Related Issues
- PR #161: Gate CRDs behind installCrds
