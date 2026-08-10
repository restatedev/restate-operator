# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

This is a Kubernetes operator for [Restate](https://restate.dev/), written in Rust. The operator manages three main Custom Resource Definitions (CRDs):
- `RestateCluster` - Manages Restate server clusters with StatefulSets
- `RestateDeployment` - Manages Restate SDK service deployments (similar to Kubernetes Deployments but with Restate-specific versioning)
- `RestateCloudEnvironment` - Integrates with Restate Cloud environments

The operator enforces network isolation by default, handles service versioning/draining, and supports both ReplicaSet and Knative Serving deployment modes.

## Development Environment

Required Rust version: 1.80+

## Common Development Commands

### Building and Running

```bash
# Build the operator
just build

# Build with specific features, architecture, or libc
just build features="telemetry"
just build arch="amd64"
just build libc="musl"

# Build Docker image
just docker

# Run the operator locally (requires OPERATOR_NAMESPACE env var)
OPERATOR_NAMESPACE=restate-operator RUST_LOG=info cargo run
```

### Code Generation

The operator uses code generation for CRDs and Pkl schemas:

```bash
# Generate CRD YAML files from Rust code
just generate

# Generate Pkl schema templates
just generate-pkl

# Generate example YAML from Pkl templates
just generate-examples
```

**Important**: When modifying CRD structs in `src/resources/*.rs`, run `just generate` to regenerate the YAML files in `crd/`.

### Testing

```bash
# Run all tests
cargo test

# Run a specific test
cargo test <test_name>
```

### Installing CRDs

```bash
# Install CRDs into the current Kubernetes cluster
just install-crds

# Note: Uses kubectl create (not apply), so remove existing CRDs first if needed
```

### Code Quality

```bash
# Format code
just fmt

# Run clippy linter
just lint

# Run both formatting and linting
just check
```

## Architecture

### Controller Structure

The operator runs three concurrent controllers (see `src/main.rs:94-105`):
1. **RestateCluster Controller** (`src/controllers/restatecluster/`) - Manages StatefulSets, Services, NetworkPolicies for Restate server clusters
2. **RestateCloudEnvironment Controller** (`src/controllers/restatecloudenvironment/`) - Manages tunnel deployments for Restate Cloud integration
3. **RestateDeployment Controller** (`src/controllers/restatedeployment/`) - Manages service deployments with versioning and draining

Each controller implements a reconciliation loop using the Kube-rs runtime.

### CRD Definitions

CRD structs are defined in `src/resources/`:
- `restateclusters.rs` - RestateCluster CRD
- `restatedeployments.rs` - RestateDeployment CRD
- `restatecloudenvironments.rs` - RestateCloudEnvironment CRD
- `knative.rs` - Knative Serving resource definitions

Additional AWS-specific resources:
- `podidentityassociations.rs` - AWS EKS Pod Identity
- `securitygrouppolicies.rs` - AWS Security Groups for Pods
- `secretproviderclasses.rs` - CSI Secret Provider

### Deployment Modes

RestateDeployment supports two modes (see `src/resources/restatedeployments.rs:20-28`):
- **ReplicaSet** (default): Traditional Kubernetes Deployment pattern
- **Knative**: Knative Serving with Configuration-per-tag architecture

**Key Concept - Tag-Based Identity**:
- A Restate deployment is immutable once registered
- The `tag` field determines deployment identity
- Same tag = in-place update (new Knative Revision within same Restate deployment)
- Changed tag = versioned update (new Restate deployment ID)
- No tag = template hash as tag (every template change creates new deployment)

### Network Isolation

By default, RestateCluster enforces network isolation via NetworkPolicies:
- Allows peer-to-peer traffic between Restate pods
- Allows operator access to admin API
- Allows egress to public internet and coredns
- Allows traffic to namespaces labeled with `allow.restate.dev/<cluster-name>`
- Denies all other traffic

Disable with `spec.security.disableNetworkPolicies: true`

### State Management

Controllers share state via `src/controllers/mod.rs:State`:
- Diagnostics (last reconciliation events)
- Prometheus metrics registry
- Operator configuration (namespace, labels, AWS settings)

## Kubernetes Integration

### kubectl Usage

Always use `kubectl apply --server-side` for CRDs to avoid client-side validation issues.

### Local Image Support for Knative Deployments

When developing with Knative deployment mode using locally built images:

**Problem**: Knative Serving's revision-controller resolves image tags to digests before creating pods. This fails for local-only images that don't exist in a remote registry.

**Solution**: Use the `dev.local` image prefix

```bash
# Build your service image
docker build -t ghcr.io/restatedev/restate-operator/greeter:latest \
  examples/services/greeter/

# Tag with dev.local prefix for Knative compatibility
docker tag ghcr.io/restatedev/restate-operator/greeter:latest \
  dev.local/restatedev/restate-operator/greeter:latest
```

**Why it works**:
- `dev.local` is in Knative's default `registries-skipping-tag-resolving` list
- Knative skips digest resolution for images with this prefix
- No cluster configuration changes needed
- Works out-of-the-box with standard Knative Serving installations

**Key differences between deployment modes**:
- **ReplicaSet mode**: Can use any image prefix (e.g., `ghcr.io/*`) with `imagePullPolicy: Never`
- **Knative mode**: Must use `dev.local/*` prefix for local images

**Alternative prefixes** (also in default skip list):
- `ko.local` - Used by the `ko` build tool
- `kind.local` - Convention for Kind clusters

All example Knative manifests in `examples/services/greeter/k8s/knative-*.yaml` already use the `dev.local` prefix.

## Common Patterns

### Service URL Construction

Use the helper function from `src/controllers/mod.rs:79-94`:
```rust
use crate::controllers::service_url;

let url = service_url("service-name", "namespace", 9070, Some("/path"), "cluster.local")?;
```

### Error Handling

Error types are defined in `src/lib.rs`. Use the `Error` enum for controller errors:
- `NotReady` - Cluster not ready (triggers requeue)
- `DeploymentNotReady` - Deployment not ready (triggers requeue)
- `InvalidRestateConfig` - Configuration validation errors
- `SecretNotFound` / `SecretKeyNotFound` - Missing credentials

### Finalizers

Both RestateCluster and RestateDeployment use finalizers for cleanup:
- `clusters.restate.dev` - Ensures namespace deletion
- `deployments.restate.dev` - Ensures service deregistration and ReplicaSet cleanup

### Restate Invocation Lifecycle and Deployment Status

**Important**: `list_deployments` (`src/controllers/restatedeployment/controller.rs`) queries the admin API for two independent signals per registered deployment, returning them as a `DeploymentState`:

- **`latest_endpoint`**: the deployment serves the latest revision of at least one service (`sys_service`)
- **`has_pinned_invocations`**: at least one *non-completed* invocation is pinned to it (`sys_invocation_status WHERE status != 'completed'`)

Only one of them is a reason to wait, and which one depends on whether the RestateDeployment is being deleted:

- **Normal reconcile**: either signal makes a version "active" — it is kept, and any pending removal timer is reset.
- **Being deleted** (`deletion_timestamp` set): only `has_pinned_invocations` blocks. Nothing can supersede the latest endpoint once the object is going away, so waiting on it would wedge the finalizer forever (issue #172). A version with live pinned invocations is held for `spec.restate.drainDelaySeconds` and then force-deregistered; `spec.revisionHistoryLimit` is bypassed so every version is actually deregistered before the finalizer is released.

Completed invocations do **not** keep a deployment alive — the operator does not wait for Restate's 24h retention purge (changed in #71). Note this differs from what `restate deployments list` reports: it shows a superseded deployment as `Draining` while completed invocations are still retained, and `Drained` only once they are purged.

**SQL tables**:
- `sys_service`: current service revisions, one row per service
- `sys_invocation_status`: ALL invocations including completed; the `status` column is what the operator filters on
- `sys_invocation`: same content as `sys_invocation_status`

For testing, `restate invocations purge <id>` immediately purges a completed invocation.

## Helm Chart

The Helm chart is located in `charts/restate-operator-helm/`.

### Deploying Locally Built Operator

To deploy a locally built operator image:

```bash
# 1. Generate CRDs
just generate

# 2. Build Docker image (tagged as ghcr.io/restatedev/restate-operator:local)
just docker

# 3. Deploy or upgrade with Helm
helm upgrade --install restate-operator charts/restate-operator-helm \
  --namespace restate-operator \
  --create-namespace \
  --set version=local

# 4. Force rollout (Helm upgrade doesn't always trigger pod restart with imagePullPolicy: IfNotPresent)
kubectl delete pod -n restate-operator -l app=restate-operator
```

**Important**:
- The chart uses the `version` parameter (not `image.tag`) to set the image tag. The `version` field defaults to `.Chart.Version` if not specified.
- After `helm upgrade`, you may need to manually delete the operator pod to force a rollout and pull the new local image, especially when using `imagePullPolicy: IfNotPresent`.

### Key Helm Values

- `version` - Image tag override (defaults to chart version)
- `image.repository` - Image repository (default: `ghcr.io/restatedev/restate-operator`)
- `image.pullPolicy` - Pull policy (default: `IfNotPresent`)
- `awsPodIdentityAssociationCluster` - Enables EKS Pod Identity support
- `gcpWorkloadIdentity` - Enables GCP Workload Identity via Config Connector
- `canaryImage` - Container image for canary jobs and the trusted CA certs init container (default: `alpine:3.21`); must provide `cat`, `grep`, `wget` and a CA bundle at `/etc/ssl/certs/ca-certificates.crt`
- `operatorNamespace` - Namespace where operator runs
- `operatorLabelName/Value` - Labels for network policy selectors

## Releasing

1. Update version in `charts/restate-operator-helm/Chart.yaml` and `Cargo.{toml,lock}`
2. Push tag `v<version>` (e.g., `v0.0.2`)
3. Accept the draft release after CI completes

## Debugging Tips

### Running Operator Locally

```bash
# Set required environment variables
export OPERATOR_NAMESPACE=restate-operator
export RUST_LOG=info

# Run with cargo
cargo run
```

### Viewing Logs

The operator exposes HTTP endpoints on port 8080:
- `/health` - Liveness; always 200 once the web server is up. Says nothing about the controllers
- `/ready` - Readiness; 200 once every controller has its CRD and is reconciling, otherwise 503 with the list of controllers still waiting (`{"ready":false,"pendingControllers":["RestateCluster"]}`)
- `/metrics` - Prometheus metrics
- `/` - Diagnostics (last event timestamp)

### kubectl Tips

```bash
# View RestateCluster status
kubectl get restatecluster -A

# View RestateDeployment status with details
kubectl get restatedeployment -o wide

# Check operator logs
kubectl logs -n restate-operator -l app=restate-operator
```
