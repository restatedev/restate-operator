#!/usr/bin/env bash
# Create a kind cluster wired to a local Docker registry, for Tilt-based
# restate-operator development. Adapted from the upstream recipe at
# https://kind.sigs.k8s.io/docs/user/local-registry/ (cluster name and registry
# port changed to match the Tiltfile; 5005 avoids the macOS AirPlay clash on 5000).
set -o errexit

CLUSTER_NAME='restate-operator'
reg_name='kind-registry'
reg_port='5005'

# 1. Create the registry container unless it already exists.
if [ "$(docker inspect -f '{{.State.Running}}' "${reg_name}" 2>/dev/null || true)" != 'true' ]; then
  docker run -d --restart=always -p "127.0.0.1:${reg_port}:5000" --network bridge \
    --name "${reg_name}" registry:2
fi

# 2. Create the kind cluster, pointing containerd at a per-registry certs.d dir.
if ! kind get clusters | grep -qx "${CLUSTER_NAME}"; then
  cat <<EOF | kind create cluster --name "${CLUSTER_NAME}" --config=-
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
containerdConfigPatches:
- |-
  [plugins."io.containerd.grpc.v1.cri".registry]
    config_path = "/etc/containerd/certs.d"
EOF
fi

# 3. Tell each node how to reach the registry by its in-network alias.
REGISTRY_DIR="/etc/containerd/certs.d/localhost:${reg_port}"
for node in $(kind get nodes --name "${CLUSTER_NAME}"); do
  docker exec "${node}" mkdir -p "${REGISTRY_DIR}"
  cat <<EOF | docker exec -i "${node}" cp /dev/stdin "${REGISTRY_DIR}/hosts.toml"
[host."http://${reg_name}:5000"]
EOF
done

# 4. Connect the registry to the kind network so nodes can pull from it.
if [ "$(docker inspect -f='{{json .NetworkSettings.Networks.kind}}' "${reg_name}")" = 'null' ]; then
  docker network connect kind "${reg_name}"
fi

# 5. Document the local registry (KEP-1755) so Tilt auto-detects it.
cat <<EOF | kubectl --context "kind-${CLUSTER_NAME}" apply -f -
apiVersion: v1
kind: ConfigMap
metadata:
  name: local-registry-hosting
  namespace: kube-public
data:
  localRegistryHosting.v1: |
    host: "localhost:${reg_port}"
    help: "https://kind.sigs.k8s.io/docs/user/local-registry/"
EOF

echo
echo "kind cluster '${CLUSTER_NAME}' + registry 'localhost:${reg_port}' ready."
echo "kube-context: kind-${CLUSTER_NAME}"
echo "Next: tilt up"
