# PRD — MongoDB 백업·복구 CLI (가칭: `x-backup`, 1차)

> Rust 단일 바이너리. **1차 릴리스는 MongoDB 전용.**
> **풀/증분(oplog) 백업·복구**, **로컬/원격 스토리지**, **암호화 중심**.
> PostgreSQL은 2차로 분리(§12). 본 문서는 핵심 기능만 정의한다.

---

## 1. 개요

### 1.1 목적
운영 중인 MongoDB를 **암호화·검증 가능·복구 가능**한 형태로 백업하고, 로컬 또는 원격(S3 호환)에서 일관되게 복구하는 단일 바이너리 CLI를 제공한다.

### 1.2 설계 원칙
- **기본은 드라이버 네이티브 엔진**(외부 의존 없음). MongoDB Rust 드라이버로 직접 데이터·인덱스·컬렉션 옵션을 읽어 자체 아카이브 포맷(`xb-native-v1`)으로 스트리밍한다. 단일 바이너리만으로 백업/복구가 완결된다. `mongodump`/`mongorestore` **오케스트레이션은 opt-in 엔진**(`mode.engine = "mongodump"`)으로 유지한다 — mongodump 아카이브나 덤프 내장 `--oplog` 일관 스냅샷이 필요한 경우. 엔진 선택과 복구 분기는 §10.1 참조.
- Rust의 실익은 *단일 바이너리 배포 · 견고한 에러 처리 · async 파이프라인 · 암호화/압축 생태계*이며, 네이티브 엔진은 여기에 *외부 도구 의존 제거*를 더한다.
- **"백업 존재 ≠ 복구 가능".** 무결성 검증(`verify`)을 1급 기능으로 둔다.
- **암호화는 기본 경로.** 평문 백업은 명시적 `--no-encrypt`가 있어야만 가능.
- **증분은 oplog에 종속적.** 토폴로지 전제를 조용히 우회하지 않고 명시적으로 처리한다.

---

## 2. 목표 / 비목표

### 2.1 목표 (1차)
1. MongoDB **풀 백업/복구**.
2. **증분 백업/복구(oplog 기반)** — replica set 전제(§6).
3. **로컬 디스크 + 원격 객체 스토리지(S3 호환)** 백엔드.
4. **저장 시 암호화(at-rest)** — 압축 후 암호화, 스트리밍.
5. **manifest + 체크섬**으로 무결성 검증.
6. **대상 서버 상태 점검(`status`)** 으로 백업 전 사전 점검.
7. **`config.toml` + 대화형 마법사(`init`) + 환경변수 오버라이드(ENV)** 로 동작·위치·기능 정의.
8. **출력 모드**: 기본 progress / `--quiet`(cron) / 비-TTY 자동 quiet.
9. **최소 보존 관리(`prune`)** — 증분 체인 안전성 검사를 포함한 백업 삭제(정책 기반 자동 rotation은 2차).
10. **동시 실행 잠금** — cron 중복 기동으로부터 증분 체인 보호.

### 2.2 비목표 (1차 제외)
- PostgreSQL·MySQL 지원(PostgreSQL은 2차).
- **샤딩 클러스터(스코프 외).** 데이터·토폴로지 모두 1차 비대상. 1차는 standalone/replica set만 다룬다(로드맵에서 재검토).
- 풀 TUI 대시보드(복구 탐색용 최소 대화형은 로드맵).
- 스케줄러 내장(cron/systemd-timer/CI에 위임).
- KMS·HSM 연동(키 소스 추상화는 두되 구현은 로드맵).

---

## 3. 대상 사용자 · 핵심 유스케이스

소규모 인프라/플랫폼 팀(Docker, cloud-agnostic). 운영자는 CLI를 cron·CI·컨테이너에서 **비대화형(headless)** 으로 실행한다.

| UC | 시나리오 |
|----|----------|
| UC-1 | prod replica set을 매일 풀 백업 → 압축 → 암호화 → S3 호환 스토리지 업로드 |
| UC-2 | 풀 백업 사이에 oplog 증분을 짧은 주기로 적재 |
| UC-3 | 장애 시 원격 백업을 받아 **다른 타깃(staging)** 으로 복구·검증 |
| UC-4 | 특정 시점(PITR)으로 복구 — base + oplog replay |
| UC-5 | 백업 무결성을 주기적으로 `verify` — 체크섬 검증(키 불필요)은 백업 호스트에서, 복호화 검증(`--deep`)은 개인키 보유 호스트에서(§8.5) |
| UC-6 | 백업 실행 전 대상 서버 상태(연결·권한·토폴로지·oplog 윈도우·예상 크기)를 점검 |
| UC-7 | 보존 기준에 따라 오래된 백업을 `prune` — 증분 체인을 끊지 않는 안전 삭제 |

---

## 4. 지원 범위 매트릭스 (MongoDB)

| 토폴로지 | 풀 백업 | 증분(oplog) | 비고 |
|----------|:------:|:----------:|------|
| Replica set | ✅ | ✅ | 증분의 기본 전제. `--oplog`로 dump 시점 일관성 확보 |
| Standalone | ✅ | ❌ | oplog 부재 → 증분 불가. 증분 요청 시 명시적 거부 |
| Sharded cluster | ❌ 스코프 외 | ❌ 스코프 외 | 데이터·토폴로지 모두 1차 비대상. 로드맵에서 재검토 |

