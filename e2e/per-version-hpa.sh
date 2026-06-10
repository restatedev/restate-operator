#!/usr/bin/env bash
#
# e2e driver for per-version autoscaling of draining RestateDeployment versions.
# See docs/per-version-autoscaling.md (the "End-to-end" test plan).
#
# It stands up a disposable kind cluster with:
#   - metrics-server (so CPU HPAs can read pod metrics)
#   - the RestateDeployment/RestateCluster CRDs
#   - the operator (built from this checkout, loaded into kind)
#   - a single-node RestateCluster
#   - the example "greeter" service
# then drives the scenarios and asserts the operator's per-version HPA behaviour.
#
# Determinism note (see the design doc): correctness here is driven by Restate's
# active/inactive signal — a long async `slowGreet` keeps a version active, and
# its completion flips the version inactive. CPU-driven *scale-up under load* is
# inherently flaky to assert (a draining version is mostly suspended and uses no
# CPU), so that scenario is BEST-EFFORT and never fails the run. The structural
# checks (HPA present/absent per version, idle scale-down to floor, teardown
# ordering, ownership-for-GC) are the deterministic ones.
#
# Prerequisites on PATH: kind, kubectl, helm, docker, jq, just, curl.
#
# Usage:
#   e2e/per-version-hpa.sh              # full run, tears the cluster down at the end
#   KEEP=1 e2e/per-version-hpa.sh       # keep the cluster for inspection
#   SKIP_BUILD=1 e2e/per-version-hpa.sh # reuse already-loaded images
#
# NOTE: this script has not been executed end-to-end in CI yet; timing waits and
# the `restatectl provision` invocation may need tuning for your environment.
#
set -euo pipefail

# ---- config ---------------------------------------------------------------
CLUSTER_NAME="${CLUSTER_NAME:-restate-hpa-e2e}"
OPERATOR_NS="${OPERATOR_NS:-restate-operator}"
RESTATE_NS="${RESTATE_NS:-restate}"      # namespace the operator creates for the cluster
APP_NS="${APP_NS:-default}"              # namespace for the RestateDeployment
RD_NAME="${RD_NAME:-greeter-rsd}"
OPERATOR_IMAGE="ghcr.io/restatedev/restate-operator:local"
GREETER_IMAGE="dev.local/restatedev/restate-operator/greeter:local"
PIN_SECONDS="${PIN_SECONDS:-150}"        # how long the pinning slowGreet stays in-flight
# In-cluster Restate ports.
CLUSTER_INGRESS_PORT=8080
CLUSTER_ADMIN_PORT=9070
# Local ports for our port-forward. Deliberately uncommon so we don't collide
# with (and silently hit) an unrelated Restate port-forward — e.g. a Restate
# Cloud control plane forwarded to 8080/9070. Override if these are taken.
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

cleanup() {
  [[ -n "$PF_PID" ]] && kill "$PF_PID" >/dev/null 2>&1 || true
  if [[ "${KEEP:-0}" != "1" ]]; then
    info "deleting kind cluster ${CLUSTER_NAME} (set KEEP=1 to keep it)"
    kind delete cluster --name "$CLUSTER_NAME" >/dev/null 2>&1 || true
  else
    info "KEEP=1 — leaving cluster ${CLUSTER_NAME} up"
  fi
}
trap cleanup EXIT

# ---- predicates / queries -------------------------------------------------
operator_hpa_names() {
  kc -n "$APP_NS" get hpa \
    -l "app.kubernetes.io/managed-by=restate-operator,restate.dev/owned-by=${RD_NAME}" \
    -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}' 2>/dev/null
}
operator_hpa_count() { operator_hpa_names | grep -c . || true; }
hpa_count_is()       { [[ "$(operator_hpa_count)" == "$1" ]]; }
rs_replicas()        { kc -n "$APP_NS" get rs "$1" -o jsonpath='{.spec.replicas}' 2>/dev/null; }
rs_replicas_is()     { [[ "$(rs_replicas "$1")" == "$2" ]]; }
rs_gone_or_zero()    { ! kc -n "$APP_NS" get rs "$1" >/dev/null 2>&1 || rs_replicas_is "$1" 0; }
rd_ready() {
  kc -n "$APP_NS" get restatedeployment "$RD_NAME" \
    -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}' 2>/dev/null | grep -qx True
}
admin_healthy() { curl -sf "localhost:${ADMIN_PORT}/health" >/dev/null 2>&1; }

