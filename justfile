export DOCKER_PROGRESS := env_var_or_default('DOCKER_PROGRESS', 'auto')

features := ""
libc := "libc"
arch := "" # use the default architecture
os := "" # use the default os
image := "ghcr.io/restatedev/restate-operator:local"

_features := if features == "all" {
        "--all-features"
    } else if features != "" {
        "--features=" + features
    } else { "" }

_arch := if arch == "" {
        arch()
    } else if arch == "amd64" {
        "x86_64"
    } else if arch == "x86_64" {
        "x86_64"
    } else if arch == "arm64" {
        "aarch64"
    } else if  arch == "aarch64" {
        "aarch64"
    } else {
        error("unsupported arch=" + arch)
    }

_os := if os == "" {
        os()
    } else {
        os
    }

_os_target := if _os == "macos" {
        "apple-darwin"
    } else if _os == "linux" {
        "unknown-linux"
    } else {
        error("unsupported os=" + _os)
    }

_default_target := `rustc -vV | sed -n 's|host: ||p'`
target := _arch + "-" + _os_target + if _os == "linux" { "-" + libc } else { "" }
_resolved_target := if target != _default_target { target } else { "" }
_target-option := if _resolved_target != "" { "--target " + _resolved_target } else { "" }

# Code generation. Everything under crd/ is generated from the Rust structs in src/resources/*.rs.
# Never hand-edit files under crd/ -- always regenerate. See AGENTS.md for the full workflow.

# The pkl CLI version the committed Pkl bindings/examples were generated with. The generator *package*
# is hash-pinned in crd/pklgen/*.pkl, but the pkl CLI that runs it is not, and other versions may emit
# spurious diffs (formatting, type naming). Regenerate with this version, or bump this pin deliberately.
pkl_version := "0.31.1"

# Regenerate every generated artifact after a CRD struct change (CRD YAML, then Pkl, then examples).
# Requires `pkl` on PATH for the pkl/examples steps.
generate-all: generate generate-pkl generate-examples

# Warn (don't fail) if the local pkl CLI differs from the pinned version used to generate the bindings.
_check-pkl:
  @pkl --version | grep -q "{{pkl_version}}" || echo "WARNING: this repo's Pkl artifacts were generated with pkl {{pkl_version}}; you have '$(pkl --version)'. Regenerated output may differ -- see AGENTS.md."

# Regenerate the CRD YAML schemas -- the authoritative contract shipped in the Helm chart.
# Always run and commit this after changing a CRD struct. Pure Rust codegen; no external tools.
generate:
  cargo run --bin cluster_crdgen | grep -vF 'categories: []' > crd/restateclusters.yaml
  cargo run --bin deployment_crdgen | grep -vF 'categories: []' > crd/restatedeployments.yaml
  cargo run --bin cloud_crdgen | grep -vF 'categories: []' > crd/restatecloudenvironments.yaml

# Regenerate the Pkl bindings (a convenience for Pkl users) from the CRD YAML. Requires `pkl`;
# run `just generate` first so the YAML is current. The generator package is pinned by version and
# sha256 in crd/pklgen/*.pkl for reproducibility. Use the pinned pkl CLI (see pkl_version above).
generate-pkl: _check-pkl
  pkl eval crd/pklgen/generate-cluster.pkl -m crd
  pkl eval crd/pklgen/generate-deployment.pkl -m crd
  pkl eval crd/pklgen/generate-cloud.pkl -m crd
  # pkl resolves the relative `source` to an absolute file:// URI in the generated header comment;
  # rewrite it back to the repo-relative form so the committed files stay portable.
  sed -i.bak -E 's#<file://[^ ]*/\./crd/#<file:./crd/#' crd/RestateCluster.pkl crd/RestateDeployment.pkl crd/RestateCloudEnvironment.pkl
  rm -f crd/*.pkl.bak

generate-examples: _check-pkl
  pkl eval crd/examples/restatedeployment.pkl > crd/examples/restatedeployment.yaml
  pkl eval crd/examples/restatecluster.pkl > crd/examples/restatecluster.yaml

install-crds: generate
  kubectl create -f crd/restateclusters.yaml
  kubectl create -f crd/restatedeployments.yaml

# Extract dependencies
chef-prepare:
    cargo chef prepare --recipe-path recipe.json

# Compile dependencies
chef-cook *flags:
    cargo chef cook --recipe-path recipe.json {{ _target-option }} {{ _features }} {{ flags }}

print-target:
    @echo {{ _resolved_target }}

build *flags:
    cargo build {{ _target-option }} {{ _features }} {{ flags }}

docker:
    docker build . -f docker/Dockerfile --tag={{ image }} --progress='{{ DOCKER_PROGRESS }}' --load

fmt:
    cargo fmt --all

lint:
    cargo clippy

check: fmt lint

# Local Tilt dev loop (kind + local registry + CRD hot reload). See hack/README.md.
dev-up:
    ./hack/kind-with-registry.sh

dev-down:
    ./hack/teardown.sh

tilt:
    tilt up
