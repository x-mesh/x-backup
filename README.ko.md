<p align="center">
  <a href="README.md">English</a> · <strong>한국어</strong>
</p>

# x-backup

> MongoDB·PostgreSQL 백업·복구 CLI — 풀/증분(oplog), PITR, 로컬/S3 호환 스토리지, 암호화 중심. Rust 단일 바이너리, **외부 덤프 도구 불필요.**

운영 중인 MongoDB(standalone/replica set) 또는 PostgreSQL을 **암호화·검증 가능·복구 가능**한
형태로 백업한다. 기본적으로 Rust 드라이버로 DB와 **직접** 통신하므로
**`mongodump`/`mongorestore`·`pg_dump`/`pg_restore`가 필요 없다.** 전 구간 스트리밍(데이터
크기와 무관한 상수 메모리 — [실측 보고서](docs/memory-profile.md): 6 GiB 백업 피크 RSS
54.6 MiB)으로 동작한다. DB 종류는 source URI 스킴(`mongodb://` vs `postgresql://`)으로 자동 선택된다.

## Features

- ✅ **풀 백업** — 드라이버 네이티브 스트리밍 아카이브(데이터 + 인덱스 + 컬렉션 옵션), 외부 도구 불필요. `mongodump --archive --oplog`는 opt-in 엔진으로 선택 가능
- ✅ **증분 백업** — oplog 직접 캡처, gap 감지 시 풀 백업 자동 승격(exit 4)
- ✅ **PITR** — `--at <RFC3339>` 시점 복구 (base + oplog replay, 체인 검증 전제)
- ✅ **스토리지** — 로컬 디스크 / S3 호환(MinIO·R2·OCI), 스트리밍 멀티파트 + abort
- ✅ **암호화 기본** — `age`(X25519, 공개키만 백업 호스트에 배치) / AES-256-GCM 대안, zstd 압축 후 암호화
- ✅ **무결성** — manifest + sha256, `verify`(키 불필요 구조 검증) / `--deep` / `--chain`
- ✅ **운영** — `status` 사전 점검(연결·토폴로지·권한·버전/FCV·시계차·oplog 윈도우·데이터 형상·**마지막 백업 나이**·**destination 쓰기 가능+여유 공간**), `--all` source/target 비교, `--watch` 라이브 모니터, `prune` 체인 안전 삭제, 동시 실행 잠금, exit code 규약 0~5
- ✅ **PostgreSQL** — COPY 프로토콜 기반 드라이버 네이티브 풀 백업/복구(데이터 + 테이블 + 제약 + 인덱스 + 시퀀스), `pg_dump`/`pg_restore` 불필요. 동일 파이프라인(압축→암호화→저장)·동일 `status`/`list`/`verify`/`restore`
- ✅ **headless** — 비-TTY 자동 quiet, `--json`, cron/CI 친화

