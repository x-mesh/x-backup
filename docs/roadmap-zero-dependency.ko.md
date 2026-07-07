# 개선 계획 — 완전 무의존(zero-dependency) 다기능 백업툴 로드맵

> 목표: **런타임에 x-backup 정적 바이너리 하나 외에는 어떤 외부 바이너리·시스템 파일·외부 서비스도
> 필요 없는** 상태를 완성하고, 그 위에서 DB 백업을 넘어 **파일 백업 + 운영 자동화(스케줄·알림·
> 리텐션·복구 리허설)까지 갖춘 다기능 백업툴**로 확장한다.
>
> 본 문서는 코드베이스 전수 조사(2026-07) 결과를 근거로 하며, 각 항목에 근거 파일:라인을 표기한다.

---

## 0. 현황 진단 — 무엇이 이미 되어 있고, 무엇이 남았나

### 0.1 이미 자기완결인 것 (조치 불필요)

| 항목 | 상태 | 근거 |
|---|---|---|
| TLS | rustls + ring, OpenSSL/native-tls 사용처 0건 | `Cargo.toml` (mongodb `rustls-tls`, mysql_async `default-rustls-ring`, tokio-postgres-rustls) |
| CA 루트 | webpki-roots **내장** — 시스템 CA 미참조 | `src/engine/postgres/conn.rs:75` |
| 링크 | linux는 musl **정적 빌드**(static-pie), CI에서 `file` 검사 | `scripts/release.sh`, `.github/workflows/ci.yml` musl 잡 |
| libc | `kill(pid,0)`·`uname` syscall만 — 정적 바이너리에 포함 | `src/lock/file_lock.rs:308,314,360` |
| 시스템 파일 | `/etc` 등 미참조(문서·기본값 예시 문자열뿐) | `src/config/file.rs:388` 등 |
| DB 3종 코어 | Mongo/PG/MySQL 풀·증분·PITR을 드라이버 직결로 수행 | `src/engine/{native,postgres,mysql}` |

### 0.2 남아 있는 런타임 외부 의존 (제거 대상)

| # | 의존 | 발동 조건 | 근거 | 분류 |
|---|---|---|---|---|
| D1 | **`mongorestore` 스폰 — MongoDB PITR oplog 재생** | `restore --at` (엔진 설정과 무관하게 **무조건**) | `src/pipeline/pitr.rs:497-551` (`spawn_replay`) | **하드 블로커** |
| D2 | `mongodump`/`mongorestore` — opt-in 엔진 | `mode.engine = "mongodump"` 선택 시 | `src/engine/mongo/dump.rs:58`, `src/engine/mongo/restore.rs:62`, `src/engine/mongo/status.rs:1275` | opt-in |
| D3 | `brew upgrade` 스폰 | `update` + Homebrew 설치본 감지 시 | `src/cli/handlers/update.rs:97` | 선택 기능 |
| D4 | `gh auth token` 스폰 | `update` + env 토큰 부재 시 폴백 | `src/update/mod.rs:140` | 선택 기능 |
| D5 | GitHub API 네트워크 | `update` 실행 시 | `src/update/mod.rs:169,192` | 선택 기능 |

개발전용 의존(docker compose, `make tools`의 fastdl 다운로드, install.sh의 curl/gh,
release.sh의 cross/gh/ruby)은 런타임 0 목표와 무관하므로 유지한다.

> **진행 현황**: D1(native applyOps 재생, P0-1)·D3·D4(P0-3)는 **제거 완료**. D2는 P0-2로
> 잔여(여전히 opt-in), D5는 update 실행 시에만 발동하는 선택 기능으로 잔여(P5에서 feature
> 격리 예정). CI no-subprocess 가드(P0-4)로 퇴행을 차단한다.

### 0.3 기능 격차 (다기능화 대상)

| 기능 | 상태 |
|---|---|
| 파일/디렉터리(비-DB) 백업 | **없음** — `DbKind`는 Mongo/PG/MySQL만 (`src/engine/mod.rs`) |
| 스케줄러/데몬 | **없음** — 외부 cron 전제 (PRD §5 FR-10 주석) |
| 알림(webhook/Slack) | **없음** (PRD 로드맵 2차) |
| GFS 리텐션 | **없음** — keep-full/days/last 합집합만 (`src/pipeline/prune.rs`) |
| 자동 복구 리허설 | **없음** — 수동 `verify --deep`/`--dry-run`뿐 |
| dedup(청크 중복 제거) | **없음** |
| Prometheus 메트릭 | **없음** (PRD 로드맵 2차) |
| 엔진 추상화 | **약점** — 전면 `Engine` trait 없이 핸들러별 `DbKind` match 분기 (`src/engine/mod.rs` 명시) |
| native 엔진 충실도 | 부분 — users/roles·view·timeseries·샤딩 메타 미커버 (`src/engine/native/` 로드맵 주석) |

