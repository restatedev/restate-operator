# Restate Operator

A Kubernetes operator that creates [Restate](https://restate.dev/) clusters. Supported features:

- Online volume expansion
- Network security via `NetworkPolicy`
- Manage credentials using [EKS Pod Identity](https://docs.aws.amazon.com/eks/latest/userguide/pod-identities.html)
- Manage security groups using [Security Groups for Pods](https://docs.aws.amazon.com/eks/latest/userguide/security-groups-for-pods.html)
- Sign requests using private keys from Secrets or CSI Secret Store
- Deploy Restate SDK services using the `RestateDeployment` crd, the operator will manage their versions automatically, draining
  old versions when there are no longer invocations running against them.

## Usage

### Installing

```bash
helm install restate-operator oci://ghcr.io/restatedev/restate-operator-helm --namespace restate-operator --create-namespace
```

### Creating a cluster

The operator watches `RestateCluster` objects, which are not namespaced. A Namespace with the same name as the
`RestateCluster` will be created, in which a StatefulSet, Service, and NetworkPolicies are created.

An example `RestateCluster` with one node:

```yaml
apiVersion: restate.dev/v1
kind: RestateCluster
metadata:
  name: restate-test
spec:
  compute:
    image: restatedev/restate:1.3.2
  storage:
    storageRequestBytes: 2147483648 # 2 GiB
```

An example `RestateCluster` with 3 nodes using S3 for metadata and [snapshots](https://docs.restate.dev/operate/snapshots/):

```yaml
apiVersion: restate.dev/v1
kind: RestateCluster
metadata:
  name: restate-test
spec:
  compute:
    replicas: 3
    image: restatedev/restate:1.3.2
  storage:
    storageRequestBytes: 2147483648 # 2 GiB
  security:
    # this kind of annotation can be used to give your cluster an IAM role in EKS
    serviceAccountAnnotations:
      eks.amazonaws.com/role-arn: arn:aws:iam::111122223333:role/my-role-that-can-read-write-to-the-bucket
  config: |
    roles = [
        "worker",
        "admin",
        "log-server",
    ]
    auto-provision = true
    default-num-partitions = 128
    default-replication = 2

    [metadata-client]
    type = "object-store"
    path = "s3://some-bucket/metadata"
    # the same aws-* parameters as below are supported here

    [bifrost]
    default-provider = "replicated"

    [worker.snapshots]
    destination = "s3://some-bucket/snapshots"
    snapshot-interval-num-records = 10000
    # you can also provide parameters here for non-S3 stores eg:
    # aws-region = "local"
    # aws-access-key-id = "minioadmin"
    # aws-secret-access-key = "minioadmin"
    # aws-endpoint-url = "http://localhost:9000"
    # aws-allow-http = true
```

Note that if using an object store for metadata, it must support conditional PUTs. They are not available on all all object store implementations.
If you don't have access to an object store with conditional PUTs, you can run using the default Raft-based metadata store, and if you don't
have access to an object store of any kind you can also run without snapshots.
Running a distributed cluster without snapshots is not recommended as they are used to speed up failover.
A config for raft metadata and without snapshots is as follows:

```yaml
apiVersion: restate.dev/v1
kind: RestateCluster
metadata:
  name: restate-test
spec:
  compute:
    replicas: 3
    image: restatedev/restate:1.3.2
  storage:
    storageRequestBytes: 2147483648 # 2 GiB
  config: |
    roles = [
        "worker",
        "admin",
        "log-server",
        "metadata-server",
    ]
    # auto-provision should not be turned on when using the raft metadata store
    # provision with kubectl -n restate-test exec -it restate-0 -- restatectl provision
    auto-provision = false
    default-num-partitions = 128
    default-replication = 2

    [metadata-server]
    type = "replicated"

    [metadata-client]
    addresses = ["http://restate:5122/"]

    [bifrost]
    default-provider = "replicated"
```

For the full schema as a [Pkl](https://pkl-lang.org/) template see [`crd/RestateCluster.pkl`](./crd/RestateCluster.pkl).

### Creating a Deployment
```yaml
apiVersion: restate.dev/v1beta1
kind: RestateDeployment
metadata:
  name: my-deployment
spec:
  replicas: 1
  restate:
    register:
      cluster: restate-test
  selector:
    matchLabels:
      app: my-deployment
  template:
    metadata:
      labels:
        app: my-deployment
    spec:
      containers:
      - name: app
        image: my-restate-service-image:main
        ports:
        - name: restate
          containerPort: 9080
```

For the full schema as a [Pkl](https://pkl-lang.org/) template see [`crd/RestateDeployment.pkl`](./crd/RestateCluster.pkl).


### EKS Pod Identity

[EKS Pod Identity](https://docs.aws.amazon.com/eks/latest/userguide/pod-identities.html) is a convenient way to have a
single AWS role shared amongst many Restate clusters, where the AWS identities will contain tags detailing their
Kubernetes identity. This can be useful for access control eg 'Restate clusters in namespace `my-cluster` may call this
Lambda'.

This operator can create objects for the
[AWS ACK EKS controller](https://github.com/aws-controllers-k8s/eks-controller) such that pod identity associations are
created for each `RestateCluster`. To enable this functionality the operator must be started with knowledge of the EKS
cluster name, by setting `awsPodIdentityAssociationCluster` in the helm chart. If this option is set, the ACK CRDs must
be installed or the operator will fail to start. Then, you may provide `awsPodIdentityAssociationRoleArn` in
the `RestateCluster` spec.

### EKS Security Groups for Pods

[EKS Security Groups for Pods](https://docs.aws.amazon.com/eks/latest/userguide/security-groups-for-pods.html) allows
you to isolate pods into separate AWS Security Groups, which is a powerful security primitive which can help you limit
Restate to public IP access, as well as to obtain VPC flow logs.

The operator can create `SecurityGroupPolicy` objects which put Restate pods into a set of Security Groups. If this CRD
is installed, you may provide `awsPodSecurityGroups` in the `RestateCluster` spec.

## Releasing

1. Update the version in charts/restate-operator/Chart.yaml and the version in Cargo.{toml,lock} eg to `0.0.2`
2. Push a new tag `v0.0.2`
3. Accept the draft release once the workflow finishes
