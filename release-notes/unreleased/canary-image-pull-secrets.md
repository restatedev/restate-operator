# Release Notes: Canary Jobs use the cluster's image pull secrets

## Bug Fix

### What Changed

The Pod Identity and Workload Identity canary Jobs now use the image pull secrets listed in the RestateCluster's `spec.compute.imagePullSecrets`, the same ones the `restate` StatefulSet uses. Before, the canary pods had no pull secrets.

### Why This Matters

When the canary image (`--canary-image`) is served by a registry that needs credentials, the canary pod stayed in `ImagePullBackOff`. The RestateCluster then never became Ready and the StatefulSet was never created, even though the Restate server image itself could be pulled with the named secret.

### Impact on Users

- Clusters that name pull secrets and use Pod Identity or Workload Identity with a canary image from an authenticated registry now get past the canary.
- On clusters that set `imagePullSecrets`, the existing canary Job, stuck or already completed, is deleted and recreated with the pull secrets on the first reconcile after the upgrade. The cluster reports `NotReady` until the new canary succeeds.
- Clusters without `imagePullSecrets` see no change.
