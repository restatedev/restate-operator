#!/usr/bin/env bash
#
# e2e driver for rolling a RestateDeployment back to a previously registered
# revision. See restatedev/restate-operator#174.
#
# The operator names ReplicaSets and Services by a content hash of the pod
# template, so v1 -> v2 -> v1 re-adopts the original v1 ReplicaSet, Service URL
# and Restate deployment id. Restoring the Kubernetes revision is therefore not
# enough on its own: Restate must also be told that the restored deployment is
# once again the one new invocations go to.
#
# Everything here asserts Restate's own view -- the service-to-deployment
# mapping from GET /services/Greeter, and the version reported by an actual
# invocation -- never the operator's status or the deployment id echoed back by
# registration. Those are exactly the signals that look healthy while routing is
# wrong, which is the bug.
#
# Restate version matters. The unversioned /deployments path resolves to
# AdminApiVersion::Unknown, where `force` defaults to *false* (restatedev/restate
# #3859, first released in v1.6.0). On such a server a re-registration of an
# unchanged endpoint returns 200 "Unchanged" with the existing id and does not
# make it latest. Pin at or above 1.6 or these scenarios cannot fail.
#
# Prerequisites on PATH: kind, kubectl, helm, docker, jq, just, curl.
#
# Usage:
#   e2e/rollback.sh                     # full run, tears the cluster down at the end
#   KEEP=1 e2e/rollback.sh              # keep the cluster for inspection
#   SKIP_BUILD=1 e2e/rollback.sh        # reuse already-loaded images
#   ONLY=clean|pinned-old|pinned-new e2e/rollback.sh
#
set -euo pipefail

# ---- config ---------------------------------------------------------------
CLUSTER_NAME="${CLUSTER_NAME:-restate-rollback-e2e}"
OPERATOR_NS="${OPERATOR_NS:-restate-operator}"
RESTATE_NS="${RESTATE_NS:-restate}"
APP_NS="${APP_NS:-default}"
RD_NAME="${RD_NAME:-greeter-rsd}"
OPERATOR_IMAGE="ghcr.io/restatedev/restate-operator:local"
GREETER_IMAGE="dev.local/restatedev/restate-operator/greeter:local"
RESTATE_IMAGE="${RESTATE_IMAGE:-restatedev/restate:1.7}"
# How long the invocation that pins a version stays in flight. Must outlast the
# rollback plus a few reconciles, or the scenario stops testing what it says.
PIN_SECONDS="${PIN_SECONDS:-120}"
DRAIN_DELAY="${DRAIN_DELAY:-10}"
CLUSTER_INGRESS_PORT=8080
CLUSTER_ADMIN_PORT=9070
# Deliberately uncommon local ports so we don't silently hit an unrelated
# Restate port-forward (e.g. a Cloud control plane on 8080/9070).
INGRESS_PORT="${INGRESS_PORT:-18081}"
ADMIN_PORT="${ADMIN_PORT:-19071}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
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

cleanup() {
  [[ -n "$PF_PID" ]] && kill "$PF_PID" >/dev/null 2>&1 || true
  if [[ "${KEEP:-0}" != "1" ]]; then
    info "deleting kind cluster ${CLUSTER_NAME} (set KEEP=1 to keep it)"
    kind delete cluster --name "$CLUSTER_NAME" >/dev/null 2>&1 || true
  else
    info "KEEP=1 -- leaving cluster ${CLUSTER_NAME} up"
  fi
}
trap cleanup EXIT

# ---- Restate-side queries (the assertions that matter) --------------------
# Every query below swallows its own failure and returns the empty string. They
# run under `set -euo pipefail` inside command substitutions, so a transient
# curl/kubectl blip would otherwise abort the entire run rather than failing a
# single assertion -- and most of them are called from `wait_until`, where
# "not yet" and "call failed" should behave identically.
#
# Which deployment does Restate send NEW invocations of Greeter to? This is the
# `active_service_revisions` index -- the thing a rollback has to move, and the
# thing a 200 "Unchanged" registration leaves untouched.
latest_deployment_id() {
  curl -sS "localhost:${ADMIN_PORT}/services/Greeter" 2>/dev/null \
    | jq -r '.deployment_id // empty' 2>/dev/null || true
}
latest_is() { [[ "$(latest_deployment_id)" == "$1" ]]; }

