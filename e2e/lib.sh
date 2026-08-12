# shellcheck shell=bash
#
# Shared harness for the operator's e2e scripts: a disposable kind cluster with the CRDs,
# the operator built from this checkout, a single-node RestateCluster, and the example
# "greeter" service — plus assertion/diagnostic helpers.
#
# Source this, then call only the setup steps your scenario needs (metrics-server, for
# instance, is only interesting to the autoscaling script). Override any of the config
# below by setting it *before* sourcing.
#
# Prerequisites on PATH: kind, kubectl, helm, docker, jq, just, curl.

# ---- config ---------------------------------------------------------------
CLUSTER_NAME="${CLUSTER_NAME:-restate-operator-e2e}"
OPERATOR_NS="${OPERATOR_NS:-restate-operator}"
RESTATE_NS="${RESTATE_NS:-restate}"      # namespace the operator creates for the cluster
APP_NS="${APP_NS:-default}"              # namespace for the RestateDeployment
OPERATOR_IMAGE="${OPERATOR_IMAGE:-ghcr.io/restatedev/restate-operator:local}"
GREETER_IMAGE="${GREETER_IMAGE:-dev.local/restatedev/restate-operator/greeter:local}"
# In-cluster Restate ports.
CLUSTER_INGRESS_PORT=8080
CLUSTER_ADMIN_PORT=9070
# Local ports for our port-forward. Deliberately uncommon so we don't collide with (and
# silently hit) an unrelated Restate port-forward — e.g. a Restate Cloud control plane
# forwarded to 8080/9070. Override if these are taken.
INGRESS_PORT="${INGRESS_PORT:-18080}"
ADMIN_PORT="${ADMIN_PORT:-19070}"
REPO_ROOT="${REPO_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
CTX="kind-${CLUSTER_NAME}"

# ---- output helpers -------------------------------------------------------
RED=$'\e[31m'; GREEN=$'\e[32m'; YELLOW=$'\e[33m'; BLUE=$'\e[34m'; BOLD=$'\e[1m'; RST=$'\e[0m'
info()  { echo "${BLUE}==>${RST} $*"; }
step()  { echo; echo "${BOLD}### $*${RST}"; }
pass()  { echo "${GREEN}PASS${RST} $*"; PASSES=$((PASSES+1)); }
fail()  { echo "${RED}FAIL${RST} $*"; FAILS=$((FAILS+1)); }
warn()  { echo "${YELLOW}WARN${RST} $*"; }
die()   { echo "${RED}fatal:${RST} $*" >&2; exit 1; }
PASSES=0; FAILS=0; PF_PID=""

kc() { kubectl --context "$CTX" "$@"; }

# Poll predicate (a command/function) until it succeeds or `timeout` elapses.
# The predicate runs in the current shell, so it can use helpers and variables.
wait_until() {
  local desc="$1" timeout="$2"; shift 2
  local deadline=$(( $(date +%s) + timeout ))
  while true; do
    if "$@" >/dev/null 2>&1; then return 0; fi
    if (( $(date +%s) >= deadline )); then
      warn "timed out after ${timeout}s waiting for: ${desc}"
      return 1
    fi
    sleep 3
  done
}

# Elapsed seconds around a command, printed on stdout. Used to assert that a forced
# teardown really did skip the drain delay rather than merely finishing eventually.
time_it() {
  local start; start=$(date +%s)
  "$@" >/dev/null 2>&1 || true
  echo $(( $(date +%s) - start ))
}

e2e_trap_cleanup() {
  [[ -n "$PF_PID" ]] && kill "$PF_PID" >/dev/null 2>&1 || true
  if [[ "${KEEP:-0}" != "1" ]]; then
    info "deleting kind cluster ${CLUSTER_NAME} (set KEEP=1 to keep it)"
    kind delete cluster --name "$CLUSTER_NAME" >/dev/null 2>&1 || true
  else
    info "KEEP=1 — leaving cluster ${CLUSTER_NAME} up"
  fi
}

