#!/usr/bin/env bash
#
# scenario-e2e.sh — x-backup E2E 시나리오 (docs/test-scenario.md의 실행체)
#
# 컨테이너(소스 :27017, 타깃 :27117 — docker/docker-compose.mongodb.yaml)를 대상으로
# 풀 백업 → 증분 ×2 → 검증(구조/--deep/--chain) → 타깃 분리 풀 복구 → PITR을
# 끝까지 수행하고 각 단계를 assertion으로 판정한다.
#
# 전제(make scenario가 보장):
#   1. target/release/x-backup 빌드됨
#   2. mongodump/mongorestore가 PATH에 있음(make tools)
#   3. compose mongodb 2식이 healthy(make mongodb-up)
#
# 단독 실행: PATH=".tools/bin:$PATH" scripts/scenario-e2e.sh
set -euo pipefail

BIN="${BIN:-target/release/x-backup}"
SRC_PORT="${XB_MONGO_PORT:-27017}"
TGT_PORT="${XB_MONGO_TARGET_PORT:-27117}"
SRC_URI="mongodb://localhost:${SRC_PORT}/?replicaSet=rs0&directConnection=true"
TGT_URI="mongodb://localhost:${TGT_PORT}/?replicaSet=rs0&directConnection=true"
SRC_CTR="x-backup-mongo"
TGT_CTR="x-backup-mongo-target"
DBNAME="scenario"

WORK="$(mktemp -d /tmp/xb-scenario.XXXXXX)"
PASS=0
FAIL=0

log()  { printf '\033[36m[scenario]\033[0m %s\n' "$*"; }
ok()   { PASS=$((PASS + 1)); printf '  \033[32m✔\033[0m %s\n' "$*"; }
die()  { printf '  \033[31m✘ %s\033[0m\n' "$*"; FAIL=$((FAIL + 1)); summary; exit 1; }
summary() {
  printf '\n\033[1m[scenario] 결과: PASS=%d FAIL=%d\033[0m\n' "$PASS" "$FAIL"
}
# 성공 시에만 정리 — 실패 시 산출물을 남겨 조사 가능하게(경로 출력).
cleanup() {
  if [[ "$FAIL" == "0" && "${SCENARIO_DONE:-0}" == "1" ]]; then
    rm -rf "$WORK"
  else
    printf '[scenario] 작업 디렉터리 보존: %s\n' "$WORK" >&2
  fi
}
trap cleanup EXIT

# expect_exit <기대코드> <설명> <커맨드...>  — exit code까지 계약대로인지 검사
expect_exit() {
  local want="$1" desc="$2"; shift 2
  set +e; "$@" >"$WORK/out.log" 2>&1; local got=$?; set -e
  if [[ "$got" == "$want" ]]; then
    ok "$desc (exit $got)"
  else
    sed -n '1,15p' "$WORK/out.log" >&2
    die "$desc — exit $got (기대 $want)"
  fi
}

mongosh_src() { docker exec "$SRC_CTR" mongosh --quiet --eval "$1"; }
mongosh_tgt() { docker exec "$TGT_CTR" mongosh --quiet --eval "$1"; }

count_src() { mongosh_src "db.getSiblingDB('$DBNAME').$1.countDocuments($2)"; }
count_tgt() { mongosh_tgt "db.getSiblingDB('$DBNAME').$1.countDocuments($2)"; }

# list --json에서 백업 id 추출: $1 = full|incr, $2 = 인덱스(생성순)
backup_id() {
  "$BIN" list --config "$WORK/config.toml" --profile scenario --json |
    python3 -c "
import json, sys
entries = [e for e in json.load(sys.stdin)['backups'] if e['type'] == '$1']
entries.sort(key=lambda e: e['created_at'])
print(entries[$2]['id'])
"
}

# ── 0) 전제 확인 ──────────────────────────────────────────────────────
command -v mongodump >/dev/null || die "mongodump가 PATH에 없음 — make tools"
[[ -x "$BIN" ]] || die "$BIN 없음 — make build"
docker inspect -f '{{.State.Health.Status}}' "$SRC_CTR" | grep -qx healthy || die "$SRC_CTR unhealthy — make mongodb-up"
docker inspect -f '{{.State.Health.Status}}' "$TGT_CTR" | grep -qx healthy || die "$TGT_CTR unhealthy — make mongodb-up"
ok "전제: 바이너리·도구·컨테이너 2식 healthy"