# Actually invoke, and report which version answered. Registration metadata can
# agree while routing does not; this is the ground truth.
greet_version() {
  curl -sS "localhost:${INGRESS_PORT}/Greeter/greet" \
    -H 'content-type: application/json' -d '"probe"' 2>/dev/null \
    | jq -r '.version // empty' 2>/dev/null || true
}
greet_version_is() { [[ "$(greet_version)" == "$1" ]]; }

admin_healthy() { curl -sf "localhost:${ADMIN_PORT}/health" >/dev/null 2>&1; }

# ---- Kubernetes-side queries ----------------------------------------------
# Find a version's ReplicaSet by the SERVICE_VERSION it was built with, rather
# than by recomputing the operator's content hash -- the test should not have to
# agree with the implementation on how names are derived.
rs_for_version() {
  kc -n "$APP_NS" get rs -l "restate.dev/owned-by=${RD_NAME}" -o json 2>/dev/null \
    | jq -r --arg v "$1" '
        .items[]
        | select([.spec.template.spec.containers[].env[]?
                  | select(.name == "SERVICE_VERSION") | .value] | index($v))
        | .metadata.name' 2>/dev/null \
    | head -n1 || true
}
rs_annotation() {
  kc -n "$APP_NS" get rs "$1" -o jsonpath="{.metadata.annotations.restate\\.dev/$2}" 2>/dev/null || true
}
svc_annotation() {
  kc -n "$APP_NS" get svc "$1" -o jsonpath="{.metadata.annotations.restate\\.dev/$2}" 2>/dev/null || true
}
rs_replicas()    { kc -n "$APP_NS" get rs "$1" -o jsonpath='{.spec.replicas}' 2>/dev/null || true; }
rs_ready()       { kc -n "$APP_NS" get rs "$1" -o jsonpath='{.status.readyReplicas}' 2>/dev/null || true; }
rs_replicas_is() { [[ "$(rs_replicas "$1")" == "$2" ]]; }
rs_ready_is()    { [[ "$(rs_ready "$1")" == "$2" ]]; }
rd_ready() {
  kc -n "$APP_NS" get restatedeployment "$RD_NAME" \
    -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}' 2>/dev/null | grep -qx True
}
rd_status_field() {
  kc -n "$APP_NS" get restatedeployment "$RD_NAME" -o jsonpath="{.status.$1}" 2>/dev/null || true
}

dump_diagnostics() {
  echo "${YELLOW}--- diagnostics ---${RST}"
  echo "  ### RestateDeployment / ReplicaSets / pods (${APP_NS})"
  kc -n "$APP_NS" get restatedeployment,rs,pods -o wide 2>&1 | sed 's/^/  /' || true
  echo "  ### RestateDeployment status"
  kc -n "$APP_NS" get restatedeployment "$RD_NAME" -o jsonpath='{.status}' 2>&1 \
    | jq . 2>/dev/null | sed 's/^/  /' || true
  echo "  ### Restate services"
  curl -sS "localhost:${ADMIN_PORT}/services" 2>&1 | jq . 2>/dev/null | sed 's/^/  /' || true
  echo "  ### Restate deployments"
  curl -sS "localhost:${ADMIN_PORT}/deployments" 2>&1 | jq . 2>/dev/null | sed 's/^/  /' || true
  echo "  ### operator logs (tail 100)"
  kc -n "$OPERATOR_NS" logs -l app=restate-operator --tail=100 2>&1 | sed 's/^/  /' || true
}

# ---- preflight ------------------------------------------------------------
step "Preflight"
for bin in kind kubectl helm docker jq just curl; do
  command -v "$bin" >/dev/null 2>&1 || die "missing required tool: $bin"
done
info "all tools present"

# ---- cluster --------------------------------------------------------------
step "Create kind cluster"
if kind get clusters 2>/dev/null | grep -qx "$CLUSTER_NAME"; then
  kind export kubeconfig --name "$CLUSTER_NAME" >/dev/null 2>&1 || true
  if kc cluster-info >/dev/null 2>&1; then
    info "cluster ${CLUSTER_NAME} healthy, reusing"
  else
    warn "cluster ${CLUSTER_NAME} is listed but unreachable; recreating"
    kind delete cluster --name "$CLUSTER_NAME" >/dev/null 2>&1 || true
    kind create cluster --name "$CLUSTER_NAME"
  fi