---

## 5. 핵심 기능 요구사항 (FR)

### FR-1. 풀 백업
- `mongodump`를 **archive 모드(`--archive`)** 로 실행해 단일 스트림을 stdout으로 받아 파이프라인(§7)에 흘린다(디렉터리 dump 대비 스트리밍·단일 산출물에 유리).
- replica set이면 **`--oplog`** 로 dump 동안의 변경을 포함시켜 dump 자체의 시점 일관성을 확보한다.
- dump 출력을 **메모리에 적재하지 않고** 스트리밍 처리한다.
- 선택적 백업: `--db`, `--collection`. **제약(중요):** `mongodump --oplog`는 전체 인스턴스 dump에서만 동작하므로 **선택적 백업과 병용 불가**. 따라서 선택적 백업은 ① dump 시점 일관성이 보장되지 않고 ② 증분 체인의 base가 될 수 없다. 실행 시 이를 경고하고 manifest에 기록한다(FR-7).

### FR-2. 증분 백업 (oplog)
- 마지막 백업 기준점(oplog 타임스탬프 `ts`) **이후**의 oplog 엔트리를 캡처해 저장한다.
- 캡처는 `mongodump`가 아닌 **드라이버 직접 질의**로 수행한다 — `mongodump`는 임의 `ts` 범위의 oplog 슬라이스 추출을 지원하지 않는다. 저장 포맷·재생 경로는 §6.3.
- 증분은 항상 유효한 **base 풀백업 + manifest 체인**에 연결된다.
- **gap 감지(필수):** 캡처 시작 전, 직전 기준점 `ts`가 현재 oplog 윈도우 안에 아직 존재하는지 확인한다. 존재하지 않으면(롤오버) 증분 체인이 끊긴 것이므로 **증분을 거부하고 풀백업으로 승격**한다(조용히 진행 금지). — *우선순위 최상, §6 상세.*
- standalone 등 oplog 부재 환경에서 증분 요청 시 **사유와 함께 거부**한다.

### FR-3. 복구 (Restore)
- 풀 복구: base 백업을 `mongorestore`로 복원(`--archive` 입력 스트림).
- PITR 복구: base 복원 후 **`mongorestore --oplogReplay`** 로 oplog 슬라이스를 적용, **`--oplogLimit`** 로 목표 시점까지 재생.
- **타깃 분리 복구**: 백업 출처와 다른 접속 URI로 복구 가능(staging 검증용).
- **선택적 복구**: `--only db.collection`(엔진 dump 포맷 허용 범위 내).
- **프로덕션 가드레일**: 기존 데이터를 덮어쓰는 복구는 `--force` 또는 대화형 확인 없이는 거부. `--drop` 류 파괴적 옵션은 기본 비활성.
- **`--dry-run`**: 실제 복원 없이 복구 계획을 출력 — 사용할 base·증분 체인, 대상 URI, 예상 크기, 충돌하는 기존 네임스페이스.
- **복구 사전 점검**: 복구 대상에 대해 연결·권한·서버 버전 호환(manifest에 기록된 백업 원본 서버 버전 대비)·기존 데이터 존재 여부를 점검 후 진행. `--skip-precheck`로 우회 가능.
- **`--at` 의미론**: 입력은 RFC 3339 UTC wall-clock. 해당 시각 **이하의 가장 큰 oplog `ts`** 로 내림 매핑해 재생을 종료하고, 결정된 실제 `ts`를 결과에 보고한다.

### FR-4. 스토리지 백엔드 (로컬 / 원격)
- 추상 `Storage` trait 뒤에 **로컬 파일시스템**과 **S3 호환 객체 스토리지**(MinIO·R2·OCI Object Storage 등) 구현.
- 원격은 **스트리밍 멀티파트 업로드**로 디스크 경유 없이 업로드(가능한 경우).
- 동일 인터페이스로 복구 시 다운로드(읽기) 지원.

### FR-5. 암호화 (핵심) — §8 상세
- 기본 경로에서 모든 산출물(풀·증분·manifest 본문 제외 메타는 평문 허용) 암호화. 평문은 `--no-encrypt` 시에만.
- **compress → encrypt 순서 고정.** 스트리밍 AEAD.

### FR-6. 압축
- 기본 **zstd**(레벨 조절·멀티스레드 옵션). 인프로세스 스트리밍 압축(외부 `gzip`·`mongodump --gzip` 대신 자체 파이프라인으로 제어).

### FR-7. Manifest · 무결성
- 각 백업/증분마다 사이드카 **manifest(JSON)**: **`format_version`**·토폴로지·서버버전·백업유형(full/incr)·base 참조·oplog 시작/끝 `ts`·시각(UTC)·원본/압축 크기·압축/암호화 알고리즘·키 식별자·**sha256 체크섬**·체인 정보·**선택적 백업 여부(증분 base 부적격 표시, FR-1)**.
- **manifest 자체의 무결성도 보호**한다(자체 체크섬 사이드카 또는 카탈로그 수준의 체크섬 목록). `format_version`으로 포맷 진화에 대비한다.
- `verify`는 **2단계**(§8.5):
  - **구조 검증(기본):** 산출물 체크섬 재계산 + manifest 정합 확인. **키 없이** 백업 호스트에서 가능.
  - **심층 검증(`--deep`):** 복호화·압축해제 스트림 디코드 가능 여부 확인. **개인키 보유 호스트에서만** 가능.