# ---- setup steps ----------------------------------------------------------
require_tools() {
  step "Preflight"
  local bin
  for bin in kind kubectl helm docker jq just curl; do
    command -v "$bin" >/dev/null 2>&1 || die "missing required tool: $bin"
  done
  info "all tools present"
}

create_kind_cluster() {
  step "Create kind cluster"
  # Reuse an existing cluster only if it is actually reachable. kind can leave a cluster
  # listed while its kubeconfig context is gone (e.g. after a partial delete); in that
  # case recreate it rather than failing later.
  if kind get clusters 2>/dev/null | grep -qx "$CLUSTER_NAME"; then
    kind export kubeconfig --name "$CLUSTER_NAME" >/dev/null 2>&1 || true
    if kc cluster-info >/dev/null 2>&1; then
      info "cluster ${CLUSTER_NAME} healthy, reusing"
      return 0
    fi
    warn "cluster ${CLUSTER_NAME} is listed but unreachable; recreating"
    kind delete cluster --name "$CLUSTER_NAME" >/dev/null 2>&1 || true
  fi
  kind create cluster --name "$CLUSTER_NAME"
}

install_metrics_server() {
  step "Install metrics-server (kind-patched)"
  kc apply -f https://github.com/kubernetes-sigs/metrics-server/releases/latest/download/components.yaml
  # kind kubelets serve metrics over a self-signed cert; allow it (idempotently — don't
  # append the flag twice when reusing a cluster).
  if ! kc -n kube-system get deploy metrics-server \
       -o jsonpath='{.spec.template.spec.containers[0].args}' 2>/dev/null | grep -q kubelet-insecure-tls; then
    kc -n kube-system patch deployment metrics-server --type=json \
      -p '[{"op":"add","path":"/spec/template/spec/containers/0/args/-","value":"--kubelet-insecure-tls"}]'
  fi
  kc -n kube-system rollout status deployment/metrics-server --timeout=180s
}

install_crds() {
  step "Install CRDs"
  kc apply --server-side -f "${REPO_ROOT}/crd/restateclusters.yaml"
  kc apply --server-side -f "${REPO_ROOT}/crd/restatedeployments.yaml"
}

build_and_load_images() {
  if [[ "${SKIP_BUILD:-0}" == "1" ]]; then
    info "SKIP_BUILD=1 — reusing already-loaded images"
    return 0
  fi
  step "Build + load operator image"
  ( cd "$REPO_ROOT" && just docker )
  kind load docker-image "$OPERATOR_IMAGE" --name "$CLUSTER_NAME"

  step "Build + load greeter image"
  ( cd "$REPO_ROOT/examples/services/greeter" && just docker )
  kind load docker-image "$GREETER_IMAGE" --name "$CLUSTER_NAME"
}

deploy_operator() {
  step "Deploy operator"
  helm --kube-context "$CTX" upgrade --install restate-operator \
    "${REPO_ROOT}/charts/restate-operator-helm" \
    --namespace "$OPERATOR_NS" --create-namespace \
    --set version=local --wait --timeout 180s
  # force a fresh pod so a rebuilt :local image is picked up
  kc -n "$OPERATOR_NS" rollout restart deployment -l app=restate-operator >/dev/null 2>&1 || true
  kc -n "$OPERATOR_NS" rollout status deployment -l app=restate-operator --timeout=120s
}

deploy_restate_cluster() {
  step "Deploy + provision RestateCluster"
  kc apply --server-side -f "${REPO_ROOT}/examples/cluster/cluster.yaml"
  wait_until "restate-0 pod to exist" 180 kc -n "$RESTATE_NS" get pod restate-0
  kc -n "$RESTATE_NS" wait --for=condition=Ready pod/restate-0 --timeout=240s \
    || die "restate-0 did not become ready"
  # single-node clusters must be provisioned once; ignore 'already provisioned'.
  kc -n "$RESTATE_NS" exec restate-0 -- restatectl provision --yes >/dev/null 2>&1 \
    || kc -n "$RESTATE_NS" exec restate-0 -- restatectl provision >/dev/null 2>&1 \
    || warn "provision returned non-zero (may already be provisioned)"
}

