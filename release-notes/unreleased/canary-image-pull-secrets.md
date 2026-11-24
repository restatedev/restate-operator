# Release Notes: Canary Jobs use the cluster's image pull secrets

## Bug Fix

### What Changed

The Pod Identity and Workload Identity canary Jobs now use the image pull secrets listed in the RestateCluster's `spec.compute.imagePullSecrets`, the same ones the `restate` StatefulSet uses. Before, the canary pods had no pull secrets.

### Why This Matters

When the canary image (`--canary-image`) is served by a registry that needs credentials, the canary pod stayed in `ImagePullBackOff`. The RestateCluster then never became Ready and the StatefulSet was never created, even though the Restate server image itself could be pulled with the named secret.

### Impact on Users

- Clusters that name pull secrets and use Pod Identity or Workload Identity with a canary image from an authenticated registry now get past the canary.
- A canary Job that is stuck from an earlier operator version is deleted and recreated with the pull secrets on the next reconcile.
- Clusters without `imagePullSecrets` see no change.
