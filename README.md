# Restate Operator

A Kubernetes operator that creates [Restate](https://restate.dev/) clusters. Supported features:

- Online volume expansion
- Network security via `NetworkPolicy`
- Manage credentials using [EKS Pod Identity](https://docs.aws.amazon.com/eks/latest/userguide/pod-identities.html)
- Manage security groups using [Security Groups for Pods](https://docs.aws.amazon.com/eks/latest/userguide/security-groups-for-pods.html)
- Sign requests using private keys from Secrets or CSI Secret Store
- Deploy Restate SDK services using the `RestateDeployment` crd, the operator will manage their versions automatically, draining
  old versions when there are no longer invocations running against them.
- Consume Kafka topics into Restate invocations using the `RestateKafkaIntegration` crd.

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

### Managing the CRDs separately

By default the operator chart installs the CRDs (via the bundled `restate-operator-crds` dependency,
gated by `installCrds`). For GitOps workflows, or to keep the CRD lifecycle decoupled from the operator
Deployment, install the CRD chart on its own and disable the bundled copy:

```bash
helm upgrade --install restate-operator-crds \
  oci://ghcr.io/restatedev/restate-operator-crds --version <version>

helm install restate-operator \
  oci://ghcr.io/restatedev/restate-operator-helm --version <version> \
  --namespace restate-operator --create-namespace \
  --set installCrds=false
```

The CRD chart renders the CRDs as templates, so upgrading it applies schema changes — unlike Helm's
native `crds/` directory, which is install-only. The CRDs are annotated `helm.sh/resource-policy: keep`,
so they and their custom resources are retained on `helm uninstall`.

## Custom Resource Definitions

The operator introduces four Custom Resource Definitions (CRDs): `RestateCluster`, `RestateDeployment`, `RestateCloudEnvironment`, and `RestateKafkaIntegration`.

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
Alternatively, you can add new allowed inbound callers of the Restate ports with `spec.security.networkPeers.{ingress,admin,node}`, which are arrays of [`NetworkPolicyPeer`](https://kubernetes.io/docs/reference/generated/kubernetes-api/v1.28/#networkpolicypeer-v1-networking-k8s-io).
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
  cluster:
    autoProvision: true
  compute:
    image: restatedev/restate:1.7
  storage:
    storageRequestBytes: 2147483648 # 2 GiB
  config: |
    auto-provision = false
```

For the full schema as a [Pkl](https://pkl-lang.org/) template see [`crd/RestateCluster.pkl`](./crd/RestateCluster.pkl).

More examples are available just below the spec that follows.

#### Spec Fields

| Field | Type | Description |
|---|---|---|
| `compute` | `object` | Compute configuration. See details below. |
| `storage` | `object` | Storage configuration. See details below. |
| `security` | `object` | Security configuration. See details below. |
| `cluster` | `object` | Cluster-wide configuration options. See details below. |
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
| `annotations` | `object` | Annotations to set on the Restate pod template. See note on merge ordering below. |
| `labels` | `object` | Labels to set on the Restate pod template. See note on merge ordering below. |
| `resources` | `object` | Compute Resources for the Restate container. e.g., `requests` and `limits` for `cpu` and `memory`. |
| `env` | `array` | List of environment variables to set in the container. |
| `affinity` | `object` | Standard Kubernetes affinity rules. |
| `nodeSelector` | `object` | Standard Kubernetes node selector. |
| `tolerations` | `array` | Standard Kubernetes tolerations. |
| `dnsPolicy` | `string` | Pod DNS policy. |
| `dnsConfig` | `object` | Pod DNS configuration. |
| `lifecycle` | `object` | Lifecycle hooks (`postStart` / `preStop`) for the Restate container. Note: `postStart` runs concurrently with the entrypoint and is not a reliable pre-start hook. |
| `sidecars` | `array` | Native sidecar containers run alongside Restate (appended as init containers with `restartPolicy: Always`). Requires Kubernetes 1.29+. |
| `terminationGracePeriodSeconds` | `integer` | Pod termination grace period. Defaults to 60. `0` means immediate SIGKILL (skips `preStop`). |
| `extraVolumes` | `array` | Additional pod volumes. Names must not collide with operator-managed volumes (`storage`, `tmp`, `config`, and the trusted-CA / request-signing volumes); a collision is rejected with a clear error. |
| `extraVolumeMounts` | `array` | Additional volume mounts for the Restate container. Referenced volumes must be declared in `extraVolumes` (or be operator-managed). |

**Pod annotation and label merge ordering**: User-specified `annotations` and `labels` are
merged with values the operator sets internally (e.g. for workload identity hashes, trusted
CA cert hashes). If the same key appears in both, the operator's internal value wins. This
means operator-managed features like GCP Workload Identity annotations cannot be
accidentally overridden. If you need to set the same annotation that a built-in feature
uses, disable the built-in feature first — otherwise your value will be silently replaced.

---

#### `spec.storage`

| Field | Type | Description |
|---|---|---|
| `storageRequestBytes` | `integer` | **Required**. Amount of storage to request in volume claims. Can be increased but not decreased. |
| `storageClassName` | `string` | The name of the `StorageClass` for the volume claims. This field is immutable. |
| `volumeAttributesClassName` | `string` | The name of the `VolumeAttributesClass` for the volume claims. |

---

#### `spec.cluster`

| Field | Type | Description |
|---|---|---|
| `autoProvision` | `boolean` | If `true`, the operator will automatically provision the cluster via gRPC after pods are running. Defaults to `false`. |

> ⚠️ **Important**: When `cluster.autoProvision` is set to `true`, you **must** disable all other forms of cluster provisioning:
> - Set `auto-provision = false` in `spec.config`, or set the `RESTATE_AUTO_PROVISION=false` environment variable
> - Do not use any sidecar containers or init containers that run `restatectl provision`
> - Do not manually provision the cluster
>
> Running multiple provisioning methods simultaneously can lead to split brain situations in the Restate cluster.

When enabled, the operator will:
1. Wait for the `restate-0` pod to be in `Running` state
2. Call the Restate gRPC `ProvisionCluster` API
3. Set `status.provisioned = true` to avoid repeated provisioning attempts

This feature is particularly useful for Raft-based metadata clusters where manual provisioning was previously required.

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
| `trustedCaCerts` | `array` | Optional list of Secrets containing trusted CA certificates. Each cert is appended to the system CA bundle via an init container. See details below. |

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

#### `spec.security.trustedCaCerts`

Use this to trust custom CA certificates (e.g. for calling SDK services behind an internal load balancer with a private certificate, or for object store access via a private CA) without building a custom Restate image.
The operator adds an init container that concatenates the system CA bundle with your custom certificates, and sets `SSL_CERT_FILE` to point to the combined bundle.

Each entry references a Kubernetes Secret:

| Field | Type | Description |
|---|---|---|
| `secretName` | `string` | **Required**. Name of the Secret containing the CA certificate. |
| `key` | `string` | **Required**. Key within the Secret that contains the PEM-encoded certificate. |

**Example:**

```yaml
spec:
  security:
    trustedCaCerts:
      - secretName: internal-ca
        key: ca.pem
```

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

*   **`auto-provision`**: A boolean that controls whether the Restate node should automatically initialize itself. **When using `cluster.autoProvision: true` (recommended), this must be set to `false`.** If not using operator-managed provisioning, this can be `true` for object-store metadata but must be `false` for `replicated` (Raft) metadata store.

*   **Resource Management**:
    *   `rocksdb-total-memory-size`: Sets the total memory allocated to RocksDB, which Restate uses for its internal state storage. Typically 20-50% of the memory requests for the pod is appropriate.
    *   `admin.query-engine.memory-size`: Allocates memory for the admin service's query engine.

For a complete list of all available options, please refer to the [official Restate Server Configuration Reference](https://docs.restate.dev/references/server_config).

If you don't have access to an object store that supports conditional PUTs for metadata, you can run using the default Raft-based metadata store. The following is an example of a `RestateCluster` configured for Raft metadata without snapshots. Note that running a distributed cluster without snapshots is not recommended as they are used to speed up failover.

```yaml
apiVersion: restate.dev/v1
kind: RestateCluster
metadata:
  name: restate-test
spec:
  cluster:
    autoProvision: true
  compute:
    replicas: 3
    image: restatedev/restate:1.7
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
    # auto-provision must be false when using operator-managed provisioning
    auto-provision = false
    default-num-partitions = 24
    default-replication = 2

    [metadata-client]
    addresses = ["http://restate-cluster:5122/"]
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
  cluster:
    autoProvision: true
  compute:
    replicas: 3
    image: restatedev/restate:1.7
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
    # auto-provision must be false when using operator-managed provisioning
    auto-provision = false
    default-num-partitions = 24
    default-replication = 2

    [metadata-client]
    type = "object-store"
    path = "s3://some-bucket/metadata"
    # the same aws-* parameters as below are supported here

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

The `RestateDeployment` CRD is similar to a standard Kubernetes `Deployment` but is tailored for deploying Restate services. It manages `ReplicaSet` and `Service` objects (or `Configuration` and `Route` objects in Knative mode) for each version of your service, which is crucial for Restate's versioning and draining capabilities. This ensures that old service versions remain available until all in-flight invocations are completed.

#### Deployment Identity

The Restate operator uses **deployment identity** to determine whether to create a new Restate deployment or update an existing one:

**ReplicaSet Mode:**
- Always uses template hash as deployment identity
- Every template change → new Restate deployment (versioned update only)
- Does NOT support in-place updates

**Knative Mode with Explicit Tag:**
- Tag value determines deployment ID
- Same tag → same Restate deployment (in-place update)
- Different tag → new Restate deployment (versioned update)

**Knative Mode without Tag:**
- Uses template hash as tag (auto-versioning)
- Every template change → new tag → new deployment

#### In-Place vs. Versioned Updates

**In-Place Update:**
- Same deployment identity
- Updates implementation without changing deployment ID
- Gradual rollout within the same deployment
- Use for: Bug fixes, config changes, minor updates

**Versioned Update:**
- New deployment identity
- Creates new deployment in Restate
- Multiple deployments coexist temporarily
- Use for: Major versions, breaking changes, parallel testing

#### Knative Serving Mode

RestateDeployment supports [Knative Serving](https://knative.dev/docs/serving/) as an alternative deployment backend. This enables:

- **Scale-to-zero**: Services automatically scale down when idle, saving resources
- **Automatic scaling**: Replicas scale based on concurrent request load
- **In-place updates**: Update service implementation without changing Restate deployment identity
- **Traffic management**: Knative handles gradual rollouts and traffic splitting

##### Prerequisites

- [Knative Serving](https://knative.dev/docs/install/) installed on your cluster
- Network connectivity between the Restate cluster and Knative pods

##### Basic Example

```yaml
apiVersion: restate.dev/v1beta1
kind: RestateDeployment
metadata:
  name: my-service
spec:
  deploymentMode: knative  # Use Knative Serving
  
  knative:
    tag: "v1"        # Explicit tag for deployment identity
    minScale: 0      # Allow scale-to-zero
    maxScale: 10     # Maximum replicas
    target: 50       # Target concurrent requests per replica
  
  template:
    metadata:
      labels:
        app: my-service
    spec:
      containers:
        - name: app
          image: my-registry/my-service:latest
          ports:
            - name: h2c  # Required: Knative only allows "h2c" or "http1"
              containerPort: 9080
  
  restate:
    register:
      cluster: my-cluster
```

##### Tag-Based Versioning

The `knative.tag` field controls how updates are handled:

| Tag Behavior | Result | Use Case |
|--------------|--------|----------|
| **Same tag** | In-place update (new Knative Revision, same Restate deployment) | Bug fixes, config changes |
| **Changed tag** | Versioned update (new Knative Configuration, new Restate deployment) | Breaking changes, major versions |
| **No tag** | Auto-versioning (template hash as tag, every change = new deployment) | Continuous deployment |

##### Knative-Specific Configuration

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `knative.tag` | string | (hash) | Deployment identity tag |
| `knative.minScale` | integer | 0 | Minimum replicas (0 enables scale-to-zero) |
| `knative.maxScale` | integer | unlimited | Maximum replicas |
| `knative.target` | integer | 100 | Target concurrent requests per replica |

##### Port Naming

Knative requires specific port names:
- `h2c` - HTTP/2 cleartext (recommended for gRPC/Restate services)
- `http1` - HTTP/1.1

##### Example Files

See complete examples in [`examples/services/greeter/k8s/`](./examples/services/greeter/k8s/):
- `knative-v1.yaml` - Knative deployment with explicit tag
- `knative-v2.yaml` - Versioned update (new tag)
- `knative-auto.yaml` - Auto-versioning (no tag)

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
| `autoscaling` | `object` | Optional. Per-version `HorizontalPodAutoscaler` template for draining (non-latest) versions. ReplicaSet mode only. See details below. |

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
| `tunnelMode` | `string` | How Restate Cloud reaches this deployment; only takes effect with `register.cloud` (`in-process` requires it, and is not supported in Knative mode). `external` (default): invocations are forwarded to the deployment's Service by the tunnel-client pods managed by the `RestateCloudEnvironment`. `in-process`: the pods hold their own outbound tunnel connections. See [In-process tunnels](#in-process-tunnels). |


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

---

#### `spec.autoscaling`

Optional. ReplicaSet mode only. When set, the operator creates one `HorizontalPodAutoscaler` per **non-latest** version that still has active invocations, so old versions shed compute as their load drains instead of holding their full `replicas` for the entire (potentially multi-hour) drain window. The HPA is removed — and the version then scaled to zero by the operator — once Restate reports the version has no remaining invocations.

The field is a pass-through `HorizontalPodAutoscaler` `.spec`: provide `minReplicas`, `maxReplicas`, `metrics` and optionally `behavior`. The operator injects `scaleTargetRef` per version, so it must be omitted. The HPA is owned by the `RestateDeployment` and garbage-collected with it.

The **latest** version is not covered here — autoscale it as usual with your own `HorizontalPodAutoscaler` targeting the `RestateDeployment` scale subresource.

Notes:
- `minReplicas` is floored at 1 — there is no scale-to-zero in ReplicaSet mode (use Knative mode if you need it).
- CPU/memory metrics require container resource `requests` to be set; prefer CPU (memory does not scale back down).
- If you run the operator with your own RBAC (not the bundled Helm chart), grant it `get,list,watch,create,patch,delete` on `horizontalpodautoscalers` in the `autoscaling` API group.

**On the scaling signal:** CPU (or memory) is a coarse proxy for a draining version's real demand — load on an old version is bursty, and an invocation pinned to it can be suspended (awaiting a timer, call, or external event) and so consume no CPU while still requiring the version to stay available. The ideal trigger would be a per-**deployment** metric exposed by Restate itself — the number of in-flight/pending invocations pinned to that specific Restate deployment (note: *deployment*, i.e. a single registered version, not the whole service) — consumed via the custom/external metrics API and targeted per version. Restate does not expose such a metric today; CPU is a pragmatic default until it does, with the caveat that a concurrency-bound but low-CPU version may under-scale.

```yaml
apiVersion: restate.dev/v1beta1
kind: RestateDeployment
metadata:
  name: greeter
spec:
  replicas: 3
  restate:
    register:
      cluster: restate
  autoscaling:
    minReplicas: 1
    maxReplicas: 10
    metrics:
      - type: Resource
        resource:
          name: cpu
          target:
            type: Utilization
            averageUtilization: 70
  selector:
    matchLabels:
      app: greeter
  template:
    metadata:
      labels:
        app: greeter
    spec:
      containers:
        - name: service
          image: my-greeter:1.0.0
          ports:
            - containerPort: 9080
          resources:
            requests:
              cpu: 100m
```

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

#### In-process tunnels

By default, invocations from Restate Cloud reach your services through the tunnel-client pods
managed by the `RestateCloudEnvironment`, which forward them to each version's `Service`. With
`tunnelMode: in-process`, your pods instead hold their own outbound tunnel connections — for
example with the [`@restatedev/restate-sdk-tunnel`](https://www.npmjs.com/package/@restatedev/restate-sdk-tunnel)
package — so invocations arrive without any inbound networking to the pods, and without
tunnel-client pods on the invocation path.

```yaml
apiVersion: restate.dev/v1beta1
kind: RestateDeployment
metadata:
  name: my-deployment
spec:
  restate:
    register:
      cloud: my-cloud-environment
    tunnelMode: in-process
  template:
    spec:
      containers:
        - name: app
          image: my-restate-service-image:main
```

In this mode the operator injects environment variables into every container of the pod
template (including init containers, so native sidecars are covered), which in-process
tunnel clients use to connect and identify themselves:

| Variable | Value |
|---|---|
| `RESTATE_INPROC_TUNNEL_NAME` | The versioned name of this revision (e.g. `my-deployment-5b8c7d9f4`). Each replica registers its tunnel connections under this name, and the tunnel server load-balances invocations across them. |
| `RESTATE_INPROC_ENVIRONMENT_ID` | The `environmentId` of the referenced `RestateCloudEnvironment`, normalized to its `env_`-prefixed form. |
| `RESTATE_INPROC_CLOUD_REGION` | The `region` of the referenced `RestateCloudEnvironment`. |
| `RESTATE_INPROC_SIGNING_PUBLIC_KEY` | The `signingPublicKey` of the referenced `RestateCloudEnvironment`, used to verify that requests genuinely come from your environment. |

The operator then registers each version under its tunnel URL
(`https://tunnel.<region>.restate.cloud:9080/<environment-id-without-env_-prefix>/<versioned-name>/http/in-process/9080/`)
instead of the Service URL, and versioning, draining and removal work exactly as in the default
mode. Because the tunnel name identifies a revision, a change to any of the injected values
(like a rotated signing key) creates a new version. Unlike a pod template change, an edit to
the `RestateCloudEnvironment` is only picked up on the next periodic reconciliation, so the
new version may take a few minutes to appear.

A few things to be aware of:

- **Credentials are not injected.** Your pods still need a Restate Cloud API key with tunnel
  access to open tunnel connections. Mount one from a `Secret` yourself, e.g. as a file whose
  path you pass to the client (`RESTATE_INPROC_AUTH_TOKEN_FILE` is the conventional variable
  name for file-based tokens, re-read on every reconnect so rotations are picked up).
- **Declaring any of the four injected variables yourself is an error**: the operator refuses
  to reconcile rather than risk pods that disagree with the URL it registers.
  (`RESTATE_INPROC_AUTH_TOKEN_FILE` is yours to set, and values supplied indirectly via
  `envFrom` are silently overridden by the injected entries — `env` takes precedence in
  Kubernetes — rather than rejected.)
- **Deployments are identified by content, not by namespace.** The tunnel name is
  `<name>-<template-hash>`, so two `RestateDeployment`s with the same name and identical
  content (template and Restate configuration) that register against the same environment —
  across namespaces or even clusters — share one tunnel name and load-balance as one
  deployment. That is coherent (same code, same environment, same registered URL), but if you
  want isolation, give them distinct names.
- **Rotate the signing key gracefully.** Old versions keep verifying requests with the key
  they were created with. After a hard key cutover their in-flight invocations can no longer
  be delivered, which also prevents them from draining; let old versions drain before
  decommissioning the old key, or purge their invocations.
- **Registration waits for pod readiness**, but pods become Ready slightly before their first
  tunnel handshake completes; if registration races ahead it is retried after 30 seconds.
  Consider a readiness probe that reflects tunnel establishment if you need tighter rollouts.
- `tunnelMode: in-process` is not supported in Knative mode: the Knative autoscaler sees no
  inbound traffic for tunnel invocations, so scale-to-zero would never scale back up.

### `RestateKafkaIntegration`

The `RestateKafkaIntegration` CRD runs the
[Restate Kafka ingress integration](https://github.com/restatedev/ingress-integration-kafka) --
a standalone container that consumes Kafka topics and turns each record into a Restate
invocation. The operator manages a `Deployment` of it in the custom resource's own namespace,
resolves the Restate ingress URL from the reference you give it, and (for inline configuration)
a `ConfigMap` to hold it.

The CRD is `restate.dev/v1alpha1`; its shape may still change.

Only the Restate destination and its credentials are modelled as fields. Everything else --
the Kafka connection, consumer settings, the record mapper, metrics, retry policy -- is the
container's own configuration surface, documented in
[CONFIGURATION.md](https://github.com/restatedev/ingress-integration-kafka/blob/main/CONFIGURATION.md),
and is passed straight through as a Java `.properties` file. That way a new option upstream is
usable immediately, without waiting for a CRD change.

#### Minimal Example

```yaml
apiVersion: restate.dev/v1alpha1
kind: RestateKafkaIntegration
metadata:
  name: orders-to-restate
spec:
  replicas: 2
  restate:
    ingress:
      cluster: restate
  config: |
    bootstrap.servers=kafka.kafka.svc.cluster.local:9092
    group.id=orders-to-restate
    topics=orders
    restate.record.mapper.service=OrderService
    restate.record.mapper.handler=onKafkaEvent
```

See [`examples/kafka`](./examples/kafka) for a runnable version, including a throwaway Kafka.

#### Spec Fields

| Field | Type | Description |
|---|---|---|
| `restate` | `object` | **Required**. Where to send invocations, and how to authenticate. See details below. |
| `replicas` | `integer` | Number of desired pods. Defaults to `1`. |
| `image` | `string` | Container image. Defaults to the operator's built-in default (override cluster-wide with the `kafkaIntegrationImage` Helm value). |
| `imagePullPolicy` | `string` | One of `Always`, `Never`, `IfNotPresent`. |
| `config` | `string` | Inline `.properties` configuration. Mutually exclusive with `configFrom`. |
| `configFrom` | `object` | Read the `.properties` configuration from a Secret or ConfigMap. Mutually exclusive with `config`. |
| `template` | `object` | Overrides merged over the pod template the operator generates. See details below. |

Throughput scales with replicas up to your topics' partition count -- Kafka distributes
partitions across the whole consumer group, so replicas beyond that sit idle. Note each pod
also runs `restate.kafka.consumer.instances` consumers of its own (default: twice its CPU
count). `kubectl scale rki/<name> --replicas=N` works, as does pointing a
`HorizontalPodAutoscaler` at the resource.

#### `spec.restate`

| Field | Type | Description |
|---|---|---|
| `ingress` | `object` | **Required**. Exactly one of `cluster`, `cloud`, `service` or `url`. |
| `authToken` | `object` | A `{name, key}` reference to a Secret **in this namespace** holding a bearer token, passed to the container as `RESTATE_AUTH_TOKEN`. |

**`ingress` Fields**

| Field | Type | Description |
|---|---|---|
| `cluster` | `string` | The name of a `RestateCluster`; resolves to `http://restate.<cluster>.svc.<clusterDns>:8080`. |
| `cloud` | `string` | The name of a `RestateCloudEnvironment`; resolves to its public ingress. Requires `authToken`. |
| `service` | `object` | A `{name, namespace, port, path}` reference to a Service. `port` defaults to `8080`. |
| `url` | `string` | A Restate ingress URL. |

The operator never reads the `authToken` Secret: it is referenced from the pod spec, so the
kubelet resolves it. This is also why `cloud` needs one rather than reusing the
`RestateCloudEnvironment`'s own credentials, which live in a Secret in the *operator's*
namespace -- copying it into your namespace would mean granting the operator cluster-wide read
access to Secrets.

#### Configuration

The container reads a `.properties` file and lets environment variables override it. The
operator uses both layers:

| Layer | Set by | Contents |
|---|---|---|
| `.properties` file at `/etc/restate-kafka/config.properties` | you | everything: `bootstrap.servers`, `group.id`, `topics`, `sasl.*`, `restate.record.mapper.*`, ... |
| environment variables | the operator | `RESTATE_INGRESS_URL`, `RESTATE_AUTH_TOKEN`, `CONFIG_FILE` |
| `spec.template` | you | anything else, applied last |

Because environment variables win, `restate.ingress.url` and `restate.auth.token` in the
properties file have no effect -- the operator owns the destination.

**`spec.config`** is stored in a `ConfigMap` the operator owns, named after the custom
resource. The pod template carries a digest of it, so editing `spec.config` rolls the pods.

**`spec.configFrom`** takes exactly one of:

| Field | Type | Description |
|---|---|---|
| `secretRef` | `object` | `{name, key}`; `key` defaults to `config.properties`. |
| `configMapRef` | `object` | `{name, key}`; `key` defaults to `config.properties`. |

Use `configFrom` with a Secret as soon as the configuration contains credentials:
`spec.config` is part of the custom resource, so it is stored and displayed in plain text. The
operator does not read the referenced object, so changing its contents does **not** restart the
pods -- run `kubectl rollout restart deployment/<name>`.

#### `spec.template`

A partial pod template, strategic-merged over the one the operator generates. Objects merge
key by key; lists whose Kubernetes merge key the operator knows (`containers`,
`initContainers`, `volumes`, `env`, `volumeMounts`, `ports`, `imagePullSecrets`, `hostAliases`)
merge entry by entry, so you can override one field without restating the rest. Any other list
replaces the generated one, and an explicit `null` removes a generated field.

The operator's container is named `kafka-integration`. Target it by name to set resources,
probes, extra environment variables or volume mounts:

```yaml
  template:
    spec:
      containers:
        - name: kafka-integration
          resources:
            requests: {cpu: 500m, memory: 512Mi}
          env:
            - name: JDK_JAVA_OPTIONS
              value: -XX:MaxRAMPercentage=75
```

The pod labels the operator manages (`app.kubernetes.io/name`, `app.kubernetes.io/instance`)
are re-applied after the merge, because a template whose labels no longer match the
Deployment's immutable selector is rejected outright by the apiserver.

The generated pod deliberately has **no readiness or liveness probe**: the image exposes no
health endpoint, and it exits non-zero on unrecoverable errors (bad configuration, exhausted
reconnect retries), so the restart policy is the real liveness mechanism. Add one via
`spec.template` if you want one.

Rollouts use `maxSurge: 0, maxUnavailable: 1`, since surging would add consumers to the group
before the old ones leave and cost an extra Kafka rebalance.

#### NetworkPolicy

A `RestateCluster` denies **all** inbound traffic to its ingress port unless
`spec.security.networkPeers.ingress` names a peer. The integration dials in to that port, so
without a peer it will never connect -- and the failure looks like a hang, not a rejection.

Add a peer selecting the integration's pods:

```yaml
kind: RestateCluster
spec:
  security:
    networkPeers:
      ingress:
        - namespaceSelector: {}
          podSelector:
            matchLabels:
              app.kubernetes.io/name: restate-kafka-integration
```

The operator also labels the pods `allow.restate.dev/<cluster-name>: "true"`, which is what
the cluster's *egress* policy matches on; that label alone does not open ingress.


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

### Canary Image

Both EKS Pod Identity and GCP Workload Identity use a canary job to validate that credentials are available before
starting the Restate cluster. The trusted CA certs feature also uses this image for its init container.
By default, this uses the `alpine:3.21` image from Docker Hub. The image must include a
CA certificate bundle at `/etc/ssl/certs/ca-certificates.crt` (required by the trusted CA
certs init container) and provide `cat`, `grep` and `wget`. In environments where nodes
cannot pull from Docker Hub (e.g. air-gapped or restricted registries), you can override
this with the `canaryImage` Helm value:

```yaml
canaryImage: my-private-registry.example.com/alpine:3.21
```

The simplest approach is to mirror the default image:

```bash
docker pull alpine:3.21
docker tag alpine:3.21 my-private-registry.example.com/alpine:3.21
docker push my-private-registry.example.com/alpine:3.21
```

### EKS Security Groups for Pods

[EKS Security Groups for Pods](https://docs.aws.amazon.com/eks/latest/userguide/security-groups-for-pods.html) allows
you to isolate pods into separate AWS Security Groups, which is a powerful security primitive which can help you limit
Restate to public IP access, as well as to obtain VPC flow logs.

The operator can create `SecurityGroupPolicy` objects which put Restate pods into a set of Security Groups. If this CRD
is installed, you may provide `awsPodSecurityGroups` in the `RestateCluster` spec.

## Troubleshooting

### DNS Resolution Issues / Pods Cannot Provision or Connect

If your Restate pods fail to start with errors like:
- `dns error caused by: failed to lookup address information`
- `transport error` when connecting to metadata store
- Pods showing `0/1 Running` with repeated restarts
- Nodes unable to connect to each other

This may be caused by the default network policies blocking DNS resolution. The operator creates network policies that allow DNS traffic to:
1. `kube-dns` pods in `kube-system` namespace
2. Node-local DNS cache at `169.254.20.10` (for GKE Autopilot and NodeLocal DNSCache)

If your cluster uses a different DNS configuration, you have two options:

**Option 1: Add custom egress rule** (Recommended)
```yaml
spec:
  security:
    networkEgressRules:
      - ports:
          - port: 53
            protocol: UDP
          - port: 53
            protocol: TCP
        to:
          - ipBlock:
              cidr: <your-dns-server-ip>/32
```

**Option 2: Disable network policies entirely**
```yaml
spec:
  security:
    disableNetworkPolicies: true
```

> **Warning**: Disabling network policies removes network isolation from your Restate cluster. This means any pod in your Kubernetes cluster can reach your Restate pods, and your Restate pods can reach any internal IP address. Only use this option if you have alternative network security measures in place (e.g., AWS Security Groups, Calico policies at the cluster level).

## Releasing

1. Update the version in `charts/restate-operator/Chart.yaml` and the version in `Cargo.{toml,lock}` eg to `0.0.2`
2. Push a new tag `v0.0.2`
3. Accept the draft release once the workflow finishes
