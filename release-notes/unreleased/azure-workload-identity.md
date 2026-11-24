# Release Notes: Azure Workload Identity through Azure Service Operator

## New Feature

### What Changed
A `RestateCluster` can now set `spec.security.azureWorkloadIdentity: true` to give its pods their own Azure managed identity on AKS. The operator creates three Azure Service Operator (ASO) objects in the cluster's namespace:

- a `UserAssignedIdentity` named `restate`, created in a resource group that holds only environment identities, which exports its client ID and principal ID into the ConfigMap `restate-azure-identity`;
- a `FederatedIdentityCredential` named `restate`, which trusts the cluster's `restate` ServiceAccount and the AKS OIDC issuer;
- a `RoleAssignment` named `restate-snapshots`, which grants a custom data-only role (you create it, see below) on the snapshot container with a condition that limits the identity to blobs and listings under `<namespace>/`.

When all three are Ready, the operator copies the client ID into the `azure.workload.identity/client-id` annotation of the `restate` ServiceAccount, labels the pods with `azure.workload.identity/use: "true"`, and runs a canary Job. The canary exchanges the federated token for a storage token, lists the cluster's own prefix, and writes and deletes a probe blob under it, so it proves the token exchange and every clause of the role assignment's condition. It uses `curl` and installs it with `apk` if the canary image lacks it. A new role assignment can take several minutes to take effect in Azure, so the canary retries for about 11 minutes before the cluster reports `AzureWorkloadIdentityCanaryFailed`. The StatefulSet is only created or updated after the canary succeeds.

Deleting the `RestateCluster` deletes the three objects, and ASO removes the managed identity, the credential and the role assignment from Azure. Setting the field to `false` or removing it does the same while the cluster stays. The operator only deletes or replaces objects that carry an owner reference to the `RestateCluster`, and reports `AzureIdentityConflict` for a same-named object that belongs to somebody else. A cluster that is already running keeps its known client ID through a transient ASO state, and only a first enable or a changed client ID waits for ASO.

### Why This Matters
On GKE every environment shares one Google service account, and on Azure a single managed identity can hold at most 20 federated credentials. One identity per environment removes that limit, and the condition on the role assignment keeps each environment out of the snapshots of every other environment.

### Impact on Users
- Nothing changes for clusters that do not set the field, and nothing changes unless the operator is given the Azure settings below.
- A `azure.workload.identity/client-id` annotation in `serviceAccountAnnotations` is replaced by the operator's value while the feature is on.
- If the field is `true` but the operator has no Azure settings, the operator logs a warning and ignores the field. If the ASO CRDs are missing, the cluster reports `AzureIdentityCRDNotFound`, and while ASO is still working it reports `AzureIdentityNotReady` with ASO's own message.
- The operator does not create the Azure side that it depends on. Create a custom role that grants only the data actions `blobs/read`, `blobs/write`, `blobs/add/action` and `blobs/delete`, with no management actions. Do not use the built-in Storage Blob Data Contributor role: it also carries `containers/delete` and other actions that the condition cannot limit, so any environment could delete the shared container. ASO must be installed before the operator starts, because the watches are decided at startup, with rights to create identities and credentials in the identity resource group, and to create role assignments on the snapshot container. The identity resource group must hold only environment identities, because a role that can add credentials can add them to every identity in its scope.

### Migration Guidance
Apply the updated CRDs first (`installCrds: true` handles this, otherwise upgrade `restate-operator-crds`), because `spec.security.azureWorkloadIdentity` is a new field. Then set the five Helm values together. The operator refuses to start with only some of them, or with a malformed ARM ID.

```yaml
azureWorkloadIdentity:
  identityResourceGroupId: /subscriptions/<id>/resourceGroups/<name>
  location: swedencentral
  oidcIssuer: https://<region>.oic.prod-aks.azure.com/<tenant>/<cluster>/
  storageContainerId: /subscriptions/<id>/resourceGroups/<name>/providers/Microsoft.Storage/storageAccounts/<account>/blobServices/default/containers/<container>
  roleDefinitionId: /subscriptions/<id>/providers/Microsoft.Authorization/roleDefinitions/<guid>
```

These map to the environment variables `AZURE_WI_IDENTITY_RESOURCE_GROUP_ID`, `AZURE_WI_LOCATION`, `AZURE_WI_OIDC_ISSUER` and `AZURE_WI_STORAGE_CONTAINER_ID` and `AZURE_WI_ROLE_DEFINITION_ID` (and the flags `--azure-wi-*`). The chart grants the operator access to the ASO kinds only when `azureWorkloadIdentity` is set.

The Restate pods then need `AZURE_STORAGE_ACCOUNT_NAME` and a snapshot destination such as `az://<container>/<namespace>/snapshots`. The AKS Workload Identity webhook supplies `AZURE_CLIENT_ID`, `AZURE_TENANT_ID` and `AZURE_FEDERATED_TOKEN_FILE`.

### Other generated changes in this release
Regenerating the CRD and the Pkl bindings for this change also picked up two updates that the committed files were missing. The description of `status.reconciliation` on `RestateCluster` now matches its Rust doc comment, and the Pkl bindings for `RestateCloudEnvironment` gained the `spec.proxyBaseUrl` field that its CRD already had. Neither changes a schema.

### Related Issues
- Plan: BYOK on Azure