# Dump operator logs + resource state; called automatically when an assertion
# fails, so a failing run is debuggable without KEEP=1.
dump_diagnostics() {
  echo "${YELLOW}--- diagnostics ---${RST}"
  echo "  ### RestateDeployment / ReplicaSets / HPAs / pods (${APP_NS})"
  kc -n "$APP_NS" get restatedeployment,rs,hpa,pods -o wide 2>&1 | sed 's/^/  /' || true
  echo "  ### operator logs (tail 80)"
  kc -n "$OPERATOR_NS" logs -l app=restate-operator --tail=80 2>&1 | sed 's/^/  /' || true
}

# True while any non-completed invocation against Greeter exists (best-effort;
# depends on the admin /query JSON shape — used only as a secondary signal).
greeter_has_active_invocations() {
  local q='SELECT count(*) AS c FROM sys_invocation_status WHERE status != '"'"'completed'"'"' AND target_service_name = '"'"'Greeter'"'"
  curl -sS "localhost:${ADMIN_PORT}/query" \
    -H 'content-type: application/json' -H 'accept: application/json' \
    -d "{\"query\": $(jq -Rs . <<<"$q")}" 2>/dev/null \
    | jq -e '.rows[0].c > 0' >/dev/null 2>&1
}
greeter_drained() { ! greeter_has_active_invocations; }

# ---- preflight ------------------------------------------------------------
step "Preflight"
for bin in kind kubectl helm docker jq just curl; do
  command -v "$bin" >/dev/null 2>&1 || die "missing required tool: $bin"
done
info "all tools present"

# ---- cluster --------------------------------------------------------------
step "Create kind cluster"
# Reuse an existing cluster only if it is actually reachable. kind can leave a
# cluster listed while its kubeconfig context is gone (e.g. after a partial
# delete); in that case recreate it rather than failing later.
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

step "Install metrics-server (kind-patched)"
kc apply -f https://github.com/kubernetes-sigs/metrics-server/releases/latest/download/components.yaml
# kind kubelets serve metrics over a self-signed cert; allow it (idempotently —
# don't append the flag twice when reusing a cluster).
if ! kc -n kube-system get deploy metrics-server \
     -o jsonpath='{.spec.template.spec.containers[0].args}' 2>/dev/null | grep -q kubelet-insecure-tls; then
  kc -n kube-system patch deployment metrics-server --type=json \
    -p '[{"op":"add","path":"/spec/template/spec/containers/0/args/-","value":"--kubelet-insecure-tls"}]'
fi
kc -n kube-system rollout status deployment/metrics-server --timeout=180s

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
# force a fresh pod so a rebuilt :local image is picked up
kc -n "$OPERATOR_NS" rollout restart deployment -l app=restate-operator >/dev/null 2>&1 || true
kc -n "$OPERATOR_NS" rollout status deployment -l app=restate-operator --timeout=120s

# ---- restate cluster ------------------------------------------------------
step "Deploy + provision RestateCluster"
kc apply --server-side -f "${REPO_ROOT}/examples/cluster/cluster.yaml"
wait_until "restate-0 pod to exist" 180 kc -n "$RESTATE_NS" get pod restate-0
kc -n "$RESTATE_NS" wait --for=condition=Ready pod/restate-0 --timeout=240s \
  || die "restate-0 did not become ready"
# single-node clusters must be provisioned once; ignore 'already provisioned'.
kc -n "$RESTATE_NS" exec restate-0 -- restatectl provision --yes >/dev/null 2>&1 \
  || kc -n "$RESTATE_NS" exec restate-0 -- restatectl provision >/dev/null 2>&1 \
  || warn "provision returned non-zero (may already be provisioned)"

