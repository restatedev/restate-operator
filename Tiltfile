# -*- mode: Python -*-
#
# Local development for restate-operator.
#
#   ./hack/kind-with-registry.sh   # one-time: kind cluster + local registry
#   tilt up                        # build, deploy, watch
#   ./hack/teardown.sh             # tear it all down
#
# What Tilt drives:
#   - builds the operator image (docker/Dockerfile) and pushes it to the local
#     registry at localhost:5005,
#   - regenerates the CRD YAML from the Rust structs (src/resources/*.rs) and
#     server-side-applies it whenever a struct changes -- CRD hot reload,
#   - installs the Helm chart with installCrds=false (CRDs are managed above).
#
# See hack/README.md for the full walkthrough.

KIND_CLUSTER = 'restate-operator'
K8S_CONTEXT = 'kind-' + KIND_CLUSTER
REGISTRY = 'localhost:5005'
IMAGE = 'ghcr.io/restatedev/restate-operator'
OPERATOR_NS = 'restate-operator'

# Safety: `tilt up` deploys to whatever the current kube-context points at. This
# repo's kubeconfig also carries live prod contexts (us, eu, dev), so refuse to
# run against anything but the local kind cluster.
if k8s_context() != K8S_CONTEXT:
    fail(("current kube-context is %r, expected %r.\n" +
          "Run ./hack/kind-with-registry.sh first -- it creates the cluster " +
          "and switches context.") % (k8s_context(), K8S_CONTEXT))
allow_k8s_contexts(K8S_CONTEXT)

default_registry(REGISTRY)

# --- Namespace --------------------------------------------------------------
k8s_yaml(blob("""
apiVersion: v1
kind: Namespace
metadata:
  name: %s
""" % OPERATOR_NS))

# --- CRDs: regenerate from Rust structs, then server-side apply --------------
# `just generate` runs the *_crdgen bins and rewrites crd/*.yaml; editing any CRD
# struct retriggers it, and crd-apply then updates the CRDs in the cluster.
CRD_FILES = [
    'crd/restateclusters.yaml',
    'crd/restatedeployments.yaml',
    'crd/restatecloudenvironments.yaml',
]

local_resource(
    'crd-generate',
    # build.rs compiles protos, so codegen needs protoc on PATH (CI installs it).
    cmd="command -v protoc >/dev/null || { echo 'protoc not found -- run: brew install protobuf'; exit 1; }; just generate",
    # CRD schemas only change when the resource structs (and their shared modules)
    # do; watching src/resources keeps this from recompiling on every code edit.
    deps=['src/resources', 'build.rs'],
    labels=['crds'],
)

# restateclusters.yaml is ~350KB -- past the 256KB last-applied-configuration
# annotation limit -- so it needs server-side apply, not Tilt's default
# client-side apply. --context pins it to the kind cluster.
local_resource(
    'crd-apply',
    cmd='kubectl --context %s apply --server-side -f %s' % (
        K8S_CONTEXT, ' -f '.join(CRD_FILES)),
    deps=CRD_FILES,
    resource_deps=['crd-generate'],
    labels=['crds'],
)

# --- Operator image + Helm chart --------------------------------------------
# Mirrors `just docker`. The multi-stage build uses cargo-chef + sccache, so
# after the first build only the operator crate's final layer recompiles.
docker_build(
    IMAGE,
    context='.',
    dockerfile='docker/Dockerfile',
)

k8s_yaml(helm(
    'charts/restate-operator-helm',
    name='restate-operator',
    namespace=OPERATOR_NS,
    set=[
        'installCrds=false',       # CRDs are managed by crd-generate / crd-apply
        'version=tilt',            # Tilt overrides the tag with the built image
        'logging.env_filter=info,restate=debug',
    ],
))

k8s_resource(
    'restate-operator',
    resource_deps=['crd-apply'],
    port_forwards=['8080:8080'],   # /health, /metrics, diagnostics
    labels=['operator'],
)

# --- A sample RestateCluster for the operator to reconcile ------------------
# Applied on `tilt up`, once the operator (and its CRDs) are ready.
k8s_yaml('hack/dev/restatecluster.yaml')
k8s_resource(
    new_name='restate-sample',
    objects=['restate:restatecluster'],
    resource_deps=['restate-operator'],
    labels=['sample'],
)
