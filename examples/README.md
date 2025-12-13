# Restate Operator Examples

This directory contains complete, end-to-end examples for deploying and testing Restate services with the Restate Operator.

## Quick Start

```bash
# 1. Deploy Restate cluster
kubectl apply -f cluster/cluster.yaml
kubectl -n restate exec -it restate-0 -- restatectl provision

# 2. Deploy example service (ReplicaSet mode)
kubectl apply -f services/greeter/k8s/greeter-replicaset-v1.yaml

# 3. Port-forward to access Restate
kubectl port-forward -n restate svc/restate 8080:8080

# 4. Test the service
curl http://localhost:8080/Greeter/greet -d '"Alice"'
```

## Directory Structure

```
examples/
├── README.md          # This file - Overview of all examples
├── cluster/           # Restate cluster setup
│   ├── cluster.yaml   # RestateCluster manifest
│   └── README.md      # Cluster setup guide
└── services/greeter/           # Greeter example service
    ├── src/           # Service implementation (TypeScript)
    ├── k8s/           # Kubernetes manifests (ReplicaSet & Knative)
    ├── README.md      # Service overview
    ├── TESTING.md     # Comprehensive test scenarios
    ├── Dockerfile     # Container build
    └── package.json   # Node.js dependencies
```

## Components

### 1. Restate Cluster (`cluster/`)

A minimal, single-node Restate cluster for development and testing.

**Features:**
- Single replica (simplest setup)
- Local Raft-based metadata storage
- 2 GiB persistent storage
- No S3 or external dependencies required

**Quick Start:**
```bash
kubectl apply -f cluster/cluster.yaml
kubectl -n restate exec -it restate-0 -- restatectl provision
```

**Documentation:** See [cluster/README.md](cluster/README.md)

### 2. Greeter Service (`services/greeter/`)

A complete example Restate service demonstrating immutable deployments.

**Service Features:**
- `greet` - Quick greeting with version/pod identity
- `slowGreet` - Long-running greeting for testing graceful draining
- Poison/antidote pattern for testing in-place fixes
- Version tracking in responses

**Deployment Modes:**
- **ReplicaSet** - Traditional Kubernetes deployment
- **Knative** - Serverless-style with autoscaling and scale-to-zero

**Quick Start (ReplicaSet):**
```bash
kubectl apply -f services/greeter/k8s/greeter-replicaset-v1.yaml
```

**Quick Start (Knative):**
```bash
kubectl apply -f services/greeter/k8s/knative-v1.yaml
```

**Documentation:**
- [services/greeter/README.md](services/greeter/README.md) - Service overview and quick reference
- [services/greeter/TESTING.md](services/greeter/TESTING.md) - Step-by-step test scenarios

## Testing Scenarios

The examples support comprehensive testing of:

### ReplicaSet Deployment
1. **Basic Deployment** - Deploy v1, verify registration
2. **Poison Pattern** - Test that v1 fails on "poison" input (no ANTIDOTE)
3. **Graceful Draining** - Long-running requests during upgrades
4. **Versioned Upgrade** - Deploy v2 with ANTIDOTE fix (new deployment ID)

### Knative Deployment

#### In-Place Updates (Explicit Tags)
1. Deploy with tag "v1"
2. Apply fix (same tag, new Revision)
3. Verify same deployment ID

#### Versioned Updates (Explicit Tags)
1. Deploy with tag "v1"
2. Change tag to "v2"
3. Verify new Configuration created
4. Verify multiple Configurations coexist

#### Auto-Versioning (Template Hash)
1. Deploy without tag
2. Change template (image, env vars)
3. Verify new Configuration per template change
4. Verify automatic versioning

## Architecture Overview

### ReplicaSet Mode

```
RestateDeployment (greeter)
    ↓
ReplicaSet
    ↓
Pods
    ↓
Service
    ↓
Restate Admin API (registration)
```

**Characteristics:**
- Manual scaling via `replicas` field
- Standard Kubernetes pod lifecycle
- Rolling updates via maxUnavailable/maxSurge
- No scale-to-zero

### Knative Mode

```
RestateDeployment (greeter-knative)
    ↓
Knative Configuration (greeter-knative-v1)
    ↓
Knative Revisions (00001, 00002, ...)
    ↓
Knative Route (greeter-knative-v1)
    ↓
Restate Admin API (registration)
```

**Characteristics:**
- Autoscaling via minScale/maxScale/target
- Scale-to-zero capability
- Tag-based deployment identity
- Gradual rollout (Knative feature)
- Multiple concurrent deployments (versioned updates)

## Prerequisites

### Required Components

1. **Kubernetes Cluster**
   - Minikube, kind, or any Kubernetes cluster
   - Version 1.24 or later recommended

2. **Restate Operator**
   - Install via Helm or apply manifests directly
   - See main repository README for installation instructions

