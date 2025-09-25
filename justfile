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

generate:
  cargo run --bin cluster_crdgen | grep -vF 'categories: []' > crd/restateclusters.yaml
  cargo run --bin deployment_crdgen | grep -vF 'categories: []' > crd/restatedeployments.yaml
  cargo run --bin cloud_crdgen | grep -vF 'categories: []' > crd/restatecloudenvironments.yaml

generate-pkl:
  cargo run --bin cluster_schemagen | pkl eval crd/pklgen/generate-cluster.pkl -m crd
  cargo run --bin deployment_schemagen | pkl eval crd/pklgen/generate-deployment.pkl -m crd
  cargo run --bin cloud_schemagen | pkl eval crd/pklgen/generate-cloud.pkl -m crd

generate-examples:
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
