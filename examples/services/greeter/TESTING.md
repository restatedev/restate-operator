# Greeter Service Testing Guide

This guide provides step-by-step testing scenarios for both ReplicaSet and Knative deployment modes.

## Prerequisites

### 1. Install Restate Operator

```bash
# Install the operator (adjust for your installation method)
helm install restate-operator ./helm/restate-operator
```

### 2. Deploy Restate Cluster

```bash
# Deploy the cluster
kubectl apply -f ../cluster/cluster.yaml

# Wait for cluster to be ready
kubectl get restatecluster restate -w
# Press Ctrl+C when STATUS shows Running

# Check pod status
kubectl get pods -n restate

# Provision the cluster (required for single-node Raft setup)
kubectl -n restate exec -it restate-0 -- restatectl provision
```

Expected output:
```
Cluster provisioned successfully
```

### 3. Set Up Port Forwarding

In a separate terminal, forward the Restate ingress and admin ports:

```bash
kubectl port-forward -n restate svc/restate 8080:8080 9070:9070
```

Keep this running for all test scenarios.

### 4. Configure Restate CLI

Set the Restate CLI host environment variable for all subsequent CLI commands:

```bash
export RESTATE_HOST=localhost:9070
```

This allows you to use `restate` CLI commands to inspect invocations, deployments, and other state.

### 5. Verify Cluster Health

```bash
curl http://localhost:8080/restate/health
```

Expected output:
```json
{"status":"ok"}
```

---

## Test Scenario 1: ReplicaSet Baseline

**Objective**: Verify basic ReplicaSet deployment, version routing, graceful draining, and versioned updates.

### Step 1.1: Deploy V1

```bash
kubectl apply -f k8s/greeter-replicaset-v1.yaml
```

Wait for deployment:
```bash
kubectl get pods -l app=greeter-replicaset -w
```

### Step 1.2: Test Greet Endpoint

```bash
curl http://localhost:8080/Greeter/greet \
  -H 'content-type: application/json' \
  -d '"Alice"'
```

Expected output:
```json
{
  "message": "Hello Alice!",
  "version": "v1",
  "pod": "greeter-replicaset-<hash>-<id>",
  "timestamp": "2025-11-22T..."
}
```

✅ **Verify**: `version` is "v1"

### Step 1.3: Test Poison (Should Retry)

**Note**: This curl will hang because Restate will retry indefinitely. Press Ctrl+C after a few seconds or use `--max-time 5` to timeout.

```bash
# This will hang - press Ctrl+C after seeing it stuck
curl -s http://localhost:8080/Greeter/greet \
  -H 'content-type: application/json' \
  -d '"poison"'

# OR use a timeout (will show "Operation timed out" - that's expected)
curl -s --max-time 5 http://localhost:8080/Greeter/greet \
  -H 'content-type: application/json' \
  -d '"poison"'
```

Expected behavior: curl hangs because Restate is retrying the invocation with exponential backoff.

The invocation is now stuck in a retry loop. Check its status:
```bash
# List invocations (should show one in BACKING_OFF state)
restate invocations list
```

✅ **Verify**: Invocation shows status "backing-off" and is retrying

### Step 1.4: Cancel Stuck Invocation

Since ReplicaSet mode creates a new deployment ID, the stuck invocation will keep retrying against the old pods. We must cancel it:

```bash
# Get the invocation ID from the previous step output
INVOCATION_ID="<invocation-id-from-above>"

# Cancel the invocation
restate invocations cancel $INVOCATION_ID
```

Expected output:
```
✅ Invocation cancelled
```

✅ **Verify**: Invocation is cancelled and no longer retrying

### Step 1.5: Test Graceful Draining

Start a long-running request in the background:
```bash
curl http://localhost:8080/Greeter/slowGreet \
  -H 'content-type: application/json' \
  -d '{"name":"Bob","delaySeconds":60}' &
```

Note the PID:
```bash
SLOW_PID=$!
```

Immediately deploy V2:
```bash
kubectl apply -f k8s/greeter-replicaset-v2.yaml
```

Watch pods (in another terminal):
```bash
kubectl get pods -l app=greeter-replicaset -w
```

Expected behavior:
- New v2 pods created
- Old v1 pods remain in Running state for ~60 seconds
- After slowGreet completes, old pods terminate