step "Port-forward Restate ingress + admin"
kc -n "$RESTATE_NS" port-forward svc/restate \
  "${INGRESS_PORT}:${CLUSTER_INGRESS_PORT}" "${ADMIN_PORT}:${CLUSTER_ADMIN_PORT}" \
  >/tmp/restate-pf.log 2>&1 &
PF_PID=$!
sleep 2
# Fail loudly if the forward could not bind — otherwise a stale listener on these
# ports (e.g. another Restate) would silently answer our health check and curls.
if ! kill -0 "$PF_PID" 2>/dev/null || grep -q "address already in use" /tmp/restate-pf.log; then
  cat /tmp/restate-pf.log >&2
  die "port-forward to Restate failed (ports ${INGRESS_PORT}/${ADMIN_PORT} in use?). Set INGRESS_PORT/ADMIN_PORT to free ports."
fi
wait_until "admin API reachable" 60 admin_healthy || die "admin API never came up"

# ---- manifest + invocation helpers ----------------------------------------
# Emit a RestateDeployment for the given version. Short drainDelay + zero history
# so teardown is observable within the run; fast HPA scale-down behaviour.
rd_manifest() {
  local version="$1"
  cat <<YAML
apiVersion: restate.dev/v1beta1
kind: RestateDeployment
metadata:
  name: ${RD_NAME}
  namespace: ${APP_NS}
spec:
  replicas: 3
  revisionHistoryLimit: 0
  selector:
    matchLabels:
      app: ${RD_NAME}
  restate:
    register:
      cluster: restate
    drainDelaySeconds: 10
  autoscaling:
    minReplicas: 1
    maxReplicas: 5
    metrics:
      - type: Resource
        resource:
          name: cpu
          target:
            type: Utilization
            averageUtilization: 70
    behavior:
      scaleDown:
        stabilizationWindowSeconds: 0
        policies:
          - type: Pods
            value: 4
            periodSeconds: 15
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
deploy_version() { rd_manifest "$1" | kc apply -f - ; }

# Fire a long async invocation, pinning it to whatever version is latest now.
# Returns immediately (Restate ingress /send); records when it will complete.
PIN_DEADLINE=0
pin_invocation() {
  local id
  id="$(curl -sS "localhost:${INGRESS_PORT}/Greeter/slowGreet/send" \
    -H 'content-type: application/json' \
    -d "{\"name\":\"drain\",\"delaySeconds\":${PIN_SECONDS}}" | jq -r '.invocationId // empty')"
  PIN_DEADLINE=$(( $(date +%s) + PIN_SECONDS ))
  [[ -n "$id" ]] && info "pinned invocation ${id} (in-flight ${PIN_SECONDS}s)" \
                 || warn "could not capture invocation id from ingress /send"
}

# --- opt-in scale demo (RUN_SCALE_DEMO=1) ----------------------------------
# A "see it with your own eyes" demo, NOT a CI assertion: drive real CPU onto a
# draining (non-latest) version and watch the operator's HPA scale it up beyond
# the floor, then back down when the load stops. Latches "max replicas seen" so
# it reports what happened rather than flaking on exact timing. Best run with
# KEEP=1 so you can inspect afterwards; may flake under CPU contention.
BURN_COUNT="${BURN_COUNT:-4}"               # concurrent burns (>= demo maxReplicas)
DEMO_MAX_REPLICAS="${DEMO_MAX_REPLICAS:-3}"
# The burst burns stay pinned-but-idle for BURN_START_DELAY (so we can see the
# version sit at its floor first), then burn for BURN_DURATION (scale up), then
# finish (scale back down). A long idle slowGreet keeps the version ALIVE through
# the whole arc so we observe floor->up->floor rather than a drain-to-zero.
BURN_START_DELAY="${BURN_START_DELAY:-130}" # idle (pinned) before the burst
BURN_DURATION="${BURN_DURATION:-120}"       # CPU burst length
KEEPALIVE_SECONDS="${KEEPALIVE_SECONDS:-480}" # idle keep-alive; must outlast the arc

# RD for the demo: floor of 1 (so 1->N->1 is visible), CPU-target autoscaling.
rd_manifest_demo() {
  cat <<YAML
apiVersion: restate.dev/v1beta1
kind: RestateDeployment
metadata:
  name: ${RD_NAME}
  namespace: ${APP_NS}
spec:
  replicas: 1
  revisionHistoryLimit: 0
  selector:
    matchLabels:
      app: ${RD_NAME}
  restate:
    register:
      cluster: restate
    drainDelaySeconds: 10
  autoscaling:
    minReplicas: 1
    maxReplicas: ${DEMO_MAX_REPLICAS}
    metrics:
      - type: Resource
        resource:
          name: cpu
          target:
            type: Utilization
            averageUtilization: 60
    behavior:
      scaleDown:
        stabilizationWindowSeconds: 0
        policies:
          - type: Pods
            value: 4
            periodSeconds: 15
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
            limits:
              cpu: "1"
              memory: "128Mi"
YAML
}

# Fire one async CPU burn at the current latest version: pinned but idle for
# BURN_START_DELAY, then burning for BURN_DURATION. Prints the invocation id on
# success; returns 1 (and warns) if the ingress rejected it.
burn_on_current() {
  local resp id
  resp="$(curl -sS "localhost:${INGRESS_PORT}/Greeter/burn/send" \
    -H 'content-type: application/json' \
    -d "{\"startDelaySeconds\":${BURN_START_DELAY},\"durationSeconds\":${BURN_DURATION}}" 2>&1)"
  id="$(jq -r '.invocationId // empty' <<<"$resp" 2>/dev/null)"
  if [[ -z "$id" ]]; then warn "burn /send rejected: ${resp}"; return 1; fi
  echo "$id"
}

# Fire a long idle (suspended) keep-alive pinned to the current latest version,
# so it stays "active" in Restate — keeping its HPA and avoiding drain — even
# after the burst load ends, letting us see it scale back to floor (not vanish).
keepalive_on_current() {
  local resp id
  resp="$(curl -sS "localhost:${INGRESS_PORT}/Greeter/slowGreet/send" \
    -H 'content-type: application/json' \
    -d "{\"name\":\"keepalive\",\"delaySeconds\":${KEEPALIVE_SECONDS}}" 2>&1)"
  id="$(jq -r '.invocationId // empty' <<<"$resp" 2>/dev/null)"
  if [[ -z "$id" ]]; then warn "keepalive /send rejected: ${resp}"; return 1; fi
  echo "$id"
}

cpu_util() { kc -n "$APP_NS" get hpa "$1" -o jsonpath='{.status.currentMetrics[0].resource.current.averageUtilization}' 2>/dev/null; }

run_scale_demo() {
  step "Scale demo — floor -> scale UP under load -> back to floor, on a draining version"
  rd_manifest_demo v1 | kc apply -f -
  wait_until "v1 Ready (registered)" 180 rd_ready || warn "v1 not Ready; continuing"

  info "pinning an idle ${KEEPALIVE_SECONDS}s keep-alive to v1…"
  local ka; ka="$(keepalive_on_current)" || { fail "keep-alive not accepted (service registered? right Restate?)"; return; }
  info "  keep-alive: ${ka}"

  info "pre-pinning ${BURN_COUNT} burns to v1 (idle ${BURN_START_DELAY}s, then burn ${BURN_DURATION}s)…"
  local fired=0 i id
  for i in $(seq 1 "$BURN_COUNT"); do
    if id="$(burn_on_current)"; then fired=$((fired+1)); info "  burn ${i}: ${id}"; fi
  done
  (( fired == 0 )) && { fail "no burns accepted by the ingress"; return; }
  (( fired < BURN_COUNT )) && warn "only ${fired}/${BURN_COUNT} burns fired"
  sleep 8   # let them pin on v1

  info "bumping to v2 (v1 becomes non-latest; keep-alive + idle burns keep it active)…"
  rd_manifest_demo v2 | kc apply -f -
  # The HPA is only stamped once v2 finishes registering, which takes a couple of
  # reconcile/readiness cycles — so wait generously here.
  wait_until "operator HPA for v1 to appear" 180 hpa_count_is 1 || { fail "no HPA stamped for v1"; return; }
  local hpa rs; hpa="$(operator_hpa_names | head -n1)"
  rs="$(kc -n "$APP_NS" get hpa "$hpa" -o jsonpath='{.spec.scaleTargetRef.name}')"
  info "watching v1 ReplicaSet ${rs} via HPA ${hpa} through floor -> up -> floor…"

  # One continuous latched watch over the whole arc.
  local saw_floor=0 up_seen=0 back_to_floor=0 maxr=0 r
  local deadline=$(( $(date +%s) + BURN_START_DELAY + BURN_DURATION + 300 ))
  while (( $(date +%s) < deadline )); do
    if ! kc -n "$APP_NS" get rs "$rs" >/dev/null 2>&1; then
      info "  watch: v1 ReplicaSet is gone (drained)"; break
    fi
    r="$(rs_replicas "$rs")"; r="${r:-0}"
    if (( r > maxr )); then maxr=$r; fi
    if (( r == 1 && up_seen == 0 )); then saw_floor=1; fi   # floor before any scale-up
    if (( maxr > 1 )); then up_seen=1; fi
    if (( up_seen == 1 && r == 1 )); then back_to_floor=1; fi   # returned, still alive
    info "  watch: v1 replicas=${r} cpu=$(cpu_util "$hpa")% (floor_seen=${saw_floor} max=${maxr} back=${back_to_floor})"
    if (( back_to_floor == 1 )); then break; fi
    sleep 8
  done

  (( saw_floor == 1 )) \
    && pass "v1 sat at its floor (1) before load" \
    || warn "didn't catch v1 at floor before scale-up (v2 registration may have lagged BURN_START_DELAY)"
  (( up_seen == 1 )) \
    && pass "v1 scaled UP to ${maxr} replicas under the burst (> floor)" \
    || fail "v1 never scaled above 1 (max=${maxr})"
  (( back_to_floor == 1 )) \
    && pass "v1 scaled back DOWN to the floor (1) after the burst, while still alive" \
    || warn "didn't observe return to floor (needs longer, or keep-alive expired)"
}

if [[ "${RUN_SCALE_DEMO:-0}" == "1" ]]; then
  run_scale_demo
  step "Demo results"
  echo "  ${GREEN}${PASSES} passed${RST}, ${RED}${FAILS} failed${RST}"
  [[ "${KEEP:-0}" == "1" ]] || warn "cluster will be torn down; re-run with KEEP=1 to inspect"
  exit $(( FAILS > 0 ? 1 : 0 ))
fi

# ===========================================================================
step "Scenario 1 — v1 latest: no operator HPA"
deploy_version v1
wait_until "RestateDeployment Ready" 180 rd_ready || warn "RD not Ready; continuing"
sleep 5
if hpa_count_is 0; then
  pass "no operator HPA while v1 is the only (latest) version"
else
  fail "expected 0 operator HPAs, got $(operator_hpa_count): $(operator_hpa_names | tr '\n' ' ')"
fi

# ---------------------------------------------------------------------------
step "Scenario 2 — pin v1, roll to v2: one HPA, targeting v1, owned by the RD"
pin_invocation
sleep 3
deploy_version v2
wait_until "v2 Ready" 180 rd_ready || warn "RD not Ready after v2; continuing"

if wait_until "one operator HPA to appear" 90 hpa_count_is 1; then
  pass "exactly one operator HPA once v1 is non-latest-and-active"
else
  fail "expected exactly 1 operator HPA, got $(operator_hpa_count)"
fi

V1_HPA="$(operator_hpa_names | head -n1)"
V1_RS=""
if [[ -n "$V1_HPA" ]]; then
  V1_RS="$(kc -n "$APP_NS" get hpa "$V1_HPA" -o jsonpath='{.spec.scaleTargetRef.name}')"
  TGT="$(kc -n "$APP_NS" get hpa "$V1_HPA" -o jsonpath='{.spec.scaleTargetRef.kind}/{.spec.scaleTargetRef.name}')"
  if [[ "$TGT" == "ReplicaSet/${V1_RS}" ]] && kc -n "$APP_NS" get rs "$V1_RS" >/dev/null 2>&1; then
    pass "HPA ${V1_HPA} targets existing ReplicaSet ${V1_RS}"
  else
    fail "HPA scaleTargetRef unexpected: ${TGT}"
  fi
  # ownerReference == the GC guarantee (we don't race a live RD deletion below)
  OWNER="$(kc -n "$APP_NS" get hpa "$V1_HPA" -o jsonpath='{.metadata.ownerReferences[0].kind}/{.metadata.ownerReferences[0].name}')"
  [[ "$OWNER" == "RestateDeployment/${RD_NAME}" ]] \
    && pass "HPA owned by ${OWNER} (will be garbage-collected with the RD)" \
    || fail "HPA owner unexpected: ${OWNER}"
fi

# ---------------------------------------------------------------------------
step "Scenario 3 — idle non-latest version scales to floor (3 -> minReplicas=1)"
# v1 is idle (its pinned invocation is suspended server-side), so CPU is ~0 and
# the HPA should walk it down to minReplicas. Requires metrics-server.
if [[ -n "$V1_RS" ]] && wait_until "v1 RS to reach 1 replica" 180 rs_replicas_is "$V1_RS" 1; then
  pass "idle v1 ReplicaSet scaled down to the floor (1)"
else
  fail "v1 RS did not reach the floor; current=$(rs_replicas "${V1_RS:-none}") (needs metrics-server)"
fi

# ---------------------------------------------------------------------------
step "Scenario 4 (best-effort) — burst load on v1"
# Driving CPU onto a *draining* version is unreliable (most pinned work is
# suspended). Fire some traffic and only report what we observe; never fail.
warn "best-effort: generating brief traffic; scale-up is not asserted"
( for _ in $(seq 1 200); do
    curl -sf "localhost:${INGRESS_PORT}/Greeter/greet" \
      -H 'content-type: application/json' -d '"load"' >/dev/null 2>&1 || true
  done ) & LOAD_PID=$!
sleep 20; kill "$LOAD_PID" >/dev/null 2>&1 || true
info "v1 RS replicas after load burst: $(rs_replicas "${V1_RS:-none}")"

# ---------------------------------------------------------------------------
step "Scenario 5 — drain to zero: invocation completes -> HPA removed -> RS torn down"
# Wait deterministically for the pinned slowGreet to finish (time-based; the
# admin query is a secondary confirmation if the JSON shape matches).
now=$(date +%s)
remaining=$(( PIN_DEADLINE - now + 15 ))
if (( remaining > 0 )); then info "waiting ${remaining}s for pinned invocation to complete…"; sleep "$remaining"; fi
wait_until "Greeter to have no active invocations" 60 greeter_drained \
  || warn "could not confirm drain via admin query (continuing on time basis)"

# HPA must be deleted at the inactive transition — BEFORE the RS is scaled to 0.
if wait_until "operator HPA to be deleted" 90 hpa_count_is 0; then
  pass "operator HPA removed once v1 is inactive"
else
  fail "operator HPA still present after v1 went inactive: $(operator_hpa_names | tr '\n' ' ')"
fi

# then the operator scales the drained RS to 0 and (revisionHistoryLimit=0) deletes it.
if [[ -n "$V1_RS" ]] && wait_until "v1 RS to be torn down" 120 rs_gone_or_zero "$V1_RS"; then
  pass "drained v1 ReplicaSet scaled to 0 / deleted after HPA removal"
else
  fail "v1 RS not torn down; current=$(rs_replicas "${V1_RS:-none}")"
fi

# ===========================================================================
step "Results"
echo "  ${GREEN}${PASSES} passed${RST}, ${RED}${FAILS} failed${RST}"
(( FAILS == 0 )) || { dump_diagnostics; die "e2e had failures"; }
info "all assertions passed"
