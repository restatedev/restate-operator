# Standalone `restate-operator-crds` chart

2.8.1 shipped the CRDs through Helm's native `crds/` directory, which is install-only: once the CRDs are in the cluster, `helm upgrade` never touches them again. That bit us — after an operator upgrade a customer's new schema fields were silently pruned at admission until the CRDs were reapplied by hand.

So we've pulled the CRDs into their own chart and gone back to templating them, which means upgrading the chart actually updates the schema:

```bash
helm upgrade --install restate-operator-crds \
  oci://ghcr.io/restatedev/restate-operator-crds --version <version>
```

They still carry `helm.sh/resource-policy: keep`, so `helm uninstall` won't take your CRDs (or the custom resources under them) down with it.

`restate-operator-helm` keeps bundling this chart behind `installCrds` (default `true`), so nothing changes if you install the operator on its own. If you'd rather own the CRD lifecycle yourself — the nicer path for ArgoCD/Flux — install `restate-operator-crds` directly and set `installCrds=false` on the operator.

## Upgrading

If your CRDs came in through the old `crds/` directory (2.8.1 or earlier), Helm doesn't consider them managed yet, so the first upgrade needs a one-time ownership hand-off: either pass `--take-ownership` to the CRD chart, or label/annotate the three CRDs with `app.kubernetes.io/managed-by=Helm` plus the `meta.helm.sh/release-name`/`-namespace` metadata. ArgoCD does this for you on sync. Either way, no custom resources are deleted.

This doesn't touch #166 (the operator still exits if an optional CRD is missing at startup) — that's a separate binary-side fix.
