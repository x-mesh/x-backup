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

### P0-1. MongoDB PITR native oplog 재생 — `mongorestore` 제거 ★핵심

유일한 하드 블로커. PG(`src/pipeline/pg_pitr.rs`)·MySQL(`mysql_pitr.rs`)이 이미 드라이버로
직접 재생하는 것과 동일한 구조를 Mongo에도 만든다.

- **방안**: 증분 슬라이스의 oplog BSON 문서를 스트리밍 디코드하여 드라이버로 직접 적용.
  - 1차: `applyOps` 커맨드 배치 적용(단순, 서버가 idempotency 처리). 권한 요구(`__system` 수준)가
    과하면 2차 방안으로.
  - 2차: op 타입별(i/u/d/c) 해석 → 일반 CRUD·DDL 커맨드로 변환 적용(mongorestore
    `--oplogReplay`가 하는 일의 부분집합). `--at` 컷은 ts 비교로 스트림에서 직접 절단 —
    현재 `--oplogLimit` +1 보정 로직(`pitr.rs`)이 순수 비교로 단순해짐.
- **수용 기준**: 기존 `scenario-e2e.sh` PITR 시나리오가 mongorestore 미설치 환경(PATH에서 제거)에서
  통과. 기존 백업 체인과 호환(아카이브 포맷 변경 없음 — 재생기만 교체).
- **공수**: 중(oplog op 의미론 — v2 update 표현(`$v:2` delta) 처리 포함 2~3주).
- **리스크**: update v2 delta 포맷 디코드가 가장 까다로움. mysql/pgoutput 디코더를 이미 자체
  구현한 전례가 있어 팀 역량상 실현 가능.

### P0-2. mongodump 엔진 경로 정리

- opt-in `engine=mongodump` 경로(`dump.rs`/`restore.rs`/버전 점검)를 **cargo feature
  `legacy-mongodump`로 격리**(기본 빌드 제외) 후 1개 마이너 버전 뒤 제거.
- 기존 `archive_format=="mongodump"` 백업의 복구 호환은 feature 빌드로만 제공하고,
  `list`가 해당 백업에 deprecation 경고를 표시.
- **수용 기준**: 기본 빌드 산출물에서 `std::process::Command` 호출이 update 관련 외 0건.

### P0-3. update 서브커맨드의 `brew`/`gh` 스폰 제거

- `gh auth token` 폴백 삭제 → env(`GITHUB_TOKEN`/`GH_TOKEN`)만 허용 (`src/update/mod.rs:130-151`).
- brew 위임 삭제 → brew 설치본 감지 시 "brew upgrade를 실행하라" 안내만 출력 (`update.rs:97`).
- **공수**: 소(1일).

### P0-4. CI "no-subprocess" 가드 신설

퇴행 방지 장치. lint 잡에 스크립트 추가:
`src/`에서 `std::process::Command`·`tokio::process` 사용을 허용 목록
(P0-2 feature 게이트 내부만) 대비 검사, 위반 시 실패. musl 정적 검사(`file`)는 기존 유지.

- **Phase 0 완료 게이트**: *"mongorestore·mongodump·brew·gh가 전혀 없는 컨테이너
  (`FROM scratch` + 바이너리)에서 backup/restore/PITR/verify/prune 전 시나리오 통과"*
  를 CI 잡으로 추가.

---

## Phase 1 — 아키텍처: `Engine` trait 추출 (다기능화의 전제)

현재 신규 엔진 추가 시 backup/restore/status/pitr/incremental/list 핸들러 곳곳의
`DbKind` match를 모두 수정해야 한다(MySQL 추가가 최대 커밋이었던 원인). 파일 엔진(Phase 2)을
얹기 전에 seam을 정리한다.

- `BackupEngine` trait 정의: `full_backup() -> AsyncRead` / `restore(AsyncRead)` /
  `incremental_capture` / `pitr_replay` / `status_report` / `peek` (+ capability 플래그:
  incremental 지원 여부, PITR 지원 여부, selective restore 지원 여부).
- `DbKind::from_uri`는 trait 객체 팩토리로 승격. 핸들러는 capability 질의로 분기 제거.
- **원칙**: 동작 불변 리팩터 — 기존 14개 통합 테스트·시나리오가 그대로 통과해야 한다.
  엔진 내부 파일 구조(conn/backup/restore/incremental/status)는 이미 3엔진이 일관된
  패턴이므로 trait 표면만 걷어올리면 된다.
- **공수**: 중(1~2주). Phase 2·3의 비용을 구조적으로 낮추는 투자.

---

## Phase 2 — 다기능화 1: 파일/디렉터리 백업 엔진 (`file://`)

"백업툴"로서 가장 큰 기능 공백. DB와 동일한 파이프라인(compress→encrypt→store,
manifest/verify/prune/list 공유)에 태우는 것이 차별점 — 별도 도구를 만드는 게 아니라
**엔진 하나를 추가**한다.

### P2-1. 풀 백업/복구

- source `file:///path`(복수 경로·exclude 글롭은 config로). 아카이브는 이미 트리에 있는
  `tar` crate(pure Rust)로 스트리밍 생성 → 기존 StageStack에 그대로 접속.
- 메타데이터: 권한/소유자/심링크/mtime 보존(tar가 커버), xattr는 로드맵.
- 복구: `restore --to-dir`(기존 플래그 의미 확장) 또는 원위치.
- **수용 기준**: 대용량(수십 GiB) 트리에서 RSS 평탄(기존 메모리 프로파일 방법론 재사용,
  `docs/memory-profile.md`), 라운드트립 후 전 파일 sha256 일치.

### P2-2. 증분 백업 (스냅샷 인덱스 방식)

- 백업마다 파일 인덱스(경로, size, mtime, sha256)를 manifest 사이드카로 기록.
  증분 = 이전 인덱스와 대조해 변경/신규 파일만 tar에 포함, 삭제는 tombstone 기록.
- 체인 규칙은 기존 `manifest/chain.rs` 계약(base + 연속 슬라이스)을 재사용 —
  oplog ts 대신 인덱스 세대 번호로 연속성 판정.
- PITR은 해당 없음(capability 플래그로 비활성 — Phase 1 trait가 이 분기를 흡수).

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

1. **P0-1 재생 방식**: `applyOps`(권한 요구 큼, 구현 단순) vs CRUD 변환(권한 최소, 구현 복잡).
   권장: CRUD 변환 — "권한 최소 원칙"이 status 점검 철학과 일관.
2. **mongodump 포맷 백업의 지원 종료 시점**: feature 격리 후 몇 버전 유지할지.
3. **파일 엔진의 URI 표면**: `file://` source 통합 vs 별도 서브커맨드. 권장: URI 통합
   (기존 "URI 스킴으로 엔진 자동 판별" 설계와 일관).
4. **스케줄러의 PRD 비목표 해제**: PRD §5가 스케줄러를 명시적 비목표로 두었으므로 PRD 개정 필요.
