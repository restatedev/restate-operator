# Restate Cluster Setup

This directory contains a minimal Restate cluster configuration for local development and testing.

## Overview

The `cluster.yaml` manifest creates a single-node Restate cluster with:
- **Name**: `restate`
- **Namespace**: `restate` (created automatically by the operator)
- **Replicas**: 1 (single node)
- **Storage**: 2 GiB persistent volume
- **Metadata**: Local Raft-based (no S3 required)
- **Image**: `restatedev/restate:1.5`

## Prerequisites

1. **Kubernetes cluster** - Minikube, kind, or any Kubernetes cluster
2. **Restate operator** - Must be installed and running
3. **kubectl** - Configured to access your cluster

## Quick Start

```bash
# 1. Deploy the cluster
kubectl apply -f examples/restate/cluster.yaml

# 2. Wait for the cluster to be ready (may take 1-2 minutes)
kubectl get restatecluster restate -w

# 3. Check pod status
kubectl get pods -n restate

# 4. Manually provision the cluster (required for Raft-based metadata)
kubectl -n restate exec -it restate-0 -- restatectl provision

# 5. Verify the cluster is running
kubectl -n restate logs restate-0
```

## Deployment Steps

### Step 1: Deploy the Cluster

```bash
kubectl apply -f examples/restate/cluster.yaml
```

The operator will:
- Create a namespace called `restate`
- Create a StatefulSet with 1 pod: `restate-0`
- Create two Services:
  - `restate` - Main service with all ports
  - `restate-cluster` - Headless service for pod communication
- Create a PersistentVolumeClaim for storage

### Step 2: Wait for Ready Status

```bash
# Watch the cluster status
kubectl get restatecluster restate -w

# Watch pod creation
kubectl get pods -n restate -w
```

Expected output:
```
NAME       READY   STATUS    RESTARTS   AGE
restate-0  1/1     Running   0          2m
```

### Step 3: Provision the Cluster

For single-node clusters with Raft-based metadata, you must manually provision:

```bash
kubectl -n restate exec -it restate-0 -- restatectl provision
```

Expected output:
```
Cluster provisioned successfully
```

**Note**: Multi-node clusters with object storage (S3) can use `auto-provision: true` in the config to skip this step.

### Step 4: Verify the Cluster

Check logs:
```bash
kubectl -n restate logs restate-0
```

Look for messages like:
```
[INFO] Restate server started
[INFO] Ingress listening on 0.0.0.0:8080
[INFO] Admin API listening on 0.0.0.0:9070
```

## Accessing the Cluster

### Port Forwarding

Forward the ingress and admin ports to your local machine:

```bash
# Ingress endpoint (for invoking services)
kubectl port-forward -n restate svc/restate 8080:8080

# Admin endpoint (for deployment registration)
kubectl port-forward -n restate svc/restate 9070:9070
```

### Testing Connectivity

```bash
# Check cluster health
curl http://localhost:9070/health

# List registered deployments
curl http://localhost:9070/deployments
```

## Architecture

### Services Created

The RestateCluster creates the following Kubernetes Services:

**`restate` (ClusterIP)**
- **8080** (ingress): HTTP ingress for invoking Restate services
- **9070** (admin): Administration API for deployment registration
- **5122** (metrics): Metrics and inter-node communication

**`restate-cluster` (Headless)**
- **5122**: Pod-to-pod communication for clustering

### Namespace and Network Policies

- **Namespace**: The operator creates a dedicated `restate` namespace
- **Network Policies**: By default, the operator configures network policies to:
  - Allow peer-to-peer Restate traffic (port 5122)
  - Allow operator-to-Restate admin API traffic (port 9070)
  - Allow pods with label `allow.restate.dev/restate: "true"` to access the cluster
  - Allow public internet egress
  - Deny all other traffic

### Storage

The cluster uses a PersistentVolumeClaim for:
- Raft log storage (metadata)
- Local state storage
- Partition store

**Size**: 2 GiB (configurable via `spec.storage.storageRequestBytes`)

## Using with Example Service

The greeter service in `examples/services/greeter/` is configured to register with this cluster:

```yaml
spec:
  restate:
    register:
      cluster: restate  # References this RestateCluster by name
```

Deploy the greeter service:
```bash
kubectl apply -f examples/services/greeter/k8s/deployment-v1.yaml
```

The operator will automatically:
1. Register the greeter service with the Restate cluster
2. Add the `allow.restate.dev/restate: "true"` label to greeter pods
3. Update RestateDeployment status with deployment ID

Verify registration:
```bash
# Port-forward admin API
kubectl port-forward -n restate svc/restate 9070:9070

# List deployments
curl http://localhost:9070/deployments
```

## Advanced Configuration

### Multi-Node Cluster

For a multi-node cluster, update the manifest:

```yaml
spec:
  compute:
    image: restatedev/restate:1.5
    replicas: 3  # 3 or more nodes recommended
```

### S3-Backed Metadata

For production, use S3 for metadata storage:

```yaml
spec:
  compute:
    image: restatedev/restate:1.5
    replicas: 3
  storage:
    storageRequestBytes: 2147483648
  config: |
    auto-provision = true

    [metadata-client]
    type = "object-store"

    [metadata-client.object-store]
    provider = "s3"
    bucket = "my-restate-metadata-bucket"
    region = "us-west-2"
```

See the main README.md for full S3 configuration examples.

### MinIO-Backed Snapshots

For self-hosted S3-compatible storage with MinIO:

```yaml
spec:
  config: |
    [worker.snapshots]
    destination = "s3://my-snapshots-bucket/snapshots/"

    [worker.snapshots.s3]
    endpoint = "http://minio.minio.svc.cluster.local:9000"
```

See `docs/minio.md` for complete MinIO setup instructions.

## Troubleshooting

### Cluster Not Ready

```bash
# Check operator logs
kubectl logs -n restate-operator deployment/restate-operator

# Check cluster events
kubectl describe restatecluster restate

# Check pod status
kubectl describe pod -n restate restate-0
```

### Provisioning Failed

If `restatectl provision` fails:
```bash
# Check if cluster is already provisioned
kubectl -n restate logs restate-0 | grep -i provision

# Try force provision (warning: resets cluster)
kubectl -n restate exec -it restate-0 -- restatectl provision --force
```

### Network Connectivity Issues

If services can't register with Restate:
```bash
# Check network policies
kubectl get networkpolicies -n restate

# Verify service labels
kubectl get pod <pod-name> -o yaml | grep -A 5 labels

# Test connectivity from service pod
kubectl exec -it <pod-name> -- curl http://restate.restate.svc.cluster.local:9070/health
```

## Cleanup

To delete the cluster and all resources:

```bash
# Delete the RestateCluster (also deletes namespace, pods, services)
kubectl delete restatecluster restate

# Verify namespace is deleted
kubectl get namespace restate
```

**Warning**: This will permanently delete all data in the cluster, including:
- All registered deployments
- All invocation state
- All persisted data

## Next Steps

- [Deploy the greeter service](../service/README.md)
- [Run the testing scenarios](../service/TESTING.md)
- [Explore Knative deployment modes](../service/k8s/)

## References

- [Restate Documentation](https://restate.dev/)
- [Server Configuration Reference](https://docs.restate.dev/references/server_config)
- [Snapshots Guide](https://docs.restate.dev/operate/snapshots/)