Wait for the background request to complete:
```bash
wait $SLOW_PID
```

Expected output (after 60 seconds):
```json
{
  "message": "Hello Bob! (after 60s)",
  "version": "v1",
  "delaySeconds": 60,
  ...
}
```

✅ **Verify**: Old pods stayed alive until request completed

### Step 1.6: Verify New Deployment Created

ReplicaSet mode creates a NEW Restate deployment for every template change:

```bash
kubectl describe restatedeployment greeter-replicaset | grep "Deployment Id"
```

Expected: Shows a new deployment ID (different from v1)

✅ **Verify**: Deployment ID changed (e.g., dp_abc123 → dp_def456)

### Step 1.7: Test V2 with Poison Fix

Test that V2 includes the ANTIDOTE fix:

```bash
curl http://localhost:8080/Greeter/greet \
  -H 'content-type: application/json' \
  -d '"poison"'
```

Expected output:
```json
{
  "message": "Hello poison! (neutralized with: cure)",
  "version": "v2",
  "antidote": "cure",
  ...
}
```

✅ **Verify**:
- `version` is "v2"
- Poison input now succeeds (v2 has ANTIDOTE)
- Different deployment identity than v1

### Step 1.8: Cleanup

```bash
kubectl delete -f k8s/greeter-replicaset-v2.yaml
```

---

## Test Scenario 2: Knative In-Place Updates

**Objective**: Test Knative deployment with explicit tags for in-place update semantics.

### Prerequisites

Ensure Knative Serving is installed:
```bash
kubectl get deployment -n knative-serving
```

### Step 2.1: Deploy V1 with Tag

```bash
kubectl apply -f k8s/knative-v1.yaml
```

Wait for Knative resources:
```bash
kubectl get configuration,route -l app=greeter-knative -w
```

### Step 2.2: Verify Configuration and Route

```bash
# Check Configuration
kubectl get configuration greeter-knative-v1 -o yaml

# Check Route
kubectl get route greeter-knative-v1 -o yaml
```

✅ **Verify**:
- Configuration name: `greeter-knative-v1`
- Route name: `greeter-knative-v1`
- Route URL: `http://greeter-knative-v1.default.svc.cluster.local`

### Step 2.3: Check Restate Registration

```bash
kubectl describe restatedeployment greeter-knative
```

Look for:
- `Deployment ID`: `dp_xxxxx`
- `Knative Configuration`: `greeter-knative-v1`
- `Knative Route`: `greeter-knative-v1`

### Step 2.4: Test Poison (Creates Stuck Invocation)

```bash
curl http://localhost:8080/Greeter/greet \
  -H 'content-type: application/json' \
  -d '"poison"'
```

Expected: HTTP 500 retryable error

The invocation is now stuck in a retry loop. Check its status:
```bash
# List invocations - should show one in BACKING_OFF state
restate invocations list

# Save the invocation ID for later verification
INVOCATION_ID=$(restate invocations list --output json | jq -r '.[0].id')
echo "Invocation ID: $INVOCATION_ID"
```

✅ **Verify**: Invocation shows status "backing-off" and is retrying

### Step 2.5: Apply In-Place Fix (Same Tag)

**Key Difference from ReplicaSet**: With Knative's in-place update, the stuck invocation will eventually succeed without manual cancellation!

```bash
kubectl apply -f k8s/knative-v1-fixed.yaml
```

Watch Knative create new Revision:
```bash
kubectl get revisions -l app=greeter-knative -w
```

Expected behavior:
- New Revision created: `greeter-knative-v1-00002` (or higher)
- Old Revision remains for gradual rollout
- Traffic gradually shifts (gradual rollout is a Knative Serving feature)
- **Stuck invocation will eventually retry against the fixed code and succeed**

### Step 2.6: Watch Invocation Recover

Monitor the stuck invocation as it recovers:
```bash
kubectl get configuration greeter-knative-v1 -o jsonpath='{.status.latestReadyRevisionName}'
```

Monitor the invocation status:
```bash
# Watch the invocation recover - it should transition from backing-off to completed
restate invocations describe $INVOCATION_ID
```

After the rollout completes and the invocation retries, it should succeed:
```bash
# Check final status
restate invocations describe $INVOCATION_ID
```

