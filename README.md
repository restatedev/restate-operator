# Restate Operator

A Kubernetes operator that creates [Restate](https://restate.dev/) clusters. Supported features:

- Online volume expansion
- Network security via `NetworkPolicy`
- Manage credentials using [EKS Pod Identity](https://docs.aws.amazon.com/eks/latest/userguide/pod-identities.html)

## Usage

### Installing

```bash
helm install restate-operator oci://ghcr.io/restatedev/restate-operator-helm --namespace restate-operator --create-namespace
```

### Creating a cluster

The operator watches `RestateCluster` objects, which are not namespaced. A Namespace with the same name as the
`RestateCluster` will be created, in which a StatefulSet, Service, and NetworkPolicies are created.

An example `RestateCluster`:

```yaml
apiVersion: restate.dev/v1
kind: RestateCluster
metadata:
  name: restate-test
spec:
  compute:
    image: restatedev/restate:0.8.0
  storage:
    storageRequestBytes: 2147483648 # 2 GiB
```

For the full schema as a [Pkl](https://pkl-lang.org/) template see [`crd/RestateCluster.pkl`](./crd/RestateCluster.pkl).

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
