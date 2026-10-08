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

### Why This Matters
Until now the operator needed list, watch and delete permissions on both resources even when no cluster used them, so
platforms that manage NetworkPolicies centrally, or do not run the Secrets Store CSI driver, could not withhold them.
Withholding them anyway left reconciles failing on 403s and watches retrying.

### Impact on Users
- Existing deployments: no change; both switches default to `true`.
- With a switch off, existing NetworkPolicies or SecretProviderClasses are left as they are. Owner references are
  untouched, so they are still garbage-collected with their `RestateCluster`.
- `MANAGE_NETWORK_POLICIES=false` overrides `spec.security.disableNetworkPolicies` on every cluster, and removes the
  operator's default network isolation; provide equivalent policies yourself.

### Migration Guidance
The Helm chart does not have dedicated values for these yet. Set them through `env`:

```yaml
env:
  - name: MANAGE_NETWORK_POLICIES
    value: "false"
  - name: MANAGE_SECRET_PROVIDER_CLASSES
    value: "false"
```

The chart's ClusterRole still grants both resources; remove those grants yourself to run without the permissions.
Roll out the new settings before revoking permissions, so no older pod that still manages the resources loses access.
If clusters use `secretProvider` signing, move them to a Kubernetes `secret` before turning that switch off.
