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

CLUSTER_NAME="${CLUSTER_NAME:-restate-hpa-e2e}"
RD_NAME="${RD_NAME:-greeter-rsd}"
PIN_SECONDS="${PIN_SECONDS:-150}"        # how long the pinning slowGreet stays in-flight

# shellcheck source=e2e/lib.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

trap e2e_trap_cleanup EXIT

# ---- predicates / queries -------------------------------------------------
operator_hpa_names() {
  kc -n "$APP_NS" get hpa \
    -l "app.kubernetes.io/managed-by=restate-operator,restate.dev/owned-by=${RD_NAME}" \
    -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}' 2>/dev/null
}
operator_hpa_count() { operator_hpa_names | grep -c . || true; }
hpa_count_is()       { [[ "$(operator_hpa_count)" == "$1" ]]; }
rs_replicas_is()     { [[ "$(rs_replicas "$1")" == "$2" ]]; }
rs_gone_or_zero()    { ! kc -n "$APP_NS" get rs "$1" >/dev/null 2>&1 || rs_replicas_is "$1" 0; }
rd_ready() {
  kc -n "$APP_NS" get restatedeployment "$RD_NAME" \
    -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}' 2>/dev/null | grep -qx True
}

# ---- setup ----------------------------------------------------------------
require_tools
create_kind_cluster
install_metrics_server
install_crds
build_and_load_images
deploy_operator
deploy_restate_cluster
port_forward_restate

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
  id="$(slow_greet "$PIN_SECONDS" || true)"
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
keepalive_on_current() { slow_greet "$KEEPALIVE_SECONDS"; }

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

# Does an HPA with this name exist? (operator HPAs are named after their RS.)
hpa_absent() { ! kc -n "$APP_NS" get hpa "$1" >/dev/null 2>&1; }

# Opt-in rollback reproducer (RUN_ROLLBACK=1). A version that drained (and was
# stamped with a per-version HPA) and is then promoted back to latest — a rollback,
# or a reintroduced identical spec — must shed that HPA, otherwise it and the
# operator's propagate-replicas fight over the ReplicaSet's scale. Deterministic
# (no metrics/load needed). Run with KEEP=1 to inspect.
run_rollback() {
  step "Rollback — a version promoted back to latest drops its operator HPA"
  deploy_version v1
  wait_until "v1 Ready (registered)" 180 rd_ready || warn "v1 not Ready; continuing"

  # Keep v1 active (idle) so it qualifies for an operator HPA once non-latest.
  local ka; ka="$(keepalive_on_current)" || { fail "keep-alive not accepted"; return; }
  info "keep-alive pinned to v1: ${ka}"
  sleep 5

  info "bumping to v2 — v1 becomes non-latest and, kept alive, gets an operator HPA…"
  deploy_version v2
  wait_until "operator HPA for draining v1 to appear" 180 hpa_count_is 1 \
    || { fail "v1 never got an operator HPA (rollback precondition not met)"; return; }
  local hpa rs
  hpa="$(operator_hpa_names | head -n1)"
  rs="$(kc -n "$APP_NS" get hpa "$hpa" -o jsonpath='{.spec.scaleTargetRef.name}')"
  pass "v1 (${rs}) is draining with operator HPA ${hpa}"

  info "rolling back to v1 (identical spec → re-adopts ${rs} as latest again)…"
  deploy_version v1

  # The fix: ${rs} is latest again, so its leftover operator HPA must be removed.
  # Assert on this specific HPA, not a total count — the (separate, unfixed)
  # registration desync can legitimately leave v2 with its own HPA.
  if wait_until "v1's leftover operator HPA (${rs}) to be removed" 90 hpa_absent "$rs"; then
    pass "operator HPA on ${rs} removed once it was promoted back to latest"
  else
    fail "${rs} still has an operator HPA after rollback (the leftover-HPA bug)"
  fi

  # And its scale must be stable — no flap between propagate-replicas and a leftover HPA.
  info "checking ${rs} replicas are stable (no flap)…"
  local stable=1 prev="" r
  for _ in $(seq 1 8); do
    r="$(rs_replicas "$rs")"; r="${r:-?}"
    info "  ${rs} replicas=${r}"
    if [[ -n "$prev" && "$r" != "$prev" ]]; then stable=0; fi
    prev="$r"
    sleep 6
  done
  if (( stable == 1 )); then
    pass "${rs} replicas stable after rollback (no flap)"
  else
    fail "${rs} replicas flapped after rollback"
  fi
}

if [[ "${RUN_ROLLBACK:-0}" == "1" ]]; then
  run_rollback
  step "Rollback results"
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
e2e_results