port_forward_restate() {
  step "Port-forward Restate ingress + admin"
  kc -n "$RESTATE_NS" port-forward svc/restate \
    "${INGRESS_PORT}:${CLUSTER_INGRESS_PORT}" "${ADMIN_PORT}:${CLUSTER_ADMIN_PORT}" \
    >/tmp/restate-pf.log 2>&1 &
  PF_PID=$!
  sleep 2
  # Fail loudly if the forward could not bind — otherwise a stale listener on these ports
  # (e.g. another Restate) would silently answer our health check and curls.
  if ! kill -0 "$PF_PID" 2>/dev/null || grep -q "address already in use" /tmp/restate-pf.log; then
    cat /tmp/restate-pf.log >&2
    die "port-forward to Restate failed (ports ${INGRESS_PORT}/${ADMIN_PORT} in use?). Set INGRESS_PORT/ADMIN_PORT to free ports."
  fi
  wait_until "admin API reachable" 60 admin_healthy || die "admin API never came up"
}

# ---- kubernetes queries ---------------------------------------------------
rs_replicas() { kc -n "$APP_NS" get rs "$1" -o jsonpath='{.spec.replicas}' 2>/dev/null; }

# ---- admin API ------------------------------------------------------------
admin_healthy() { curl -sf "localhost:${ADMIN_PORT}/health" >/dev/null 2>&1; }

# Run a SQL query against the admin API, echoing the raw JSON response.
admin_query() {
  curl -sS "localhost:${ADMIN_PORT}/query" \
    -H 'content-type: application/json' -H 'accept: application/json' \
    -d "{\"query\": $(jq -Rs . <<<"$1")}" 2>/dev/null
}

# True while any non-completed invocation against Greeter exists.
greeter_has_active_invocations() {
  admin_query "SELECT count(*) AS c FROM sys_invocation_status WHERE status != 'completed' AND target_service_name = 'Greeter'" \
    | jq -e '.rows[0].c > 0' >/dev/null 2>&1
}
greeter_drained() { ! greeter_has_active_invocations; }

# True if Restate still has a registration for this deployment id.
deployment_registered() {
  admin_query "SELECT count(*) AS c FROM sys_deployment WHERE id = '$1'" \
    | jq -e '.rows[0].c > 0' >/dev/null 2>&1
}
deployment_deregistered() { ! deployment_registered "$1"; }

# ---- invocation helpers ---------------------------------------------------
# Fire a long async invocation, pinning it to whatever version is latest now. Returns
# immediately (Restate ingress /send) and echoes the invocation id.
slow_greet() {
  local resp id
  resp="$(curl -sS "localhost:${INGRESS_PORT}/Greeter/slowGreet/send" \
    -H 'content-type: application/json' \
    -d "{\"name\":\"e2e\",\"delaySeconds\":${1}}" 2>&1)"
  id="$(jq -r '.invocationId // empty' <<<"$resp" 2>/dev/null)"
  if [[ -z "$id" ]]; then warn "slowGreet /send rejected: ${resp}"; return 1; fi
  echo "$id"
}

# ---- diagnostics / results ------------------------------------------------
operator_logs() { kc -n "$OPERATOR_NS" logs -l app=restate-operator --tail="${1:-200}" 2>/dev/null; }

# Dump operator logs + resource state; call when an assertion fails, so a failing run is
# debuggable without KEEP=1.
dump_diagnostics() {
  echo "${YELLOW}--- diagnostics ---${RST}"
  echo "  ### RestateDeployment / ReplicaSets / HPAs / pods (${APP_NS})"
  kc -n "$APP_NS" get restatedeployment,rs,hpa,pods -o wide 2>&1 | sed 's/^/  /' || true
  echo "  ### operator logs (tail 80)"
  operator_logs 80 2>&1 | sed 's/^/  /' || true
}

e2e_results() {
  step "Results"
  echo "  ${GREEN}${PASSES} passed${RST}, ${RED}${FAILS} failed${RST}"
  (( FAILS == 0 )) || { dump_diagnostics; die "e2e had failures"; }
  info "all assertions passed"
}
