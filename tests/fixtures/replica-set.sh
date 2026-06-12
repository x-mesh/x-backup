#!/usr/bin/env bash
#
# replica-set.sh — x-backup integration test fixture
#
# Boots a single-node MongoDB replica set in Docker and waits (polling, no sleep
# guesswork) until a PRIMARY is elected. Idempotent: re-running recreates the
# container cleanly.
#
# This fixture is reused by integration tests (full backup, oplog/incremental,
# PITR). The contract is:
#   - container name:   $XB_RS_CONTAINER  (default: x-backup-rs)
#   - host port:        $XB_RS_PORT       (default: 27017)
#   - replica set name: $XB_RS_NAME       (default: rs0)
#   - mongo image:      $XB_MONGO_IMAGE   (default: mongo:7)
#   - connection URI:   mongodb://localhost:$XB_RS_PORT/?replicaSet=$XB_RS_NAME&directConnection=true
#
# Usage:
#   tests/fixtures/replica-set.sh up      # start (default)
#   tests/fixtures/replica-set.sh down    # remove container
#   tests/fixtures/replica-set.sh uri     # print connection URI
#   tests/fixtures/replica-set.sh wait    # wait for PRIMARY (assumes running)
#
set -euo pipefail

XB_RS_CONTAINER="${XB_RS_CONTAINER:-x-backup-rs}"
XB_RS_PORT="${XB_RS_PORT:-27017}"
XB_RS_NAME="${XB_RS_NAME:-rs0}"
XB_MONGO_IMAGE="${XB_MONGO_IMAGE:-mongo:7}"

# directConnection=true is required so the driver/shell talks to the single node
# directly instead of doing SDAM topology discovery against an advertised
# internal host that is not reachable from the host network.
uri() {
  echo "mongodb://localhost:${XB_RS_PORT}/?replicaSet=${XB_RS_NAME}&directConnection=true"
}

log() { printf '[replica-set] %s\n' "$*" >&2; }

# Run a mongosh eval against the node WITHOUT replicaSet topology checks
# (used before/while the set is being initiated).
mongosh_direct() {
  docker exec "$XB_RS_CONTAINER" mongosh --quiet --port 27017 --eval "$1"
}

up() {
  # Idempotent: drop any prior container with the same name.
  if docker ps -a --format '{{.Names}}' | grep -qx "$XB_RS_CONTAINER"; then
    log "removing existing container $XB_RS_CONTAINER"
    docker rm -f "$XB_RS_CONTAINER" >/dev/null
  fi

  log "starting $XB_MONGO_IMAGE as single-node replica set '$XB_RS_NAME' on :$XB_RS_PORT"
  docker run -d \
    --name "$XB_RS_CONTAINER" \
    -p "${XB_RS_PORT}:27017" \
    "$XB_MONGO_IMAGE" \
    --replSet "$XB_RS_NAME" --bind_ip_all >/dev/null

  # 1) Wait until mongod answers pings.
  log "waiting for mongod to accept connections..."
  local i
  for i in $(seq 1 60); do
    if mongosh_direct 'db.runCommand({ ping: 1 }).ok' 2>/dev/null | grep -q '^1$'; then
      break
    fi
    if [ "$i" -eq 60 ]; then
      log "ERROR: mongod did not accept connections in time"
      docker logs "$XB_RS_CONTAINER" 2>&1 | tail -20 >&2
      exit 1
    fi
    sleep 1
  done

  # 2) Initiate the replica set (idempotent: ignore 'already initialized').
  log "initiating replica set..."
  mongosh_direct "
    try {
      rs.initiate({
        _id: '${XB_RS_NAME}',
        members: [{ _id: 0, host: 'localhost:27017' }]
      });
    } catch (e) {
      if (!/already initialized/i.test(e.message)) { throw e; }
    }
  " >/dev/null

  # 3) Poll for PRIMARY election (no fixed sleep — poll the actual state).
  log "waiting for PRIMARY election..."
  for i in $(seq 1 60); do
    if mongosh_direct 'db.hello().isWritablePrimary' 2>/dev/null | grep -q '^true$'; then
      log "PRIMARY is ready"
      log "URI: $(uri)"
      return 0
    fi
    if [ "$i" -eq 60 ]; then
      log "ERROR: no PRIMARY elected in time"
      mongosh_direct 'rs.status()' >&2 || true
      exit 1
    fi
    sleep 1
  done
}

wait_primary() {
  local i
  for i in $(seq 1 60); do
    if mongosh_direct 'db.hello().isWritablePrimary' 2>/dev/null | grep -q '^true$'; then
      log "PRIMARY is ready"
      return 0
    fi
    sleep 1
  done
  log "ERROR: no PRIMARY elected in time"
  exit 1
}

down() {
  if docker ps -a --format '{{.Names}}' | grep -qx "$XB_RS_CONTAINER"; then
    log "removing container $XB_RS_CONTAINER"
    docker rm -f "$XB_RS_CONTAINER" >/dev/null
  else
    log "no container named $XB_RS_CONTAINER"
  fi
}

case "${1:-up}" in
  up)   up ;;
  down) down ;;
  uri)  uri ;;
  wait) wait_primary ;;
  *)    log "usage: $0 {up|down|uri|wait}"; exit 2 ;;
esac
