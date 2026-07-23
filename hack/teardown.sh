#!/usr/bin/env bash
# Tear down the kind cluster and local registry created by kind-with-registry.sh.
set -o errexit

CLUSTER_NAME='restate-operator'
reg_name='kind-registry'

kind delete cluster --name "${CLUSTER_NAME}"

if [ "$(docker inspect -f '{{.State.Running}}' "${reg_name}" 2>/dev/null || true)" = 'true' ]; then
  docker rm -f "${reg_name}"
fi

echo "Torn down kind cluster '${CLUSTER_NAME}' and registry '${reg_name}'."
