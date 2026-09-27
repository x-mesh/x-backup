#!/usr/bin/env bash
#
# demo-setup.sh — README GIF를 녹화하기 위한 데모 환경을 만든다.
#
# `make mongodb-up`으로 띄운 **로컬 테스트 컨테이너**만 대상으로 하며, 그 안의
# `shop` 데이터베이스 하나만 만들고 채운다. 다른 DB는 읽지도 쓰지도 않는다.
#
# 사용:
#   make mongodb-up
#   docs/assets/demo-setup.sh
#   vhs docs/assets/backup.tape      # (status.tape / verify.tape)
#
# 정리:
#   rm -rf /tmp/xb-demo && make mongodb-down
set -euo pipefail

PORT="${XB_DEMO_PORT:-27017}"
CTR="${XB_DEMO_CONTAINER:-x-backup-mongo}"
WS="${XB_DEMO_DIR:-/tmp/xb-demo}"
DEMO_DB="shop"
REPO="$(cd "$(dirname "$0")/../.." && pwd)"
XB="$REPO/target/release/x-backup"

die() { printf 'demo-setup: %s\n' "$*" >&2; exit 1; }
log() { printf 'demo-setup: %s\n' "$*"; }

# ── 안전 가드 ────────────────────────────────────────────────────────────────
# 이 스크립트는 데이터를 지운다(`shop` 드롭 후 재시딩). 그러니 대상이 정말로
# `make mongodb-up`이 띄운 일회용 컨테이너인지 먼저 증명하고, 아니면 멈춘다.
# 실서비스 DB로 실수로 향하는 경로를 코드로 막아 두는 게 목적이다.

command -v docker >/dev/null || die "docker가 필요합니다"
[ -x "$XB" ] || die "릴리스 바이너리가 없습니다 — 'make build'를 먼저 실행하세요"

docker inspect "$CTR" >/dev/null 2>&1 \
  || die "컨테이너 '$CTR'가 없습니다 — 'make mongodb-up'을 먼저 실행하세요"

compose_project=$(docker inspect "$CTR" \
  --format '{{index .Config.Labels "com.docker.compose.project"}}')
[ "$compose_project" = "docker" ] \
  || die "'$CTR'가 이 리포의 docker compose로 만든 컨테이너가 아닙니다(project=$compose_project) — 중단합니다"

published=$(docker inspect "$CTR" \
  --format '{{range $p, $c := .NetworkSettings.Ports}}{{range $c}}{{.HostPort}} {{end}}{{end}}')
grep -qw "$PORT" <<<"$published" \
  || die "'$CTR'가 포트 $PORT를 게시하고 있지 않습니다(게시: $published) — 중단합니다"

# 시스템 DB와 데모 DB 말고 다른 게 있으면 남의 데이터일 수 있으니 멈춘다.
foreign=$(docker exec "$CTR" mongosh --quiet --eval '
  db.adminCommand("listDatabases").databases
    .map(d => d.name)
    .filter(n => !["admin", "config", "local", "'"$DEMO_DB"'"].includes(n))
    .join(",")')
[ -z "$foreign" ] \
  || die "'$CTR'에 데모용이 아닌 데이터베이스가 있습니다($foreign) — 중단합니다"

log "대상 확인 완료: $CTR (compose=$compose_project, port=$PORT), '$DEMO_DB'만 건드립니다"

# ── 워크스페이스 ─────────────────────────────────────────────────────────────
# 경로를 /tmp 아래 짧게 두는 이유: status/doctor가 config 경로를 그대로 출력하므로
# 녹화본에 홈 디렉터리(= 사용자 이름)가 찍히지 않게 하려는 것이다.
rm -rf "$WS"
mkdir -p "$WS/store"
(cd "$REPO" && cargo run --quiet --example gen_age_key -- "$WS" >/dev/null)

cat > "$WS/config.toml" <<EOF
default_profile = "prod"

[output]
language = "en"

[profiles.prod.source]
uri = "mongodb://localhost:$PORT/?replicaSet=rs0&directConnection=true"

[profiles.prod.destination]
type = "local"
path = "store"

[profiles.prod.features.compression]
algorithm = "zstd"
level     = 6

[profiles.prod.features.encryption]
enabled        = true
algorithm      = "age"
recipient_file = "age.pub"
EOF

log "워크스페이스 준비 완료: $WS"

# ── 시드 ─────────────────────────────────────────────────────────────────────
# 압축이 잘 되지 않는 무작위 문자열을 섞는다. 반복 문자열로 채우면 zstd가 수백 KiB로
# 눌러 버려서 진행률 표시가 한 프레임에 끝나고, 압축률 수치도 현실과 동떨어진다.
log "'$DEMO_DB' 시드 중(600k orders + 20k customers, 약 176 MiB)..."
docker exec "$CTR" mongosh --quiet --eval '
  const db = db.getSiblingDB("'"$DEMO_DB"'");
  db.dropDatabase();
  function rnd(n) {
    let s = ""; const c = "abcdefghijklmnopqrstuvwxyz0123456789";
    for (let i = 0; i < n; i++) s += c[Math.floor(Math.random() * 36)];
    return s;
  }
  let id = 0;
  for (let b = 0; b < 12; b++) {
    const bulk = [];
    for (let i = 0; i < 50000; i++, id++) {
      bulk.push({ _id: id, sku: "sku-" + rnd(8), qty: (id % 7) + 1,
                  total: (id % 97) * 130 + 500, region: ["kr","us","jp","de"][id % 4],
                  token: rnd(120), note: rnd(90) });
    }
    db.orders.insertMany(bulk, { ordered: false });
  }
  db.customers.insertMany(Array.from({ length: 20000 }, (_, i) => ({
    _id: i, name: "customer-" + rnd(6), tier: ["free","pro","team"][i % 3], ref: rnd(40)
  })));
  db.orders.createIndex({ sku: 1 });
  db.orders.createIndex({ region: 1, total: -1 });
  print("  orders=" + db.orders.countDocuments({}) +
        " customers=" + db.customers.countDocuments({}) +
        " dataSize=" + (db.stats().dataSize / 1048576).toFixed(1) + " MiB");
'

log "완료. 이제 녹화할 수 있습니다:"
log "  vhs docs/assets/backup.tape"
log "  vhs docs/assets/status.tape"
log "  vhs docs/assets/verify.tape"