✅ **Verify**:
- Invocation status changed from "backing-off" to "completed"
- **No manual cancellation needed** - the in-place fix allowed the retry to succeed!

### Step 2.7: Test New Poison Requests

New poison requests should now succeed immediately:
```bash
curl http://localhost:8080/Greeter/greet \
  -H 'content-type: application/json' \
  -d '"poison"'
```

Expected: Success with antidote

### Step 2.8: Verify Same Deployment ID

```bash
kubectl describe restatedeployment greeter-knative
```

✅ **Verify**:
- Deployment ID unchanged (same as Step 2.3)
- Configuration name unchanged: `greeter-knative-v1`
- Latest Revision updated to `greeter-knative-v1-00002`

### Step 2.9: Cleanup

```bash
kubectl delete -f k8s/knative-v1-fixed.yaml
```

---

## Test Scenario 3: Knative Versioned Updates (Explicit Tags)

**Objective**: Test Knative deployment with tag changes for versioned update semantics.

### Step 3.1: Deploy V1

```bash
kubectl apply -f k8s/knative-v1.yaml
```

Wait for ready:
```bash
kubectl wait --for=condition=Ready configuration/greeter-knative-v1 --timeout=2m
```

### Step 3.2: Record V1 Deployment ID

```bash
kubectl get restatedeployment greeter-knative -o jsonpath='{.status.knative.deploymentId}'
```

Save this value (e.g., `dp_abc123`)

### Step 3.3: Deploy V2 (New Tag)

```bash
kubectl apply -f k8s/knative-v2.yaml
```

Watch resources:
```bash
kubectl get configuration,route -l app=greeter-knative -w
```

Expected behavior:
- New Configuration created: `greeter-knative-v2`
- New Route created: `greeter-knative-v2`
- Old Configuration remains: `greeter-knative-v1`

### Step 3.4: Verify Multiple Configurations

```bash
kubectl get configuration -l app=greeter-knative
```

Expected output:
```
NAME                  LATESTCREATED             LATESTREADY               READY
greeter-knative-v1    greeter-knative-v1-00001  greeter-knative-v1-00001  True
greeter-knative-v2    greeter-knative-v2-00001  greeter-knative-v2-00001  True
```

✅ **Verify**: Two Configurations exist

### Step 3.5: Verify New Deployment ID

```bash
kubectl get restatedeployment greeter-knative -o jsonpath='{.status.knative.deploymentId}'
```

✅ **Verify**: Deployment ID changed (different from Step 3.2)

### Step 3.6: Check Previous Configuration Status

```bash
kubectl get configuration greeter-knative-v1 -o yaml | grep -A 5 ownerReferences
```

✅ **Verify**: `controller: false` (previous Configuration)

```bash
kubectl get configuration greeter-knative-v2 -o yaml | grep -A 5 ownerReferences
```

✅ **Verify**: `controller: true` (current Configuration)

### Step 3.7: Test V2 Endpoint

```bash
curl http://localhost:8080/Greeter/greet \
  -H 'content-type: application/json' \
  -d '"Alice"'
```

✅ **Verify**: `version` is "v2"

### Step 3.8: Wait for Cleanup

The operator should clean up the v1 Configuration when no active invocations remain.

Check status:
```bash
kubectl describe restatedeployment greeter-knative
```

Look for `previousConfigurations` section showing active invocation count.

After a few minutes:
```bash
kubectl get configuration greeter-knative-v1
```

Expected: Eventually shows "NotFound" (cleaned up)

### Step 3.9: Cleanup

```bash
kubectl delete -f k8s/knative-v2.yaml
```

---

## Test Scenario 4: Knative Auto-Versioning (Template Hash)

**Objective**: Test automatic versioning when no tag is specified.

### Step 4.1: Deploy Without Tag

```bash
kubectl apply -f k8s/knative-auto.yaml
```

### Step 4.2: Find Configuration Name

```bash
kubectl get configuration -l app=greeter-knative
```

Expected output:
```
NAME                       LATESTCREATED                LATESTREADY                  READY
greeter-knative-<hash>     greeter-knative-<hash>-00001 greeter-knative-<hash>-00001 True
```

Note the hash value (e.g., `7mz6h89b`)

