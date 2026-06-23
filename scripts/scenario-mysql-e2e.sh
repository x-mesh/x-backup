#!/usr/bin/env bash
#
# scenario-mysql-e2e.sh — x-backup MySQL E2E 시나리오
#
# 컨테이너(x-backup-mysql, :3306 — docker/docker-compose.mysql.yaml)를 대상으로
# 풀 백업 → 증분(binlog ROW) → 구조 검증 → 풀 복구 → PITR(전체/중간)을 끝까지 수행하고 각
# 단계를 assertion으로 판정한다. DECIMAL·DATETIME(6)·JSON·VARBINARY·STORED generated·FK·
# AUTO_INCREMENT 같은 까다로운 경로를 의도적으로 포함한다. 전 구간 zstd 압축 + age 암호화.
#
# 전제(make scenario-mysql가 보장):
#   1. target/release/x-backup 빌드됨(make build)
#   2. compose mysql이 healthy + binlog/gtid ON(make mysql-up)
#
# 단독 실행: scripts/scenario-mysql-e2e.sh
set -euo pipefail

cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

BIN="${BIN:-target/release/x-backup}"
CTR="x-backup-mysql"
MYPASS="${XB_MYSQL_PASSWORD:-xbackup-dev}"
MYPORT="${XB_MYSQL_PORT:-3306}"
SRC_DB="xb_scn"
TGT_DB="xb_scn_restore"
PROFILE="myscn"
SRC_URI="mysql://root:${MYPASS}@127.0.0.1:${MYPORT}/${SRC_DB}"
TGT_URI="mysql://root:${MYPASS}@127.0.0.1:${MYPORT}/${TGT_DB}"

WORK="$(mktemp -d /tmp/xb-myscn.XXXXXX)"
PASS=0
FAIL=0

log()  { printf '\033[36m[mysql-scenario]\033[0m %s\n' "$*"; }
ok()   { PASS=$((PASS + 1)); printf '  \033[32m✔\033[0m %s\n' "$*"; }
die()  { printf '  \033[31m✘ %s\033[0m\n' "$*"; FAIL=$((FAIL + 1)); summary; exit 1; }
summary() { printf '\n\033[1m[mysql-scenario] 결과: PASS=%d FAIL=%d\033[0m\n' "$PASS" "$FAIL"; }
cleanup() {
  if [[ "$FAIL" == "0" && "${SCENARIO_DONE:-0}" == "1" ]]; then
    my_admin "DROP DATABASE IF EXISTS ${SRC_DB}; DROP DATABASE IF EXISTS ${TGT_DB};" >/dev/null 2>&1 || true
    rm -rf "$WORK"
  else
    printf '[mysql-scenario] 작업 디렉터리 보존: %s\n' "$WORK" >&2
  fi
}
trap cleanup EXIT

# mysql 헬퍼 — 컨테이너 안에서 실행(호스트 mysql 불필요). -N(헤더 없음) -s(탭 구분) 단일 값용.
my_admin() { docker exec -i "$CTR" mysql -uroot -p"$MYPASS" -Nse "$1" 2>/dev/null; }
q_src()    { docker exec -i "$CTR" mysql -uroot -p"$MYPASS" "$SRC_DB" -Nse "$1" 2>/dev/null; }
q_tgt()    { docker exec -i "$CTR" mysql -uroot -p"$MYPASS" "$TGT_DB" -Nse "$1" 2>/dev/null; }
exec_src() { docker exec -i "$CTR" mysql -uroot -p"$MYPASS" "$SRC_DB" 2>/dev/null; }
reset_target() {
  my_admin "DROP DATABASE IF EXISTS ${TGT_DB}; CREATE DATABASE ${TGT_DB};" >/dev/null
}

# ── Phase 0: 전제 점검 ────────────────────────────────────────────────
log "Phase 0 — 전제 점검"
[[ -x "$BIN" ]] || die "바이너리 없음: $BIN (make build)"
docker inspect -f '{{.State.Health.Status}}' "$CTR" 2>/dev/null | grep -q healthy \
  || die "컨테이너 $CTR 가 healthy 아님 (make mysql-up)"
BL=$(my_admin "SELECT @@log_bin;")
[[ "$BL" == "1" ]] || die "log_bin 비활성 — 증분 불가"
ok "전제 OK (바이너리·컨테이너 healthy·log_bin=ON)"

# ── Phase 1: age 키 + config ──────────────────────────────────────────
log "Phase 1 — age 키 + config(zstd + age, mysql_binlog=true)"
cargo run --quiet --release --example gen_age_key -- "$WORK" >/dev/null
export XB_AGE_IDENTITY_FILE="$WORK/age.key"   # 복구 복호화용
export XB_SCN_URI="$SRC_URI"
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
mysql_binlog = true
EOF
ok "age 키 + config 준비"

run() { "$BIN" --config "$WORK/config.toml" "$@"; }

# ── Phase 2: 소스 시드(까다로운 타입) ─────────────────────────────────
log "Phase 2 — 소스 시드"
my_admin "DROP DATABASE IF EXISTS ${SRC_DB}; CREATE DATABASE ${SRC_DB};" >/dev/null
exec_src <<'SQL'
CREATE TABLE accounts (
  id INT AUTO_INCREMENT PRIMARY KEY,
  email VARCHAR(255) UNIQUE,
  balance DECIMAL(12,2),
  data JSON,
  blob_col VARBINARY(16),
  created DATETIME(6),
  tag VARCHAR(300) GENERATED ALWAYS AS (UPPER(email)) STORED
);
INSERT INTO accounts (email,balance,data,blob_col,created) VALUES
  ('a@x.com',100.00,'{"k":1}',0x00FF10,'2026-06-01 10:00:00.123456'),
  ('b@x.com',200.00,'{"k":2}',X'1234','2026-06-02 11:00:00'),
  ('c@x.com',300.00,NULL,'','2026-06-03 12:00:00');
