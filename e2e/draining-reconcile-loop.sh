#!/usr/bin/env bash
#
# Regression test for #138, where a RestateDeployment with an old version still draining reconciled
# in a tight loop (~8/sec, ~30k ReplicaSet writes an hour, forever) with nothing actually changing.
#
# Two things closed the loop: the owned-ReplicaSet stream had no change predicate, so any ReplicaSet
# update re-enqueued the owner including our own writes; and the already-exists path re-applied the
# scale subresource and the annotations every reconcile, where a no-op apply still bumped
# resourceVersion.
#
# So we assert on rates, not state. Once a draining version has settled, the reconcile rate and the
# ReplicaSet write rate should be bounded by the drain poll interval, not by how fast the operator
# can talk to the apiserver.
#
# Stands up a throwaway kind cluster with the CRDs, the operator built from this checkout, a
# single-node RestateCluster and the example greeter, pins an invocation to v1, then rolls to v2 so
# v1 drains but can't be removed, and measures. No metrics-server needed, this is about write volume
# rather than autoscaling.
#
# Needs on PATH: kind, kubectl, helm, docker, jq, just, curl.
#
# Usage:
#   e2e/draining-reconcile-loop.sh              # full run, tears the cluster down at the end
#   KEEP=1 e2e/draining-reconcile-loop.sh       # keep the cluster for inspection
#   SKIP_BUILD=1 e2e/draining-reconcile-loop.sh # reuse already-loaded images
#   WINDOW=300 e2e/draining-reconcile-loop.sh   # measure over a longer window
#
set -euo pipefail

# ---- config ---------------------------------------------------------------
CLUSTER_NAME="${CLUSTER_NAME:-restate-loop-e2e}"
OPERATOR_NS="${OPERATOR_NS:-restate-operator}"
RESTATE_NS="${RESTATE_NS:-restate}"
APP_NS="${APP_NS:-default}"
RD_NAME="${RD_NAME:-greeter-rsd}"
OPERATOR_IMAGE="ghcr.io/restatedev/restate-operator:local"
GREETER_IMAGE="dev.local/restatedev/restate-operator/greeter:local"
# the drain delay drives the poll interval for a still-active old version (a tenth of it, clamped to
# [10s, 60s]), so 300 means one reconcile every ~30s
DRAIN_DELAY_SECONDS="${DRAIN_DELAY_SECONDS:-300}"
POLL_SECONDS=$(( DRAIN_DELAY_SECONDS / 10 )); (( POLL_SECONDS < 10 )) && POLL_SECONDS=10
(( POLL_SECONDS > 60 )) && POLL_SECONDS=60
# measurement window, and how long the pinning invocation has to outlive it
WINDOW="${WINDOW:-120}"
PIN_SECONDS="${PIN_SECONDS:-900}"
# what the window is allowed to contain. generous multiples of the expected poll count; the failure
# we're guarding against is three orders of magnitude out, not 2x.
EXPECTED_RECONCILES=$(( WINDOW / POLL_SECONDS + 1 ))
MAX_RECONCILES="${MAX_RECONCILES:-$(( EXPECTED_RECONCILES * 3 + 3 ))}"
MAX_RS_WRITES="${MAX_RS_WRITES:-5}"
CLUSTER_INGRESS_PORT=8080
CLUSTER_ADMIN_PORT=9070
INGRESS_PORT="${INGRESS_PORT:-18080}"
ADMIN_PORT="${ADMIN_PORT:-19070}"
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
    info "KEEP=1, leaving cluster ${CLUSTER_NAME} up"
  fi
}
trap cleanup EXIT

# ---- predicates / queries -------------------------------------------------
rd_ready() {
  kc -n "$APP_NS" get restatedeployment "$RD_NAME" \
    -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}' 2>/dev/null | grep -qx True
}
admin_healthy() { curl -sf "localhost:${ADMIN_PORT}/health" >/dev/null 2>&1; }

# ReplicaSets owned by the RestateDeployment, oldest first.
owned_replicasets() {
  kc -n "$APP_NS" get rs -l "restate.dev/owned-by=${RD_NAME}" \
    --sort-by=.metadata.creationTimestamp \
    -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}' 2>/dev/null
}
rs_field() { kc -n "$APP_NS" get rs "$1" -o jsonpath="{$2}" 2>/dev/null; }

# Reconciles logged for this RestateDeployment in the last $1 seconds.
reconciles_since() {
  kc -n "$OPERATOR_NS" logs -l app=restate-operator --since="${1}s" --tail=-1 2>/dev/null \
    | grep -c "Reconciling RestateDeployment ${RD_NAME} " || true
}

