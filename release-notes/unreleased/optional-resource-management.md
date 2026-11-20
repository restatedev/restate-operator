# Release Notes: Opt out of NetworkPolicy and SecretProviderClass management

## New Feature

### What Changed
Two new operator-wide switches, both defaulting to `true`:

- `--manage-network-policies` / `MANAGE_NETWORK_POLICIES`
- `--manage-secret-provider-classes` / `MANAGE_SECRET_PROVIDER_CLASSES`

Set to `false`, the operator never reads, watches, creates, updates or deletes that resource. It also skips the cleanup
it previously did even for clusters that never used it: deleting its NetworkPolicies when
`spec.security.disableNetworkPolicies: true`, and deleting the `request-signing-key-v1` SecretProviderClass whenever
the CSI API was installed but a cluster had no `secretProvider` signing key.

With `MANAGE_SECRET_PROVIDER_CLASSES=false`, a `RestateCluster` whose `requestSigningPrivateKey` uses `secretProvider`
is rejected with `Ready=False`, reason `SecretProviderClassesDisabled`, before its StatefulSet is touched. Clusters
signing with a Kubernetes `secret`, or not signing at all, are unaffected.

With `MANAGE_NETWORK_POLICIES=false`, a `RestateCluster` that sets `networkPeers`, `networkEgressRules` or
`allowOperatorAccessToAdmin: false` without `disableNetworkPolicies: true` reports `Ready=False`, reason
`NetworkPoliciesDisabled`, instead of silently running without the settings it asked for. Unlike the signing-key
case, the cluster is still reconciled in full: its pods keep running and its existing policies stay as they are.
Only the condition says so, which also holds back new `RestateDeployment` registrations against that cluster until
it is resolved.

The Helm chart has matching values, `manageNetworkPolicies` and `manageSecretProviderClasses` (both `true`). Setting
one to `false` sets the environment variable and removes that resource from the chart's ClusterRole. Use the values
rather than setting the environment variables through `env`.

### Why This Matters
Until now the operator needed list, watch and delete permissions on both resources even when no cluster used them, so
platforms that manage NetworkPolicies centrally, or do not run the Secrets Store CSI driver, could not withhold them.
Withholding them anyway left reconciles failing on 403s and watches retrying.

### Impact on Users
- Existing deployments: no change; both switches default to `true`.
- With a switch off, existing NetworkPolicies or SecretProviderClasses are left as they are, frozen at what was last
  applied. Owner references are untouched, so they are still garbage-collected with their `RestateCluster`.
- `MANAGE_NETWORK_POLICIES=false` overrides `spec.security.disableNetworkPolicies` on every cluster. New clusters get
  no network isolation from the operator, and existing policies are no longer updated or removed; provide and maintain
  policies yourself.

### Migration Guidance
With the Helm chart:

```yaml
manageNetworkPolicies: false
manageSecretProviderClasses: false
```

A single `helm upgrade` changes the RBAC and the operator Deployment together, so the old operator pod may log 403s for
these resources until the new one has rolled out.

If clusters use `secretProvider` signing, move them to a Kubernetes `secret` before turning that switch off.

Turn NetworkPolicy management off first, then adjust the clusters: once it is off, the operator no longer touches
their policies, so clusters that set `networkPeers`, `networkEgressRules` or `allowOperatorAccessToAdmin: false` keep
running with the policies they have while they report `Ready=False`. Set `disableNetworkPolicies: true` on them, or
remove those fields, and they become Ready again. Do not remove the fields while management is still on: the
operator would re-apply the stricter default policies and cut off the traffic they allowed.
