# Suspend reconciliation by annotation

## New Feature

Annotating a `RestateCluster`, `RestateDeployment` or `RestateCloudEnvironment` with
`restate.dev/reconcile: disabled` makes the operator leave that resource and everything it owns
alone until the annotation is removed — Flux's `suspend`, per resource, for hand-editing
generated objects during an incident without scaling the whole operator to zero. Only the exact
value `disabled` suspends, so a typo cannot silently stop reconciliation; `.status.reconciliation`
reports `Reconciling`, `Disabled` or `ResumingReconciliation` (annotation gone, not `Ready`
again yet), and the rest of the status stays frozen as of the last real reconcile. Deletion is
not suspended — the annotation stops the operator managing a resource, not tearing it down, so a
suspended resource still deletes and cleans up normally. `RestateCloudEnvironment`
gained a status subresource for this, so if you manage CRDs yourself (`installCrds: false`),
apply the new CRDs before rolling the operator image