- `verify --chain`: PITR에 필요한 base+증분 체인 전체의 연속성(gap 없음)·무결성을 검증. §6.4 PITR의 전제 조건.
- `list`: manifest 기반 가용 백업·증분 체인 카탈로그.

### FR-8. 대상 서버 상태 점검 (`status`) — 백업 사전 점검
백업/복구를 실행하지 않고, 대상 MongoDB가 **백업 가능한 상태인지**를 점검·리포트한다. 단순 ping이 아니라 백업 성공 여부를 좌우하는 항목을 본다.

점검 항목:
1. **연결·인증:** 주어진 URI/자격증명으로 접속 가능 여부, 사용 인증 메커니즘.
2. **권한:** 백업 사용자가 필요한 역할/권한을 가졌는지(예: 백업용 역할, oplog 읽기 권한). 부족 시 어떤 권한이 빠졌는지 보고.
3. **버전 정합:** 서버(mongod) 버전과 클라이언트 도구(`mongodump`/`mongorestore`) 버전, 호환 여부.
4. **토폴로지:** standalone / replica set 판별. replica set이면 멤버 상태(PRIMARY 존재 여부, SECONDARY lag) 요약. **샤딩이면 스코프 외임을 명확히 보고하고 거부.**
5. **oplog 상태(증분 가능성 판단):** oplog 존재 여부, **윈도우 길이(가장 오래된 ~ 최신 `ts` 시간 폭)**. 설정된 증분 주기 대비 윈도우가 충분한지(§6.2 gap 위험) 경고.
6. **저장 엔진:** WiredTiger 등.
7. **예상 백업 크기:** `dbStats` 기반 데이터/스토리지 크기로 백업 산출물 규모를 추정(용량 계획용).
8. (선택) **백업 소스 선택성:** secondary에서 백업할 수 있는 구성인지(부하 분리용).

동작 요건:
- **읽기 전용·무부작용.** 어떤 쓰기/변경도 하지 않는다.
- 결과를 사람이 읽는 요약과 **`--json`** 구조화 출력 둘 다 지원(스크립트·모니터링 연동).
- 점검 결과를 신호등(정상/경고/실패)으로 요약하고, 항목별 **종료 코드**로 비대화형 분기 지원.
- `backup` 실행 시 **사전 점검을 자동 선행**(핵심 항목 실패면 백업 중단)하되, `--skip-precheck`로 우회 가능.

### FR-9. 출력 모드 (quiet / progress)
백업·복구 실행 시 환경에 맞는 출력을 제공한다.

- **기본(대화형, TTY):** 단계별 **진행 표시(progress)** — dump→압축→암호화→업로드 파이프라인의 처리 바이트·throughput·경과시간을 보여준다.
  - 백업은 **dump 총량을 사전에 모를 수 있으므로**, `status`의 `dbStats` 추정치가 있으면 근사 퍼센트를, 없으면 처리 바이트·속도 기반의 **부정형(indeterminate) 진행**으로 표시한다(*추정치이며 정확한 %가 아님을 명시*).
  - 복구는 저장 산출물 크기(`stored_size_bytes`)는 알지만, **압축 백업은 복호화·압축해제 후 입력량을 사전에 정확히 알 수 없어** 처리 바이트·속도 기반 **부정형(indeterminate) 진행**으로 표시한다(실제 구현 결정). 비압축 산출물은 저장 크기 = 복원 입력량이므로 근사 퍼센트 진행이 가능하다.
- **`--quiet`(cron/CI용):** 진행 표시를 억제하고 **결과 요약·경고·에러만** 구조화 로그로 출력. 종료 코드로 성공/부분실패/실패를 전달.
- **자동 감지:** **비-TTY 환경에서는 자동으로 quiet** 동작(진행 바가 로그를 오염시키지 않게).
- **강제 옵션:** `--progress`로 비-TTY에서도 진행 출력 강제, `--json`으로 진행/결과를 기계 판독 형식으로.
- config의 출력 모드 기본값(§FR-10)을 따르되, CLI 플래그가 우선한다.

### FR-10. 설정 파일(`config.toml`) + 마법사(wizard) + 환경변수(ENV)
동작을 코드가 아닌 **`config.toml`** 로 정의하고, 대화형 마법사로 이를 생성한다. 모든 설정값은 **환경변수로 오버라이드**할 수 있다.

- **마법사(`init`/`config wizard`):** 질문에 답하면 `config.toml`을 생성한다. 항목: 접속 대상, 백업 위치(로컬/원격), 압축·암호화·증분·출력 모드 기본값. 생성 후 선택적으로 `status`를 돌려 연결·권한을 즉시 검증.
  - **기존 파일 가드:** 기존 `config.toml`을 덮어쓸 때는 `--force` 또는 대화형 확인을 요구.
  - **시크릿 비저장 원칙:** 비밀번호·키 같은 시크릿은 config에 평문 저장하지 않고 **환경변수 참조(`uri_env = "MONGO_URI"`)** 나 외부 키 파일 경로로 유도한다. 마법사는 시크릿 입력을 env 참조로 안내한다.