CREATE TABLE orders (oid INT AUTO_INCREMENT PRIMARY KEY, acct INT, amount DECIMAL(10,2),
  FOREIGN KEY (acct) REFERENCES accounts(id));
INSERT INTO orders (acct,amount) VALUES (1,50.00),(2,75.50);
CREATE VIEW account_summary AS SELECT id,email,balance FROM accounts;
SQL
[[ "$(q_src 'SELECT COUNT(*) FROM accounts')" == "3" ]] || die "시드 실패"
ok "소스 시드(accounts 3 + orders 2 + view)"

# ── Phase 3: 풀 백업 ──────────────────────────────────────────────────
log "Phase 3 — 풀 백업"
run backup --profile "$PROFILE" >/dev/null
BACKUPS=$(run list --profile "$PROFILE" --json 2>/dev/null | grep -c '"id"' || true)
[[ "$(run list --profile "$PROFILE" 2>/dev/null | grep -c full)" -ge 1 ]] || die "풀 백업 미생성"
ok "풀 백업 생성"

# ── Phase 4: 증분 체인(배치 A → 마커 → 배치 B) ───────────────────────
log "Phase 4 — 변경 + 증분 캡처(binlog ROW)"
q_src "INSERT INTO accounts (email,balance,created) VALUES ('d@x.com',400.00,'2026-06-10 09:00:00'); UPDATE accounts SET balance=111.00 WHERE email='a@x.com';"
sleep 2; MARKER=$(date -u +%Y-%m-%dT%H:%M:%SZ); sleep 2
q_src "DELETE FROM accounts WHERE email='c@x.com'; INSERT INTO accounts (email,balance,created) VALUES ('e@x.com',500.00,'2026-06-11 09:00:00');"
run backup --profile "$PROFILE" --type incr >/dev/null
INCR=$(run list --profile "$PROFILE" 2>/dev/null | grep -c incr || true)
[[ "$INCR" -ge 1 ]] || die "증분 미생성"
ok "증분 백업 생성(배치 A+B 캡처)"

# ── Phase 5: 풀 복구(base만) ──────────────────────────────────────────
log "Phase 5 — 풀 복구(base only) → target"
reset_target
FULL_ID=$(run list --profile "$PROFILE" --json 2>/dev/null | python3 -c "import json,sys
d=json.load(sys.stdin)
for b in d.get('backups',[]):
  if b.get('type')=='full': print(b['id']); break")
[[ -n "$FULL_ID" ]] || die "풀 백업 ID 추출 실패"
run restore --profile "$PROFILE" --id "$FULL_ID" --target "$TGT_URI" --force >/dev/null
[[ "$(q_tgt "SELECT COUNT(*) FROM accounts")" == "3" ]] || die "base 복구 행 수 불일치"
[[ "$(q_tgt "SELECT balance FROM accounts WHERE email='a@x.com'")" == "100.00" ]] || die "base a=100.00 아님"
[[ "$(q_tgt "SELECT balance FROM accounts WHERE email='c@x.com'")" == "300.00" ]] || die "base c=300.00 아님"
[[ "$(q_tgt "SELECT tag FROM accounts WHERE email='a@x.com'")" == "A@X.COM" ]] || die "STORED generated tag 미복원"
[[ "$(q_tgt "SELECT COUNT(*) FROM account_summary")" == "3" ]] || die "view 미복원"
ok "풀 복구 — 행 수·decimal·generated·view 일치(증분 미적용)"

# ── Phase 6: PITR latest(전체 재생) ──────────────────────────────────
log "Phase 6 — PITR latest"
run restore --profile "$PROFILE" --target "$TGT_URI" --at latest --force >/dev/null
[[ "$(q_tgt "SELECT COUNT(*) FROM accounts")" == "4" ]] || die "PITR latest 행 수≠4"
[[ "$(q_tgt "SELECT balance FROM accounts WHERE email='a@x.com'")" == "111.00" ]] || die "PITR latest a≠111(update 미반영)"
[[ -z "$(q_tgt "SELECT id FROM accounts WHERE email='c@x.com'")" ]] || die "PITR latest c 미삭제(delete 미반영)"
[[ "$(q_tgt "SELECT balance FROM accounts WHERE email='e@x.com'")" == "500.00" ]] || die "PITR latest e 미추가(insert 미반영)"
ok "PITR latest — update/delete/insert 전부 반영"

# ── Phase 7: PITR 중간(배치 A까지만) ─────────────────────────────────
log "Phase 7 — PITR --at $MARKER (배치 A까지)"
run restore --profile "$PROFILE" --target "$TGT_URI" --at "$MARKER" --force >/dev/null
[[ "$(q_tgt "SELECT balance FROM accounts WHERE email='a@x.com'")" == "111.00" ]] || die "PITR mid a≠111(배치 A 미반영)"
[[ "$(q_tgt "SELECT balance FROM accounts WHERE email='d@x.com'")" == "400.00" ]] || die "PITR mid d 미추가(배치 A 미반영)"
[[ "$(q_tgt "SELECT balance FROM accounts WHERE email='c@x.com'")" == "300.00" ]] || die "PITR mid c≠300(배치 B delete가 잘못 반영)"
[[ -z "$(q_tgt "SELECT id FROM accounts WHERE email='e@x.com'")" ]] || die "PITR mid e 존재(배치 B insert가 잘못 반영)"
ok "PITR 중간 — 배치 A만 반영, 배치 B 필터됨(1초 경계)"

SCENARIO_DONE=1
summary
[[ "$FAIL" == "0" ]]
