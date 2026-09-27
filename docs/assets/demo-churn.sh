#!/usr/bin/env bash
#
# demo-churn.sh — 데모 DB에 변경을 발생시켜 증분 백업이 잡을 oplog를 만든다.
# backup.tape가 풀 백업과 증분 백업 사이에서 조용히(Hide) 호출한다.
#
# demo-setup.sh와 같은 가드를 건다: 이 리포의 compose가 만든 컨테이너의 `shop`만 건드린다.
set -euo pipefail

CTR="${XB_DEMO_CONTAINER:-x-backup-mongo}"
DEMO_DB="shop"

compose_project=$(docker inspect "$CTR" \
  --format '{{index .Config.Labels "com.docker.compose.project"}}' 2>/dev/null || echo "")
[ "$compose_project" = "docker" ] || {
  printf 'demo-churn: %s\n' "'$CTR'가 이 리포의 compose 컨테이너가 아닙니다 — 중단합니다" >&2
  exit 1
}

docker exec "$CTR" mongosh --quiet --eval '
  const db = db.getSiblingDB("'"$DEMO_DB"'");
  function rnd(n) {
    let s = ""; const c = "abcdefghijklmnopqrstuvwxyz0123456789";
    for (let i = 0; i < n; i++) s += c[Math.floor(Math.random() * 36)];
    return s;
  }
  const bulk = [];
  for (let i = 0; i < 8000; i++) {
    bulk.push({ sku: "sku-" + rnd(8), qty: 1, total: 999, region: "kr",
                token: rnd(120), note: rnd(90) });
  }
  db.orders.insertMany(bulk, { ordered: false });
  db.orders.updateMany({ region: "us" }, { $set: { flagged: true } });
' >/dev/null