---

## Phase 0 — 런타임 외부 의존 0 완성 (최우선, 가장 작고 가장 확실한 가치)

### P0-1. MongoDB PITR native oplog 재생 — `mongorestore` 제거 ★핵심 — **구현됨**

유일한 하드 블로커였다. PG(`src/pipeline/pg_pitr.rs`)·MySQL(`mysql_pitr.rs`)이 이미 드라이버로
직접 재생하는 것과 동일한 구조를 Mongo에도 만들었다.

- **채택 방안: `applyOps` 배치 적용**(`src/engine/mongo/apply.rs`). CRUD 변환안 대신
  applyOps를 택한 이유 — `mongorestore --oplogReplay` 자체가 applyOps 기반이라 의미론
  충실도가 가장 높고, `$v:2` update delta 해석을 **서버에** 맡길 수 있으며(클라이언트
  재구현 리스크 0), 권한 요구는 종전 mongorestore 경로와 동일해 회귀가 아니다.
  - 적용 규칙: noop 스킵, `local.*`/`config.*`/`admin.system.version` 제외, `ui`(컬렉션
    UUID)·세션/재시도 메타 스트립, 트랜잭션(applyOps 체인) 커밋 지점 재조립,
    CRUD 배치(1000개/12MiB) + 커맨드 단독 적용.
  - limit 컷은 `--oplogLimit` 문자열 대신 `(t,i)` 비교로 스트림에서 직접 절단 —
    `+1` 보정 의미론은 유지. 임시 `oplog.bson` 파일도 사라져 재생 단계가 전 구간
    스트리밍이 됐다(PRD §7 예외 제거).
- **수용 기준**: 기존 `scenario-e2e.sh` PITR 시나리오가 mongorestore 미설치 환경(PATH에서 제거)에서
  통과 — CI integration 잡에서 확인 필요(잔여). 기존 백업 체인과 호환(아카이브 포맷 변경
  없음 — 재생기만 교체).
- **잔여 리스크**: prepared/대형 트랜잭션 재조립은 통합 테스트로 실측 필요.

### P0-2. mongodump 엔진 경로 정리 — **구현됨**

- opt-in `engine=mongodump` 경로(`dump.rs`/`restore.rs`/버전 점검 스폰)를 **cargo feature
  `legacy-mongodump`로 격리**(기본 빌드 제외). `integration-tests` feature가 이를 함께 켠다.
- 기본 빌드에서: `engine = "mongodump"`는 `Engine::parse`에서 설정 오류(exit 2)로 조기
  거부, mongodump 포맷 백업 복구는 안내와 함께 실패(exit 1), doctor는 engine 항목을
  Fail로 표시. CI lint 잡에 기본(feature 없는) 빌드 clippy 스텝 추가.
- **수용 기준 달성**: 기본 빌드 산출물에서 서브프로세스 스폰 코드 0건(컴파일 제외).
- **잔여**: `list`의 mongodump 포맷 백업 deprecation 경고 표시, 1개 마이너 버전 뒤 제거.

### P0-3. update 서브커맨드의 `brew`/`gh` 스폰 제거 — **구현됨**

- `gh auth token` 폴백 삭제 → env(`GITHUB_TOKEN`/`GH_TOKEN`)만 허용 (`src/update/mod.rs`).
- brew 위임 삭제 → brew 설치본 감지 시 `brew upgrade` 명령만 안내(cargo 설치와 동일한 정책).

### P0-4. CI "no-subprocess" 가드 신설 — **구현됨**

퇴행 방지 장치. lint 잡에 스텝 추가(ci.yml `no-subprocess guard`): `src/`에서 스폰
표면(`tokio::process`·`std::process::{Command,Stdio,Child}`) 사용을 허용 목록
(`engine/mongo/{dump,restore,status}.rs` — opt-in mongodump 엔진) 대비 검사, 위반 시 실패.
musl 정적 검사(`file`)는 기존 유지.

- **Phase 0 완료 게이트**: *"mongorestore·mongodump·brew·gh가 전혀 없는 컨테이너
  (`FROM scratch` + 바이너리)에서 backup/restore/PITR/verify/prune 전 시나리오 통과"*
  를 CI 잡으로 추가.

---

## Phase 1 — 아키텍처: `Engine` trait 추출 (다기능화의 전제)