else
  kind create cluster --name "$CLUSTER_NAME"
fi

step "Install CRDs"
# All three, not just the two these scenarios use: the operator runs a controller
# per CRD and each holds a readiness gate until its CRD is served, so omitting
# restatecloudenvironments leaves the pod NotReady and `helm --wait` times out.
kc apply --server-side -f "${REPO_ROOT}/crd/restateclusters.yaml"
kc apply --server-side -f "${REPO_ROOT}/crd/restatedeployments.yaml"
kc apply --server-side -f "${REPO_ROOT}/crd/restatecloudenvironments.yaml"

# ---- images + operator ----------------------------------------------------
if [[ "${SKIP_BUILD:-0}" != "1" ]]; then
  step "Build + load operator image"
  ( cd "$REPO_ROOT" && just docker )
  kind load docker-image "$OPERATOR_IMAGE" --name "$CLUSTER_NAME"

  step "Build + load greeter image"
  ( cd "$REPO_ROOT/examples/services/greeter" && just docker )
  kind load docker-image "$GREETER_IMAGE" --name "$CLUSTER_NAME"
fi

step "Deploy operator"
# The chart depends on restate-operator-crds through a local file:// path, and
# both the vendored copy under charts/restate-operator-helm/charts/ and Chart.lock
# are gitignored. A machine that has built the chart before has them and a fresh
# checkout does not, which is why this only ever failed in CI. `update` rather
# than `build` because `build` reads the lock that isn't there; the dependency is
# a sibling directory at this same commit, so resolving it is a local copy.
helm dependency update "${REPO_ROOT}/charts/restate-operator-helm" >/dev/null \
  || die "could not resolve the operator chart's dependencies"

# installCrds=false: the CRDs were applied above straight from crd/, which is the
# copy under test. Letting the subchart install them too would have Helm try to
# adopt objects created by kubectl, which it refuses for want of its own
# ownership labels.
helm --kube-context "$CTX" upgrade --install restate-operator \
  "${REPO_ROOT}/charts/restate-operator-helm" \
  --namespace "$OPERATOR_NS" --create-namespace \
  --set installCrds=false \
  --set version=local --wait --timeout 180s
kc -n "$OPERATOR_NS" rollout restart deployment -l app=restate-operator >/dev/null 2>&1 || true
kc -n "$OPERATOR_NS" rollout status deployment -l app=restate-operator --timeout=120s

# ---- restate cluster ------------------------------------------------------
step "Deploy + provision RestateCluster (${RESTATE_IMAGE})"
# Pinned here rather than reusing examples/cluster/cluster.yaml: these scenarios
# depend on the admin API's `force` default, which changed in v1.6.0.
kc apply --server-side -f - <<YAML
apiVersion: restate.dev/v1
kind: RestateCluster
metadata:
  name: restate
spec:
  compute:
    image: ${RESTATE_IMAGE}
  storage:
    storageRequestBytes: 2147483648
YAML
wait_until "restate-0 pod to exist" 180 kc -n "$RESTATE_NS" get pod restate-0
kc -n "$RESTATE_NS" wait --for=condition=Ready pod/restate-0 --timeout=240s \
  || die "restate-0 did not become ready"
kc -n "$RESTATE_NS" exec restate-0 -- restatectl provision --yes >/dev/null 2>&1 \
  || kc -n "$RESTATE_NS" exec restate-0 -- restatectl provision >/dev/null 2>&1 \
  || warn "provision returned non-zero (may already be provisioned)"

step "Port-forward Restate ingress + admin"
kc -n "$RESTATE_NS" port-forward svc/restate \
  "${INGRESS_PORT}:${CLUSTER_INGRESS_PORT}" "${ADMIN_PORT}:${CLUSTER_ADMIN_PORT}" \
  >/tmp/restate-rollback-pf.log 2>&1 &
