#!/usr/bin/env bash
#
# memory-profile.sh — x-backup 상주 메모리(RSS) 상한 실측 하네스 (PRD §11.1 #2)
#
# 목적:
#   "데이터 크기와 무관하게 x-backup 프로세스의 상주 메모리(RSS)가 상수 상한
#    (목표 ≤512MiB)을 유지한다"를 **실측**으로 입증한다. 작은 데이터셋과 큰
#    데이터셋을 같은 파이프라인(기본 경로: zstd + age 암호화)으로 백업/복구하면서
#    x-backup 프로세스의 피크 RSS를 0.5s 간격으로 폴링해 비교한다. RSS가 데이터
#    크기에 비례하지 않고 평탄하면, 전 구간 스트리밍(메모리 상수) 가설이 참이다.
#
# 측정 방법(정직성):
#   - x-backup 본체 RSS: ./target/release/x-backup 프로세스를 `ps -o rss=`로 폴링.
#   - mongodump/mongorestore 자식 RSS: 별도로 폴링해 **참고치**로 기록(x-backup이
#     오케스트레이션하는 외부 도구이며 PRD 상한의 직접 대상은 x-backup 본체다).
#   - `ps rss`는 macOS/Linux 모두 KiB 단위. 본 스크립트는 MiB로 환산해 보고한다.
#   - 폴링 방식이라 0.5s 사이의 순간 스파이크는 놓칠 수 있다(샘플링 한계). 대신
#     백업/복구는 수십 초~분 단위라 충분한 표본을 얻는다.
#
# 전제(이 호스트에서 미리 준비):
#   1. cargo build --release 완료 (./target/release/x-backup 존재).
#   2. MongoDB Database Tools(mongodump/mongorestore)가 PATH에 있음.
#   3. mongosh가 PATH에 있음.
#   4. age 키쌍(공개키 recipient 파일)이 준비됨.
#   5. 소스 replica set이 떠 있고 데이터가 시드됨(아래 ENV로 지정).
#
# 사용법:
#   WORK=/tmp/xb-memprof \
#   XB_BIN=/path/to/target/release/x-backup \
#   SOURCE_URI="mongodb://localhost:27090/?replicaSet=rsmem&directConnection=true" \
#   RESTORE_URI="mongodb://localhost:27091/?replicaSet=rsrst&directConnection=true" \
#   AGE_PUB=/tmp/xb-memprof/age.pub \
#   scripts/memory-profile.sh <dataset-label> <db-name>
#
#   예) scripts/memory-profile.sh small small
#       scripts/memory-profile.sh large large
#
# 출력: WORK/result-<label>.txt 에 피크 RSS(백업/복구, x-backup vs 자식)와
#       데이터셋 크기를 기록하고, stdout에도 요약을 찍는다.

set -euo pipefail

LABEL="${1:?dataset label required (예: small|large)}"
DBNAME="${2:?db name required (예: small|large)}"

WORK="${WORK:-/tmp/xb-memprof}"
XB_BIN="${XB_BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/release/x-backup}"
SOURCE_URI="${SOURCE_URI:?SOURCE_URI required}"
RESTORE_URI="${RESTORE_URI:?RESTORE_URI required}"
AGE_PUB="${AGE_PUB:-$WORK/age.pub}"
# 복구는 age 개인키(identity)로 복호화한다 — XB_AGE_IDENTITY_FILE로 주입(§8.5 키 격리).
AGE_KEY="${AGE_KEY:-$WORK/age.key}"
POLL_INTERVAL="${POLL_INTERVAL:-0.5}"

DEST="$WORK/dest-$LABEL"
CFG="$WORK/config-$LABEL.toml"
RESULT="$WORK/result-$LABEL.txt"

mkdir -p "$DEST"

log() { printf '[memprof] %s\n' "$*" >&2; }

# ── config.toml 생성(기본 경로: zstd level 10 + age 암호화 ON) ──
write_config() {
  cat > "$CFG" <<EOF
default_profile = "mem"

[profiles.mem.mode]
backup_type = "full"
output      = "quiet"
precheck    = true

[profiles.mem.source]
uri_env = "MONGO_URI"

[profiles.mem.destination]
type = "local"
path = "$DEST"

[profiles.mem.features.compression]
algorithm = "zstd"
level     = 10

[profiles.mem.features.encryption]
enabled        = true
algorithm      = "age"
recipient_file = "$AGE_PUB"
EOF
}

# ── RSS 폴러: 주어진 pgrep 패턴에 매칭되는 프로세스들의 합산 RSS(KiB) 피크를 추적 ──
# 인자: $1 = pgrep -f 패턴, $2 = 피크를 쓸 파일
# 동작: 부모 프로세스가 끝날 때까지(또는 패턴 미매칭이 연속되면) 폴링.
poll_peak() {
  local pattern="$1" out="$2" parent_pid="$3"
  local peak=0
  # 샘플링 루프 안에서는 errexit/pipefail을 끈다 — pgrep이 순간적으로 매칭 0건이면
  # 비정상 종료(rc!=0)하는데, 그건 정상(자식이 아직/이미 없음)이지 폴러 종료 사유가
  # 아니다. pipefail+set -e면 그 순간 폴러가 죽어 피크를 0으로 보고하게 된다(버그).
  set +e +o pipefail
  while kill -0 "$parent_pid" 2>/dev/null; do
    # 패턴 매칭 프로세스들의 RSS(KiB) 합산(여러 자식 가능성 대비).
    local sum
    sum=$(pgrep -f "$pattern" 2>/dev/null \
      | xargs -I{} ps -o rss= -p {} 2>/dev/null \
      | awk '{s+=$1} END{print s+0}')
    if [ "${sum:-0}" -gt "$peak" ]; then
      peak="$sum"
    fi
    sleep "$POLL_INTERVAL"
  done
  echo "$peak" > "$out"
}