현재 신규 엔진 추가 시 backup/restore/status/pitr/incremental/list 핸들러 곳곳의
`DbKind` match를 모두 수정해야 한다(MySQL 추가가 최대 커밋이었던 원인). 파일 엔진(Phase 2)을
얹기 전에 seam을 정리한다.

슬라이스로 나눠 진행한다(동작 불변 원칙 — 기존 테스트가 그대로 통과해야 한다):

- **슬라이스 A — 엔진 판별 단일화: 구현됨.** `DbKind::from_archive_format`이
  manifest 포맷 → 엔진 판별의 단일 진실 원천(list/picker가 위임). 부수 수정:
  picker가 xb-mysql을 몰라 MySQL 백업을 "mongodb"로 표기하던 버그 해결.
- **슬라이스 B — 풀 백업 저장 코어 추출: 구현됨.** 세 엔진이 각자 들고 있던
  "합성(카운터→스테이지→sha256)→저장→종료 판정→확정→manifest 기록/정리" 골격
  (~180라인 3중복)을 `store_dump_stream<T: DumpTermination>` + `write_manifest_or_cleanup`
  공통 코어로 통합(`src/pipeline/backup.rs`). 새 엔진은 dump 스트림과 `DumpTermination`
  구현(finish/abort 훅, MySQL처럼 종료 시 메타 반환 가능)만 만들면 접속된다 —
  **Phase 2 파일 엔진이 꽂히는 실제 seam**.
- **슬라이스 C — 증분 경로 코어 공유(잔여):** mongo/pg/mysql 증분 캡처의 저장 골격도
  같은 코어를 태운다(빈 슬라이스 특례 포함).
- **슬라이스 D — 핸들러 플로우 trait(잔여):** backup/status/peek/migrate 핸들러의
  per-DB 함수(handle_pg_*/handle_mysql_*)를 capability 플래그를 가진 trait 뒤로 —
  4번째 엔진 추가 시점에 맞춰 진행(먼저 하면 추측성 추상화가 된다).
- **공수**: A·B 완료. C 소, D 중.

---

## Phase 2 — 다기능화 1: 파일/디렉터리 백업 엔진 (`file://`)

"백업툴"로서 가장 큰 기능 공백. DB와 동일한 파이프라인(compress→encrypt→store,
manifest/verify/prune/list 공유)에 태우는 것이 차별점 — 별도 도구를 만드는 게 아니라
**엔진 하나를 추가**한다.

### P2-1. 풀 백업/복구 — **구현됨**

- `DbKind::File`(`file://` 스킴) + `engine/file`(tar 스트리밍 덤프/복구, `xb-file-tar-v1`).
  tar 직렬화는 blocking task + SyncIoBridge로 async 경계를 잇고, Phase 1 슬라이스 B의
  `DumpTermination` seam으로 공통 저장 코어에 접속했다 — 설계대로 "엔진 하나 추가"였다.
- 권한/mtime/심링크 보존(심링크는 따라가지 않음), 복구는 `unpack_in`으로 경로 탈출 방어.
  비어 있지 않은 대상은 기존 가드레일(--force/확인) 적용. status는 경로/예상 크기 보고,
  doctor는 URI 형식 검증. incr(--type)·PITR(--at)·--only·peek·migrate는 명확히 거부.
- 실 바이너리 E2E 검증: doctor→status→backup→list→verify --deep→restore(트리 diff 동일,
  실행 권한 보존)→덮어쓰기 가드(exit 1)→--force 재복구까지 확인.
- **잔여**: 복수 경로·exclude 글롭(config), xattr, 대용량 트리 RSS 프로파일
  (`docs/memory-profile.md` 방법론 재사용).

### P2-2. 증분 백업 (스냅샷 인덱스 방식) — **구현됨**

- 인덱스 사이드카 `<id>/index.json.zst`(경로·종류·크기·mtime·mode·링크; zstd, **비암호화**
  — 키 격리 호스트가 다음 diff를 위해 읽어야 함, 메타데이터 노출 트레이드오프 문서화).
  sha256은 제외(매 백업 전체 해시 비용 회피 — rsync 휴리스틱과 동일).
- 증분 tar(`xb-file-incr-v1`) = 변경/신규 파일 + `.xb/tombstones.json`(삭제 목록,
  암호화 스트림 내부). `.xb`는 예약 경로 — 소스에 있으면 백업 거부.