- **프로파일:** 하나의 `config.toml`에 복수 프로파일(예: `prod`/`staging`)을 둘 수 있고, CLI `--profile`로 선택.

#### 환경변수(ENV) 지원 — 두 용도
1. **시크릿 참조:** config는 시크릿 *값*이 아니라 *env 변수명*만 담는다(`uri_env`, `credentials_env`). 실행 시 해당 env에서 시크릿을 읽는다. config 파일이 유출돼도 시크릿은 노출되지 않는다.
2. **설정 오버라이드(12-factor):** 임의의 config 값을 환경변수로 덮어쓸 수 있다. 컨테이너·CI에서 파일 없이/부분 변경으로 운영하기 위함.
   - 규칙(예): 접두사 + 중첩 키를 구분자로 평탄화. 예 `XB_DESTINATION__S3__BUCKET=db-backups-staging`, `XB_MODE__OUTPUT=quiet`. (*접두사·구분자 최종안은 구현 시 확정 — 현 시점 단정하지 않음.*)
   - config 파일이 아예 없어도 ENV만으로 최소 동작 구성이 가능해야 한다.

- **우선순위(높음→낮음): `CLI 플래그 > 환경변수(ENV) > config.toml > 내장 기본값`.**

### FR-11. 보존 관리 (`prune`) — 1차 최소 범위
무한 증가하는 백업 스토리지를 1차에서도 운영 가능하게 하는 최소 삭제 기능. 정책 기반 자동 rotation(GFS)은 2차(§12).

- 기준 인자(`--keep-full N`, `--keep-days D`)에 따라 오래된 백업을 삭제한다.
- **체인 안전 규칙(필수):** 살아있는 증분이 참조하는 base 풀백업은 삭제하지 않는다. 체인은 **base+증분을 한 단위**로만 삭제한다.
- `--dry-run`으로 삭제 대상 목록만 출력. 실제 삭제는 `--force` 또는 대화형 확인 필요.
- 백업 산출물을 prune을 거치지 않고 수동 삭제하면 체인이 끊어질 수 있음을 문서화하고, `list`/`verify --chain`이 끊어진 체인을 감지·표시한다.

### FR-12. 동시 실행 잠금
- 동일 프로파일(동일 destination)에 대한 `backup`/`restore`/`prune` 동시 실행을 잠금으로 방지한다 — 풀 백업 장기화 중 증분 cron이 중복 기동하는 시나리오가 대표적이며, 방치하면 증분 체인 정합성이 깨진다.
- 잠금 충돌 시 즉시 실패하고 전용 exit code(§9)로 보고한다.
- 비정상 종료로 남은 stale lock의 감지·해제 절차를 둔다(구현 방식은 §13).

#### config.toml 정의 범위 (스키마)

config는 **두 형식**으로 쓸 수 있고, 로더가 자동 판별한다:

- **v2 (권장)** — 프로파일마다 flat한 `[profile.<name>]` 테이블. 공통 정책은 `[defaults]`
  (전체 적용)·`[base.<name>]` + `extends`(재사용)로 한 번만 정의한다. compact 한 줄 표기
  (`dest`·`compress`·`encrypt`)를 지원한다.
- **v1 (계속 지원)** — 아래 §"v1 중첩 스키마(레거시)"의 깊은 중첩 레이아웃. 기존 config는
  무변경으로 로드된다.

**판별:** 단수 `[profile]`/`[defaults]`/`[base]` 테이블 → v2, 복수 `[profiles]` → v1. **한 파일에
둘을 섞으면 에러**(반쪽 마이그레이션 방지). v2 표면 문법은 `normalize_v2`가 v1 중첩 트리로
정규화하므로 내부 구조·역직렬화·ENV 오버라이드 파이프라인은 무변경이다.

##### v2 표면 문법 (권장)
```toml
default_profile = "prod"

[output]
language = "ko"             # 설명/안내 문구 언어(en | ko). 라벨·기술용어는 항상 영문.

[defaults]                  # 모든 프로파일에 적용(최하위 우선순위)
compress = "zstd:10"        # 알고리즘[:레벨]
encrypt  = "age:/etc/x-backup/age.pub"   # "age:<공개키-경로>" | true | false | "off"

[base.s3-central]           # 재사용 destination 정책(프로파일 아님, extends로만 참조)
dest      = "s3:db-backups" # "local:/path" | "s3:bucket/prefix"
s3_region = "ap-northeast-2"
s3_endpoint = "https://s3.example.com"    # MinIO/R2/OCI 등 S3 호환
s3_creds  = "S3_CREDS"      # "ACCESS_KEY:SECRET_KEY"를 담은 env 변수 *이름*

[profile.prod]
extends          = "s3-central"  # base/다른 프로파일 상속. 배열도 가능(뒤가 우선)
uri_env          = "MONGO_URI"   # 시크릿은 env 참조(평문 금지)
prefer_secondary = true          # 가능하면 secondary에서 백업
s3_prefix        = "mongo/prod"
incr_interval    = "15m"         # gap 위험 경고 기준(스케줄러 아님 — cron에 위임)
incr_on_gap      = "promote_full"# gap 감지 시 풀백업으로 자동 승격
keep_last        = 100           # retention(prune 기본값, CLI 우선)
keep_days        = 30

[profile.pg]
extends    = "s3-central"
uri_env    = "PG_URI"            # postgresql:// → PostgreSQL 엔진 자동 선택
s3_prefix  = "pg/prod"
pg_logical = true                # PG 증분/PITR opt-in(서버 wal_level=logical 필요)
```