dump_diagnostics() {
  echo "${YELLOW}--- diagnostics ---${RST}"
  echo "  ### RestateDeployment / ReplicaSets / Services / pods (${APP_NS})"
  kc -n "$APP_NS" get restatedeployment,rs,svc,pods -o wide 2>&1 | sed 's/^/  /' || true
  echo "  ### ReplicaSet managedFields"
  for rs in $(owned_replicasets); do
    echo "  --- ${rs}"
    kc -n "$APP_NS" get rs "$rs" -o json --show-managed-fields 2>/dev/null \
      | jq -c '.metadata.managedFields[] | {manager, subresource, time}' 2>&1 | sed 's/^/    /' || true
  done
  echo "  ### operator logs (tail 120)"
  kc -n "$OPERATOR_NS" logs -l app=restate-operator --tail=120 2>&1 | sed 's/^/  /' || true
}

# ---- preflight ------------------------------------------------------------
step "Preflight"
for bin in kind kubectl helm docker jq just curl; do
  command -v "$bin" >/dev/null 2>&1 || die "missing required tool: $bin"
done
info "all tools present"
info "drainDelaySeconds=${DRAIN_DELAY_SECONDS}, expecting a poll every ~${POLL_SECONDS}s"
info "window=${WINDOW}s, allowing <= ${MAX_RECONCILES} reconciles (expecting ~${EXPECTED_RECONCILES})"

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
kc apply --server-side -f "${REPO_ROOT}/crd/restateclusters.yaml"
kc apply --server-side -f "${REPO_ROOT}/crd/restatedeployments.yaml"

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
helm --kube-context "$CTX" upgrade --install restate-operator \
  "${REPO_ROOT}/charts/restate-operator-helm" \
  --namespace "$OPERATOR_NS" --create-namespace \
  --set version=local --wait --timeout 180s
kc -n "$OPERATOR_NS" rollout restart deployment -l app=restate-operator >/dev/null 2>&1 || true
kc -n "$OPERATOR_NS" rollout status deployment -l app=restate-operator --timeout=120s

# ---- restate cluster ------------------------------------------------------
step "Deploy + provision RestateCluster"
kc apply --server-side -f "${REPO_ROOT}/examples/cluster/cluster.yaml"
wait_until "restate-0 pod to exist" 180 kc -n "$RESTATE_NS" get pod restate-0
kc -n "$RESTATE_NS" wait --for=condition=Ready pod/restate-0 --timeout=240s \
  || die "restate-0 did not become ready"
kc -n "$RESTATE_NS" exec restate-0 -- restatectl provision --yes >/dev/null 2>&1 \
  || kc -n "$RESTATE_NS" exec restate-0 -- restatectl provision >/dev/null 2>&1 \
  || warn "provision returned non-zero (may already be provisioned)"

step "Port-forward Restate ingress + admin"
kc -n "$RESTATE_NS" port-forward svc/restate \
  "${INGRESS_PORT}:${CLUSTER_INGRESS_PORT}" "${ADMIN_PORT}:${CLUSTER_ADMIN_PORT}" \
  >/tmp/restate-loop-pf.log 2>&1 &
PF_PID=$!
sleep 2
if ! kill -0 "$PF_PID" 2>/dev/null || grep -q "address already in use" /tmp/restate-loop-pf.log; then
  cat /tmp/restate-loop-pf.log >&2
  die "port-forward to Restate failed (ports ${INGRESS_PORT}/${ADMIN_PORT} in use?)"
fi
wait_until "admin API reachable" 60 admin_healthy || die "admin API never came up"

# ---- manifest + invocation helpers ----------------------------------------
# revisionHistoryLimit is high and the drain delay is the default, so nothing here is trying to make
# removal happen quickly. the point is the steady state.
rd_manifest() {
  cat <<YAML
apiVersion: restate.dev/v1beta1
kind: RestateDeployment
metadata:
  name: ${RD_NAME}
  namespace: ${APP_NS}
  annotations:
    example.com/propagated: "yes"
spec:
  replicas: 2
  revisionHistoryLimit: 10
  selector:
    matchLabels:
      app: ${RD_NAME}
  restate:
    register:
      cluster: restate
    drainDelaySeconds: ${DRAIN_DELAY_SECONDS}
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
              value: "${1}"
            - name: POD_NAME
              valueFrom:
                fieldRef:
                  fieldPath: metadata.name
          resources:
            requests:
              cpu: "50m"
              memory: "64Mi"
YAML
}
deploy_version() { rd_manifest "$1" | kc apply -f - ; }