- 체인은 설계대로 `manifest/chain.rs` 계약 재사용 — oplog ts 자리에 세대 카운터
  `(gen,0)`을 일반화(풀=(1,0), 증분 n=(n,0)→(n+1,0), 빈 슬라이스=zero-width).
  **`list`의 CHAIN 표시·`verify --chain`·prune 체인 안전성이 무수정으로 동작**했다.
  혼재 저장소 안전: mongo PITR base 선택이 파일 풀백업을 집지 않게 필터 추가.
- 복구: `--id <증분>` = base+슬라이스 순차 재생(verify_chain 통과 전제), 기본 restore는
  base만 + 최신 증분 안내 경고. gap(헤드 인덱스 소실) = 풀 승격(exit 4, DB 엔진과 동일).
- 실 바이너리 E2E: 풀(64KiB) → 증분(119B) → 삭제 증분(139B) → 빈 슬라이스(0B) →
  verify --chain continuous → 증분 id 체인 복구 후 트리 diff 동일 확인.
- PITR(--at)은 해당 없음 — 거부 유지.

### P2-3. (검토) SQLite·Redis

- SQLite: 파일 엔진 + 저널/WAL 일관성 처리(백업 전 `PRAGMA wal_checkpoint` 시도 —
  단, 드라이버 연결은 pure-Rust 구현 성숙도 확인 후 결정. 미성숙 시 "정지 상태 파일 백업"만 지원 명시).
- Redis: RESP 프로토콜 직접 구현(SCAN+DUMP/RESTORE) — 프로토콜이 단순해 자체 구현 가능.
  수요 확인 후 착수.

---

## Phase 3 — 다기능화 2: 운영 레이어 (외부 cron 의존까지 제거)

"아무런 외부 의존 없음"의 마지막 조각은 **운영 의존**이다 — 현재는 cron 없이는 주기 백업이 안 된다.

### P3-1. 내장 스케줄러 — `x-backup daemon`

- config에 `schedule = "0 3 * * *"`(프로파일별) 추가. `daemon` 서브커맨드가 포그라운드
  상주하며 스케줄 발화 → 기존 backup 파이프라인 호출(잠금 계약 재사용).
- cron 표현식 파서는 자체 구현(5필드, ~200 LOC — 외부 크레이트도 pure Rust지만 의존 최소
  원칙에 부합). PRD의 "interval은 스케줄링용이 아니다" 주석과의 충돌은 config 키 분리로 해소.
- `daemon --install-systemd`가 systemd unit 파일을 **출력**(설치는 사용자 몫 — 시스템 변경을
  임의로 하지 않는 기존 원칙 유지). 신뢰성 요건: 발화 누락 시 다음 기동에서 catch-up 여부를
  config로 명시(`catchup = true`).
- 실패 시 exit-code 계약(0~5) 유지 — 데몬 내부에서도 실행 단위별로 동일 코드 기록.

### P3-2. 알림 — webhook 우선

- 백업/prune/리허설 완료·실패 시 generic JSON webhook POST(reqwest 이미 트리에 있음 —
  신규 의존 0). Slack은 webhook의 페이로드 템플릿 한 종. SMTP는 후순위(순수 Rust
  lettre+rustls 검토 — 의존 추가라 수요 확인 후).
- 페이로드: profile, backup_id, type, 소요, 크기, exit code, 에러 요약 — `--json` 출력
  스키마 재사용.

### P3-3. GFS 리텐션 (정책 기반 자동 prune)

- config `[profile.x.retention]`에 `daily/weekly/monthly/yearly` 보존 수 추가.
  기존 keep-full/days/last와 합집합 의미론 유지, **체인 단위 안전 삭제 계약
  (`prune.rs` — 살아있는 증분의 base 미삭제) 불변**.
- daemon이 백업 성공 후 자동 prune 실행(옵트인).

### P3-4. 자동 복구 리허설 — `x-backup rehearse`

- 최신 체인을 임시 대상(`--target-profile` 또는 로컬 임시 디렉터리/컨테이너 없는 in-process
  검증)에 실제 복원 → 원본 status의 데이터 셰이프(문서 수·테이블 행 수)와 대조 → 결과를
  manifest에 `last_rehearsal` 기록, 알림 발송. PRD 로드맵 "복구 리허설(후반)"의 구체화.
- 1차 범위: 파일 엔진(디렉터리 대조)과 DB `--to-dir` 계열 검증부터. 라이브 DB 대상 리허설은
  endpoint 전용 프로파일로.

### P3-5. Prometheus 메트릭 — textfile 우선

- `daemon`이 node_exporter textfile collector 포맷으로 메트릭 파일 기록(HTTP 서버 없이 —
  의존·공격면 최소). HTTP `/metrics`는 수요 확인 후(순수 std 소켓 mini-HTTP ~150 LOC로 충분).