kib_to_mib() { awk -v k="$1" 'BEGIN{printf "%.1f", k/1024}'; }

# ── 데이터셋 크기(dbStats) ──
dataset_size_mib() {
  mongosh "$SOURCE_URI" --quiet --eval "
    const s = db.getSiblingDB('$DBNAME').stats(1048576);
    print(s.dataSize.toFixed(1) + ',' + s.storageSize.toFixed(1) + ',' + db.getSiblingDB('$DBNAME').items.countDocuments({}));
  "
}

write_config

log "=== dataset=$LABEL db=$DBNAME ==="
DS=$(dataset_size_mib)
DATA_MIB=$(echo "$DS" | cut -d, -f1)
STORAGE_MIB=$(echo "$DS" | cut -d, -f2)
DOCS=$(echo "$DS" | cut -d, -f3)
log "dataSize=${DATA_MIB}MiB storageSize=${STORAGE_MIB}MiB docs=$DOCS"

# ───────────────────────── BACKUP 측정 ─────────────────────────
log "backup 실행 + RSS 폴링..."
export MONGO_URI="$SOURCE_URI"
rm -rf "${DEST:?}"/*
"$XB_BIN" --config "$CFG" backup --profile mem --db "$DBNAME" --quiet \
  > "$WORK/backup-$LABEL.out" 2> "$WORK/backup-$LABEL.err" &
BK_PID=$!

# x-backup 본체와 mongodump 자식을 동시에 폴링.
poll_peak "release/x-backup .*backup" "$WORK/peak-backup-xb-$LABEL.kib" "$BK_PID" &
P1=$!
poll_peak "mongodump" "$WORK/peak-backup-dump-$LABEL.kib" "$BK_PID" &
P2=$!

wait "$BK_PID"; BK_RC=$?
wait "$P1" "$P2" 2>/dev/null || true

BK_XB_KIB=$(cat "$WORK/peak-backup-xb-$LABEL.kib" 2>/dev/null || echo 0)
BK_DUMP_KIB=$(cat "$WORK/peak-backup-dump-$LABEL.kib" 2>/dev/null || echo 0)
ARTIFACT=$(find "$DEST" -name data.bin -print0 | xargs -0 stat -f%z 2>/dev/null | head -1 || true)
BACKUP_ID=$(find "$DEST" -maxdepth 1 -mindepth 1 -type d -exec basename {} \; 2>/dev/null | head -1 || true)
log "backup rc=$BK_RC id=$BACKUP_ID artifact=${ARTIFACT:-0}B"
log "backup peak RSS: x-backup=$(kib_to_mib "$BK_XB_KIB")MiB mongodump(child)=$(kib_to_mib "$BK_DUMP_KIB")MiB"

# ───────────────────────── RESTORE 측정 ─────────────────────────
# 별도의 깨끗한 타깃(RESTORE_URI)으로 복구한다 — 프로덕션 가드레일/덮어쓰기 회피.
# --target으로 분리 복구, --force로 빈 타깃에 진행, --skip-precheck로 빈 타깃 점검 우회.
log "restore 실행(별도 타깃) + RSS 폴링..."
export XB_AGE_IDENTITY_FILE="$AGE_KEY"
"$XB_BIN" --config "$CFG" restore --profile mem --id "$BACKUP_ID" \
  --target "$RESTORE_URI" --force --skip-precheck --quiet \
  > "$WORK/restore-$LABEL.out" 2> "$WORK/restore-$LABEL.err" &
RS_PID=$!

poll_peak "release/x-backup .*restore" "$WORK/peak-restore-xb-$LABEL.kib" "$RS_PID" &
P3=$!
poll_peak "mongorestore" "$WORK/peak-restore-rst-$LABEL.kib" "$RS_PID" &
P4=$!

wait "$RS_PID"; RS_RC=$?
wait "$P3" "$P4" 2>/dev/null || true

RS_XB_KIB=$(cat "$WORK/peak-restore-xb-$LABEL.kib" 2>/dev/null || echo 0)
RS_RST_KIB=$(cat "$WORK/peak-restore-rst-$LABEL.kib" 2>/dev/null || echo 0)
log "restore rc=$RS_RC"
log "restore peak RSS: x-backup=$(kib_to_mib "$RS_XB_KIB")MiB mongorestore(child)=$(kib_to_mib "$RS_RST_KIB")MiB"

# ───────────────────────── 결과 기록 ─────────────────────────
{
  echo "label=$LABEL"
  echo "db=$DBNAME"
  echo "data_size_mib=$DATA_MIB"
  echo "storage_size_mib=$STORAGE_MIB"
  echo "docs=$DOCS"
  echo "artifact_bytes=${ARTIFACT:-0}"
  echo "backup_rc=$BK_RC"
  echo "restore_rc=$RS_RC"
  echo "backup_peak_xbackup_mib=$(kib_to_mib "$BK_XB_KIB")"
  echo "backup_peak_mongodump_mib=$(kib_to_mib "$BK_DUMP_KIB")"
  echo "restore_peak_xbackup_mib=$(kib_to_mib "$RS_XB_KIB")"
  echo "restore_peak_mongorestore_mib=$(kib_to_mib "$RS_RST_KIB")"
} | tee "$RESULT"

log "결과 기록: $RESULT"