# a long invocation pinned to whatever version is latest now, in-flight for the whole run. this is
# what keeps the old version active and unremovable.
pin_invocation() {
  local id
  id="$(curl -sS "localhost:${INGRESS_PORT}/Greeter/slowGreet/send" \
    -H 'content-type: application/json' \
    -d "{\"name\":\"drain\",\"delaySeconds\":${PIN_SECONDS}}" | jq -r '.invocationId // empty')"
  [[ -n "$id" ]] && info "pinned invocation ${id} (in-flight ${PIN_SECONDS}s)" \
                 || warn "could not capture invocation id from ingress /send"
}

# ===========================================================================
step "Set up a draining version"
deploy_version v1
wait_until "v1 Ready (registered)" 180 rd_ready || warn "v1 not Ready; continuing"

pin_invocation
sleep 3

info "rolling to v2; v1 becomes non-latest but stays active (pinned invocation)"
deploy_version v2
wait_until "v2 Ready (registered)" 180 rd_ready || warn "v2 not Ready; continuing"

two_replicasets() { (( $(owned_replicasets | grep -c .) >= 2 )); }
wait_until "two owned ReplicaSets to exist" 120 two_replicasets \
  || warn "did not observe two owned ReplicaSets; the draining precondition may not hold"

mapfile -t REPLICASETS < <(owned_replicasets)
info "owned ReplicaSets: ${REPLICASETS[*]:-none}"
(( ${#REPLICASETS[@]} >= 2 )) || fail "expected >= 2 owned ReplicaSets (old + latest), got ${#REPLICASETS[@]}"

# let the rollout settle: registration, scale-up, removal scheduling. the bug was about the steady
# state after all that.
info "letting the rollout settle for 45s before measuring"
sleep 45

# ---------------------------------------------------------------------------
step "Measure the steady state over ${WINDOW}s"

# a ReplicaSet's resourceVersion only moves when that ReplicaSet is written, so sampling it counts
# writes. (the value is a cluster-wide etcd revision, so the difference between two samples means
# nothing; only "did it change" does.) it's the only signal that tells the bug from the fix, since
# neither managedFields timestamps nor generation move on the no-op applies at issue, which is
# exactly why they were so hard to spot.
declare -A RV_LAST RS_WRITES
for rs in "${REPLICASETS[@]}"; do
  RV_LAST[$rs]="$(rs_field "$rs" .metadata.resourceVersion)"
  RS_WRITES[$rs]=0
  info "  ${rs}: resourceVersion=${RV_LAST[$rs]} generation=$(rs_field "$rs" .metadata.generation)"
done

deadline=$(( $(date +%s) + WINDOW ))
while (( $(date +%s) < deadline )); do
  sleep 5
  for rs in "${REPLICASETS[@]}"; do
    rv="$(rs_field "$rs" .metadata.resourceVersion)"
    # gone (removed during the window), stop counting it
    [[ -z "$rv" ]] && continue
    if [[ "$rv" != "${RV_LAST[$rs]}" ]]; then
      RS_WRITES[$rs]=$(( RS_WRITES[$rs] + 1 ))
      RV_LAST[$rs]="$rv"
    fi
  done
done

RECONCILES="$(reconciles_since "$WINDOW")"
info "reconciles of ${RD_NAME} in the last ${WINDOW}s: ${RECONCILES}"

if (( RECONCILES <= MAX_RECONCILES )); then
  pass "reconcile rate is bounded by the drain poll (${RECONCILES} <= ${MAX_RECONCILES} in ${WINDOW}s)"
else
  fail "hot reconcile loop: ${RECONCILES} reconciles in ${WINDOW}s (expected ~${EXPECTED_RECONCILES}, limit ${MAX_RECONCILES})"
fi

# the ReplicaSet writes are what showed up in the audit log, and what fed the loop back into itself.
# a settled version should see none; the allowance is for legitimate status updates from
# kube-controller-manager. sampling every 5s wildly undercounts a hot loop (it wrote ~8 times a
# second), which is fine: any count in this range is unambiguous either way.
for rs in "${REPLICASETS[@]}"; do
  writes="${RS_WRITES[$rs]}"
  if (( writes <= MAX_RS_WRITES )); then
    pass "${rs} was written <= ${MAX_RS_WRITES} times in ${WINDOW}s (observed ${writes})"
  else
    fail "${rs} is being rewritten in a steady state: ${writes} writes observed in ${WINDOW}s"
  fi
done

# ===========================================================================
step "Results"
echo "  ${GREEN}${PASSES} passed${RST}, ${RED}${FAILS} failed${RST}"
(( FAILS == 0 )) || { dump_diagnostics; die "e2e had failures"; }
info "all assertions passed"