상속 우선순위(낮음→높음): `[defaults] < extends 체인(왼→오, 뒤가 우선) < 프로파일 자신 키`,
그 위로 `ENV(XB_*) > 파일`, `CLI > ENV`. 생략 가능한 기본값: `backup_type=full`,
`output_mode=progress`, `precheck=true`, `engine=native`, 압축 `zstd`/레벨 `10`, 암호화
`enabled=true`+`age`, 증분 `interval=15m`/`on_gap=promote_full`/`pg_logical=false`,
`prefer_secondary=false`.

v2 flat 키 → v1 중첩 매핑: `uri`/`uri_env`/`prefer_secondary`/`connect_timeout_secs` →
`source.*`; `backup_type`/`output_mode`(→`mode.output`)/`precheck`/`engine` → `mode.*`;
`dest`/`dest_name`/`s3_*` → `destination.*`(또는 `[[profile.x.dest]]` 배열 → `destinations[]`);
`compress` → `features.compression.*`; `encrypt` → `features.encryption.*`;
`incr_interval`/`incr_on_gap`/`pg_logical` → `features.incremental.*`;
`keep_full`/`keep_days`/`keep_last` → `retention.*`.

##### v1 중첩 스키마 (레거시 — 계속 지원)
```toml
default_profile = "prod"

# ── 동작 모드 ──
[profiles.prod.mode]
backup_type = "full"        # full | incr (기본 백업 유형)
output      = "progress"    # progress | quiet (기본 출력 모드)
precheck    = true          # 백업 전 status 자동 선행 여부

# ── 접속 대상 ──
[profiles.prod.source]
uri_env          = "MONGO_URI"   # 시크릿은 env 참조(평문 금지)
prefer_secondary = true          # 가능하면 secondary에서 백업

# ── 백업되는 위치(destination) ──
[profiles.prod.destination]
type = "s3"                 # local | s3
# local 예: path = "/var/backups/mongo"

[profiles.prod.destination.s3]
endpoint        = "https://s3.example.com"   # MinIO/R2/OCI 등 S3 호환
bucket          = "db-backups"
prefix          = "mongo/prod"
region          = "ap-northeast-2"
credentials_env = "S3_CREDS"                 # env 참조

# ── 기능 정의 ──
[profiles.prod.features.compression]
algorithm = "zstd"
level     = 10

[profiles.prod.features.encryption]
enabled        = true
algorithm      = "age"      # age | aes-256-gcm
recipient_file = "/etc/x-backup/age.pub"     # 공개키(복호화 키는 별도 격리)

[profiles.prod.features.incremental]
# interval은 스케줄링용이 아니다(스케줄러는 비목표 — cron에 위임).
# status/backup이 oplog 윈도우 대비 gap 위험을 경고하는 기준값으로만 사용한다.
interval = "15m"            # 권장: oplog 윈도우보다 충분히 짧게
on_gap   = "promote_full"   # gap 감지 시 풀백업으로 자동 승격
# retention: 1차는 prune의 CLI 인자로 지정(FR-11). 정책 기반 자동화는 로드맵(§12)
```

---

## 6. 증분(oplog) 설계 상세 — 1차의 핵심 난점

### 6.1 oplog 개요
- replica set의 oplog는 capped collection(`local.oplog.rs`)이며, 각 엔트리는 BSON Timestamp `ts`를 가진다.
- 증분 = `ts > last_backup_ts` 인 oplog 엔트리를 질의·저장.

### 6.2 gap(윈도우 롤오버) 처리 — 최우선
- oplog는 capped라 용량 초과 시 오래된 엔트리가 밀려난다. **백업 간격 > oplog 보존 윈도우** 이면 직전 기준점이 사라져 증분 체인에 구멍(gap)이 생긴다.
- 규칙: 증분 시작 전 `last_backup_ts`가 oplog 최소 `ts` 이상으로 **여전히 존재**하는지 확인. 없으면 **증분 불가 → 풀백업 승격**(명시 로그·exit code 구분).
- 운영 가이드: oplog 윈도우 대비 충분히 짧은 증분 주기 권장(설정·문서화).

### 6.3 증분 캡처·저장 포맷
- `mongodump`는 임의 `ts` 범위의 oplog 슬라이스 추출을 지원하지 않는다. 증분 캡처는 **MongoDB 드라이버로 `local.oplog.rs`를 직접 질의**(`ts > last_backup_ts`, natural order)해 수행한다.
- 캡처한 엔트리는 **BSON 스트림**으로 저장하며, 파이프라인(§7)의 압축·암호화 단계를 풀 백업과 동일하게 통과한다.
- **재생 경로:** `mongorestore --oplogReplay`는 dump 디렉터리 내 **`oplog.bson`** 배치를 기대한다. 복구 시 증분 산출물을 복호화·압축해제해 이 구조로 변환하고, 체인 순서대로 슬라이스를 재생한다.
- 대량 트랜잭션(`applyOps`)·DDL 엔트리·서버 버전별 oplog 포맷 차이는 구현 시 대상 버전 실측으로 재검증한다(하단 불확실성 참조).