# ── 1) 키·config 준비 ────────────────────────────────────────────────
log "1) age 키쌍·config.toml 준비"
cargo run --quiet --release --example gen_age_key -- "$WORK" >/dev/null
export XB_AGE_IDENTITY_FILE="$WORK/age.key"   # --deep 검증·복구 복호화용(§8.5)
export MONGO_URI="$SRC_URI"
mkdir -p "$WORK/store"
cat > "$WORK/config.toml" <<EOF
default_profile = "scenario"

[profiles.scenario.mode]
backup_type = "full"
output      = "quiet"
precheck    = true

[profiles.scenario.source]
uri_env = "MONGO_URI"

[profiles.scenario.destination]
type = "local"
path = "$WORK/store"

[profiles.scenario.features.compression]
algorithm = "zstd"
level     = 3

[profiles.scenario.features.encryption]
enabled        = true
algorithm      = "age"
recipient_file = "$WORK/age.pub"
EOF
ok "config 생성(zstd 3 + age, 시크릿은 env 참조만)"

# ── 2) 소스 시드 + 사전 점검 ─────────────────────────────────────────
log "2) 데이터 시드 + status 사전 점검"
mongosh_src "
  const db2 = db.getSiblingDB('$DBNAME');
  db2.dropDatabase();
  db2.events.insertMany(Array.from({length: 1000}, (_, i) => ({seq: i, phase: 'seed', pad: 'x'.repeat(256)})));
  db2.users.insertMany(Array.from({length: 200}, (_, i) => ({uid: i, name: 'user' + i})));
  db2.users.createIndex({uid: 1}, {unique: true});
" >/dev/null
[[ "$(count_src events '{}')" == "1000" ]] || die "시드 실패"
ok "시드: events 1000 + users 200(unique index)"

# status: 정상(0) 또는 경고 동반(4 — 작은 oplog 윈도우 등)을 허용
set +e; "$BIN" status --config "$WORK/config.toml" --profile scenario --json >"$WORK/status.json" 2>/dev/null; rc=$?; set -e
[[ "$rc" == "0" || "$rc" == "4" ]] || die "status 실패(exit $rc)"
ok "status --json (exit $rc)"

# ── 3) 풀 백업 ───────────────────────────────────────────────────────
log "3) 풀 백업"
expect_exit 0 "backup --type full" \
  "$BIN" backup --config "$WORK/config.toml" --profile scenario --type full --quiet
FULL_ID="$(backup_id full 0)"
[[ -s "$WORK/store/$FULL_ID/data.bin" && -s "$WORK/store/$FULL_ID/manifest.json" && -s "$WORK/store/$FULL_ID/manifest.json.sha256" ]] \
  || die "산출물 3종(data/manifest/사이드카) 누락"
# 평문 시그니처 부재(SC3): mongodump archive magic(6d e2 99 81)이 보이면 안 된다
if head -c 1048576 "$WORK/store/$FULL_ID/data.bin" | xxd -p | tr -d '\n' | grep -q "6de29981"; then
  die "산출물에 평문 archive magic 노출 — 암호화 미동작"
fi
ok "풀 백업 $FULL_ID — 산출물 3종 + 암호문 확인"

# ── 4) 쓰기 A → 증분1 → (PITR 기준점) → 쓰기 B → 증분2 ──────────────
log "4) 증분 체인 구성(A → incr1 → 기준점 → B → incr2)"
mongosh_src "db.getSiblingDB('$DBNAME').events.insertMany(Array.from({length: 50}, (_, i) => ({seq: 10000 + i, phase: 'A'})))" >/dev/null
sleep 1.2   # 초 경계 확보 — --at은 초 단위 RFC3339(PRD FR-3)
expect_exit 0 "backup --type incr (A 캡처)" \
  "$BIN" backup --config "$WORK/config.toml" --profile scenario --type incr --quiet
sleep 1.2
PITR_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"   # A 이후·B 이전 시점
sleep 1.2
mongosh_src "db.getSiblingDB('$DBNAME').events.insertMany(Array.from({length: 50}, (_, i) => ({seq: 20000 + i, phase: 'B'})))" >/dev/null
sleep 1.2
expect_exit 0 "backup --type incr (B 캡처)" \
  "$BIN" backup --config "$WORK/config.toml" --profile scenario --type incr --quiet
INCR2_ID="$(backup_id incr 1)"
AFTER_B="$(date -u +%Y-%m-%dT%H:%M:%SZ)"   # B까지 전부 포함하는 PITR 시점
ok "체인: full + incr ×2, PITR 기준점 $PITR_AT"

# ── 5) 검증 3종 ──────────────────────────────────────────────────────
log "5) verify 구조 / --deep / --chain"
expect_exit 0 "verify 구조(키 불필요)" \
  "$BIN" verify --config "$WORK/config.toml" --id "$FULL_ID"
