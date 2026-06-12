# Spike: `--archive` + `--oplog` E2E 실측 (Slice0 / A4)

> 상태: **완료 (실측 기반)** · 일시: 2026-06-12 · 대상 태스크: t1
> 가설 A4 검증 + A3(드라이버 oplog 질의) 확인 + 아키텍처 분기 확정.

---

## 1. 요약 (TL;DR)

| 검증 항목 | 가설 | 실측 결과 | 판정 |
|---|---|---|---|
| **A4-1** `mongodump --archive --oplog`가 oplog를 archive 스트림에 포함하는가 | 포함된다 | dump 로그 `dumped 5 oplog entries` + restore가 archive prelude에서 `.oplog` 인식, `found oplog.bson file to replay` | ✅ **참** |
| **A4-2** `mongorestore --archive --oplogReplay`가 hang 없이 동작하는가 (TOOLS-2131 회귀) | 4.0.4+에서 수정, hang 없음 | exit code **0**, 1초 내 완료, `demux finishing (err:<nil>)`, `applied 5 oplog entries` | ✅ **참 (hang 미재현)** |
| **A3** 드라이버/클라이언트로 `local.oplog.rs` 직접 질의 가능한가 | 가능하다 | 최신 엔트리 조회·`ts > pivot` 필터·`Timestamp({t,i})` 생성자 필터 모두 동작 | ✅ **참** |

### 채택 모드 결정

> **archive 모드(`mongodump --archive --oplog`)를 유지한다.** 디렉터리 모드(`--out`) + 자체 패킹으로의 전환은 **불필요**.

가설 A4가 실측으로 참임이 확정되었으므로 PRD §7 파이프라인의 archive 스트리밍 전제(FR-1)를 그대로 채택한다.

---

## 2. 실측 환경

| 구성 | 버전/값 |
|---|---|
| mongod (서버) | **7.0.35** (Docker `mongo:7`, digest `sha256:948b635b…`) |
| 토폴로지 | single-node replica set (`--replSet`, `rs.initiate()`) |
| mongodump / mongorestore / bsondump | **100.16.1** (Database Tools, Go 1.25.9, darwin/arm64) |
| mongosh | 2.8.3 |
| archive 포맷 버전 | `0.1` (archive prelude 보고값) |
| archive magic | `6d e2 99 81` |
| 호스트 | macOS (darwin/arm64), Docker 29.4.0 |

도구 설치 메모: Homebrew의 `mongodb/brew` tap이 새 brew sandbox 빌드에서 실패하여, **공식 tarball(zip)을 직접 내려받아 sha256 검증** 후 사용했다. URL은 `.tgz`가 아니라 `.zip`:
`https://fastdl.mongodb.org/tools/db/mongodb-database-tools-macos-arm64-100.16.1.zip`
(sha256 `5bef906f…`, brew formula에서 추출). CI에서는 동일 tarball을 pin하거나 패키지 매니저로 설치(pitfall 8-3).

---

## 3. 실측 절차와 결과 (재현 가능)

### 3.0 픽스처 기동

`tests/fixtures/replica-set.sh up` — single-node replica set을 기동하고 **PRIMARY를 폴링으로 대기**(고정 sleep 아님, pitfall 8-1 준수). 멱등(재실행 시 컨테이너 재생성), 별도 포트로 두 번째 인스턴스 기동 가능(`XB_RS_PORT`/`XB_RS_NAME`/`XB_RS_CONTAINER` env).

```
[replica-set] PRIMARY is ready
[replica-set] URI: mongodb://localhost:27017/?replicaSet=rs0&directConnection=true
```

> **핵심 옵션**: 단일 노드 replica set에는 접속 URI에 `directConnection=true`가 필수. 없으면 드라이버/shell이 SDAM 토폴로지 디스커버리로 광고된 내부 호스트(`localhost:27017` 컨테이너 내부)를 찾다가 호스트에서 접속 실패할 수 있다.

### 3.1 A4-1 — archive에 oplog 포함 여부

1000개 시드 후, **dump 도중 동시 쓰기를 발생**시키며 dump 실행:

```bash
mongodump --uri="$URI" --archive=/tmp/spike.archive --oplog
```

dump 로그(핵심):
```
done dumping `testdb.items` (1113 documents)
writing captured oplog to ``
	dumped 5 oplog entries
```

→ `--oplog`는 디렉터리 모드의 별도 `oplog.bson` 파일 대신 **archive 스트림 내부**에 oplog 섹션을 기록한다. `strings`로 archive에서 `oplog` 네임스페이스 마커 확인됨.

**복원 측 검증** (`mongorestore --archive --oplogReplay --dryRun -vv`):
```
archive prelude `.oplog`
archive format version `0.1`
archive server version `7.0.35`
found oplog.bson file to replay      ← restore가 archive에서 oplog를 인식
enqueued collection `.oplog`
```

**Negative control** (대조군): `--oplog` **없이** dump → restore dryRun에서 `(no oplog section found)`. 즉 oplog 섹션의 존재는 `--oplog` 플래그에 의해서만 생긴다. ⇒ 인과관계 확정.

