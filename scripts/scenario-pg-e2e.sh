#!/usr/bin/env bash
#
# scenario-pg-e2e.sh — x-backup PostgreSQL E2E 시나리오
#
# 컨테이너(x-backup-postgres, :5432 — docker/docker-compose.postgres.yaml)를 대상으로
# 풀 백업 → 증분 ×2(logical decoding/pgoutput) → 구조 검증 → 풀 복구 → PITR(전체/중간)을
# 끝까지 수행하고 각 단계를 assertion으로 판정한다. IDENTITY(GENERATED ALWAYS)·STORED
# generated·FK·시퀀스 재동기화 같은 까다로운 경로를 의도적으로 포함한다.
#
# 전제(make scenario-pg가 보장):
#   1. target/release/x-backup 빌드됨(make build)
#   2. compose postgres가 healthy + wal_level=logical(make postgres-up)
#
# 단독 실행: scripts/scenario-pg-e2e.sh
#   BIN=경로로 백업/복구 바이너리를 바꿀 수 있으나, age 키 생성은 cargo example을 쓰므로
#   Rust 툴체인 + 소스트리가 여전히 필요하다(아래 cd로 cwd를 repo 루트에 고정한다).
set -euo pipefail

# 호출 위치와 무관하게 상대경로(BIN 기본값·cargo run·scripts/)가 repo 루트 기준이 되게 한다.
cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

BIN="${BIN:-target/release/x-backup}"
CTR="x-backup-postgres"
PGUSER="${XB_PG_USER:-xbackup}"
PGPASS="${XB_PG_PASSWORD:-xbackup-dev}"
PGPORT="${XB_PG_PORT:-5432}"
SRC_DB="xb_scn"
TGT_DB="xb_scn_restore"
PROFILE="pgscn"
SRC_URI="postgres://${PGUSER}:${PGPASS}@localhost:${PGPORT}/${SRC_DB}"
TGT_URI="postgres://${PGUSER}:${PGPASS}@localhost:${PGPORT}/${TGT_DB}"

WORK="$(mktemp -d /tmp/xb-pgscn.XXXXXX)"
PASS=0
FAIL=0

log()  { printf '\033[36m[pg-scenario]\033[0m %s\n' "$*"; }
ok()   { PASS=$((PASS + 1)); printf '  \033[32m✔\033[0m %s\n' "$*"; }
die()  { printf '  \033[31m✘ %s\033[0m\n' "$*"; FAIL=$((FAIL + 1)); summary; exit 1; }
summary() { printf '\n\033[1m[pg-scenario] 결과: PASS=%d FAIL=%d\033[0m\n' "$PASS" "$FAIL"; }
# 성공 시에만 정리 — 실패 시 산출물·슬롯을 남겨 조사 가능하게.
cleanup() {
  if [[ "$FAIL" == "0" && "${SCENARIO_DONE:-0}" == "1" ]]; then
    # DROP DATABASE는 트랜잭션 블록 불가 → 한 -c에 한 문장씩(각자 autocommit).
    psql_admin "SELECT pg_drop_replication_slot('xb_${PROFILE}') WHERE EXISTS(SELECT 1 FROM pg_replication_slots WHERE slot_name='xb_${PROFILE}');" >/dev/null 2>&1 || true
    psql_admin "DROP DATABASE IF EXISTS ${SRC_DB} WITH (FORCE);" >/dev/null 2>&1 || true
    psql_admin "DROP DATABASE IF EXISTS ${TGT_DB} WITH (FORCE);" >/dev/null 2>&1 || true
    rm -rf "$WORK"
  else
    printf '[pg-scenario] 작업 디렉터리 보존: %s\n' "$WORK" >&2
  fi
}
trap cleanup EXIT

# expect_exit <기대코드> <설명> <커맨드...>
expect_exit() {
  local want="$1" desc="$2"; shift 2
  set +e; "$@" >"$WORK/out.log" 2>&1; local got=$?; set -e
  if [[ "$got" == "$want" ]]; then
    ok "$desc (exit $got)"
  else
    sed -n '1,20p' "$WORK/out.log" >&2
    die "$desc — exit $got (기대 $want)"
  fi
}