PF_PID=$!
sleep 2
if ! kill -0 "$PF_PID" 2>/dev/null || grep -q "address already in use" /tmp/restate-rollback-pf.log; then
  cat /tmp/restate-rollback-pf.log >&2
  die "port-forward failed (ports ${INGRESS_PORT}/${ADMIN_PORT} in use?). Set INGRESS_PORT/ADMIN_PORT."
fi
wait_until "admin API reachable" 60 admin_healthy || die "admin API never came up"

step "Check the Restate version actually exercises the bug"
SERVER_VERSION="$(curl -sS "localhost:${ADMIN_PORT}/version" 2>/dev/null | jq -r '.version // "unknown"' 2>/dev/null || echo unknown)"
info "Restate server version: ${SERVER_VERSION}"
case "$SERVER_VERSION" in
  1.0.*|1.1.*|1.2.*|1.3.*|1.4.*|1.5.*)
    warn "Restate ${SERVER_VERSION} predates the force-default change (v1.6.0);"
    warn "registration still force-overwrites, so these scenarios cannot fail."
    ;;
esac

# ---- manifest + fixtures --------------------------------------------------
# revisionHistoryLimit is deliberately generous: the rollback under test only
# exists if the superseded ReplicaSet is *retained* after draining. A limit of 0
# would delete it (and deregister its deployment), turning every rollback into a
# plain first-time registration and testing nothing.
rd_manifest() {
  local version="$1"
  cat <<YAML
apiVersion: restate.dev/v1beta1
kind: RestateDeployment
metadata:
  name: ${RD_NAME}
  namespace: ${APP_NS}
spec:
  replicas: 1
  revisionHistoryLimit: 10
  selector:
    matchLabels:
      app: ${RD_NAME}
  restate:
    register:
      cluster: restate
    drainDelaySeconds: ${DRAIN_DELAY}
  template:
    metadata:
      labels:
        app: ${RD_NAME}
    spec:
      containers:
        - name: service
          image: ${GREETER_IMAGE}
          imagePullPolicy: Never
          ports:
            - name: h2c
              containerPort: 9080
          env:
            - name: SERVICE_VERSION
              value: "${version}"
            - name: POD_NAME
              valueFrom:
                fieldRef:
                  fieldPath: metadata.name
          resources:
            requests:
              cpu: "50m"
              memory: "64Mi"
            limits:
              cpu: "200m"
              memory: "128Mi"
YAML
}
deploy_version() { rd_manifest "$1" | kc apply -f - >/dev/null; }

# Start a long slowGreet synchronously in the background, capturing the response
# body. Reading `.version` from it afterwards proves which version actually ran
# the invocation -- not merely which one it was routed to at submission time.
PIN_FILE=""
start_pinned_invocation() {
  PIN_FILE="$(mktemp)"
  curl -sS --max-time $((PIN_SECONDS + 120)) \
    "localhost:${INGRESS_PORT}/Greeter/slowGreet" \
    -H 'content-type: application/json' \
    -d "{\"name\":\"pin\",\"delaySeconds\":${PIN_SECONDS}}" >"$PIN_FILE" 2>/dev/null &
  PIN_PID=$!
  sleep 5   # let it reach the server and pin
  info "pinned invocation started (in flight for ~${PIN_SECONDS}s)"
}
# Sets PIN_VERSION rather than printing it. The wait has to happen in the shell
# that started the curl: called as "$(await_pinned_invocation)" the function runs
# in a command-substitution subshell, which is not the background job's parent, so
# `wait` returns instantly ("not a child of this shell") and jq reads a response
# file nothing has written yet -- every pinned assertion then sees '<no result>'.
PIN_VERSION=""
await_pinned_invocation() {
  wait "$PIN_PID" 2>/dev/null || true
  PIN_VERSION="$(jq -r '.version // empty' <"$PIN_FILE" 2>/dev/null || true)"
}

reset_fixture() {
  kc -n "$APP_NS" delete restatedeployment "$RD_NAME" --ignore-not-found --wait=true --timeout=180s >/dev/null 2>&1 || true
  wait_until "old ReplicaSets to be cleared" 120 bash -c \
    "[[ -z \"\$(kubectl --context ${CTX} -n ${APP_NS} get rs -l restate.dev/owned-by=${RD_NAME} -o name 2>/dev/null)\" ]]" || true
}

