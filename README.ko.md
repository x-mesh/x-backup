<p align="center">
  <a href="README.md">English</a> · <strong>한국어</strong>
</p>

# x-backup

> MongoDB·PostgreSQL·MySQL 백업·복구 CLI — 풀/증분, PITR, 로컬/S3 호환 스토리지, 암호화 중심. Rust 단일 바이너리, **외부 덤프 도구 불필요.**

운영 중인 MongoDB(standalone/replica set)·PostgreSQL·MySQL을 **암호화·검증 가능·복구 가능**한
형태로 백업한다. 기본적으로 Rust 드라이버로 DB와 **직접** 통신하므로
**`mongodump`/`mongorestore`·`pg_dump`/`pg_restore`·`mysqldump`/`mysql`이 필요 없다.** 전 구간
스트리밍(데이터 크기와 무관한 상수 메모리 — [실측 보고서](docs/memory-profile.md): 6 GiB 백업
피크 RSS 54.6 MiB)으로 동작한다. DB 종류는 source URI 스킴(`mongodb://`·`postgresql://`·`mysql://`/`mariadb://`)으로 자동 선택된다.

## Features

- ✅ **풀 백업** — 드라이버 네이티브 스트리밍 아카이브(데이터 + 인덱스 + 컬렉션 옵션), 외부 도구 불필요. `mongodump --archive --oplog`는 opt-in 엔진으로 선택 가능
- ✅ **증분 백업** — oplog 직접 캡처, gap 감지 시 풀 백업 자동 승격(exit 4)
- ✅ **PITR** — `--at <RFC3339>|latest` 시점 복구 (base + replay, 체인 검증 전제). MongoDB(oplog)·PostgreSQL(logical decoding) 모두 지원
- ✅ **스토리지** — 로컬 디스크 / S3 호환(MinIO·R2·OCI), 스트리밍 멀티파트 + abort
- ✅ **암호화 기본** — `age`(X25519, 공개키만 백업 호스트에 배치) / AES-256-GCM 대안, zstd 압축 후 암호화
- ✅ **무결성** — manifest + sha256, `verify`(키 불필요 구조 검증) / `--deep` / `--chain`
- ✅ **운영** — `doctor` config 정적 점검(오프라인·DB 연결 없음), `status` 사전 점검(연결·토폴로지·권한·버전/FCV·시계차·oplog 윈도우·데이터 형상·**마지막 백업 나이**·**destination 쓰기 가능+여유 공간**), `--all` source/target 비교, `--watch` 라이브 모니터, `prune` 체인 안전 삭제(`--keep-last`·config retention), 동시 실행 잠금, exit code 규약 0~5
- ✅ **PostgreSQL** — COPY 프로토콜 기반 드라이버 네이티브 풀 백업/복구(데이터 + 테이블 + 제약 + 인덱스 + 시퀀스), `pg_dump`/`pg_restore` 불필요. 동일 파이프라인(압축→암호화→저장)·동일 `status`/`list`/`verify`/`restore`
- ✅ **MySQL** — `mysql_async` 기반 드라이버 네이티브 풀 백업/복구(데이터 + DDL — 테이블·뷰·트리거·루틴·이벤트), `mysqldump`/`mysql` 불필요. 동일 파이프라인·동일 `status`/`list`/`verify`/`restore`. 증분·PITR은 binlog ROW 스트리밍(opt-in).
- ✅ **headless** — 비-TTY 자동 quiet, `--json`, cron/CI 친화

