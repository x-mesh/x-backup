# x-backup

> MongoDB 백업·복구 CLI — 풀/증분(oplog), PITR, 로컬/S3 호환 스토리지, 암호화 중심. Rust 단일 바이너리.

운영 중인 MongoDB(standalone/replica set)를 **암호화·검증 가능·복구 가능**한 형태로 백업한다.
`mongodump`/`mongorestore`를 오케스트레이션하며, 전 구간 스트리밍(데이터 크기와 무관한
상수 메모리 — [실측 보고서](docs/memory-profile.md): 6 GiB 백업 피크 RSS 54.6 MiB)으로 동작한다.

## Features

- ✅ **풀 백업** — `mongodump --archive --oplog` 스트리밍, 시점 일관성
- ✅ **증분 백업** — oplog 직접 캡처, gap 감지 시 풀 백업 자동 승격(exit 4)
- ✅ **PITR** — `--at <RFC3339>` 시점 복구 (base + oplog replay, 체인 검증 전제)
- ✅ **스토리지** — 로컬 디스크 / S3 호환(MinIO·R2·OCI), 스트리밍 멀티파트 + abort
- ✅ **암호화 기본** — `age`(X25519, 공개키만 백업 호스트에 배치) / AES-256-GCM 대안, zstd 압축 후 암호화
- ✅ **무결성** — manifest + sha256, `verify`(키 불필요 구조 검증) / `--deep` / `--chain`
- ✅ **운영** — `status` 사전 점검(신호등), `prune` 체인 안전 삭제, 동시 실행 잠금, exit code 규약 0~5
- ✅ **headless** — 비-TTY 자동 quiet, `--json`, cron/CI 친화

지원 범위: replica set(풀+증분) / standalone(풀만) / 샤딩 클러스터는 감지 시 거부(스코프 외).

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

전제: `mongodump`/`mongorestore`(MongoDB Database Tools 100.x)가 PATH에 필요하다 —
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
x-backup backup  --profile prod                 # 풀 백업 → 압축 → 암호화 → 저장
x-backup backup  --profile prod --type incr     # oplog 증분
x-backup list    --profile prod                 # 카탈로그(체인 상태 포함)
x-backup verify  --id <backup-id>               # 키 없는 구조 검증
x-backup restore --profile prod --target mongodb://staging --dry-run
x-backup restore --profile prod --target mongodb://staging --force
x-backup restore --profile prod --at 2026-06-01T00:00:00Z --force   # PITR
x-backup prune   --profile prod --keep-full 7 --dry-run
```

### config.toml

```toml
default_profile = "prod"

[profiles.prod.source]
uri_env = "MONGO_URI"            # 시크릿은 env 참조만(평문 금지)

[profiles.prod.destination]
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

PostgreSQL 어댑터(2차) · GFS retention · Prometheus 메트릭 · KMS/HSM 키 연동 — [PRD §12](docs/PRD.md)
