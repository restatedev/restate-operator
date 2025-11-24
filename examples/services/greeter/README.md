# Restate Example Service

A minimal Restate service for demonstrating **immutable deployments** with the `RestateDeployment` CRD.

## What It Shows

1. **Version Identity** — Every response includes `version` and `pod` fields
2. **Graceful Draining** — `slowGreet` handler for testing in-flight request handling during upgrades
3. **Retryable Errors** — `poison` input causes retryable errors that Restate handles with exponential backoff
4. **In-Place Fixes** — Setting `ANTIDOTE` env var allows stuck invocations to succeed (Knative mode only)
5. **Versioned Updates** — Each template change creates a new Restate deployment
6. **Deployment Modes** — Support for both ReplicaSet and Knative Serving deployments

## Prerequisites

Before deploying this service, you need a running Restate cluster:

```bash
# Deploy the Restate cluster (see ../cluster/README.md for details)
kubectl apply -f ../cluster/cluster.yaml

# Wait for cluster to be ready
kubectl get restatecluster restate -w

# Provision the cluster (required for single-node setup)
kubectl -n restate exec -it restate-0 -- restatectl provision
```

## Quick Reference

```bash
# Normal greeting - shows version/pod in response
curl localhost:8080/Greeter/greet -H 'content-type: application/json' -d '"Alice"'
# {"message":"Hello Alice!","version":"v1","pod":"greeter-replicaset-xyz"}

# Slow greeting - keeps pod alive during deployment updates
curl localhost:8080/Greeter/slowGreet -H 'content-type: application/json' \
  -d '{"name":"Bob","delaySeconds":60}'

# Poison - causes retryable error, invocation gets stuck retrying
curl localhost:8080/Greeter/greet -H 'content-type: application/json' -d '"poison"'
# Error 500: "Temporarily poisoned! Restate will retry. Set ANTIDOTE environment variable to fix."
# Restate will retry this invocation with exponential backoff until fixed or cancelled
```

## Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `SERVICE_VERSION` | Shown in responses | `v1` |
| `POD_NAME` | Pod identifier | `$HOSTNAME` |
| `ANTIDOTE` | Set to any value to fix "poison" handling | (unset) |

## Deployment Modes

This service supports two deployment modes:

### ReplicaSet Mode (Default)

Uses Kubernetes ReplicaSet for traditional pod management:
- Manual scaling with `replicas` field
- Standard Kubernetes pod lifecycle
- Fixed number of replicas (no scale-to-zero)
- **Every template change creates a NEW Restate deployment** (versioned updates only)

**Manifests:**
- `k8s/greeter-replicaset-v1.yaml` - Initial deployment (no ANTIDOTE)
- `k8s/greeter-replicaset-v2.yaml` - Version upgrade (includes ANTIDOTE fix)

### Knative Mode

Uses Knative Serving for serverless-style deployment:
- Autoscaling with `minScale`, `maxScale`, `target`
- Scale-to-zero capability (save resources when idle)
- Gradual rollout with configurable duration
- Tag-based deployment identity

**Manifests:**
- `k8s/knative-v1.yaml` - Explicit tag "v1" (enables in-place updates)
- `k8s/knative-v1-fixed.yaml` - In-place fix (same tag, adds ANTIDOTE)
- `k8s/knative-v2.yaml` - Versioned update (new tag "v2")
- `k8s/knative-auto.yaml` - Auto-versioning (no tag, uses template hash)

**Key Differences:**

| Feature | ReplicaSet Mode | Knative Mode |
|---------|----------------|--------------|
| Scaling | Manual (`replicas`) | Autoscaling (`minScale`/`maxScale`) |
| Scale-to-zero | ❌ No | ✅ Yes (with `minScale: 0`) |
| Deployment identity | Template hash (always versioned) | Tag-based or template hash |
| In-place updates | ❌ Not supported | ✅ Same tag = in-place |
| Versioned updates | ✅ Every template change | ✅ Different tag |
| Gradual rollout | Via `maxUnavailable`/`maxSurge` | Knative feature |
| URL stability | Via Service | Via Knative Route (per tag) |

**Choosing a Mode:**
- Use **ReplicaSet** for traditional workloads with predictable load
- Use **Knative** for variable workloads, development/testing, or to leverage scale-to-zero

For detailed test scenarios, see [TESTING.md](TESTING.md).

## Demo Scenarios

### 1. Version Routing

See which deployment handles each request:

```bash
# Deploy v1
kubectl apply -f k8s/greeter-replicaset-v1.yaml

curl localhost:8080/Greeter/greet -d '"Alice"' | jq
# {
#   "message": "Hello Alice!",
#   "version": "v1",
#   "pod": "greeter-replicaset-v1-abc123"
# }

# Deploy v2
kubectl apply -f k8s/greeter-replicaset-v2.yaml

curl localhost:8080/Greeter/greet -d '"Alice"' | jq
# {
#   "message": "Hello Alice!",
#   "version": "v2",
#   "pod": "greeter-replicaset-v2-def456"
# }
```