✅ **Verify**: Configuration uses template hash as tag

### Step 4.3: Record First Deployment ID

```bash
kubectl get restatedeployment greeter-knative -o jsonpath='{.status.knative.deploymentId}'
```

Save this value.

### Step 4.4: Modify Template

Edit the manifest to change the image or environment variable:
```bash
# Edit knative-auto.yaml and change SERVICE_VERSION from "auto" to "auto-v2"
```

Apply the change:
```bash
kubectl apply -f k8s/knative-auto.yaml
```

### Step 4.5: Verify New Configuration Created

```bash
kubectl get configuration -l app=greeter-knative
```

Expected output:
```
NAME                       LATESTCREATED                LATESTREADY                  READY
greeter-knative-<hash1>    greeter-knative-<hash1>-00001 greeter-knative-<hash1>-00001 True
greeter-knative-<hash2>    greeter-knative-<hash2>-00001 greeter-knative-<hash2>-00001 True
```

✅ **Verify**:
- Two Configurations with different hashes
- Each has its own Route

### Step 4.6: Verify New Deployment ID

```bash
kubectl get restatedeployment greeter-knative -o jsonpath='{.status.knative.deploymentId}'
```

✅ **Verify**: Deployment ID changed

### Step 4.7: Cleanup

```bash
kubectl delete -f k8s/knative-auto.yaml
```

---

## Test Scenario 5: Advanced - Direct Configuration Edit

**Objective**: Test the controller=false pattern for hotfixing previous deployments.

### Step 5.1: Deploy V1 and V2

```bash
kubectl apply -f k8s/knative-v1.yaml
# Wait for ready
sleep 30
kubectl apply -f k8s/knative-v2.yaml
# Wait for ready
sleep 30
```

### Step 5.2: Verify Two Configurations

```bash
kubectl get configuration -l app=greeter-knative
```

Expected:
- `greeter-knative-v1` (controller=false, previous)
- `greeter-knative-v2` (controller=true, current)

### Step 5.3: Manually Edit Previous Configuration

```bash
kubectl edit configuration greeter-knative-v1
```

Change the image tag or add an environment variable:
```yaml
spec:
  template:
    spec:
      containers:
      - image: dev.local/restatedev/restate-operator/greeter:hotfix  # Change this
```

Save and exit.

### Step 5.4: Watch Revision Creation

```bash
kubectl get revisions -l serving.knative.dev/configuration=greeter-knative-v1 -w
```

Expected: New Revision created (e.g., `greeter-knative-v1-00002`)

### Step 5.5: Verify Change Persists

Wait a few minutes for reconciliation cycle, then check:
```bash
kubectl get configuration greeter-knative-v1 -o jsonpath='{.spec.template.spec.containers[0].image}'
```

✅ **Verify**: Your manual change is still present (not reverted)

### Step 5.6: Try Editing Current Configuration

```bash
kubectl edit configuration greeter-knative-v2
```

Make the same change, save and exit.

Wait a few minutes, then check:
```bash
kubectl get configuration greeter-knative-v2 -o jsonpath='{.spec.template.spec.containers[0].image}'
```

✅ **Verify**: Your manual change was REVERTED (operator reconciled it)

### Step 5.7: Cleanup

```bash
kubectl delete -f k8s/knative-v2.yaml
```

---

## Test Scenario 6: Knative Configuration Cleanup

**Objective:** Verify that old Configurations are automatically cleaned up after the 5-minute drain period when deployments become inactive.

**Prerequisites:**
- RestateCluster running (`kubectl apply -f ../restate/cluster.yaml`)
- Greeter service built and tagged as `dev.local/restatedev/restate-operator/greeter:latest`

### Steps

1. **Deploy V1 with explicit tag:**
   ```bash
   kubectl apply -f k8s/knative-v1.yaml
   # Wait for ready
   kubectl wait --for=condition=Ready restatedeployment greeter-knative --timeout=120s
   ```

2. **Verify initial state:**
   ```bash
   # Should see one Configuration and Route for v1-0
   kubectl get configurations -l restate.dev/owned-by=greeter-knative
   kubectl get routes -l restate.dev/owned-by=greeter-knative

   # Get the Configuration name
   kubectl get configuration greeter-knative-v1-0 -o yaml

   # Check deployment registered
   curl http://restate.restate.svc.cluster.local:9070/deployments | jq
   ```

