# Restate Operator

A Kubernetes operator that creates [Restate](https://restate.dev/) clusters. Supported features:

- Online volume expansion
- Network security via `NetworkPolicy`
- Manage credentials using [EKS Pod Identity](https://docs.aws.amazon.com/eks/latest/userguide/pod-identities.html)
- Manage security groups using [Security Groups for Pods](https://docs.aws.amazon.com/eks/latest/userguide/security-groups-for-pods.html)
- Sign requests using private keys from Secrets or CSI Secret Store
- Deploy Restate SDK services using the `RestateDeployment` crd, the operator will manage their versions automatically, draining
  old versions when there are no longer invocations running against them.

## Installation

```bash
helm install restate-operator \
  oci://ghcr.io/restatedev/restate-operator-helm \
  --namespace restate-operator \
  --create-namespace
```

To render the chart templates locally for inspection or for use with a GitOps workflow, you can use `helm template`. For example, to a file named `manifests.yaml`:

```bash
helm template restate-operator oci://ghcr.io/restatedev/restate-operator-helm \
  --namespace restate-operator \
  --create-namespace \
  --include-crds \
  > manifests.yaml
```

Optionally split these into separate files for each kind:

```bash
# brew install yq
# For CRDs, include the metadata.name in the filename to avoid collisions.
yq eval 'select(.kind == "CustomResourceDefinition")' manifests.yaml | \
  yq eval --split-exp '"k8s/base/" + (.metadata.name | downcase) + "-" + (.kind | downcase) + ".yaml"' -

# For all others, just use the kind.
yq eval 'select(.kind != "CustomResourceDefinition")' manifests.yaml | \
  yq eval --split-exp '"k8s/base/" + (.kind | downcase) + ".yaml"' -
```

## Custom Resource Definitions

The operator introduces three Custom Resource Definitions (CRDs): `RestateCluster`, `RestateDeployment`, and `RestateCloudEnvironment`.

### `RestateCluster`

The `RestateCluster` CRD defines a Restate cluster. The operator watches for these objects and creates the necessary Kubernetes resources, such as `StatefulSet`, `Service`, and `NetworkPolicy` objects in a new namespace that matches the `RestateCluster` name.

**By default, the operator enforces network isolation on the cluster, allowing only the following**:
1. Peer to peer traffic between Restate pods
2. Traffic from the operator to Restate pods
3. Egress traffic to the public internet
4. Egress traffic to coredns
4. Egress traffic to pods in any namespace labelled with `allow.restate.dev/<cluster-name>`

**All other traffic is denied by default.**

The default behaviour can be disabled with `spec.security.disableNetworkPolicies: true`.
Alternatively, you can add new allowed inbound callers of the Restate ports with `spec.security.networkPeers.{ingress,admin,metrics}`, which are arrays of [`NetworkPolicyPeer`](https://kubernetes.io/docs/reference/generated/kubernetes-api/v1.28/#networkpolicypeer-v1-networking-k8s-io).
You can allow new outbound destinations by adding to the `spec.security.networkEgressRules` list, which is an array of [`NetworkPolicyEgressRule`](https://kubernetes.io/docs/reference/generated/kubernetes-api/v1.28/#networkpolicyegressrule-v1-networking-k8s-io).

**NOTE**: Each cluster is created in its own namespace. Naming the cluster after an existing Namespace is supported, but deploying into namespaces with other applications is not recommended.

#### Minimal Example

An example `RestateCluster` with one node:

```yaml
apiVersion: restate.dev/v1
kind: RestateCluster
metadata:
  name: restate-test
spec:
  compute:
    image: restatedev/restate:1.5
  storage:
    storageRequestBytes: 2147483648 # 2 GiB
```

For the full schema as a [Pkl](https://pkl-lang.org/) template see [`crd/RestateCluster.pkl`](./crd/RestateCluster.pkl).

More examples are available just below the spec that follows.

#### Spec Fields

| Field | Type | Description |
|---|---|---|
| `compute` | `object` | Compute configuration. See details below. |
| `storage` | `object` | Storage configuration. See details below. |
| `security` | `object` | Security configuration. See details below. |
| `config` | `string` | TOML-encoded Restate config file. See details below. |
| `clusterName` | `string` | Sets the `RESTATE_CLUSTER_NAME` environment variable. Defaults to the object name. |

---

#### `spec.compute`

| Field | Type | Description |
|---|---|---|
| `replicas` | `integer` | The desired number of Restate nodes. Defaults to 1. |
| `image` | `string` | **Required**. Container image name. |
| `imagePullPolicy` | `string` | Image pull policy. One of `Always`, `Never`, `IfNotPresent`. Defaults to `Always` if `:latest` tag is specified, or `IfNotPresent` otherwise. |
| `imagePullSecrets` | `array` | Optional list of references to secrets in the same namespace to use for pulling the image. |
| `resources` | `object` | Compute Resources for the Restate container. e.g., `requests` and `limits` for `cpu` and `memory`. |
| `env` | `array` | List of environment variables to set in the container. |
| `affinity` | `object` | Standard Kubernetes affinity rules. |
| `nodeSelector` | `object` | Standard Kubernetes node selector. |
| `tolerations` | `array` | Standard Kubernetes tolerations. |
| `dnsPolicy` | `string` | Pod DNS policy. |
| `dnsConfig` | `object` | Pod DNS configuration. |

---

#### `spec.storage`

| Field | Type | Description |
|---|---|---|
| `storageRequestBytes` | `integer` | **Required**. Amount of storage to request in volume claims. Can be increased but not decreased. |
| `storageClassName` | `string` | The name of the `StorageClass` for the volume claims. This field is immutable. |
| `volumeAttributesClassName` | `string` | The name of the `VolumeAttributesClass` for the volume claims. |

---

#### `spec.security`

| Field | Type | Description |
|---|---|---|
| `disableNetworkPolicies` | `boolean` | If `true`, the operator will not create any network policies. Defaults to `false`. |
| `allowOperatorAccessToAdmin` | `boolean` | If `true`, adds a rule to allow the operator to access the admin API. Needed for `RestateDeployment`. Defaults to `true`. |
| `networkPeers` | `object` | Defines network peers to allow inbound access to `admin`, `ingress`, and `node` ports. |
| `networkEgressRules` | `array` | Custom egress rules for outbound traffic from the cluster. |
| `serviceAccountAnnotations` | `object` | Annotations to add to the `ServiceAccount`. |
| `serviceAnnotations`| `object` | Annotations to add to the `Service`. |
| `awsPodIdentityAssociationRoleArn` | `string` | **Use this to grant your Restate cluster fine-grained access to other AWS resources (like S3) without managing static credentials.** Creates a `PodIdentityAssociation` to grant the cluster an IAM role. Requires the ACK EKS controller. |
| `awsPodSecurityGroups` | `array` | **Use this to isolate your Restate cluster within specific AWS Security Groups for enhanced network control and auditing.** Creates a `SecurityGroupPolicy` to place pods into these security groups. Requires the Security Groups for Pods CRD. |
| `requestSigningPrivateKey` | `object` | Configures a private key to sign outbound requests from this cluster. Can be sourced from a `secret` or a CSI `secretProvider`. See details below. |

---

#### `spec.security.requestSigningPrivateKey`

| Field | Type | Description |
|---|---|---|
| `version` | `string` | **Required**. The version of Restate request signing. Currently, only "v1" is accepted. |
| `secret` | `object` | A Kubernetes Secret source for the private key. |
| `secretProvider` | `object` | A CSI secret provider source for the private key. |

**`secret` Fields**

| Field | Type | Description |
|---|---|---|
| `secretName` | `string` | **Required**. The name of the secret. |
| `key` | `string` | **Required**. The key within the secret that contains the private key. |

**`secretProvider` Fields**

| Field | Type | Description |
|---|---|---|
| `provider` | `string` | The name of the CSI secret provider (e.g., `secrets-store.csi.k8s.io`). |
| `path` | `string` | **Required**. The path of the private key file within the mounted volume. |
| `parameters` | `object` | Provider-specific configuration parameters. |

---

#### `spec.config`

This field allows you to provide a TOML-encoded configuration string for the Restate server. This maps directly to the Restate server's configuration file. You can use this to configure aspects like roles, metadata storage, snapshotting, and more.

For a complete list of configuration options, see the [official Restate Server Configuration Reference](https://docs.restate.dev/references/server_config).

#### Key `spec.config` Options

While the `config` field accepts any valid [Restate server configuration](https://docs.restate.dev/references/server_config), some options are particularly important for defining the cluster's topology and behavior.

*   **`roles`**: An array of strings defining the functions of the nodes in the cluster. Common roles include:
    *   `worker`: Executes service code.
    *   `admin`: Provides the administration API for deployments and cluster management.
    *   `log-server`: Part of the replicated log for durable state.
    *   `metadata-server`: Part of the Raft-based replicated metadata store. Not required if using object store for metadata.
    *   `http-ingress`: Exposes an HTTP endpoint for invoking services.

*   **`[metadata-client]`**: Configures how the cluster stores its core metadata. This is a critical choice for production deployments.
    *   `type = "replicated"`: Uses a built-in Raft consensus protocol. This is simpler to set up but requires careful management of the Raft cluster members.
    *   `type = "object-store"`: Uses an S3-compatible object store for metadata, which is simpler to operate particularly if using an object store for snapshots. You must provide the `path` to the bucket.

*   **`[worker.snapshots]`**: Configures durable snapshots of service state, which is essential for fault tolerance and fast recovery.
    *   `destination`: The S3 URI where snapshots will be stored (e.g., `s3://my-bucket/snapshots`).
    *   `snapshot-interval-num-records`: How many log records are processed in a particular partition before a new snapshot is taken.

*   **`auto-provision`**: A boolean that controls whether the cluster should automatically initialize itself. This should be `true` for object-store metadata but `false` when using the `replicated` (Raft) metadata store, which requires manual provisioning.

*   **Resource Management**:
    *   `rocksdb-total-memory-size`: Sets the total memory allocated to RocksDB, which Restate uses for its internal state storage. Typically 75% of the memory requests for the pod is appropriate.
    *   `admin.query-engine.memory-size`: Allocates memory for the admin service's query engine.

For a complete list of all available options, please refer to the [official Restate Server Configuration Reference](https://docs.restate.dev/references/server_config).

If you don't have access to an object store that supports conditional PUTs for metadata, you can run using the default Raft-based metadata store. The following is an example of a `RestateCluster` configured for Raft metadata without snapshots. Note that running a distributed cluster without snapshots is not recommended as they are used to speed up failover.

```yaml
apiVersion: restate.dev/v1
kind: RestateCluster
metadata:
  name: restate-test
spec:
  compute:
    replicas: 3
    image: restatedev/restate:1.5
  storage:
    storageRequestBytes: 2147483648 # 2 GiB
  config: |
    roles = [
        "worker",
        "admin",
        "log-server",
        "metadata-server",
        "http-ingress",
    ]
    # auto-provision should not be turned on when using the raft metadata store
    # provision with kubectl -n restate-test exec -it restate-0 -- restatectl provision
    auto-provision = false
    default-num-partitions = 128
    default-replication = 2

    [metadata-server]
    type = "replicated"

    [metadata-client]
    addresses = ["http://restate-cluster:5122/"]

    [bifrost]
    default-provider = "replicated"
```

#### Advanced Example

> ⚠️ **Supported object stores for metadata:** Only AWS S3 is currently tested and supported as a metadata backend.
  We are aware of issues with GCS and with MinIO's consistency models that make them an unsafe choice for metadata,
  but they can be used for snapshots where consistency is not needed.

An example `RestateCluster` with 3 nodes using S3 for metadata and [snapshots](https://docs.restate.dev/operate/snapshots/):

```yaml
apiVersion: restate.dev/v1
kind: RestateCluster
metadata:
  name: restate-test
spec:
  compute:
    replicas: 3
    image: restatedev/restate:1.5
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
        "http-ingress",
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

Note that Restate needs `s3:ListBucket` on the bucket, and `s3:GetObject`/`s3:PutObject` on the bucket contents.

#### MinIO example
See [docs/minio.md](./docs/minio.md)

### `RestateDeployment`

The `RestateDeployment` CRD is similar to a standard Kubernetes `Deployment` but is tailored for deploying Restate services. It manages `ReplicaSet` and `Service` objects for each version of your service, which is crucial for Restate's versioning and draining capabilities. This ensures that old service versions remain available until all in-flight invocations are completed.

#### Example

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
For the full schema as a [Pkl](https://pkl-lang.org/) template see [`crd/RestateDeployment.pkl`](./crd/RestateDeployment.pkl).

#### Spec Fields

| Field | Type | Description |
|---|---|---|
| `replicas` | `integer` | Number of desired pods. Defaults to 1. |
| `selector` | `object` | **Required**. Label selector for pods. Must match the pod template's labels. See details below. |
| `template` | `object` | **Required**. Pod template for the deployment. See details below. |
| `restate` | `object` | **Required**. Restate-specific configuration. See details below. |
| `minReadySeconds` | `integer` | Minimum seconds a new pod should be ready before it's considered available. Defaults to 0. |
| `revisionHistoryLimit`| `integer` | Number of old ReplicaSets to retain for rollbacks. Defaults to 10. |

---

#### `spec.selector`

This is a standard Kubernetes [label selector](https://kubernetes.io/docs/concepts/overview/working-with-objects/labels/#label-selectors). It must match the labels of the pod template.

| Field | Type | Description |
|---|---|---|
| `matchLabels` | `object` | A map of key-value pairs. |
| `matchExpressions` | `array` | A list of label selector requirements. |

---

#### `spec.template`

This is a standard Kubernetes `PodTemplateSpec`. The contents of this field are passed through directly from the operator to the created `ReplicaSet` and are not validated by the operator.

For details on the `PodTemplateSpec` schema, see the [official Kubernetes API documentation](https://kubernetes.io/docs/reference/kubernetes-api/workload-resources/pod-template-v1/#PodTemplateSpec).

---

#### `spec.restate`

This field contains Restate-specific configuration.

| Field | Type | Description |
|---|---|---|
| `register` | `object` | **Required**. The location of the Restate Admin API to register this deployment against. See details below. |
| `servicePath` | `string` | Optional path to append to the Service url when registering with Restate. |
| `useHttp11`  | `boolean` | Force the use of HTTP/1.1 when registering with Restate. Defaults to HTTP/2 if not specified. |


The `register` field must specify exactly one of `cluster`, `service`, or `url`.

| Field | Type | Description |
|---|---|---|
| `cluster` | `string` | The name of a `RestateCluster` CRD object in the same Kubernetes cluster. |
| `cloud` | `string` | The name of a `RestateCloudEnvironment` CRD object in the same Kubernetes cluster. |
| `service` | `object` | A reference to a Kubernetes `Service` that points to the Restate admin API. See details below. |
| `url` | `string` | The direct URL of the Restate admin endpoint. |

**`register.service` Fields**

| Field | Type | Description |
|---|---|---|
| `name` | `string` | **Required**. The name of the service. |
| `namespace` | `string` | **Required**. The namespace of the service. |
| `path` | `string` | An optional URL path to be prepended to admin API paths. Should not end with a `/`. |
| `port` | `integer` | The port on the service that hosts the admin API. Defaults to 9070. |

### `RestateCloudEnvironment`

The `RestateCloudEnvironment` CRD allows you to use the `RestateDeployment` feature with a Restate Cloud environment. This resource describes a cloud environment, references a secret used to communicate with it, and manages a Deployment of tunnel pods in your Kubernetes cluster which allows Restate Cloud to call into your services without having to expose them over the public internet.

#### Minimal Example

```yaml
apiVersion: restate.dev/v1beta1
kind: RestateCloudEnvironment
metadata:
  name: my-cloud-environment
spec:
  environmentId: env_201j05r9g0f12ygtphdszbb4scp
  signingPublicKey: publickeyv1_BBuEJnx28hdGb5Ky6qpvuXQG4aVoWBnubJtHXpznzgQk
  region: us
  authentication:
    secret:
      name: my-cloud-environment-secret
      key: token
```

#### Spec Fields

| Field | Type | Description |
|---|---|---|
| `environmentId` | `string` | **Required**. The environment ID of your cluster, which begins with `env_`. |
| `region` | `string` | **Required**. The short region identifier of your cluster, e.g., `us`, `eu`. |
| `signingPublicKey` | `string` | **Required**. The request signing public key of your cluster, which begins `publickeyv1_`. It is not a secret. |
| `authentication` | `object` | **Required**. Where to get credentials for communication with the Cloud environment. See details below. |
| `tunnel` | `object` | Optional configuration for the deployment of tunnel pods. See details below. |

---

#### `spec.authentication`

| Field | Type | Description |
|---|---|---|
| `secret` | `object` | **Required**. A reference to a secret in the same namespace as the operator. See details below. |

**`secret` Fields**

| Field | Type | Description |
|---|---|---|
| `name` | `string` | **Required**. The name of the referenced secret. It must be in the same namespace as the operator. |
| `key` | `string` | **Required**. The key to read from the referenced Secret. |

---

#### `spec.tunnel`

| Field | Type | Description |
|---|---|---|
| `remoteProxy` | `boolean` | If true, the tunnel pods will expose unauthenticated access to the Restate Cloud environment on ports 8080 and 9070. Defaults to false. |
| `replicas` | `integer` | The desired number of tunnel pods. Defaults to 1. |
| `image` | `string` | Container image name. Defaults to a suggested version of the ghcr.io/restatedev/restate-cloud-tunnel-client. |
| `imagePullPolicy` | `string` | Image pull policy. One of `Always`, `Never`, `IfNotPresent`. Defaults to `Always` if `:latest` tag is specified, or `IfNotPresent` otherwise. |
| `env` | `array` | List of environment variables to set in the container; these may override defaults. |
| `resources` | `object` | Compute Resources for the tunnel pods. |
| `dnsConfig` | `object` | DNS configuration for the tunnel pod. |
| `dnsPolicy` | `string` | DNS policy for the pod. Defaults to `ClusterFirst`. Valid values are `ClusterFirstWithHostNet`, `ClusterFirst`, `Default` or `None`. |
| `tolerations` | `array` | Pod tolerations. |
| `nodeSelector` | `object` | Node selector for the pod. |
| `affinity` | `object` | Pod affinity. Defaults to zone anti-affinity, provide `{}` to disable all affinity. |

Most of these fields correspond to fields in a native `DeploymentSpec`. See the [official Kubernetes API documentation] (https://kubernetes.io/docs/reference/kubernetes-api/workload-resources/deployment-v1/#DeploymentSpec).

#### Setup Instructions

1. **Obtain an admin API token** from the Restate Cloud UI and place it in a secret in the same namespace as the operator:

```shell
# paste your API key into a local file
pbpaste > token
# create the Secret in the restate-operator namespace
kubectl -n restate-operator create secret generic my-cloud-environment-secret --from-file token
```

2. **Create a RestateCloudEnvironment** referencing your environment ID, region, and token:

```yaml
apiVersion: restate.dev/v1beta1
kind: RestateCloudEnvironment
metadata:
  name: my-cloud-environment
spec:
  environmentId: env_201j05r9g0f12ygtphdszbb4scp
  region: us
  authentication:
    secret:
      name: my-cloud-environment-secret
      key: token
```

3. **Reference this cluster** when creating RestateDeployment objects in any namespace:

```yaml
apiVersion: restate.dev/v1beta1
kind: RestateDeployment
metadata:
  name: my-deployment
spec:
  restate:
    register:
      cloud: my-cloud-environment
```


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

1. Update the version in `charts/restate-operator/Chart.yaml` and the version in `Cargo.{toml,lock}` eg to `0.0.2`
2. Push a new tag `v0.0.2`
3. Accept the draft release once the workflow finishes