# ===========================================================================
# Scenario: clean rollback -- v1 drained and retained, then restored.
#
# Reproduces the "registered but not latest" case: v1's deployment is still
# registered, so registration is attempted but the server answers 200
# "Unchanged" with v1's existing id and leaves v2 latest.
# ===========================================================================
scenario_clean() {
  step "Scenario: clean rollback (no pinned invocations)"
  deploy_version v1
  wait_until "v1 Ready" 240 rd_ready || { fail "v1 never became Ready"; return; }
  local rs_v1 dp_v1; rs_v1="$(rs_for_version v1)"; dp_v1="$(rs_annotation "$rs_v1" deployment-id)"
  [[ -n "$dp_v1" ]] || { fail "v1 ReplicaSet has no deployment-id annotation"; return; }
  info "v1: rs=${rs_v1} deployment=${dp_v1}"

  wait_until "Restate to route to v1" 60 latest_is "$dp_v1" \
    && pass "v1 is latest after first registration" \
    || fail "v1 is not latest after first registration (latest=$(latest_deployment_id))"

  deploy_version v2
  wait_until "v2 Ready" 240 rd_ready || warn "v2 not Ready; continuing"
  local rs_v2 dp_v2; rs_v2="$(rs_for_version v2)"; dp_v2="$(rs_annotation "$rs_v2" deployment-id)"
  info "v2: rs=${rs_v2} deployment=${dp_v2}"
  wait_until "Restate to route to v2" 90 latest_is "$dp_v2" \
    && pass "v2 is latest after the forward roll" \
    || fail "v2 is not latest after the forward roll (latest=$(latest_deployment_id))"

  # The rollback only tests the retained-endpoint path once v1 has actually
  # drained to zero but is still registered.
  wait_until "v1 to drain to 0 replicas (still retained)" 180 rs_replicas_is "$rs_v1" 0 \
    || warn "v1 did not scale to 0; the rollback may exercise a different path"
  kc -n "$APP_NS" get rs "$rs_v1" >/dev/null 2>&1 \
    || { fail "v1 ReplicaSet was deleted, not retained -- revisionHistoryLimit too low?"; return; }

  info "rolling back to v1 (identical spec -> re-adopts ${rs_v1})…"
  deploy_version v1

  wait_until "v1 ReplicaSet to be ready again" 240 rs_ready_is "$rs_v1" 1 \
    && pass "retained v1 ReplicaSet scaled back up and became ready" \
    || fail "v1 ReplicaSet did not become ready again"

  if wait_until "Restate to route back to v1" 180 latest_is "$dp_v1"; then
    pass "Restate promoted v1 back to latest"
  else
    fail "v1 was restored in Kubernetes but Restate still routes to $(latest_deployment_id) (expected ${dp_v1})"
  fi

  if wait_until "invocations to land on v1" 90 greet_version_is v1; then
    pass "new invocations execute on v1"
  else
    fail "new invocations still execute on '$(greet_version)' (expected v1)"
  fi

  # Ready must mean "Restate routes here", not merely "the pods are up".
  if rd_ready && ! latest_is "$dp_v1"; then
    fail "Ready=True while ${dp_v1} is not latest -- Ready does not imply promotion"
  else
    pass "Ready=True agrees with Restate's routing"
  fi
}