# psql 헬퍼 — 컨테이너 안에서 실행(호스트 psql 불필요). 단일 값/한 줄 결과용(-tA).
psql_admin() { docker exec -i "$CTR" psql -v ON_ERROR_STOP=1 -U "$PGUSER" -d postgres -tAc "$1"; }
q_src() { docker exec -i "$CTR" psql -v ON_ERROR_STOP=1 -U "$PGUSER" -d "$SRC_DB" -tAc "$1"; }
q_tgt() { docker exec -i "$CTR" psql -v ON_ERROR_STOP=1 -U "$PGUSER" -d "$TGT_DB" -tAc "$1"; }
exec_src() { docker exec -i "$CTR" psql -v ON_ERROR_STOP=1 -q -U "$PGUSER" -d "$SRC_DB"; }
# 대상 DB를 비우고 새로 만든다(복구 타깃 초기화).
reset_target() {
  psql_admin "DROP DATABASE IF EXISTS ${TGT_DB} WITH (FORCE);" >/dev/null
  psql_admin "CREATE DATABASE ${TGT_DB};" >/dev/null
}

# ── 0) 전제 확인 ──────────────────────────────────────────────────────
[[ -x "$BIN" ]] || die "$BIN 없음 — make build"
docker inspect -f '{{.State.Health.Status}}' "$CTR" 2>/dev/null | grep -qx healthy \
  || die "$CTR unhealthy — make postgres-up"
[[ "$(psql_admin 'SHOW wal_level;')" == "logical" ]] \
  || die "wal_level != logical — compose의 command(-c wal_level=logical) 확인 후 make postgres-down && make postgres-up"
ok "전제: 바이너리·컨테이너 healthy·wal_level=logical"

# ── 1) age 키 + config.toml ───────────────────────────────────────────
log "1) age 키쌍·config.toml 준비(zstd + age, 증분 활성)"
cargo run --quiet --release --example gen_age_key -- "$WORK" >/dev/null
export XB_AGE_IDENTITY_FILE="$WORK/age.key"   # 복구 복호화용
export XB_SCN_URI="$SRC_URI"
mkdir -p "$WORK/store"
cat > "$WORK/config.toml" <<EOF
default_profile = "$PROFILE"

[profiles.$PROFILE.mode]
backup_type = "full"
output      = "quiet"

[profiles.$PROFILE.source]
uri_env = "XB_SCN_URI"

[profiles.$PROFILE.destination]
type = "local"
path = "$WORK/store"

[profiles.$PROFILE.features.compression]
algorithm = "zstd"
level     = 3

[profiles.$PROFILE.features.encryption]
enabled        = true
algorithm      = "age"
recipient_file = "$WORK/age.pub"

[profiles.$PROFILE.features.incremental]
pg_logical = true
EOF
ok "config 생성(zstd 3 + age, pg_logical=true)"

xb() { "$BIN" --config "$WORK/config.toml" "$@"; }

# ── 2) 소스 초기화 + 시드(IDENTITY·STORED generated·FK) ───────────────
log "2) 소스 DB 시드(accounts: GENERATED ALWAYS IDENTITY + STORED generated, orders: FK)"
psql_admin "SELECT pg_drop_replication_slot('xb_${PROFILE}') WHERE EXISTS(SELECT 1 FROM pg_replication_slots WHERE slot_name='xb_${PROFILE}');" >/dev/null 2>&1 || true
psql_admin "DROP DATABASE IF EXISTS ${SRC_DB} WITH (FORCE);" >/dev/null
psql_admin "CREATE DATABASE ${SRC_DB};" >/dev/null
reset_target
exec_src >/dev/null <<'SQL'
CREATE TABLE accounts (
  id      bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  email   text NOT NULL UNIQUE,
  balance numeric(12,2) NOT NULL DEFAULT 0,
  tag     text GENERATED ALWAYS AS (upper(email)) STORED
);
CREATE TABLE orders (
  oid    uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  acct   bigint NOT NULL REFERENCES accounts(id),
  amount numeric(10,2) NOT NULL,
  status text NOT NULL DEFAULT 'new'
);
INSERT INTO accounts (email, balance) VALUES ('a@x.io',100.00),('b@x.io',250.50),('c@x.io',0.00);
INSERT INTO orders (acct, amount, status) SELECT id, 9.99, 'paid' FROM accounts WHERE email='a@x.io';
SQL
[[ "$(q_src "SELECT count(*) FROM accounts;")" == "3" ]] || die "시드 실패"
ok "시드: accounts 3 + orders 1"

# ── 3) 풀 백업 → slot/publication 생성 확인 ───────────────────────────
log "3) 풀 백업(slot+publication 생성)"
expect_exit 0 "backup --type full" xb backup --profile "$PROFILE" --type full --quiet
[[ "$(q_src "SELECT plugin FROM pg_replication_slots WHERE slot_name='xb_${PROFILE}';")" == "pgoutput" ]] \
  || die "replication slot(xb_${PROFILE}) 미생성"