### 2. Graceful Draining

Old pods stay alive until in-flight requests complete:

```bash
# Start a 60-second request (in background)
curl localhost:8080/Greeter/slowGreet \
  -d '{"name":"Bob","delaySeconds":60}' &

# Immediately deploy v2
kubectl apply -f k8s/greeter-replicaset-v2.yaml

# Watch pods - v1 stays until slowGreet completes
kubectl get pods -l app=greeter-replicaset -w
# NAME                              READY   STATUS    AGE
# greeter-replicaset-v1-abc123      1/1     Running   2m   ← stays for in-flight request
# greeter-replicaset-v2-def456      1/1     Running   5s   ← new requests go here

# After 60s, v1 pod terminates
```

### 3. Poison/Antidote (Knative In-Place Fix)

Demonstrate fixing stuck invocations by updating an environment variable (Knative mode only):

```bash
# Deploy v1 (no antidote)
kubectl apply -f k8s/knative-v1.yaml

# Poison causes retryable error - invocation gets stuck retrying
curl localhost:8080/Greeter/greet -d '"poison"'
# Error 500: "Temporarily poisoned! Restate will retry..."

# Check invocation status using Restate CLI - it's BACKING_OFF, will retry indefinitely
restate invocations list
# Shows invocation in "backing-off" status

# Apply the fix (same tag "v1", adds ANTIDOTE=cure)
kubectl apply -f k8s/knative-v1-fixed.yaml

# Wait for rollout, then check invocation - it eventually succeeds!
# The stuck invocation retries, hits the fixed code, and completes
curl localhost:8080/Greeter/greet -d '"poison"' | jq
# {
#   "message": "Hello poison! (neutralized with: cure)",
#   "version": "v1",
#   "antidote": "cure"
# }
```

**Note:** ReplicaSet mode does NOT support in-place fixes. Every template change creates a new Restate deployment with a different ID. Stuck invocations continue retrying against the old pods, so you must manually cancel them. Use `k8s/greeter-replicaset-v2.yaml` which includes the ANTIDOTE fix as a versioned update.

## Files

```
├── src/index.ts                    # Service implementation
├── Dockerfile                      # Multi-stage build
├── package.json
├── tsconfig.json
├── README.md                       # This file
├── TESTING.md                      # Detailed test scenarios
└── k8s/
    ├── greeter-replicaset-v1.yaml  # ReplicaSet: Initial deployment (no ANTIDOTE)
    ├── greeter-replicaset-v2.yaml  # ReplicaSet: Version upgrade (with ANTIDOTE)
    ├── knative-v1.yaml             # Knative: Tagged deployment (v1, no ANTIDOTE)
    ├── knative-v1-fixed.yaml       # Knative: In-place fix (same tag v1, with ANTIDOTE)
    ├── knative-v2.yaml             # Knative: Versioned update (tag v2, with ANTIDOTE)
    └── knative-auto.yaml           # Knative: Auto-versioning (no tag, template hash)
```

## Building

```bash
# Local dev
npm install
SERVICE_VERSION=v1 npm run dev

# Docker - Build and tag for Knative local development
docker build -t ghcr.io/restatedev/restate-operator/greeter:latest .

# Tag with dev.local prefix for Knative compatibility
# Knative requires dev.local prefix to skip digest resolution for local images
docker tag ghcr.io/restatedev/restate-operator/greeter:latest \
  dev.local/restatedev/restate-operator/greeter:latest

# For production, push to registry
docker push ghcr.io/restatedev/restate-operator/greeter:latest
```

### Local Image Support for Knative

When using Knative deployment mode with locally built images:

**Why `dev.local` prefix is needed:**
- Knative Serving's revision-controller resolves image tags to digests before creating pods
- This digest resolution fails for local-only images that don't exist in a registry
- The `dev.local` prefix tells Knative to skip digest resolution (it's in Knative's default skip list)
- No cluster configuration changes are needed - `dev.local` works out-of-the-box

**Image naming:**
- **ReplicaSet mode**: Can use any image name (e.g., `ghcr.io/...`) with `imagePullPolicy: Never`
- **Knative mode**: Must use `dev.local/...` prefix for local images

**Example workflow:**
```bash
# Build image
docker build -t ghcr.io/restatedev/restate-operator/greeter:latest .

# Tag for Knative
docker tag ghcr.io/restatedev/restate-operator/greeter:latest \
  dev.local/restatedev/restate-operator/greeter:latest

# Deploy (manifests already use dev.local prefix)
kubectl apply -f k8s/knative-v1.yaml
```

## License

MIT
