# Local development with Tilt

A [Tilt](https://tilt.dev)-based inner loop for the operator: one command builds
the image into a local registry, keeps the CRDs in sync with the Rust structs,
and installs the Helm chart into a throwaway [kind](https://kind.sigs.k8s.io)
cluster.

## Prerequisites

- `docker`, `kind`, `kubectl`, `helm`, `just`
- [`tilt`](https://docs.tilt.dev/install.html)
- `protoc` (`brew install protobuf`) — the operator's `build.rs` compiles protos, so CRD codegen needs it

## Usage

```bash
./hack/kind-with-registry.sh   # one-time: kind cluster + local registry (localhost:5005)
tilt up                        # build, deploy, watch  (Ctrl-C to stop; resources keep running)
./hack/teardown.sh             # delete the cluster and registry
```

`tilt up` opens a UI at http://localhost:10350 with per-resource logs.

## What Tilt manages

| Resource        | What it does |
|-----------------|--------------|
| `crd-generate`  | Runs `just generate` when a CRD struct under `src/resources/` changes, rewriting `crd/*.yaml`. |
| `crd-apply`     | Server-side-applies the regenerated CRDs — **CRD hot reload**. Server-side apply is required because `restateclusters.yaml` is larger than the 256 KB client-side-apply annotation limit. |
| `restate-operator` | Builds `docker/Dockerfile`, pushes to the local registry, installs the Helm chart (`installCrds=false` — CRDs are handled above), forwards `:8080` for `/health` and `/metrics`. |
| `restate-sample` | A single-node `RestateCluster` (from `hack/dev/restatecluster.yaml`) applied once the operator is ready, giving it something to reconcile. |

## How the local registry is wired

`kind-with-registry.sh` follows the upstream
[kind local-registry recipe](https://kind.sigs.k8s.io/docs/user/local-registry/):
it runs a `registry:2` container on `localhost:5005`, points the kind nodes'
containerd at it, and publishes the KEP-1755 `local-registry-hosting` ConfigMap
so Tilt discovers it. The Tiltfile also sets `default_registry('localhost:5005')`
explicitly.

## Safety

The Tiltfile refuses to run unless the current kube-context is
`kind-restate-operator`, so it can never deploy against the prod (`us`, `eu`) or
`dev` contexts that share this kubeconfig. The `crd-apply` `kubectl` call is
likewise pinned with `--context kind-restate-operator`.

## Editing the operator

Changing operator source triggers a rebuild of `docker/Dockerfile`. The build is
a musl release build via cargo-chef + sccache: the dependency layer is cached, so
after the first build only the operator crate's final layer recompiles. Changing
a CRD struct additionally reruns `crd-generate` → `crd-apply`.