### 6.4 PITR 복구 절차
1. 목표 시점 직전의 base 풀백업 복원.
2. base 이후 증분 oplog 슬라이스를 순서대로 `--oplogReplay` 적용.
3. 목표 `ts`까지 `--oplogLimit`로 재생 종료.
- **`verify --chain`으로 체인 무결성(연속성·gap 없음)을 검증한 후에만** PITR을 허용한다(FR-7).

### 6.5 샤딩 클러스터 (스코프 외)
- 샤드별 oplog가 독립적이라 교차 샤드 단일 시점 일관성을 보장할 수 없고, 토폴로지(샤드 키·청크 분포)도 논리 dump로 복원되지 않는다.
- 따라서 **1차에서 샤딩은 데이터·토폴로지 모두 비대상**이다. `status`가 샤딩을 감지하면 명확히 보고하고 백업을 거부한다(조용히 부분 처리 금지). 향후 스냅샷 기반 접근은 로드맵(§12).

> **불확실성 명시:** oplog 슬라이싱 경계, 대량 트랜잭션·DDL 엔트리 처리, 서버 버전별 oplog 포맷 차이는 **구현 시 대상 버전 문서·실측으로 재검증**한다.

---

## 7. 데이터 파이프라인

```
[mongodump --archive (--oplog)] → stdout
   → [zstd 스트리밍 압축]
   → [AEAD 스트리밍 암호화]
   → [Storage: 로컬 파일 | S3 멀티파트 업로드]
   → [manifest 기록(체크섬·oplog ts·체인)]
```

- 복구 역방향: `Storage 읽기 → 복호화 → 압축해제 → mongorestore stdin`.
- **증분의 소스는 `mongodump`가 아니라 드라이버 oplog 리더(§6.3)** 이며, 이후 압축→암호화→저장 단계는 동일하다.
- 전 구간 스트리밍(전체 산출물을 메모리/디스크에 적재하지 않음) 원칙.
- async(tokio)로 백업/업로드 파이프 동시성 제어.

---

## 8. 암호화 설계 (핵심)

### 8.1 1순위 권장: 비대칭 `age`
- **이유:** 공개키로 암호화 → **백업 호스트는 공개키만 보유**, 개인키 없이는 복호화 불가. 백업 서버가 침해돼도 과거 백업이 복호화되지 않는다. "암호화 중심" 요구에 가장 부합.
- 개인키는 별도 보관(운영자 vault·복구 전용 호스트)에 격리.

### 8.2 대안: AES-256-GCM (대칭)
- 키 공유가 단순한 폐쇄 환경용. **반드시 청크 단위 AEAD(프레이밍)** 로 스트리밍 — 단일 GCM으로 대용량 처리 금지(nonce 재사용 방지).

### 8.3 키 관리
- 키 소스 추상화: `파일 | 환경변수 | (로드맵)KMS`. 시크릿 인자 평문 노출 금지.
- manifest에 **키 식별자·알고리즘** 기록(키 자체는 미저장) → 복구 시 필요한 키 식별 가능.

### 8.4 순서 규칙
- **compress → encrypt 고정**(암호문은 압축 불가).

### 8.5 검증과 키 격리의 관계
- 공개키-only 백업 호스트(§8.1)에서는 **복호화 검증이 불가능**하다. 키 격리의 이점과 검증 가능성은 트레이드오프 관계다.
- 따라서 `verify` 기본 모드는 키 없이 가능한 **구조 검증**(체크섬·manifest 정합)까지만 수행하고, 복호화까지 확인하는 **`--deep`은 개인키 보유 호스트**(복구 전용 호스트 등)에서 주기 실행한다(UC-5, FR-7).

---

## 9. CLI 인터페이스 (스케치)

```
x-backup init                          # 대화형 마법사 → config.toml 생성
                  [--force]            #  (기존 파일 덮어쓰기 시)
x-backup backup   --profile <name> [--type full|incr]
                  [--db <db>] [--collection <coll>]
                  [--no-encrypt] [--compress-level N]
                  [--quiet | --progress] [--json] [--skip-precheck]
x-backup restore  --profile <name> [--target <mongo-uri>]
                  [--at <timestamp>] [--only db.collection] [--force]
                  [--dry-run]          # 복구 계획만 출력(체인·대상·예상 크기·충돌)
                  [--skip-precheck]
                  [--quiet | --progress] [--json]
x-backup list     [--profile <name>]
x-backup verify   --id <backup-id> [--deep] [--chain]
                  # 기본: 체크섬·manifest 구조 검증(키 불필요)
                  # --deep: 복호화·디코드 검증(개인키 필요, §8.5)
                  # --chain: PITR용 base+증분 체인 연속성 검증
x-backup prune    --profile <name> [--keep-full N] [--keep-days D]
                  [--dry-run] [--force]
                  # 체인 안전 삭제(FR-11): base+증분을 한 단위로
x-backup status   --profile <name> [--json]
                  # 대상 서버 상태 점검(연결·권한·토폴로지·oplog 윈도우·예상 크기)
                  #  + mongodump/mongorestore 존재·버전 정합. 읽기 전용, 무부작용
```