### 3.2 A4-2 — `--oplogReplay` hang 여부 (TOOLS-2131)

**별도의 깨끗한 빈 replica set**(rs1, :27018)을 타깃으로 `timeout 60`으로 감싸 실행:

```bash
timeout 60 mongorestore --uri="$TARGET" --archive=/tmp/spike.archive --oplogReplay --drop
echo $?   # → 0
```

결과:
```
replaying oplog
applied 5 oplog entries
demux finishing (err:<nil>)
1113 document(s) restored successfully. 0 document(s) failed to restore.
EXIT_CODE=0  (0=OK, 124=HANG)
```

→ **hang 미재현. 1초 내 정상 종료. exit code 0.** TOOLS-2131(demux goroutine 조기 종료 무한 hang)은 Database Tools 100.16.1 + mongod 7.0.35 조합에서 발생하지 않는다.

**시점 일관성 정합 검증** (`--oplog`의 본래 목적):
- restore된 `testdb.items` = **1113건** (= dump가 본 일관된 스냅샷)
- 그중 `during_dump:true`(dump 도중 동시 삽입) = **113건** → oplog replay로 정확히 반영됨
- source는 dump 이후로도 쓰기가 계속되어 2299건이 되었으나, archive는 dump 시점 스냅샷(1113)만 담음 → **dump 시점 일관성이 작동함**을 실증.

### 3.3 A3 — 드라이버/클라이언트 oplog 직접 질의

`db.getSiblingDB("local").oplog.rs`에 직접 질의:

```js
// 최신 엔트리
oplog.find().sort({$natural:-1}).limit(1)
// → ts = {"$timestamp":{"t":1781272133,"i":1}}, ts instanceof Timestamp = true

// ts 범위 필터 (증분 캡처 경로)
const after = oplog.find({ ts: { $gt: pivot } }).sort({$natural:1}).toArray();
// → pivot 이후 9건 정확히 반환 (natural order)

// gap 감지 패턴: last_backup_ts가 윈도우 내 존재하는가
const probe = Timestamp({ t, i });
oplog.find({ ts: { $gte: probe } }).limit(1).hasNext();   // → true
```

결과:
- oplog 윈도우: **130초**, **2332 엔트리** (capped collection)
- `ts`는 BSON **Timestamp{t,i}** 타입 (DateTime 아님 — pitfall 2-2 확인). `Timestamp({t,i})` 생성자로 필터 구성 가능.
- `{ts: {$gt: ...}}` natural-order 질의가 정확히 동작 → §6.3 증분 캡처 경로 실현 가능.
- gap 감지(§6.2): `{ts: {$gte: last_backup_ts}}.hasNext()`로 윈도우 내 존재 여부 판정 가능.

> **드라이버 범위 한정 정직성 표기**: A3는 **mongosh**(MongoDB 공식 client, wire protocol/드라이버 위에서 동작)로 검증했다. PRD가 채택할 **Rust `mongodb` 크레이트로의 동일 질의는 별도 미실측** — Cargo 스캐폴드를 t2가 소유 중이라 본 스파이크에서 빌드하지 않았다. 단 oplog 질의는 표준 `find` + BSON Timestamp 필터이며 Rust 드라이버도 `Bson::Timestamp`/`collection.find()`를 동일 지원하므로(리서치 노트 §4: mongodb v3.7.0 "local.oplog.rs find + BSON Timestamp 필터 가능") 위험은 낮다. **t8 착수 시 Rust 드라이버로 1건 재확인 권장.**

---

## 4. 채택 모드 결정 (아키텍처 분기)

### 결정: **archive 모드 유지** (`mongodump --archive --oplog`)

근거:
1. `--oplog`가 archive 스트림에 oplog를 포함함이 확정(3.1).
2. `--oplogReplay`가 archive 입력에서 hang 없이 정상 동작함이 확정(3.2).
3. 단일 산출물 스트림이라 PRD §7 파이프라인(stdout → zstd → AEAD → Storage)에 그대로 흐를 수 있음.

→ **디렉터리 모드(`--out`) + tar/zstd 자체 패킹으로의 전환은 채택하지 않는다.** (pitfall 1-1의 fallback 경로는 불필요해짐.)

---

## 5. 후속 태스크 지침

### t4 (풀 백업 파이프라인)
- **`mongodump --archive=- --oplog`로 stdout 스트리밍**을 채택한다(파일 경유 불필요). replica set이면 `--oplog` 자동 부착(FR-1).
- archive 입력 restore는 `mongorestore --archive=- --oplogReplay`로 직결 가능 — **별도 oplog.bson 추출/재패킹 불필요**.
- 시점 일관성: `--oplog`가 dump 중 변경을 archive에 함께 담아 restore 시 자동 반영됨(3.2에서 1113/113 정합 확인). manifest의 oplog 시작/끝 ts는 dump 로그/oplog 질의로 기록(FR-7).
- 선택적 백업(`--db`/`--collection`)과 `--oplog`는 병용 불가(PRD §FR-1, pitfall 1-3) — 이 스파이크 범위 밖이나 CLI에서 사전 차단할 것.
- archive 포맷 버전은 `0.1`(서버 7.0 기준). manifest에 dump 시 archive format version과 tool version(100.16.1) 기록 권장 — 버전 호환 추적용(pitfall 1-5).