---

## Phase 4 — 스토리지·데이터 효율

### P4-1. 스토리지 백엔드 확장 (저비용)

- `Storage` trait(4 메서드) + `from_config` match 한 줄이면 됨 — **GCS·Azure**는
  object_store feature 플래그만 켜면 되므로 공수 소. SFTP는 pure-Rust russh 검토(의존 추가라
  수요 기반 결정).

### P4-2. dedup — content-defined chunking (대형, 후반)

- FastCDC 청킹 + 청크 sha256 인덱스 → 스토리지에 청크 풀 레이아웃 신설. 파일 엔진 증분과
  시너지가 가장 큼(DB 덤프는 압축 후 청크 재현성이 낮아 효과 제한 — 압축 전 청킹으로 설계).
- manifest format_version 상향 필요. **prune의 체인 계약과 청크 참조 카운팅의 상호작용이
  최대 난점** — 별도 설계 문서 선행 필수.

### P4-3. 키 관리 강화 (zero-dep 철학 내에서)

- age passphrase(scrypt) 지원 추가 — 키 파일조차 없이 운영 가능한 모드.
- KMS/HSM 연동은 외부 서비스 의존이므로 **비목표로 명문화**(PRD 로드맵에서 강등).

---

## Phase 5 — 배포·갱신 자기완결

- **릴리스 서명**: ed25519(pure Rust) 서명을 릴리스 자산에 추가, `update`가 sha256 + 서명을
  함께 검증 — GitHub 인프라 신뢰에서 키 신뢰로 전환(공개키는 바이너리에 내장).
- `update`를 cargo feature(`self-update`, 기본 on)로 격리 — 완전 오프라인 배포판(airgap)
  빌드 옵션 제공.
- install.sh는 부트스트랩이므로 curl 의존 불가피 — 대신 checksums 검증 경로를 서명 검증으로
  강화.

---

## 우선순위·공수 매트릭스

| 순위 | 항목 | 가치 | 공수 | 비고 |
|---|---|---|---|---|
| 1 | P0-1 native PITR 재생 | ★★★ (제품 표어 완성) | 중 | 유일한 하드 블로커 |
| 2 | P0-3·P0-4 update 정리 + CI 가드 | ★★ | 소 | 1~2일 |
| 3 | P0-2 mongodump feature 격리 | ★★ | 소 | 호환 공지 필요 |
| 4 | P1 Engine trait | ★★★ (구조 투자) | 중 | Phase 2·3 전제 |
| 5 | P2-1·2 파일 엔진 풀+증분 | ★★★ (시장 확장) | 대 | tar 스트리밍 재사용 |
| 6 | P3-1 내장 스케줄러 | ★★★ (cron 의존 제거) | 중 | |
| 7 | P3-3 GFS + P3-2 webhook | ★★ | 소~중 | |
| 8 | P3-4 복구 리허설 | ★★★ (백업의 존재 이유) | 중 | |
| 9 | P4-1 GCS/Azure | ★ | 소 | |
| 10 | P3-5 메트릭 textfile | ★ | 소 | |
| 11 | P4-2 dedup | ★★ | 대 | 설계 문서 선행 |
| 12 | P5 서명·airgap | ★★ | 중 | |

**마일스톤 제안**
- **v0.3 — "진짜 zero-dependency"**: Phase 0 전부 + CI `FROM scratch` 게이트. 표어를
  "no external dump tools"에서 "no external anything"으로 상향.
- **v0.4 — 아키텍처 + 파일 백업**: Phase 1 + P2-1/2.
- **v0.5 — 자율 운영**: P3-1/2/3 (+P3-4).
- **v0.6+**: 리허설 고도화, 메트릭, GCS/Azure, dedup 설계.

## 결정이 필요한 사항

1. ~~**P0-1 재생 방식**~~ — **applyOps로 확정·구현됨**(P0-1 절 참조). 권한 요구는
   종전 mongorestore --oplogReplay와 동일하므로 "권한 최소" 관점의 회귀가 없다.
2. **mongodump 포맷 백업의 지원 종료 시점**: feature 격리 후 몇 버전 유지할지.
3. **파일 엔진의 URI 표면**: `file://` source 통합 vs 별도 서브커맨드. 권장: URI 통합
   (기존 "URI 스킴으로 엔진 자동 판별" 설계와 일관).
4. **스케줄러의 PRD 비목표 해제**: PRD §5가 스케줄러를 명시적 비목표로 두었으므로 PRD 개정 필요.