- `backup`은 실행 전 `status`의 핵심 점검을 자동 선행하며 `--skip-precheck`로 우회 가능.

- 설정: **`config.toml`** 프로파일(`--profile prod`)에 접속·백업 위치·기능·동작 모드 정의(§FR-10). 시크릿은 env 참조로 외부 주입하고, 모든 값은 **환경변수(ENV)로 오버라이드** 가능. 우선순위 `CLI > ENV > config.toml > 기본값`.
- **출력 모드(§FR-9):** 기본은 progress, `--quiet`(cron/CI)로 억제, **비-TTY는 자동 quiet**, `--progress`로 강제, `--json`으로 기계 판독.
- **종료 코드 규약:**

| 코드 | 의미 |
|:---:|------|
| 0 | 성공 |
| 1 | 실패 — 작업 미완료(파이프라인·업로드·복구 오류) |
| 2 | 사용법·설정 오류(잘못된 플래그, config 결함) |
| 3 | 사전 점검 실패 — 작업 미시작(`status` 핵심 항목 실패) |
| 4 | 경고 동반 성공(예: gap 감지로 증분→풀 승격, verify 경고) |
| 5 | 잠금 충돌 — 동일 프로파일의 다른 인스턴스 실행 중(FR-12) |

---

## 10. 아키텍처 (trait 경계)

```
Engine   (trait): status / backup_full / backup_incr / restore   — MongoAdapter (1차)
Storage  (trait): put_stream / get_stream / list / delete — LocalFs, S3Compatible
Crypto   (trait): encrypt_stream / decrypt_stream         — AgeCrypto, AesGcmCrypto
Compress (trait): compress_stream / decompress_stream     — Zstd
Manifest        : 메타·체크섬·oplog ts·체인 기록/조회
```

- `Engine` trait를 미리 분리해 2차 PostgreSQL 어댑터를 무변경으로 추가할 수 있게 한다.
- 후보 크레이트(빌드 시 버전·관리상태 재검증): `tokio`, `clap`, `zstd`, `age` 또는 `aes-gcm`, S3 클라이언트(예: `aws-sdk-s3` 또는 멀티백엔드 추상화), `sha2`, `serde`/`serde_json`, `indicatif`, `tracing`.
> 크레이트는 후보이며 **현 시점 단정하지 않는다.**

### 10.1 백업 엔진 선택 (native | mongodump)

프로파일 `mode.engine`으로 dump/restore를 수행할 엔진을 고른다. **기본 `native`.**

| 엔진 | 외부 도구 | 아카이브 포맷 | 캡처(1차 스코프) | dump 경로 | restore 경로 |
|------|----------|--------------|-----------------|----------|-------------|
| `native`(기본) | 없음 | `xb-native-v1` | 데이터 + 인덱스 + 컬렉션 옵션(capped·validator·collation 등) | 드라이버 커서 → 태그+BSON 프레임 스트림(`DuplexStream` + spawn task, 상수 메모리) | 프레임 파싱 → `create`(옵션) + `createIndexes` + `insert_many` 배치 |
| `mongodump` | `mongodump`/`mongorestore` | mongodump `--archive` | mongodump 산출물 + 아카이브 내장 `--oplog` 일관 스냅샷 | `mongodump --archive=-` stdout | `mongorestore --archive=-` stdin |

- **공통:** 두 엔진 모두 동일한 압축→암호화 파이프라인(§7)을 통과하고, 풀 백업 시 체이닝용 oplog 타임스탬프를 드라이버로 기록한다(엔진 무관). 증분 캡처는 항상 드라이버 oplog 리더(§6.3)다.
- **복구 분기:** 백업을 만든 엔진은 manifest `tool_versions.archive_format`에 기록된다. `restore`는 이 값으로 자동 분기한다 — `xb-native-v1`이면 드라이버 네이티브 복구, 그 외(mongodump)는 `mongorestore`. 프로파일 엔진을 바꿔도 과거 백업은 원래 엔진 경로로 복구된다.
- **네이티브 1차 스코프 제외:** view·timeseries 등 비일반 컬렉션은 건너뛰고 경고한다(후속 확장). mongodump 엔진의 아카이브 내장 일관 `--oplog` 스냅샷은 네이티브에 없다(네이티브는 풀 백업 시점의 oplog *타임스탬프*만 기록).
- **migrate 명령:** backup/restore와 동일하게 프로파일 `mode.engine`을 따른다(기본 native). native면 `NativeDumper`(source) → `native_restore`(target)를 in-process로 직접 흘려 외부 도구 없이 복사하고, mongodump면 `mongodump | mongorestore` 파이프를 쓴다. 어느 쪽이든 파일·디스크 경유 없이 데이터+인덱스+옵션을 복사한다.

---

## 11. 비기능 요구사항

