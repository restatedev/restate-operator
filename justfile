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

# Show available commands
default:
    @just --list

# Run all tests
test *flags:
    cargo test {{ _target-option }} {{ _features }} {{ flags }}

# Run only unit tests
test-unit:
    cargo test {{ _target-option }} {{ _features }} unit_tests

# Run tests with coverage
test-coverage:
    cargo test {{ _target-option }} {{ _features }} --all-features

# Run integration tests (requires envtest setup)
test-integration:
    cargo test {{ _target-option }} {{ _features }} --ignored

# Check code formatting
check-fmt:
    cargo fmt --check

# Format code
fmt:
    cargo fmt

# Run clippy linter
clippy *flags:
    cargo clippy {{ _target-option }} {{ _features }} {{ flags }}

# Run all checks (fmt, clippy, test)
check: check-fmt clippy test

# Clean build artifacts
clean:
    cargo clean

# Run the operator locally (requires KUBECONFIG)
run *flags:
    cargo run {{ _target-option }} {{ _features }} {{ flags }}

# Watch for changes and rebuild
watch:
    cargo watch -x 'build {{ _target-option }} {{ _features }}'

# Watch for changes and run tests
watch-test:
    cargo watch -x 'test {{ _target-option }} {{ _features }}'

generate:
  cargo run --bin cluster_crdgen | grep -vF 'categories: []' > crd/restateclusters.yaml
  cargo run --bin deployment_crdgen | grep -vF 'categories: []' > crd/restatedeployments.yaml

generate-pkl:
  cargo run --bin cluster_schemagen | pkl eval crd/pklgen/generate-cluster.pkl -m crd
  cargo run --bin deployment_schemagen | pkl eval crd/pklgen/generate-deployment.pkl -m crd

generate-examples:
  pkl eval crd/examples/restatedeployment.pkl > crd/examples/restatedeployment.yaml
  pkl eval crd/examples/restatecluster.pkl > crd/examples/restatecluster.yaml

install-crds: generate
  kubectl create -f crd/restateclusters.yaml
  kubectl create -f crd/restatedeployments.yaml

# Apply CRDs to cluster (create or update)
apply-crds: generate
  kubectl apply -f crd/restateclusters.yaml
  kubectl apply -f crd/restatedeployments.yaml

# Delete CRDs from cluster
delete-crds:
  kubectl delete -f crd/restateclusters.yaml --ignore-not-found
  kubectl delete -f crd/restatedeployments.yaml --ignore-not-found

# Deploy operator to current kubectl context
deploy: docker
  kubectl apply -f charts/restate-operator-helm/templates/

# Undeploy operator from current kubectl context  
undeploy:
  kubectl delete -f charts/restate-operator-helm/templates/ --ignore-not-found

# Generate Helm manifests for inspection
helm-template:
  helm template restate-operator charts/restate-operator-helm/ --namespace restate-operator --create-namespace

# Generate Helm manifests to file
helm-manifests:
  helm template restate-operator charts/restate-operator-helm/ --namespace restate-operator --create-namespace > restate-operator-manifests.yaml

# Split Helm manifests into individual files
helm-split: helm-manifests
  yq eval --split-exp '(.kind | downcase) + "_" + .metadata.name + ".yaml"' restate-operator-manifests.yaml

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

# Build and push Docker image
docker-push: docker
    docker push {{ image }}

# Create a development cluster with kind
kind-create:
    kind create cluster --name restate-operator-dev

# Delete the development cluster
kind-delete:
    kind delete cluster --name restate-operator-dev

# Load Docker image into kind cluster
kind-load: docker
    kind load docker-image {{ image }} --name restate-operator-dev

# Full development setup: create cluster, load image, apply CRDs
dev-setup: kind-create kind-load apply-crds
    @echo "Development cluster ready!"

# Clean up development environment
dev-cleanup: undeploy delete-crds kind-delete
    @echo "Development environment cleaned up!"

# Show cluster info and resources
status:
    @echo "=== Cluster Info ==="
    kubectl cluster-info
    @echo "\n=== RestateCluster Resources ==="
    kubectl get restateclusters --all-namespaces || echo "No RestateCluster resources found"
    @echo "\n=== RestateDeployment Resources ==="  
    kubectl get restatedeployments --all-namespaces || echo "No RestateDeployment resources found"
    @echo "\n=== Operator Pods ==="
    kubectl get pods -l app.kubernetes.io/name=restate-operator --all-namespaces || echo "No operator pods found"