# ===========================================================================
# Scenario: rollback while an invocation is pinned to the OLD version.
#
# Reproduces the "retained by pinned invocations" case: v1 reads as active, so
# registration is skipped entirely and v2 silently stays latest.
# ===========================================================================
scenario_pinned_old() {
  step "Scenario: rollback with an invocation pinned to v1"
  deploy_version v1
  wait_until "v1 Ready" 240 rd_ready || { fail "v1 never became Ready"; return; }
  local rs_v1 dp_v1; rs_v1="$(rs_for_version v1)"; dp_v1="$(rs_annotation "$rs_v1" deployment-id)"
  info "v1: rs=${rs_v1} deployment=${dp_v1}"

  start_pinned_invocation   # pins to v1, which is latest right now

  deploy_version v2
  wait_until "v2 Ready" 240 rd_ready || warn "v2 not Ready; continuing"
  local rs_v2 dp_v2; rs_v2="$(rs_for_version v2)"; dp_v2="$(rs_annotation "$rs_v2" deployment-id)"
  wait_until "Restate to route to v2" 90 latest_is "$dp_v2" \
    || warn "v2 never became latest; the rollback assertion below is weaker"

  # v1 must still be held by the in-flight invocation -- that is the precondition
  # this scenario exists to cover.
  if rs_replicas_is "$rs_v1" 0; then
    warn "v1 already scaled to 0; the pinned invocation did not hold it (PIN_SECONDS too short?)"
  else
    info "v1 still held by its in-flight invocation, as intended"
  fi

  info "rolling back to v1 while it still has a pinned invocation…"
  deploy_version v1

  if wait_until "Restate to route back to v1" 180 latest_is "$dp_v1"; then
    pass "Restate promoted v1 back to latest despite the pinned invocation"
  else
    fail "v1 held a pinned invocation, so registration was skipped and Restate still routes to $(latest_deployment_id)"
  fi

  if wait_until "invocations to land on v1" 90 greet_version_is v1; then
    pass "new invocations execute on v1"
  else
    fail "new invocations still execute on '$(greet_version)' (expected v1)"
  fi

  await_pinned_invocation
  if [[ "$PIN_VERSION" == "v1" ]]; then
    pass "the invocation pinned to v1 completed on v1"
  else
    fail "the pinned invocation completed on '${PIN_VERSION:-<no result>}' (expected v1)"
  fi
}

# ===========================================================================
# Scenario: rollback while an invocation is pinned to the NEW version.
#
# Not in the issue's list, but it is the half that proves the superseded
# revision is still drained properly rather than merely abandoned: v2 must keep
# its own in-flight work, take no new work, and only then scale away.
# ===========================================================================
scenario_pinned_new() {
  step "Scenario: rollback with an invocation pinned to v2"
  deploy_version v1
  wait_until "v1 Ready" 240 rd_ready || { fail "v1 never became Ready"; return; }
  local rs_v1 dp_v1; rs_v1="$(rs_for_version v1)"; dp_v1="$(rs_annotation "$rs_v1" deployment-id)"

  deploy_version v2
  wait_until "v2 Ready" 240 rd_ready || warn "v2 not Ready; continuing"
  local rs_v2 dp_v2; rs_v2="$(rs_for_version v2)"; dp_v2="$(rs_annotation "$rs_v2" deployment-id)"
  wait_until "Restate to route to v2" 90 latest_is "$dp_v2" \
    || { fail "v2 never became latest; cannot pin to it"; return; }

  start_pinned_invocation   # pins to v2, which is latest right now

  info "rolling back to v1 while v2 holds a pinned invocation…"
  deploy_version v1

  if wait_until "Restate to route back to v1" 180 latest_is "$dp_v1"; then
    pass "Restate promoted v1 back to latest while v2 was still draining"
  else
    fail "Restate still routes to $(latest_deployment_id) (expected ${dp_v1})"
  fi

  if wait_until "invocations to land on v1" 90 greet_version_is v1; then
    pass "new invocations execute on v1, not the draining v2"
  else
    fail "new invocations execute on '$(greet_version)' (expected v1)"
  fi

  await_pinned_invocation
  if [[ "$PIN_VERSION" == "v2" ]]; then
    pass "the invocation pinned to v2 completed on v2 after the rollback"
  else
    fail "the pinned invocation completed on '${PIN_VERSION:-<no result>}' (expected v2)"
  fi

  if wait_until "v2 to drain to 0 replicas" 180 rs_replicas_is "$rs_v2" 0; then
    pass "v2 drained and scaled down once its invocation finished"
  else
    fail "v2 did not scale down after draining (replicas=$(rs_replicas "$rs_v2"))"
  fi
}

# ===========================================================================
for s in ${ONLY:-clean pinned-old pinned-new}; do
  reset_fixture
  case "$s" in
    clean)      scenario_clean ;;
    pinned-old) scenario_pinned_old ;;
    pinned-new) scenario_pinned_new ;;
    *)          die "unknown scenario: $s" ;;
  esac
done

step "Results"
echo "  ${GREEN}${PASSES} passed${RST}, ${RED}${FAILS} failed${RST}"
(( FAILS == 0 )) || { dump_diagnostics; die "e2e had failures"; }
info "all assertions passed"