3. **Deploy V2 (new tag - triggers versioned update):**
   ```bash
   kubectl apply -f k8s/knative-v2.yaml
   # Wait for ready
   kubectl wait --for=condition=Ready restatedeployment greeter-knative --timeout=120s
   ```

4. **Check old Configuration gets drain annotation:**
   ```bash
   # Should see annotation restate.dev/remove-version-at with timestamp ~5 minutes in future
   kubectl get configuration greeter-knative-v1-0 -o jsonpath='{.metadata.annotations.restate\.dev/remove-version-at}'
   echo ""  # newline

   # Annotation should be in RFC3339 format like: 2025-11-23T12:30:00Z
   ```

5. **Verify both Configurations exist during drain period:**
   ```bash
   # Within 5 minutes, should see BOTH configurations
   kubectl get configurations -l restate.dev/owned-by=greeter-knative

   # NAME                      LATESTCREATED              LATESTREADY                READY
   # greeter-knative-v1-0      greeter-knative-v1-0-00001 greeter-knative-v1-0-00001 True
   # greeter-knative-v2        greeter-knative-v2-00001   greeter-knative-v2-00001   True

   # Both Routes should also exist
   kubectl get routes -l restate.dev/owned-by=greeter-knative
   ```

6. **Wait for drain period to expire (5+ minutes):**
   ```bash
   # Wait 6 minutes to ensure drain period passes
   echo "Waiting for 5-minute drain period to expire..."
   sleep 360
   ```

7. **Verify old Configuration deleted:**
   ```bash
   # After 5+ minutes, old Configuration should be deleted
   kubectl get configuration greeter-knative-v1-0
   # Should get: Error from server (NotFound): configurations.serving.knative.dev "greeter-knative-v1-0" not found

   # Old Route should also be deleted
   kubectl get route greeter-knative-v1-0
   # Should get: Error from server (NotFound): routes.serving.knative.dev "greeter-knative-v1-0" not found

   # Only v2 Configuration should remain
   kubectl get configurations -l restate.dev/owned-by=greeter-knative
   # NAME                      LATESTCREATED              LATESTREADY                READY
   # greeter-knative-v2        greeter-knative-v2-00001   greeter-knative-v2-00001   True
   ```

8. **Check Restate deployment force-deleted:**
   ```bash
   # Old v1 deployment should be gone from Restate
   curl http://restate.restate.svc.cluster.local:9070/deployments | jq '.deployments[] | select(.id | contains("greeter"))'

   # Should only see v2 deployment, not v1
   ```

9. **Test revision history limit (optional):**
   ```bash
   # Deploy multiple versions to test history limit
   # Create copies of knative-v2.yaml with tags: v3, v4, v5
   # Apply each in sequence
   # Check that old Configurations are cleaned up respecting revision_history_limit
   ```

10. **Test finalization blocking:**
    ```bash
    # Deploy v2.0, then immediately delete RestateDeployment
    kubectl apply -f k8s/knative-v2.yaml
    kubectl wait --for=condition=Ready restatedeployment greeter-knative --timeout=120s

    # Delete should block if old Configurations still in drain period
    kubectl delete restatedeployment greeter-knative

    # Watch operator logs to see drain messages
    kubectl logs -n restate-operator deployment/restate-operator -f | grep -i "drain"

    # Deletion should complete after drain period expires
    ```

### Expected Results

- ✅ Old Configurations get `restate.dev/remove-version-at` annotation (5min timestamp)
- ✅ Old Configurations remain during 5-minute drain period
- ✅ Old Configurations deleted after drain period expires
- ✅ Old Routes also deleted when Configuration is deleted

### Important Notes on Invocation Retention

**Deployment Activity and Invocation Retention:**
The operator considers a deployment "active" if it has ANY invocations in Restate's `sys_invocation_status` table, including **completed invocations** awaiting purge. This aligns with Restate's internal design:

- **Default retention**: Completed invocations remain for 24 hours before automatic purging
- **Cleanup timing**: Configurations tied to deployments with completed invocations are retained until Restate purges them
- **Deployment states**: A deployment shows as "Draining" (not "Drained") until all invocations are purged