- **신뢰성:** 실패 시 부분 산출물을 남기지 않거나 manifest에 미완료로 표기. 업로드 재시도/재개 고려. 동시 실행은 잠금으로 직렬화(FR-12). 중단된 멀티파트 업로드의 잔여 파트 정리(abort) 포함 — 방치 시 스토리지 비용 누수.
- **이식성:** 단일 정적 바이너리. Linux x86_64/arm64 우선, macOS 개발 지원.
- **보안:** 시크릿 로그 미출력. 기본 암호화. 임시 파일 권한 제한. **자식 프로세스(`mongodump`/`mongorestore`)에 시크릿을 argv로 전달 금지**(`ps`에 평문 노출) — 환경변수 또는 권한 제한된 임시 config 파일로 전달.
- **관측성:** 구조화 로그(JSON 옵션). 마지막 성공 시각·소요·크기 메트릭 노출은 로드맵.
- **외부 의존:** 기본 `native` 엔진은 외부 도구 의존이 없다(드라이버만 사용). `mongodump` 엔진을 선택한 경우에만 `mongodump`/`mongorestore` 존재·버전을 `status` 사전 점검에 포함한다(FR-8) — 네이티브 엔진은 도구 존재 점검을 건너뛴다. 서버 토폴로지·oplog·권한 점검은 엔진과 무관히 수행한다.

### 11.1 수용 기준 (1차 릴리스 게이트)
측정 가능한 완료 조건. 수치는 기준 환경 확정 시 조정할 수 있으나, 항목 자체는 릴리스 전 실측 충족을 요구한다.

1. **RPO:** 정상 운영 시 복구 가능 시점 손실 ≤ 설정한 증분 주기(기본 15m). gap 발생 시 자동 풀 승격으로 체인이 복원됨을 확인.
2. **스트리밍 상한:** 데이터 크기와 무관하게 프로세스 상주 메모리가 상수 상한(예: ≤ 512 MiB)을 유지 — 100 GB급 dump로 실측.
3. **복구 정합:** 기준 데이터셋에 대해 backup → restore 후 모든 컬렉션의 문서 수·콘텐츠 해시가 원본과 일치. PITR은 목표 `ts` 이후의 쓰기가 결과에 없음을 확인.
4. **gap 시나리오:** oplog 롤오버를 유도한 테스트에서 증분이 거부되고 풀 승격 + exit code 4가 보고됨.
5. **암호화 경로:** `--no-encrypt` 없이 생성된 모든 산출물이 암호문임을 확인. 공개키-only 호스트에서 복호화 불가 확인.

---

## 12. 로드맵

| 항목 | 단계 |
|------|------|
| **PostgreSQL 어댑터 — 풀 백업/복구/migrate/status/peek/watch(드라이버 COPY, 외부 도구 0)** | **2차 — 구현됨** |
| PostgreSQL 스키마 충실도 — 멀티스키마·IDENTITY·generated·확장·enum/도메인/복합 타입·함수/프로시저·트리거·뷰/머티뷰·파티셔닝·시퀀스 파라미터 | **2차 — 구현됨** |
| PostgreSQL 잔여 — 소유권/권한·코멘트·집계/윈도우 함수·user-defined base/range 타입·증분/PITR(WAL) | 2차 — 후속 |
| Slack 알림(성공/실패/소요/크기) | 2차 |
| 정책 기반 retention/rotation(GFS) — 최소 `prune`은 1차(FR-11) | 2차 |
| Prometheus 메트릭 노출 | 2차 |
| 복구 탐색용 최소 TUI(ratatui) | 2차 |
| 샤딩 클러스터 지원(스냅샷 기반 검토) | 검토 |
| KMS/HSM 키 연동 | 후반 |
| 복구 리허설(ephemeral 자동 복원 테스트) | 후반 |

---

## 13. 미해결 질문 (구현 전 확정)

1. **S3 백엔드 추상화:** 단일 SDK vs 멀티 클라우드 추상화 계층.
2. **암호화 기본값:** `age`(비대칭) 기본 확정 및 키 배포·복구 운영 절차.
3. **증분 주기 정책:** oplog 윈도우 대비 안전 주기 권장값·gap 시 자동 풀백업 정책 확정.
4. **manifest 저장 위치:** 백업과 동일 스토리지 vs 별도 카탈로그.
5. **mongodump 호환:** 대상 MongoDB 서버 버전 분포와 클라이언트 도구 버전 정합(*현재 정보로는 확인 모호*).
6. **status 권한 점검 기준:** 백업 사용자에게 요구할 최소 역할/권한 집합 확정.
7. **oplog 캡처 포맷 검증:** 드라이버 직접 질의 → BSON 저장 → `oplog.bson` 재생 경로(§6.3)를 대상 서버 버전별로 실측 검증.
8. **prune 보존 기본값:** `--keep-full`/`--keep-days` 권장 기본값과 체인 단위 삭제 UX 확정.
9. **잠금 구현 방식:** 로컬 lock 파일 vs destination 측 마커, stale lock 감지·해제 절차(FR-12).

> 확정됨: 대상 토폴로지 = standalone/replica set만(샤딩 스코프 외, 데이터 백업/복구만).

---

*1차는 MongoDB(replica set) 풀/증분 + 로컬·원격 + 암호화에 집중한다. §13 확정 후 모듈별 상세 설계로 전개하고, 검증된 `Engine` trait 위에 2차 PostgreSQL을 얹는다.*
