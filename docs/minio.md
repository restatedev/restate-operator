# MinIO Configuration Example

To configure a `RestateCluster` to send snapshots to self-hosted S3-compatible object store like [MinIO](https://min.io/), you can point the server to your MinIO instance. For security, it's best to create a dedicated service account with credentials scoped only to the buckets Restate needs.

> ⚠️ **Supported object stores for metadata:** Only AWS S3 is currently tested and supported as a metadata backend.
  MinIO cannot be used for metadata as it breaks certain consistency properties when read quorum is lost.
  MinIO should only be used for storing snapshots, which have no consistency requirements.

## 1. Create a Scoped Access Key & Buckets

First, we'll define a policy, create the buckets, and then create a service account with a new access key that is restricted by that policy.

The following commands use the `mc` client to set up your MinIO instance. They assume you have port-forwarded your MinIO service.

```bash
# Forward the MinIO service to your local machine
kubectl port-forward --namespace storage svc/minio 9000:443

# Alias your MinIO deployment for easier access
# Use https:// and --insecure if connecting to a port-forwarded TLS port (like 443)
mc alias set local-minio https://localhost:9000 YOUR_ADMIN_ACCESS_KEY YOUR_ADMIN_SECRET_KEY --insecure

# Create the buckets
mc mb --insecure local-minio/restate-metadata
mc mb --insecure local-minio/restate-snapshots

# Add the new policy to MinIO

cat <<EOF | mc admin policy add local-minio restate-s3-policy
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": [
        "s3:ListBucket"
      ],
      "Resource": [
        "arn:aws:s3:::restate-snapshots"
      ]
    },
    {
      "Effect": "Allow",
      "Action": [
        "s3:PutObject",
        "s3:GetObject",
      ],
      "Resource": [
        "arn:aws:s3:::restate-snapshots/*"
      ]
    }
  ]
}
EOF

# Create a new service account for the Restate application
# This command will output a new AccessKey and SecretKey.
mc admin service-account add local-minio restate

# Attach the policy to the new service account
mc admin policy set local-minio restate-s3-policy service-account=restate
```

When you run `mc admin service-account add`, it will output a new `AccessKey` and `SecretKey`. **Save these securely**, as you will use them in the next step.

### 2. Create a Kubernetes Secret

Next, create a Kubernetes `Secret` containing the new, **scoped credentials** you just generated for the `restate` service account.

**Important**: This secret must be created in the namespace where the cluster will run, which is the same as the `metadata.name` of your `RestateCluster`. For this example, the namespace is `restate-with-minio`.

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: minio-credentials
  namespace: restate-with-minio
type: Opaque
stringData:
  # Use the keys generated from the 'mc admin service-account add' command
  access-key: YOUR_NEW_RESTATE_ACCESS_KEY
  secret-key: YOUR_NEW_RESTATE_SECRET_KEY
```

##### 3. Configure the `RestateCluster`

Finally, define your `RestateCluster` resource. This manifest is the same as before, but it will now use the Kubernetes secret containing the limited-permission keys.
Additionally, we add the MinIO namespace to the allowed list of egress peers.

```yaml
apiVersion: restate.dev/v1
kind: RestateCluster
metadata:
  name: restate-minio-test
spec:
  clusterProvisioning:
    enabled: true
  compute:
    replicas: 3
    image: restatedev/restate:1.5
    env:
    - name: RESTATE_WORKER__SNAPSHOTS__AWS_ACCESS_KEY_ID
      valueFrom:
        secretKeyRef:
          name: minio-credentials
          key: access-key
    - name: RESTATE_WORKER__SNAPSHOTS__AWS_SECRET_ACCESS_KEY
      valueFrom:
        secretKeyRef:
          name: minio-credentials
          key: secret-key
  security:
    networkEgressRules:
    - to:
      - namespaceSelector:
          matchLabels:
            kubernetes.io/metadata.name: minio-namespace
  storage:
    storageRequestBytes: 2147483648 # 2 GiB
  config: |
    roles = [ "worker", "admin", "log-server", "metadata-server" , "http-ingress" ]
    # auto-provision must be false when using operator-managed provisioning
    auto-provision = false
    default-num-partitions = 24
    default-replication = 2

    [metadata-server]
    type = "replicated"

    [metadata-client]
    addresses = ["http://restate-cluster:5122/"]

    [bifrost]
    default-provider = "replicated"

    [worker.snapshots]
    destination = "s3://restate-snapshots/snapshots"
    snapshot-interval-num-records = 10000
    aws-endpoint-url = "http://minio.minio-namespace.svc.cluster.local:9000"
    aws-allow-http = true
    aws-region = "local"
```