지원 범위: MongoDB replica set(풀+증분)/standalone(풀만)/샤딩은 감지 시 거부. PostgreSQL은 풀 백업+복구+status(증분/PITR은 로드맵). [PostgreSQL](#postgresql-1) 참조.

## Install

### Homebrew

```bash
brew install x-mesh/tap/x-backup
```

private 단계에서는 릴리스 자산 다운로드에 GitHub 토큰이 필요하다:

```bash
export HOMEBREW_GITHUB_API_TOKEN=$(gh auth token)
brew install x-mesh/tap/x-backup
```

### curl (install.sh)

```bash
# 저장소 공개 후:
curl -fsSL https://raw.githubusercontent.com/x-mesh/x-backup/main/install.sh | sh

# private 단계(gh 인증 재사용):
gh api repos/x-mesh/x-backup/contents/install.sh --jq '.content' | base64 -d | sh
```

`~/.local/bin/x-backup`에 설치된다. `XB_VERSION`, `XB_INSTALL_DIR`로 조정.

### 소스 빌드

```bash
git clone git@github.com:x-mesh/x-backup.git && cd x-backup
make build        # → target/release/x-backup
```

기본 `native` 엔진은 외부 도구가 필요 없다. `mongodump`/`mongorestore`(MongoDB Database
Tools 100.x)는 `mongodump` 엔진을 선택할 때만 PATH에 필요하다([백업 엔진](#백업-엔진) 참조) —
`make tools`로 프로젝트 로컬(.tools/)에 sha256 검증 설치 가능.

## Update

```bash
x-backup update           # 설치 소스 자동 감지
x-backup update --check   # 확인만
```

- **brew 설치** → `brew upgrade x-mesh/tap/x-backup`으로 위임
- **install.sh 설치** → 최신 릴리스 다운로드 + sha256 검증 + 원자적 자기 교체
- **cargo install** → 갱신 명령 안내만(덮어쓰지 않음)

private 단계에서는 `GITHUB_TOKEN`(또는 `gh auth login`)이 필요하다.

## Quick Start

```bash
x-backup init                                   # 대화형 마법사 → config.toml
x-backup status  --profile prod                 # 백업 가능 상태 점검(신호등)
x-backup status  --all                          # 모든 프로파일 비교(source vs target)
x-backup status  --profile prod --watch         # 라이브 모니터: 네임스페이스별 문서/크기 Δ (Ctrl-C)
x-backup peek    --profile prod                 # 데이터 육안 확인: 컬렉션별 문서 수 + 최신 문서
x-backup backup  --profile prod                 # 풀 백업 → 압축 → 암호화 → 저장
x-backup backup  --profile prod --type incr     # oplog 증분
x-backup list    --profile prod                 # 카탈로그(체인 상태 포함)
x-backup verify  --id <backup-id>               # 키 없는 구조 검증
x-backup restore --profile prod --target mongodb://staging --dry-run
x-backup restore --profile prod --target mongodb://staging --force
x-backup restore --profile prod --at 2026-06-01T00:00:00Z --force   # PITR
x-backup prune   --profile prod --keep-full 7 --dry-run
x-backup migrate --profile prod --target mongodb://newcluster --force # 파일 없이 직접 복사
```

### Migrate (파일 없이 직접 복사)

`migrate`는 한 MongoDB를 다른 MongoDB로 중간 파일 없이 바로 스트리밍 복사한다. 저장·검증
가능한 백업본이 필요 없는 일회성 이전에 쓴다. `backup`/`restore`처럼 프로파일의
[엔진](#백업-엔진)을 따른다 — 기본 `native` 엔진은 **외부 도구 없이** 드라이버끼리 직접
스트리밍하고, `mongodump` 엔진은 `mongodump | mongorestore` 파이프를 쓴다. 어느 쪽이든
데이터·인덱스·컬렉션 옵션을 복사한다.

```bash
x-backup migrate --profile prod --target mongodb://newcluster --dry-run
x-backup migrate --profile prod --target mongodb://newcluster --drop --force
x-backup migrate --profile prod --target-profile staging --drop --force   # target을 프로파일로
```

target은 URI 직접(`--target`) 또는 다른 프로파일의 source(`--target-profile <name>`,
같은 config에서 해석)로 줄 수 있다 — URI를 붙여넣지 않고 양쪽 접속을 config.toml에
둘 수 있다.

**target 규칙**(migrate는 *교체*이고, target 전체를 지우지 않는다):
- **빈 target** → 플래그 없이 그냥 복사된다.
- **데이터 있는 target** → `--drop` **필수**. 없으면 거부(exit 2)한다 — `--drop` 없는
  복사는 어중간한 merge(insert만, 같은 `_id`는 기존 유지, 옛 문서 잔존)라 마이그레이션
  의도와 거의 안 맞기 때문. `--drop`이면 **source에 있는 컬렉션만** drop 후 재생성하고
  target의 다른 컬렉션은 손대지 않는다. target DB/인스턴스 전체는 지우지 않는다.
- `--drop`은 파괴적이라 `--force`(또는 대화형 확인)도 필요하다. 즉 깨끗한 교체는 `--drop --force`.

증분 마이그레이션은 없다 — `migrate`는 일회성 복사다. 시점 이전이나 체인이 필요하면
파일 경로(`backup` → `restore --at`)를 쓴다. 라이브 마이그레이션(oplog tailing 무중단
cutover)은 로드맵 항목이며 미구현이다.

백업이 아니라 복사다: manifest·체크섬·at-rest 암호화·PITR이 없다. 쓰기가 많은
replica set을 정확한 시점으로 옮기거나 검증 가능한 산출물을 남기려면 파일 경로
(`backup` → `restore --target`, `--oplog`/PITR 지원)를 쓴다.

### config.toml

config에는 헷갈리기 쉬운 세 축이 있다:

- **source** — 백업 대상 MongoDB(prod). 자격증명이 있으면 `uri_env`(환경변수 *이름*),
  로컬·무자격증명이면 `uri`(직접 값)도 된다.
- **destination** — 백업 *파일*을 둘 곳. 저장소(`local`/`s3`)이며 **MongoDB가 아니다**.
- **복구 대상** — 복구를 부을 MongoDB. config가 아니라 복구 시 `restore --target <uri>`로 지정.

```toml
default_profile = "prod"

[profiles.prod.source]
uri_env = "MONGO_URI"            # prod: env 참조(config는 유출될 수 있으니 시크릿은 밖에)
# uri = "mongodb://localhost:27017/?replicaSet=rs0"   # 개발·무자격증명: 직접 값도 OK
                                 # 둘 다 있으면 uri_env(해당 env가 있을 때)가 우선
connect_timeout_secs = 5         # MongoDB 접속/server-selection 타임아웃(기본 5초)
                                 # URI의 serverSelectionTimeoutMS가 우선(다르면 경고)

[profiles.prod.destination]      # 백업 *파일*을 둘 곳 — 저장소이지 MongoDB가 아님
type = "s3"                      # local | s3

[profiles.prod.destination.s3]
endpoint        = "https://s3.example.com"
bucket          = "db-backups"
prefix          = "mongo/prod"
region          = "ap-northeast-2"
credentials_env = "S3_CREDS"     # 값 형식: "ACCESS_KEY:SECRET_KEY"

[profiles.prod.features.compression]
algorithm = "zstd"
level     = 10

[profiles.prod.features.encryption]
enabled        = true
algorithm      = "age"
recipient_file = "/etc/x-backup/age.pub"   # 공개키만 — 개인키는 복구 호스트에 격리
```

모든 값은 `XB_` 접두사 환경변수로 오버라이드된다(`XB_DESTINATION__S3__BUCKET=...`).
우선순위: `CLI > ENV > config.toml > 기본값`.

### 여러 destination

`[[...destinations]]`(배열)로 여러 곳에 동시 백업한다. 백업은 한 번만 돌고,
산출물을 각 destination으로 **바이트 단위 동일하게** 복제하므로 모든 복제본이 같은
체크섬·같은 백업 id를 갖는다 — 어느 복제본에서든 `verify`/`restore`가 동일하게 동작한다.

```toml
[[profiles.prod.destinations]]      # 첫 항목 = primary(필수)
name = "local"
type = "local"
path = "/var/backups/mongo"

[[profiles.prod.destinations]]      # 보조(best-effort)
name = "offsite"
type = "s3"
[profiles.prod.destinations.s3]
endpoint        = "https://s3.example.com"
bucket          = "db-backups"
credentials_env = "S3_CREDS"
```

`destinations`(복수)가 있으면 단일 `destination`보다 우선한다. 정책은 **primary 필수,
나머지는 경고**다: primary가 실패하면 백업 실패, 보조가 실패하면 백업은 성공하되
exit 4(경고)로 실패한 destination을 알린다. 복구는 기본적으로 primary에서 읽고,
`restore --from <name>`으로 특정 복제본을 고른다.

### 백업 엔진

프로파일마다 `mode.engine`으로 MongoDB를 읽고 쓰는 방식을 고른다. 기본값은 `native`로,
외부 바이너리가 필요 없다.

```toml
[profiles.prod.mode]
engine = "native"     # native(기본) | mongodump
```

| 엔진 | 외부 도구 | 아카이브 포맷 | 캡처 대상 | 사용 시점 |
|------|----------|--------------|----------|----------|
| `native`(기본) | 없음 | `xb-native-v1` | 데이터 + 인덱스 + 컬렉션 옵션(capped·validator·collation 등) | 기본 — 의존성 없는 단일 바이너리 |
| `mongodump` | PATH의 `mongodump`/`mongorestore` | mongodump `--archive` | mongodump가 내보내는 것 + 아카이브 내장 `--oplog` 일관 스냅샷 | mongodump 아카이브나 덤프 내장 oplog가 꼭 필요할 때 |

두 엔진 모두 동일한 압축 → 암호화 파이프라인을 통과하고 체이닝용 oplog 타임스탬프를
기록하므로 증분/PITR 동작은 같다. 백업을 만든 엔진은 manifest(`tool_versions.archive_format`)에
기록되고, `restore`가 자동으로 분기한다 — `native` 아카이브는 드라이버로, mongodump
아카이브는 `mongorestore`로 복구한다. 프로파일을 `native`로 바꾼 뒤에도 예전 mongodump
백업을 복구할 수 있다.

### PostgreSQL

프로파일의 `source.uri`를 `postgresql://…`(또는 `postgres://…`)로 두면 PostgreSQL 엔진이
자동 선택된다 — **`pg_dump`/`pg_restore` 불필요**. 드라이버의 COPY 프로토콜(이 도구들이
내부적으로 쓰는 바로 그 경로)로 백업하므로 단일 바이너리로 완결된다.

```toml
[profiles.pg.source]
uri_env = "PG_URI"               # 예: postgresql://user:pass@host:5432/mydb
[profiles.pg.destination]
type = "local"
path = "/var/backups/pg"
```

DB 비의존 명령은 MongoDB와 동일하게 동작한다:

```bash
x-backup backup  --profile pg                    # COPY 기반 풀 백업 → 압축 → 암호화 → 저장
x-backup restore --profile pg --target postgresql://host:5432/restored --force
x-backup status  --profile pg [--all] [--watch]  # 버전·DB 크기·테이블/행 수·마지막 백업; 라이브 Δ
x-backup peek    --profile pg [--ns schema.table]# 데이터 육안 확인: 테이블 행 수 + 최신 행
x-backup migrate --profile pg --target postgresql://host/other --drop --force   # 드라이버 COPY, PG → PG
x-backup list/verify/prune ...                    # manifest 기반(DB 비의존)
```

잡는 것: **테이블 데이터**(text COPY, 값 정확)와 스키마 대부분 — **여러 스키마**·컬럼
(**`GENERATED … AS IDENTITY`**/**`… STORED`** 포함)·**제약**(PK/UNIQUE/FK/CHECK)·**인덱스**·
**시퀀스**(serial+identity, 파라미터+값, 데이터 max로 리셋)·**확장**·**사용자 정의 타입**
(enum/도메인/복합)·**함수/프로시저**·**트리거**·**뷰 + 머티리얼라이즈드뷰**(WITH DATA)·
**선언적 파티셔닝**(부모 `PARTITION BY` + 자식 `PARTITION OF`, 다중 레벨 — 복구 후 새 행도 올바른
파티션으로 라우팅). 복구는 선행 객체(확장·타입·함수)를 의존성 재시도로 깔고, 스키마+테이블 재생성
→ COPY 적재(generated 자동 재계산) → 제약·인덱스 → 시퀀스 리셋 → 뷰/머티뷰/트리거(재시도)를 적용한다.

연결은 rustls TLS + `sslmode` 협상을 쓴다 — 기본 `prefer`는 TLS를 시도하고 미지원 서버엔
평문으로 폴백, `require`/`verify-full`은 TLS를 강제한다. 데이터는 text COPY(pg_dump가 쓰는
이식성 포맷)로 옮기므로 PostgreSQL 메이저 버전이 달라도 복구가 안전하다(메이저 불일치는 경고).

아직 미지원(로드맵): 소유권/권한·코멘트·집계/윈도우 함수·사용자 정의 base/range 타입, 그리고
증분/PITR(`restore --at`은 PG에서 거부 — PITR은 WAL 아카이빙으로 oplog와 다른 메커니즘).
데이터가 있는 DB로 복구할 땐 `--force`(백업에 든 테이블을 drop 후 재생성)를 쓰고, 빈 대상은
플래그가 필요 없다. `migrate`는 PG → PG만(엔진 간 불가).

### 라이브 모니터 (`status --watch`)

`status --watch`는 읽기 전용 점검을 라이브 대시보드로 바꾼다. 주기(기본 1초,
`--interval <초>`)마다 갱신하며 **직전 틱 대비 변화량(Δ)** 을 보여준다(증가=초록, 감소=빨강).
틱 사이에 드라이버 연결을 재사용하고 가벼운 메타데이터(`estimatedDocumentCount`·`dbStats`)만
질의하므로 서버 부하가 작다.

```bash
x-backup status --profile prod --watch                 # 네임스페이스별 문서 수 + Δ, 총 크기 + Δ
x-backup status --all --watch --interval 2             # 프로파일별 한 행, 2초마다 갱신
x-backup status --profile prod --watch --count 5       # 5회 샘플 후 종료(스크립트/CI)
```

`scripts/xb churn`과 함께 쓰면 증분이 실시간으로 쌓이는 걸 볼 수 있다. `Ctrl-C`로 종료.

### Exit codes

| 코드 | 의미 |
|:---:|------|
| 0 | 성공 |
| 1 | 실패(작업 미완료) |
| 2 | 사용법·설정 오류 |
| 3 | 사전 점검 실패(작업 미시작) |
| 4 | 경고 동반 성공(예: gap → 풀 승격) |
| 5 | 잠금 충돌(다른 인스턴스 실행 중) |

cron에서 4를 성공으로 다루려면: `x-backup backup ...; rc=$?; [ $rc -eq 4 ] && rc=0; exit $rc`

## 복구 의미론

- `restore`(--at 없음) = **base 풀백업 스냅샷만** 복원
- `restore --at <시각>` = PITR — base 복원 후 증분 oplog를 해당 시각(이하 최대 ts)까지 재생.
  `verify --chain` 통과가 전제이며, `--only`(선택 복구)와는 병용 불가(mongorestore 제약)
- 복구 검증: `verify --deep`은 개인키 보유 호스트에서만 동작한다(§8.5 키 격리 — 백업
  호스트는 공개키만 가지므로 침해돼도 과거 백업을 복호화할 수 없다)

## Development

```bash
make help               # 전체 타깃
make build              # release 빌드
make build-debug        # 디버그 빌드
make lint               # fmt --check + clippy -D warnings
make test               # 단위 + E2E(exit code) — DB 불필요
make mongodb-up         # 테스트용 replica set 2식(소스:27017 + 타깃:27117)
make test-integration   # Docker replica set 통합 테스트
make test-s3            # MinIO S3 통합 테스트
make scenario           # E2E 시나리오(풀→증분→verify→복구→PITR, 22 assertions)
make postgres-up        # 2차 PostgreSQL 어댑터 대비
```

### 컨테이너에 직접 테스트하기

`scripts/xb`는 `make mongodb-up` 컨테이너에 대고 손으로 테스트하기 위한 래퍼다. 처음
실행하면 `.devenv/`에 `config.toml`과 age 키쌍을 만들고, 필요한 env(`XB_CONFIG`,
`MONGO_URI`, `XB_AGE_IDENTITY_FILE`)·도구 PATH·`--profile`을 자동으로 주입한다 —
설정을 손으로 엮을 필요가 없다.

```bash
make mongodb-up           # 컨테이너 기동
scripts/xb setup          # .devenv 준비 + status 점검
scripts/xb seed           # 결정적 베이스라인(drop 후 삽입)
scripts/xb backup         # 풀 백업(압축+암호화)
scripts/xb list
scripts/xb verify-latest  # 최신 백업 구조+심층 검증
scripts/xb restore-target # 타깃(:27117)으로 복구 후 문서 수 출력
make devenv-down          # 컨테이너 종료 + .devenv 삭제
```

**증분**을 확인하려면 백업 사이에 쓰기가 있어야 한다. `churn`이 소스에 무작위
데이터를 drop 없이 추가해 oplog 변경을 만든다:

```bash
scripts/xb churn 100            # 무작위 문서 100건 추가(oplog 생성)
scripts/xb backup --type incr   # 그 변경분을 증분으로 캡처
scripts/xb list                 # full ← incr 체인

scripts/xb incr-demo            # 위 과정을 한 번에:
                                # seed → full → (churn → incr) ×2 → list
```

실제 x-backup 서브커맨드는 그대로 통과한다(`scripts/xb backup --type incr`,
`scripts/xb status --json`). 환경만 export해서 `x-backup`을 직접 쓰려면
`eval "$(scripts/xb env)"`. 평문 백업은 `XB_NO_ENCRYPT=1 scripts/xb setup`.

`scripts/xb`는 소스가 바뀌면 자동으로 재빌드한다(mtime 비교 — 변경 없으면 빌드 안 함).
그래서 코드를 고친 뒤 `make build`를 따로 안 해도 된다. `XB_BIN=<경로>`로 바이너리를
지정하거나 `XB_NO_BUILD=1`로 자동 빌드를 끌 수 있다.

생성된 config에는 프로파일이 둘 있다 — `demo`(source `:27017`)와 `target`(`:27117`) —
그래서 양쪽 상태를 다 볼 수 있다. `--profile`은 위치에 상관없이 동작한다(래퍼가 올바른
자리에 끼워 준다): `scripts/xb --profile target status` 또는 `scripts/xb status --profile target`.

oplog가 거의 빈 갓 띄운 컨테이너에서는 증분이 스스로 풀 백업으로 승격될 수 있다 —
gap 가드가 동작하는 것이지 오류가 아니다. churn으로 데이터를 먼저 쌓으면 oplog
윈도우가 건강해진다.

## Docs

| 문서 | 내용 |
|------|------|
| [docs/PRD.md](docs/PRD.md) | 제품 요구사항(FR-1~12, 증분 설계, 암호화 설계) |
| [docs/test-scenario.md](docs/test-scenario.md) | E2E 시나리오 정의 |
| [docs/acceptance-report.md](docs/acceptance-report.md) | 수용 기준 10/10 실측 근거 |
| [docs/memory-profile.md](docs/memory-profile.md) | 메모리 상한 실측(상수 RSS 입증) |
| [docs/spike-oplog-archive.md](docs/spike-oplog-archive.md) | archive/oplog 경로 실측 스파이크 |
| [docs/ci.md](docs/ci.md) | CI 구성·운영 노트 |

## Roadmap

PostgreSQL 어댑터(2차) · GFS retention · Prometheus 메트릭 · KMS/HSM 키 연동 ·
라이브 마이그레이션(oplog tailing 무중단 cutover) — [PRD §12](docs/PRD.md)