expect_exit 0 "verify --deep(개인키 보유 호스트)" \
  "$BIN" verify --config "$WORK/config.toml" --id "$FULL_ID" --deep
expect_exit 0 "verify --chain(PITR 전제)" \
  "$BIN" verify --config "$WORK/config.toml" --id "$INCR2_ID" --chain
( unset XB_AGE_IDENTITY_FILE
  expect_exit 1 "verify --deep는 키 부재 시 거부(§8.5)" \
    "$BIN" verify --config "$WORK/config.toml" --id "$FULL_ID" --deep )

# ── 6) 타깃 분리 풀 복구(UC-3) — base만 복원, 증분 미반영이 정상 ─────
log "6) 풀 복구 → 타깃(:$TGT_PORT) — base 스냅샷만"
expect_exit 0 "restore --dry-run(무변경 계획)" \
  "$BIN" restore --config "$WORK/config.toml" --profile scenario --target "$TGT_URI" --dry-run
[[ "$(count_tgt events '{}')" == "0" ]] || die "--dry-run이 타깃을 변경함"
expect_exit 0 "restore --target --force" \
  "$BIN" restore --config "$WORK/config.toml" --profile scenario --target "$TGT_URI" --force --quiet
[[ "$(count_tgt events '{}')" == "1000" ]] || die "풀 복구 events 수 불일치(base 시드=1000 기대)"
[[ "$(count_tgt users '{}')"  == "200"  ]] || die "풀 복구 users 수 불일치"
[[ "$(count_tgt events "{phase:'A'}")" == "0" && "$(count_tgt events "{phase:'B'}")" == "0" ]] \
  || die "풀 복구는 base만 복원해야 함 — 증분(A/B)이 섞임"
ok "풀 복구 정합: base 스냅샷만(events 1000 / users 200, A·B 없음 — PRD FR-3)"

# ── 7) PITR(--at) — ① 전체 체인 재생(A+B) ② 중간 시점(A만) ──────────
log "7-1) PITR → --at $AFTER_B (체인 전체 — A·B 포함)"
mongosh_tgt "db.getSiblingDB('$DBNAME').dropDatabase()" >/dev/null
expect_exit 2 "PITR + --only 병용은 즉시 거부(mongorestore 제약)" \
  "$BIN" restore --config "$WORK/config.toml" --profile scenario --target "$TGT_URI" \
        --at "$AFTER_B" --only "$DBNAME.events" --force
expect_exit 0 "restore --at(B 이후 시점 — 전체 재생)" \
  "$BIN" restore --config "$WORK/config.toml" --profile scenario --target "$TGT_URI" \
        --at "$AFTER_B" --force --quiet
[[ "$(count_tgt events '{}')" == "$(count_src events '{}')" ]] || die "PITR(전체): 소스와 events 수 불일치"
[[ "$(count_tgt events "{phase:'A'}")" == "50" && "$(count_tgt events "{phase:'B'}")" == "50" ]] \
  || die "PITR(전체): A/B 재생 누락"
ok "PITR(전체) 정합: 소스와 동일(A=50, B=50)"

log "7-2) PITR → --at $PITR_AT (A 이후·B 이전 — SC4)"
mongosh_tgt "db.getSiblingDB('$DBNAME').dropDatabase()" >/dev/null
expect_exit 0 "restore --at(중간 시점)" \
  "$BIN" restore --config "$WORK/config.toml" --profile scenario --target "$TGT_URI" \
        --at "$PITR_AT" --force --quiet
[[ "$(count_tgt events "{phase:'A'}")" == "50" ]] || die "PITR: A(기준점 이전) 누락"
[[ "$(count_tgt events "{phase:'B'}")" == "0"  ]] || die "PITR: B(기준점 이후)가 포함됨 — SC4 위반"
ok "PITR(중간) 정합: A=50 존재, B=0 부재 (SC4)"

# ── 8) prune 가드 ────────────────────────────────────────────────────
log "8) prune --dry-run(체인 보호)"
expect_exit 0 "prune --keep-full 1 --dry-run" \
  "$BIN" prune --config "$WORK/config.toml" --profile scenario --keep-full 1 --dry-run
[[ -s "$WORK/store/$FULL_ID/data.bin" ]] || die "--dry-run이 산출물을 삭제함"
ok "prune dry-run 무변경 + 유일 체인 보존"

summary
[[ "$FAIL" == "0" ]] || exit 1
SCENARIO_DONE=1
log "전체 시나리오 PASS"