[[ "$(q_src "SELECT puballtables FROM pg_publication WHERE pubname='xb_${PROFILE}_pub';")" == "t" ]] \
  || die "publication(xb_${PROFILE}_pub) 미생성"
FULL_ID="$(xb list --profile "$PROFILE" --json | python3 -c "import json,sys;b=[e for e in json.load(sys.stdin)['backups'] if e['type']=='full'];b.sort(key=lambda e:e['created_at']);print(b[0]['id'])")"
[[ -s "$WORK/store/$FULL_ID/data.bin" && -s "$WORK/store/$FULL_ID/manifest.json" ]] || die "풀 산출물 누락"
ok "풀 백업 $FULL_ID — slot(pgoutput)+publication(all) 생성, 산출물 확인"

# ── 4) DML A → 증분1 → 기준점 → DML B → 증분2 ────────────────────────
log "4) 증분 체인 구성(A → incr1 → 기준점 → B → incr2)"
exec_src >/dev/null <<'SQL'
INSERT INTO accounts (email, balance) VALUES ('d@x.io',42.00);
UPDATE accounts SET balance = 1100 WHERE email='a@x.io';
DELETE FROM accounts WHERE email='c@x.io';
INSERT INTO orders (acct, amount, status) SELECT id, 5.00, 'new' FROM accounts WHERE email='b@x.io';
SQL
sleep 1.2
expect_exit 0 "backup --type incr (A 캡처)" xb backup --profile "$PROFILE" --type incr --quiet
sleep 1.2
# 경계 비교는 commit ts(컨테이너 PG 클럭) 기준이므로 PITR_AT도 같은 클럭에서 따온다 —
# 호스트-컨테이너 클럭 스큐를 제거한다. 초 단위 RFC3339 유지(A 이후·B 이전).
PITR_AT="$(q_src "SELECT to_char(clock_timestamp() AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')")"
sleep 1.2
exec_src >/dev/null <<'SQL'
UPDATE accounts SET email='a2@x.io', balance=0 WHERE email='a@x.io';
INSERT INTO accounts (email, balance) VALUES ('e@x.io',7.77);
DELETE FROM accounts WHERE email='d@x.io';
UPDATE orders SET status='shipped';
SQL
sleep 1.2
expect_exit 0 "backup --type incr (B 캡처)" xb backup --profile "$PROFILE" --type incr --quiet
ok "체인: full + incr ×2, PITR 기준점 $PITR_AT"

# 변경 없는 증분(빈 슬라이스)도 exit 0이어야 한다.
expect_exit 0 "backup --type incr (빈 슬라이스)" xb backup --profile "$PROFILE" --type incr --quiet
ok "빈 슬라이스 증분(변경 0건) 정상 처리"

# ── 5) 구조 검증(엔진 무관 체크섬) ───────────────────────────────────
log "5) verify 구조(data.bin sha256 재계산)"
expect_exit 0 "verify --id(구조)" xb verify --id "$FULL_ID"

# ── 6) 풀 복구(증분 미반영 = base 스냅샷) ─────────────────────────────
log "6) 풀 복구 → $TGT_DB (base 스냅샷만 — A/B 미반영)"
reset_target
expect_exit 0 "restore --target --force(풀)" \
  xb restore --profile "$PROFILE" --target "$TGT_URI" --force --quiet
[[ "$(q_tgt "SELECT count(*) FROM accounts;")" == "3" ]] || die "풀 복구 accounts 수 불일치(기대 3)"
[[ "$(q_tgt "SELECT count(*) FROM accounts WHERE email='c@x.io';")" == "1" ]] || die "풀 복구는 base여야(c 존재)"
[[ "$(q_tgt "SELECT count(*) FROM accounts WHERE email='d@x.io';")" == "0" ]] || die "풀 복구에 증분(d) 혼입"
[[ "$(q_tgt "SELECT tag FROM accounts WHERE email='a@x.io';")" == "A@X.IO" ]] || die "STORED generated 컬럼 복원 오류"
ok "풀 복구 정합: base 스냅샷(accounts 3, c 존재, d 없음, STORED tag 정상)"

# ── 7) PITR 전체(--at latest = 소스 최종 상태) ───────────────────────
log "7-1) PITR --at latest → 소스 최종과 일치"
reset_target
expect_exit 0 "restore --at latest" \
  xb restore --profile "$PROFILE" --target "$TGT_URI" --at latest --force --quiet
# 소스와 복구의 (email,balance,tag) 집합이 동일해야 한다.
SRC_SNAP="$(q_src "SELECT string_agg(email||'|'||balance||'|'||tag, ',' ORDER BY email) FROM accounts;")"
TGT_SNAP="$(q_tgt "SELECT string_agg(email||'|'||balance||'|'||tag, ',' ORDER BY email) FROM accounts;")"
[[ "$SRC_SNAP" == "$TGT_SNAP" ]] || die "PITR(latest) accounts 불일치: src=[$SRC_SNAP] tgt=[$TGT_SNAP]"
[[ "$(q_tgt "SELECT count(*) FROM orders WHERE status='shipped';")" == "2" ]] || die "PITR(latest) orders 불일치"
ok "PITR(latest) 정합: 소스와 동일 [$TGT_SNAP]"

# 시퀀스 재동기화: 복구 후 새 insert가 PK 충돌 없이 max+1을 받아야 한다.
# (RETURNING은 명령 태그가 섞여 산술 비교를 깨므로 insert와 SELECT를 분리)
MAX_BEFORE="$(q_tgt "SELECT max(id) FROM accounts;")"
expect_exit 0 "복구 후 새 insert(시퀀스 정합 — PK 충돌 없음)" \
  docker exec -i "$CTR" psql -v ON_ERROR_STOP=1 -q -U "$PGUSER" -d "$TGT_DB" \
    -c "INSERT INTO accounts(email,balance) VALUES('post@x.io',1);"
NEXT_ID="$(q_tgt "SELECT id FROM accounts WHERE email='post@x.io';")"
[[ "$NEXT_ID" -gt "$MAX_BEFORE" ]] || die "시퀀스 재동기화 실패(new id=$NEXT_ID, max=$MAX_BEFORE)"
ok "시퀀스 재동기화 정합: 복구 후 새 id=$NEXT_ID (> 기존 max=$MAX_BEFORE)"

# ── 8) PITR 중간(--at 기준점 = A만, B 미반영) ────────────────────────
log "8) PITR --at $PITR_AT (A 이후·B 이전)"
reset_target
expect_exit 0 "restore --at(중간 시점)" \
  xb restore --profile "$PROFILE" --target "$TGT_URI" --at "$PITR_AT" --force --quiet
[[ "$(q_tgt "SELECT count(*) FROM accounts WHERE email='d@x.io';")" == "1" ]] || die "PITR(중간): A의 d 누락"
[[ "$(q_tgt "SELECT balance FROM accounts WHERE email='a@x.io';")" == "1100.00" ]] || die "PITR(중간): A의 update 누락"
[[ "$(q_tgt "SELECT count(*) FROM accounts WHERE email='e@x.io';")" == "0" ]] || die "PITR(중간): B의 e 혼입(기준점 이후)"
[[ "$(q_tgt "SELECT count(*) FROM accounts WHERE email='a2@x.io';")" == "0" ]] || die "PITR(중간): B의 rename 혼입"
[[ "$(q_tgt "SELECT count(*) FROM orders WHERE status='shipped';")" == "0" ]] || die "PITR(중간): B의 orders update 혼입"
ok "PITR(중간) 정합: A 반영(d 존재, a=1100), B 미반영(e/a2/shipped 없음)"

# 부분 재생(중간 PITR) 후에도 시퀀스가 그 부분 상태의 max로 재동기화돼야 한다 — latest와
# max가 다르므로(중간엔 d=4까지) 별도 검증(부분 재생 시퀀스 회귀 방지).
MID_MAX="$(q_tgt "SELECT max(id) FROM accounts;")"
expect_exit 0 "중간 PITR 후 새 insert(부분 상태 시퀀스 정합)" \
  docker exec -i "$CTR" psql -v ON_ERROR_STOP=1 -q -U "$PGUSER" -d "$TGT_DB" \
    -c "INSERT INTO accounts(email,balance) VALUES('post-mid@x.io',1);"
MID_NEXT="$(q_tgt "SELECT id FROM accounts WHERE email='post-mid@x.io';")"
[[ "$MID_NEXT" -gt "$MID_MAX" ]] || die "중간 PITR 시퀀스 재동기화 실패(new=$MID_NEXT, max=$MID_MAX)"
ok "중간 PITR 시퀀스 재동기화 정합: 새 id=$MID_NEXT (> 기존 max=$MID_MAX)"

summary
[[ "$FAIL" == "0" ]] || exit 1
SCENARIO_DONE=1
log "전체 PG 시나리오 PASS"