### t8 (증분 캡처)
- **증분 oplog 캡처는 `mongodump`가 아니라 드라이버 직접 질의**로 한다(PRD §6.3 확정). archive `--oplog`는 *dump 시점 일관성*용이지 임의 `ts` 범위 슬라이스 추출용이 아니다 — 임의 범위는 archive로 못 뽑는다.
- 캡처 질의: `db.local.oplog.rs.find({ ts: { $gt: last_backup_ts } }).sort({$natural:1})`. `ts`는 **Bson::Timestamp{t,i}** 로 다룰 것(DateTime 변환 금지, pitfall 2-2).
- gap 감지: 캡처 전 `oplog.rs.find({ ts: { $gte: last_backup_ts } }).limit(1)` 존재 확인. 없으면 롤오버 → 증분 거부, 풀 승격(§6.2, FR-2).
- 재생 경로: 캡처한 BSON을 `oplog.bson`으로 배치해 `mongorestore --oplogReplay`. **A4-2에서 archive 내 oplog replay가 정상임이 확인됐으므로**, 자체 캡처 oplog도 동일 replay 엔진으로 적용 가능(별도 hang 위험 없음).
- **t8 착수 시 1건 추가 실측 권장**: (a) Rust `mongodb` 크레이트로 oplog `find` 1회(A3 드라이버 확정), (b) 대량 트랜잭션 `applyOps`/`partialTxn` 경계 처리(pitfall 2-3), (c) `op:"c"` admin.$cmd DDL 엔트리의 oplogReplay 처리(pitfall 9-4) — 본 스파이크는 일반 `i`(insert) 엔트리만 다뤘다.

---

## 6. 미실측 / 한계 (정직성)

- **Rust 드라이버 직접 질의**: mongosh로 대체 검증(§3.3 표기). t8에서 재확인.
- **대량 트랜잭션/DDL oplog**: 본 스파이크는 일반 insert(`op:"i"`) oplog만 검증. `applyOps`(16MB+ partialTxn), `op:"c"` 엔트리의 oplogReplay 동작은 t8에서 별도 실측 필요(pitfall 2-3, 9-4).
- **서버 버전 다양성**: mongod 7.0.35 단일 버전만 검증. 6.0 등 다른 대상 버전은 status의 호환 매트릭스로 별도 관리(pitfall 1-5).
- **대용량/메모리 상한**: §11.1 수용 기준의 100GB 메모리 실측은 본 스파이크 범위 밖(릴리스 전 별도 통합 테스트).

---

## 7. 재현 명령 (요약)

```bash
# 0. 도구 (macOS arm64; CI는 패키지 매니저/pin)
curl -fsSLo tools.zip \
  https://fastdl.mongodb.org/tools/db/mongodb-database-tools-macos-arm64-100.16.1.zip
# sha256: 5bef906f3d9b593e70155b01b8eff8de37f9717cfc1c77568853a9b122e6adbf
unzip -q tools.zip   # → mongodump/mongorestore/bsondump

# 1. 픽스처 기동 (PRIMARY까지 폴링 대기)
tests/fixtures/replica-set.sh up
URI=$(tests/fixtures/replica-set.sh uri)

# 2. dump (replica set → --oplog가 archive에 oplog 포함)
mongodump --uri="$URI" --archive=/tmp/spike.archive --oplog

# 3. archive에 oplog 포함 확인
mongorestore --archive=/tmp/spike.archive --oplogReplay --dryRun -vv 2>&1 | grep oplog
#   → "found oplog.bson file to replay"

# 4. 빈 타깃에 restore (hang 검출: timeout으로 감쌈)
XB_RS_CONTAINER=x-backup-rs-restore XB_RS_PORT=27018 XB_RS_NAME=rs1 \
  tests/fixtures/replica-set.sh up
timeout 60 mongorestore \
  --uri="mongodb://localhost:27018/?replicaSet=rs1&directConnection=true" \
  --archive=/tmp/spike.archive --oplogReplay --drop
echo $?   # → 0 (124였으면 hang)

# 5. oplog 직접 질의 (A3)
mongosh "$URI" --quiet --eval '
  const o = db.getSiblingDB("local").oplog.rs;
  printjson(o.find().sort({$natural:-1}).limit(1).next().ts);            // Timestamp{t,i}
  print(o.find({ts:{$gt: o.find().sort({$natural:-1}).skip(9).next().ts}}).itcount());
'

# 정리
tests/fixtures/replica-set.sh down
XB_RS_CONTAINER=x-backup-rs-restore tests/fixtures/replica-set.sh down
```