**Testing Implications:**
If you invoke the service before deploying v2, the completed invocation will make v1 appear "active" for up to 24 hours, preventing immediate cleanup testing.

**Quick cleanup for testing:**
```bash
# List invocations for a specific deployment
restate sql "SELECT id, target, status FROM sys_invocation_status
WHERE pinned_deployment_id = '<deployment-id>'"

# Manually purge completed invocations to speed up testing
restate invocations purge <invocation-id> --yes
```

After purging completed invocations, the deployment transitions from "Draining" to "Drained", allowing cleanup to proceed normally.
- ✅ Restate deployments force-deleted via admin API (`DELETE /deployments/{id}?force=true`)
- ✅ Revision history limit respected (keep N most recent)
- ✅ Finalization blocks during drain period, completes after

### Verification

```bash
# Watch operator logs during cleanup
kubectl logs -n restate-operator deployment/restate-operator -f | grep -E "(cleanup|removal|drain)"

# Check operator diagnostics
kubectl get events --field-selector involvedObject.kind=RestateDeployment --sort-by='.lastTimestamp'

# Verify no orphaned Configurations
kubectl get configurations -A | grep greeter-knative

# Verify Restate state
curl http://restate.restate.svc.cluster.local:9070/deployments | jq '.deployments | length'
```

### Cleanup

```bash
kubectl delete -f k8s/knative-v2.yaml
```

---

## Verification Checklist

### ReplicaSet Mode
- [ ] V1 deployment successful
- [ ] Poison endpoint fails without ANTIDOTE (v1)
- [ ] Graceful draining during version upgrade
- [ ] New deployment ID created for V2 (versioned update)
- [ ] V2 deployment successful with ANTIDOTE fix

### Knative In-Place Updates
- [ ] Configuration created with correct tag
- [ ] Route created with stable URL
- [ ] Deployment registered with Restate
- [ ] In-place fix creates new Revision
- [ ] Same deployment ID after fix
- [ ] Gradual rollout observed

### Knative Versioned Updates
- [ ] V1 Configuration created
- [ ] V2 Configuration created with new tag
- [ ] Different deployment IDs for V1 and V2
- [ ] Previous Configuration marked controller=false
- [ ] Current Configuration marked controller=true
- [ ] Previous Configuration eventually cleaned up

### Knative Auto-Versioning
- [ ] Configuration uses template hash as tag
- [ ] Template change creates new Configuration
- [ ] Different deployment IDs for each template
- [ ] Cleanup of old Configurations

### Advanced Patterns
- [ ] Previous Configuration edits persist (controller=false)
- [ ] Current Configuration edits reverted (controller=true)

### Knative Configuration Cleanup
- [ ] Old Configuration gets drain annotation (5min timestamp)
- [ ] Old Configuration persists during drain period
- [ ] Old Configuration deleted after drain expires
- [ ] Old Route also deleted with Configuration
- [ ] Restate deployment force-deleted via admin API
- [ ] Revision history limit respected
- [ ] Finalization blocks during drain

---

## Troubleshooting

### Deployment Not Registering

```bash
# Check operator logs
kubectl logs -n restate-operator deployment/restate-operator

# Check RestateDeployment events (use appropriate name: greeter-replicaset or greeter-knative)
kubectl describe restatedeployment greeter-replicaset
kubectl describe restatedeployment greeter-knative

# Check network connectivity
kubectl run -it --rm debug --image=curlimages/curl -- \
  curl http://restate.restate.svc.cluster.local:9070/health
```

### Knative Resources Not Created

```bash
# Check Knative Serving installation
kubectl get deployment -n knative-serving

# Check Configuration status
kubectl describe configuration greeter-knative-v1

# Check for errors
kubectl get events --sort-by='.lastTimestamp'
```

### Cleanup Not Happening

```bash
# Check operator logs for cleanup errors
kubectl logs -n restate-operator deployment/restate-operator | grep cleanup

# Manually check invocation count via Restate CLI
export RESTATE_HOST=localhost:9070
restate deployments list
```

---

## Next Steps

- Explore the [design document](../../docs/design/design-4.md) for implementation details
- Review [Restate documentation](https://restate.dev/) for service development
- Check [Knative documentation](https://knative.dev/docs/serving/) for advanced Knative features