3. **For Knative Mode:**
   - Knative Serving installed
   - Version 1.12 or later recommended
   - See https://knative.dev/docs/install/

### Optional Tools

- `kubectl` - Required for all operations
- `restate` - Restate CLI for managing deployments and invocations
- `curl` - For testing service endpoints
- `jq` - For parsing JSON responses
- `watch` - For monitoring resource changes

### Installing the Restate CLI

```bash
# Install Restate CLI (see https://docs.restate.dev/installation)
brew install restatedev/tap/restate-server restatedev/tap/restate

# List deployments
restate deployments list

# List invocations
restate invocations list
```

## Common Workflows

### Development Workflow

```bash
# 1. Set up cluster (once)
kubectl apply -f cluster/cluster.yaml
kubectl -n restate exec -it restate-0 -- restatectl provision

# 2. Deploy service
kubectl apply -f services/greeter/k8s/greeter-replicaset-v1.yaml

# 3. Make changes to service code
# (edit services/greeter/src/index.ts)

# 4. Rebuild and push image
docker build -t ghcr.io/restatedev/restate-operator/greeter:dev services/greeter/
docker push ghcr.io/restatedev/restate-operator/greeter:dev

# 5. Update manifest with new image
# (edit services/greeter/k8s/greeter-replicaset-v1.yaml)

# 6. Apply update
kubectl apply -f services/greeter/k8s/greeter-replicaset-v1.yaml
```

### Testing Workflow

```bash
# 1. Set up cluster
kubectl apply -f cluster/cluster.yaml
kubectl -n restate exec -it restate-0 -- restatectl provision

# 2. Port-forward Restate
kubectl port-forward -n restate svc/restate 8080:8080 &

# 3. Run test scenarios
# Follow the step-by-step guide in services/greeter/TESTING.md

# 4. Cleanup
kubectl delete -f services/greeter/k8s/greeter-replicaset-v1.yaml
kubectl delete restatecluster restate
```

### CI/CD Workflow

```bash
# In CI pipeline (example using Knative auto-versioning):

# 1. Build and push image with git commit SHA
IMAGE_TAG=$CI_COMMIT_SHA
docker build -t myorg/greeter:$IMAGE_TAG .
docker push myorg/greeter:$IMAGE_TAG

# 2. Update manifest (no tag specified = auto-versioning)
sed "s|image:.*|image: myorg/greeter:$IMAGE_TAG|" \
  services/greeter/k8s/knative-auto.yaml | kubectl apply -f -

# 3. Wait for deployment
kubectl wait --for=condition=Ready \
  restatedeployment/greeter-knative --timeout=5m

# 4. Run integration tests
./run-integration-tests.sh
```

## Troubleshooting

### Cluster Not Starting

```bash
# Check operator logs
kubectl logs -n restate-operator deployment/restate-operator

# Check cluster status
kubectl describe restatecluster restate

# Check pod logs
kubectl logs -n restate restate-0
```

### Service Not Registering

```bash
# Check RestateDeployment status (adjust name: greeter-replicaset or greeter-knative)
kubectl describe restatedeployment greeter-knative

# Check operator logs
kubectl logs -n restate-operator deployment/restate-operator | grep greeter-knative

# Test connectivity to Restate admin API
kubectl run -it --rm debug --image=curlimages/curl -- \
  curl http://restate.restate.svc.cluster.local:9070/health
```

### Knative Configuration Not Created

```bash
# Verify Knative Serving is installed
kubectl get deployment -n knative-serving

# Check Configuration status
kubectl describe configuration greeter-knative-v1

# Check operator logs for Knative errors
kubectl logs -n restate-operator deployment/restate-operator | grep -i knative
```

## Next Steps

1. **Start with the Basics:**
   - Deploy the Restate cluster: [cluster/README.md](cluster/README.md)
   - Deploy the greeter service: [services/greeter/README.md](services/greeter/README.md)

2. **Run Test Scenarios:**
   - Follow the testing guide: [services/greeter/TESTING.md](services/greeter/TESTING.md)
   - Experiment with both ReplicaSet and Knative modes

3. **Explore Advanced Features:**
   - Read the design documentation: `docs/design/design-4.md`
   - Try multi-node Restate clusters with S3
   - Configure custom autoscaling policies

4. **Build Your Own Service:**
   - Use the greeter service as a template
   - Follow the Restate SDK documentation: https://restate.dev/
   - Deploy with the RestateDeployment CRD

## Resources

- [Restate Documentation](https://restate.dev/)
- [Restate Operator Design Docs](../docs/design/)
- [Knative Serving Documentation](https://knative.dev/docs/serving/)
- [Kubernetes Documentation](https://kubernetes.io/docs/)

## Contributing

Found an issue with the examples? Please open an issue or submit a pull request!