지원 범위: MongoDB replica set(풀+증분)/standalone(풀만)/샤딩은 감지 시 거부. PostgreSQL은 풀 백업+복구+status에 더해 증분(logical decoding)·PITR(opt-in). [PostgreSQL](#postgresql-1) 참조. MySQL은 동일한 명령 세트(풀/복구/status/peek/migrate)에 더해 증분·PITR(binlog ROW 스트리밍, opt-in — 서버에 `log_bin=ROW` 필요). [MySQL](#mysql-1) 참조.

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
x-backup doctor  --config config.toml           # config 정적 점검(오프라인·전 프로파일·DB 연결 없음)
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
x-backup prune   --profile prod --keep-last 100 --dry-run   # 최신 N벌 보존(또는 --keep-full/--keep-days, config retention)
x-backup migrate --profile prod --target mongodb://newcluster --force # 파일 없이 직접 복사
```

다중 DB 툴이라, 모든 명령은 실행 시 stderr에 활성 프로파일·DB를 한 줄로 보여준다(`▸ 프로파일 prod · DB postgresql`) — 지금 무엇을 건드리는지 항상 보이게(`--json`이면 생략). `--profile`은 `XB_PROFILE` 환경변수로도 줄 수 있다(`--config`/`XB_CONFIG`와 동일).

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

- **source** — 백업 대상 DB(prod). 자격증명이 있으면 `uri_env`(환경변수 *이름*),
  로컬·무자격증명이면 `uri`(직접 값)도 된다.
- **destination** — 백업 *파일*을 둘 곳. 저장소(`local`/`s3`)이며 **DB가 아니다**.
- **복구 대상** — 복구를 부을 DB. config가 아니라 복구 시 `restore --target <uri>`로 지정.

x-backup은 **두 가지 config 형식**을 읽고, 파일이 어느 쪽인지 자동 판별한다:

- **v2 (권장)** — 프로파일마다 flat한 `[profile.<name>]` 테이블 하나. 공통 정책은
  `[defaults]`(전체 프로파일에 적용)·`[base.<name>]` + `extends`(재사용, opt-in)로 한 번만
  정의한다. 흔한 경우는 compact 한 줄(`dest`·`compress`·`encrypt`)로 쓴다.
- **v1 (계속 지원)** — 기존의 깊은 중첩 레이아웃(`[profiles.<name>.features.encryption]` …).
  기존 v1 config는 무변경으로 그대로 로드된다.

**판별:** 단수 `[profile]`/`[defaults]`/`[base]` 테이블이 있으면 v2, 복수 `[profiles]`이면 v1.
**한 파일에 둘을 섞으면 에러**다 — 한 형식만 쓴다.

#### v2 (권장)

위 prod 프로파일 전체를 v2로 — 공통 압축/암호화는 `[defaults]`에 한 번만:

```toml
default_profile = "prod"

[output]
language = "ko"                  # 설명/안내 문구 언어(en | ko). 라벨·기술용어는 항상 영문.

[defaults]                       # 모든 프로파일에 적용(최하위 우선순위)
compress = "zstd:10"             # 알고리즘[:레벨]
encrypt  = "age:/etc/x-backup/age.pub"   # "age:<공개키-경로>" | true | false | "off"

[profile.prod]
uri_env = "MONGO_URI"            # prod: env 참조(config는 유출될 수 있으니 시크릿은 밖에)
# uri = "mongodb://localhost:27017/?replicaSet=rs0"   # 개발·무자격증명: 직접 값도 OK
                                 # 둘 다 있으면 uri_env(해당 env가 있을 때)가 우선
prefer_secondary     = true      # 가능하면 secondary에서 백업
connect_timeout_secs = 5         # 접속/server-selection 타임아웃(기본 5초)
dest      = "s3:db-backups/mongo/prod"   # "local:/path" | "s3:bucket/prefix"
dest_name = "central-s3"
s3_region = "ap-northeast-2"
s3_endpoint = "https://s3.example.com"
s3_creds  = "S3_CREDS"           # "ACCESS_KEY:SECRET_KEY"를 담은 환경변수 *이름*
keep_last = 100                  # retention: 최신 100벌 보존(체인 단위)
keep_days = 30                   # 최근 30일 이내 체인 보존

[profile.pg]
uri_env    = "PG_URI"            # postgresql:// → PostgreSQL 엔진 자동 선택
dest       = "local:/var/backups/pg"
pg_logical = true                # PG 증분/PITR(서버 wal_level=logical 필요)
```

`[profile.pg]`는 `[defaults]`의 `compress`/`encrypt`를 상속한다 — 다시 적지 않는다.

#### v2 — 상속(`[defaults]`·`[base.<name>]`·`extends`)

공통 정책을 한 번만 정의하고 프로파일마다 덮어쓴다. 우선순위(낮음→높음):

```
[defaults]  <  extends 체인(왼→오, 뒤가 우선)  <  프로파일 자신의 키
```

`extends`는 `[base.<name>]`(재사용 전용, 프로파일로 선택 불가) 또는 다른 `[profile.<name>]`를
가리키며, 문자열·배열을 받는다(`extends = ["a", "b"]`이면 `b`가 `a`를 덮음). endpoint 전용
대상 프로파일은 **`dest`를 적지 않고**(복구/이관 대상으로만 남게) 상속된 암호화를
`encrypt = false`로 끈다:

```toml
default_profile = "mongo"

[defaults]
compress = "zstd:6"
encrypt  = "age:/etc/x-backup/age.pub"

[base.s3-central]                # 재사용 destination 정책(프로파일 아님)
dest      = "s3:db-backups"
s3_region = "ap-northeast-2"
s3_creds  = "S3_CREDS"

[profile.mongo]                  # 백업 잡: source + dest + 상속 정책
uri  = "mongodb://localhost:27017/?replicaSet=rs0&directConnection=true"
extends   = "s3-central"
s3_prefix = "mongo"

[profile.mongo-target]           # endpoint 전용: source만, dest 없음, 암호화 off
uri     = "mongodb://localhost:27117/?replicaSet=rs0&directConnection=true"
encrypt = false

[profile.pg]
uri        = "postgres://xbackup:xbackup-dev@localhost:5432/app"
dest       = "local:/var/backups/pg"
pg_logical = true

[profile.pg-target]              # endpoint 전용
uri     = "postgres://xbackup:xbackup-dev@localhost:5433/app_restore"
encrypt = false
```

#### v2 — 전체 키 레퍼런스

아래 키는 모두 `[profile.<name>]`(또는 `[defaults]`/`[base.<name>]`) 안의 **flat** 키다.
정규화기가 이를 v1 중첩 트리로 다시 쓰므로 동작은 동일하다.

| flat 키 | 매핑 | 비고 |
|---------|------|------|
| `uri`, `uri_env` | `source.uri` / `source.uri_env` | `uri_env`는 env 변수 *이름*(시크릿) |
| `prefer_secondary`, `connect_timeout_secs` | `source.*` | |
| `backup_type` | `mode.backup_type` | `full`(기본) \| `incr` |
| `output_mode` | `mode.output` | `progress`(기본) \| `quiet` |
| `precheck` | `mode.precheck` | 기본 `true` |
| `engine` | `mode.engine` | `native`(기본) \| `mongodump` |
| `dest = "local:/path"` \| `"s3:bucket/prefix"` | `destination.{type,path}` 또는 `destination.s3.{bucket,prefix}` | compact 단일 destination |
| `dest_name` | `destination.name` | |
| `s3_bucket`, `s3_prefix`, `s3_region`, `s3_endpoint`, `s3_creds` | `destination.s3.{bucket,prefix,region,endpoint,credentials_env}` | `s3_creds`는 env 변수 *이름* |
| `[[profile.x.dest]]`(테이블 배열) | `destinations[]` | 멀티 destination(아래 참조) |
| `compress = "zstd:6"` | `features.compression.{algorithm,level}` | 또는 `compress_algorithm` + `compress_level` |
| `encrypt = "age:/path"` \| `true` \| `false` \| `"off"` | `features.encryption.{enabled,algorithm,recipient_file}` | 또는 `encrypt_algorithm` + `recipient_file`. `recipient_file`은 공개키 *경로* |
| `incr_interval`, `incr_on_gap`, `pg_logical`, `mysql_binlog` | `features.incremental.{interval,on_gap,pg_logical,mysql_binlog}` | |
| `keep_full`, `keep_days`, `keep_last` | `retention.{keep_full,keep_days,keep_last}` | |
| `extends = "name"` \| `["a","b"]` | (상속) | base/profile 참조, 뒤가 우선 |

생략 가능한 기본값(emitter가 생략 가능): `backup_type=full`, `output_mode=progress`,
`precheck=true`, `engine=native`, 압축 `zstd`/레벨 `10`, 암호화 `enabled=true`+`age`,
증분 `interval=15m`/`on_gap=promote_full`/`pg_logical=false`, `prefer_secondary=false`.

더 풍부한 v2 예시 — S3 + retention + 프로파일별 오버라이드:

```toml
default_profile = "prod"

[defaults]
compress = "zstd:10"
encrypt  = "age:/etc/x-backup/age.pub"

[base.s3-central]
dest      = "s3:db-backups"
s3_region = "ap-northeast-2"
s3_endpoint = "https://s3.ap-northeast-2.amazonaws.com"
s3_creds  = "S3_CREDS"

[profile.prod]
extends     = "s3-central"
uri_env     = "MONGO_PROD_URI"
prefer_secondary = true
s3_prefix   = "mongo/prod"
incr_interval = "15m"
incr_on_gap = "promote_full"
keep_last   = 30
keep_days   = 14

[profile.pg-prod]
extends     = "s3-central"
uri_env     = "PG_PROD_URI"
s3_prefix   = "pg/prod"
pg_logical  = true
keep_last   = 30
keep_days   = 14
```

#### v1 (계속 지원)

기존 중첩 레이아웃은 마이그레이션 없이 그대로 로드된다:

```toml
default_profile = "prod"

[profiles.prod.source]
uri_env = "MONGO_URI"            # prod: env 참조(config는 유출될 수 있으니 시크릿은 밖에)
# uri = "mongodb://localhost:27017/?replicaSet=rs0"   # 개발·무자격증명: 직접 값도 OK
                                 # 둘 다 있으면 uri_env(해당 env가 있을 때)가 우선
connect_timeout_secs = 5         # 접속/server-selection 타임아웃(기본 5초)
                                 # URI의 serverSelectionTimeoutMS가 우선(다르면 경고)

[profiles.prod.destination]      # 백업 *파일*을 둘 곳 — 저장소이지 DB가 아님
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

[profiles.prod.retention]        # prune의 기본값(CLI 플래그가 없을 때, CLI 우선)
keep_last = 100                  # 최신 100벌 보존(체인 단위)
keep_days = 30                   # 최근 30일 이내 체인 보존
```

모든 값은 `XB_` 접두사 환경변수로 오버라이드된다(`XB_DESTINATION__S3__BUCKET=...`).
**ENV 오버라이드는 파일 형식과 무관하게 항상 v1 중첩 dot-path를 쓴다** — v2 flat 키가
아니라 정규화된 트리를 가리킨다. 우선순위: `CLI > ENV > config.toml > 기본값`.

### 여러 destination

destination 테이블 배열로 여러 곳에 동시 백업한다. 백업은 한 번만 돌고,
산출물을 각 destination으로 **바이트 단위 동일하게** 복제하므로 모든 복제본이 같은
체크섬·같은 백업 id를 갖는다 — 어느 복제본에서든 `verify`/`restore`가 동일하게 동작한다.

v2는 `[[profile.<name>.dest]]`를 쓴다(각 항목은 compact `dest = "..."` 또는 명시
`type`/`path`/`name`/`s3_*` 키):

```toml
[profile.prod]
uri_env = "MONGO_URI"

[[profile.prod.dest]]               # 첫 항목 = primary(필수)
name = "local"
dest = "local:/var/backups/mongo"

[[profile.prod.dest]]               # 보조(best-effort)
name = "offsite"
dest = "s3:db-backups"
s3_endpoint = "https://s3.example.com"
s3_creds    = "S3_CREDS"
```

v1은 `[[profiles.<name>.destinations]]`를 쓴다:

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
[profile.prod]
engine = "native"     # native(기본) | mongodump     (v2 flat 키 → mode.engine)

# v1 등가:
# [profiles.prod.mode]
# engine = "native"
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
[profile.pg]                     # v2: flat 테이블 하나
uri_env    = "PG_URI"            # 예: postgresql://user:pass@host:5432/mydb
dest       = "local:/var/backups/pg"
pg_logical = true                # PG 증분/PITR opt-in(서버 wal_level=logical 필요)
keep_last  = 100                 # prune 기본값(CLI 플래그 없을 때, CLI 우선)
keep_days  = 30
```

<details><summary>v1 등가</summary>

```toml
[profiles.pg.source]
uri_env = "PG_URI"
[profiles.pg.destination]
type = "local"
path = "/var/backups/pg"

[profiles.pg.features.incremental]
pg_logical = true

[profiles.pg.retention]
keep_last = 100
keep_days = 30
```

</details>

DB 비의존 명령은 MongoDB와 동일하게 동작한다:

```bash
x-backup backup  --profile pg                    # COPY 기반 풀 백업 → 압축 → 암호화 → 저장
x-backup backup  --profile pg --type incr        # logical decoding 증분(pg_logical=true 전제)
x-backup restore --profile pg --target postgresql://host:5432/restored --force
x-backup restore --profile pg --target postgresql://host/restored --at latest --force  # PITR(latest=전체)
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

증분·PITR: PG PITR은 **logical decoding**(pgoutput)으로 동작한다 — WAL 아카이빙이 아니다.
서버 `wal_level=logical` + 프로파일 `pg_logical = true`(v2: `[profile.<name>]`의 flat 키,
v1: `[profiles.<name>.features.incremental] pg_logical = true`)로 opt-in하면, 풀 백업이 replication slot을 만들어 그 시점부터 WAL을 잡고, `backup --type incr`가
변경을 캡처하며, `restore --at <RFC3339>|latest`가 base 복원 후 증분을 목표 시점까지 재생한다
(`latest`=전체 재생). PITR 정밀도는 변경 단위 commit 타임스탬프로 마이크로초까지 간다.

아직 미지원(로드맵): 소유권/권한·코멘트·집계/윈도우 함수·사용자 정의 base/range 타입.
데이터가 있는 DB로 복구할 땐 `--force`(백업에 든 테이블을 drop 후 재생성)를 쓰고, 빈 대상은
플래그가 필요 없다. `migrate`는 PG → PG만(엔진 간 불가).

### MySQL

프로파일의 `source.uri`를 `mysql://…`(또는 `mariadb://…`)로 두면 MySQL 엔진이 자동 선택된다 —
**`mysqldump`/`mysql` 불필요**. 순수 Rust `mysql_async` 드라이버로 동작하므로 단일 바이너리로
완결된다. URI에 데이터베이스 이름이 반드시 포함돼야 한다(`mysql://user:pass@host:3306/dbname`).

```toml
[profile.mysql]                  # v2: flat 테이블 하나
uri_env      = "MYSQL_URI"       # 예: mysql://root:pass@127.0.0.1:3306/mydb
dest         = "local:/var/backups/mysql"
mysql_binlog = true              # MySQL 증분/PITR opt-in(서버 log_bin=ROW 필요)
keep_last    = 100
keep_days    = 30
```

<details><summary>v1 등가</summary>

```toml
[profiles.mysql.source]
uri_env = "MYSQL_URI"
[profiles.mysql.destination]
type = "local"
path = "/var/backups/mysql"

[profiles.mysql.features.incremental]
mysql_binlog = true

[profiles.mysql.retention]
keep_last = 100
keep_days = 30
```

</details>

DB 비의존 명령은 MongoDB·PostgreSQL과 동일하게 동작한다:

```bash
x-backup backup  --profile mysql                   # SELECT 스트리밍 풀 백업 → 압축 → 암호화 → 저장
x-backup backup  --profile mysql --type incr       # binlog ROW 증분(mysql_binlog=true 전제)
x-backup restore --profile mysql --target mysql://host:3306/restored --force
x-backup restore --profile mysql --target mysql://host/restored --at latest --force  # PITR(latest=전체)
x-backup status  --profile mysql [--all] [--watch] # 버전·DB 크기·테이블/행 수·레플리카 상태; 라이브 Δ
x-backup peek    --profile mysql                   # 데이터 육안 확인: 테이블 행 수 + 최신 행
x-backup migrate --profile mysql --target mysql://host/other --drop --force   # 드라이버 직접, MySQL → MySQL
x-backup list/verify/prune ...                     # manifest 기반(DB 비의존)
```

잡는 것: **테이블 데이터**(SELECT 스트리밍, 값 정확)와 스키마 대부분 — **테이블**(`SHOW CREATE TABLE`)·
**뷰**·**트리거**·**스토어드 프로시저**·**함수**·**이벤트**(`SHOW CREATE …`). 바이트 타입 →
`0x` hex, JSON 컬럼 보존, STORED 생성 컬럼은 INSERT 목록에서 제외(서버 재계산), 인비저블 컬럼
포함. FK 의존 순서로 테이블 덤프(`FOREIGN_KEY_CHECKS=0`). `AUTO_INCREMENT` 값 보존. 뷰·트리거·
루틴·이벤트는 데이터 적재 후 적용하며 `DEFINER` 절은 이식성을 위해 제거한다. 복구 세션 preamble:
`FOREIGN_KEY_CHECKS=0`·`UNIQUE_CHECKS=0`·`SQL_MODE='NO_AUTO_VALUE_ON_ZERO'`·`time_zone='+00:00'`·`NAMES utf8mb4`.

풀 백업은 스냅샷 시점 binlog 좌표(`file:pos` + `gtid_executed`)를 매니페스트에 기록해 증분 체인의 앵커로 쓴다.

증분·PITR: **binlog ROW 스트리밍**으로 동작한다(WAL 아카이빙이나 logical decoding이 아니다).
서버에 `log_bin=ON`·`binlog_format=ROW`·`binlog_row_image=FULL`·`binlog_row_metadata=FULL`
(MySQL 8.0.1+)·`gtid_mode=ON`(권장)을 설정하고, 프로파일에 `mysql_binlog = true`(v2 flat 키,
v1: `[profiles.<name>.features.incremental] mysql_binlog = true`)로 opt-in하면,
풀 백업이 binlog 좌표를 기록하고, `backup --type incr`가 ROW 이벤트를 캡처하며,
`restore --at <RFC3339>|latest`가 base 복원 후 목표 시점까지 재생한다(`latest`=전체 재생).
PITR 타임스탬프 정밀도는 **1초 단위**(binlog 이벤트 헤더 해상도) — 정확한 컷은 파일:포지션 또는
GTID 권장. gap: base binlog 파일이 서버에서 퍼지됐으면 증분은 거부되고 풀 백업으로 자동 승격(exit 4).

계정에는 `REPLICATION SLAVE`·`REPLICATION CLIENT` 권한이 필요하다. 풀 백업·복구·status·peek는
이 요건 없이 MySQL 8.0+·MariaDB 10.x에서 동작한다.

알려진 제한: FLOAT/DOUBLE는 서버 텍스트 표현(`mysqldump` 동등 — 10진 소수 완전 보존 미보장),
일관 스냅샷은 InnoDB에만 유효(병행 DDL 차단 불가), 뷰·트리거별 `sql_mode` 미재현. MySQL 8.4의
`SHOW MASTER STATUS` 이름 변경(`SHOW BINARY LOG STATUS`) — 엔진이 양쪽 모두 처리. `migrate`는
MySQL → MySQL만(엔진 간 불가). 상세: [docs/mysql.md](docs/mysql.md).

### 카탈로그 (`list`)

`list`는 store(destination)의 백업·증분 체인을 한 표로 보여준다. 첫 줄에 **store 위치**를,
백업마다 **DB**(postgresql/mongodb) 칼럼을 함께 출력하며, 기본은 **최신순** 정렬이다.

```bash
x-backup list --profile prod                       # 최신순(기본), store 위치 + DB 칼럼
x-backup list --profile prod --sort size           # 큰 것이 위(--sort created|size, 기본 created)
x-backup list --profile prod --asc                 # 오름차순(기본은 내림차순=최신/큰 것이 위)
x-backup list --profile prod --type incr           # 유형 필터(full|incr|orphan)
x-backup list --profile prod --engine pg --limit 5 # DB 엔진 필터(postgresql|mongodb, pg/mongo 약어) + 상위 N개
x-backup list --profile prod --json                # store 필드 포함 JSON
```

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

### 보존·삭제 (`prune`)

`prune`은 보존 기준에 따라 오래된 백업을 **체인 단위**로 안전 삭제한다. 기준은 세 가지다:

- `--keep-full N` — 최신 풀백업 체인 N개 보존
- `--keep-days D` — 최근 D일 이내 체인 보존
- `--keep-last N` — 최신 N벌 보존(체인 단위 누적이라, 살아있는 증분의 base는 단독으로 삭제되지 않는다)

CLI 플래그가 없으면 config의 retention 기본값(`keep_full`/`keep_days`/`keep_last` — v2 flat
키, v1은 `[profiles.<name>.retention]`)을 쓴다(**CLI 우선**). 기준이 하나도 없으면 아무것도
삭제하지 않는다(안전).

```bash
x-backup prune --profile prod --keep-last 100 --dry-run   # 삭제 대상만 출력(무변경)
x-backup prune --profile prod --keep-full 7 --force       # 최신 7체인만 남기고 삭제
x-backup prune --profile prod --force                     # config retention을 기본값으로
```

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
- `restore --at <시각>` = PITR — base 복원 후 증분 oplog를 해당 시각(이하 최대 ts)까지
  드라이버 `applyOps`로 직접 재생(외부 도구 불필요). `verify --chain` 통과가 전제이며,
  `--only`(선택 복구)와는 병용 불가(oplog 재생은 전체 복구 전제)
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
make postgres-up        # 테스트용 PostgreSQL :5432 기동(PG 엔진 백업/복구/status)
make scenario-pg        # PostgreSQL E2E(풀→증분(pgoutput)→복구→PITR 전체·중간→시퀀스 재동기화)
make xbenv-pg           # PG 격리 테스트 워크스페이스 준비 + activate 안내
make xbenv-mongo        # Mongo 격리 테스트 워크스페이스 준비 + activate 안내
make mysql-up           # 테스트용 MySQL 소스(:3306) + 타깃(:3307) 기동
make test-mysql         # MySQL 엔진 단위 테스트(DB 불필요)
make scenario-mysql     # MySQL E2E(풀→증분→복구→PITR)
make xbenv-mysql        # MySQL 격리 테스트 워크스페이스 준비 + activate 안내
make xbenv-clean        # 격리 워크스페이스 제거
```

### 컨테이너에 직접 테스트하기

`scripts/xb`는 `make mongodb-up` 컨테이너에 대고 손으로 테스트하기 위한 래퍼다. 처음
실행하면 `.devenv/`에 `config.toml`과 age 키쌍을 만들고, 필요한 env(`XB_CONFIG`,
`MONGO_URI`, `XB_AGE_IDENTITY_FILE`)·도구 PATH·`--profile`을 자동으로 주입한다 —
설정을 손으로 엮을 필요가 없다.

격리된 워크스페이스가 필요하면 `scripts/xbenv`(Python venv형 모델)를 쓴다 — 워크스페이스마다
config·키·`XB_PROFILE`을 따로 두고 `source <dir>/activate`로 활성화한다. `make xbenv-pg`/
`xbenv-mongo`/`xbenv-clean`으로 간편하게 준비·정리할 수 있다(상세는 [docs/postgres.md](docs/postgres.md)).

**mongo+pg를 한 환경에서** 다루려면 `make xbenv-both`(또는 `scripts/xbenv new <dir> --engine both`).
한 config에 백업 잡 `mongo`/`pg`와 복구·이관 대상(endpoint 전용) `mongo-target`/`pg-target`
프로파일을 만든다(source URI는 config에 직접 기재 — 로컬 테스트라 어디로 붙는지 한눈에).
PostgreSQL도 MongoDB처럼 소스(:5432)·타깃(:5433) **두 서버**를 띄운다. 기본 프로파일은 `mongo`라
plain `x-backup status`/`backup`은 mongo를 대상으로 동작하고, `--profile pg`로 전환하며,
`x-backup status --all`은 전체를 본다:

```bash
make xbenv-both                  # mongo·pg 컨테이너 기동 + 결합 워크스페이스 생성
source .xbenv-both/activate
x-backup status --all
x-backup backup  --profile mongo &&  x-backup restore --profile mongo --target-profile mongo-target --force
x-backup backup  --profile pg    &&  x-backup restore --profile pg    --target-profile pg-target
x-backup migrate --profile mongo --target-profile mongo-target   # 파일 없이 서버→서버
deactivate
make xbenv-clean                 # 워크스페이스 + 격리 PG DB·slot 정리
```

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
| [docs/mysql.md](docs/mysql.md) | MySQL 엔진 상세(스키마 충실도·binlog 내부 구조·PITR·개발/CI) |
| [docs/control-server.ko.md](docs/control-server.ko.md) | 중앙 control 서버 운영(다중 DB 백업·복구·마이그레이션) — 예시: [examples/control-server.toml](examples/control-server.toml) |
| [docs/PRD.md](docs/PRD.md) | 제품 요구사항(FR-1~12, 증분 설계, 암호화 설계) |
| [docs/test-scenario.md](docs/test-scenario.md) | E2E 시나리오 정의 |
| [docs/acceptance-report.md](docs/acceptance-report.md) | 수용 기준 10/10 실측 근거 |
| [docs/memory-profile.md](docs/memory-profile.md) | 메모리 상한 실측(상수 RSS 입증) |
| [docs/spike-oplog-archive.md](docs/spike-oplog-archive.md) | archive/oplog 경로 실측 스파이크 |
| [docs/ci.md](docs/ci.md) | CI 구성·운영 노트 |

## Roadmap

GFS retention · Prometheus 메트릭 · KMS/HSM 키 연동 ·
라이브 마이그레이션(oplog tailing 무중단 cutover) — [PRD §12](docs/PRD.md)
