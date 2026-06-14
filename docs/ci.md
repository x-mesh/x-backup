# CI 워크플로 (GitHub Actions)

> 대상: `.github/workflows/ci.yml` · 작성: 2026-06-13 · 태스크: CI 자동화
> x-backup의 검증(단위 343 / 통합=replica set / S3=MinIO / PG=docker compose / musl=cross)을 GitHub Actions로 자동화한다.

## 1. 트리거

| 트리거 | 실행 잡 |
|---|---|
| `pull_request` → main | `lint-and-test`, `msrv`, `musl` |
| `push` → main | 전체 (`lint-and-test`, `msrv`, `integration`, `s3`, `postgres`, `musl`) |
| `schedule` (nightly 03:17 UTC) | 전체 |

- `concurrency`: 같은 ref의 진행 중 실행을 자동 취소(`cancel-in-progress: true`).
- `permissions: contents: read` (최소 권한).
- 잡별 `timeout-minutes` 명시(30~40).

무거운 잡(`integration`, `s3`, `postgres`)을 PR에서 빼고 main push + nightly로만 돌리는 이유:
Docker 컨테이너·mongodb-database-tools 다운로드·MinIO/PostgreSQL 기동이 PR 피드백 루프를
느리게 한다. PR은 fmt/clippy/단위/E2E(no-DB)/musl로 빠르게 검증하고, 통합/S3/PG는 머지
후·야간에 보강한다.

## 2. 잡 구성

### `lint-and-test` (모든 PR/push)
DB·Docker 불필요. 로컬과 동일 커맨드:
```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --lib                 # 단위 343
cargo test --test exit_codes_e2e # DB 불필요 E2E(종료 코드, SC6) 7
```
통합/S3 스위트는 `#![cfg(feature = ...)]`로 격리돼 있어 feature 미지정 시 **컴파일 대상에서
제외**된다 — 따라서 `cargo test --lib`/`--test exit_codes_e2e`는 DB가 전혀 필요 없다.

### `integration` (push main + nightly)
single-node replica set + mongodb-database-tools가 필요하다.
```bash
# 1) mongodb-database-tools 100.16.1 — 공식 tarball + sha256 검증(spike §2)
curl -fsSLo tools.tgz \
  https://fastdl.mongodb.org/tools/db/mongodb-database-tools-ubuntu2204-x86_64-100.16.1.tgz
echo "04692b94a63a4f01d6c31b0ddd65dc4620ec69702b9036b5879cb4e6ee2cf31a  tools.tgz" \
  | sha256sum --check --strict
tar xzf tools.tgz   # → .../bin/{mongodump,mongorestore,...}

# 2) replica set 기동(서비스 컨테이너로는 rs.initiate가 안 되므로 fixture를 step에서 실행)
tests/fixtures/replica-set.sh up

# 3) 통합 테스트
XB_TEST_MONGO_URI="mongodb://localhost:27017/?replicaSet=rs0&directConnection=true" \
  cargo test --features integration-tests -- --include-ignored --nocapture

tests/fixtures/replica-set.sh down
```
- **sha256는 실측값**: 위 Linux x86_64 tarball을 실제 내려받아 계산했다(72,189,927 bytes).
  spike §2의 macOS arm64 zip(`5bef906f…`)과는 플랫폼이 다르므로 값이 다르다.
- replica set은 **GitHub 서비스 컨테이너로 띄울 수 없다** — 서비스 컨테이너는 임의 명령
  실행(`rs.initiate()`) 단계가 없어 single-node RS 초기화가 불가하다. 그래서
  `tests/fixtures/replica-set.sh`를 step에서 직접 실행해 PRIMARY까지 폴링 대기한다.
- 통합 테스트는 `#[ignore]`라 `--include-ignored`로 강제 실행한다.

### `s3` (push main + nightly)
```bash
docker info >/dev/null   # Docker 가용성만 확인
cargo test --features s3-integration --test s3_minio -- --nocapture
```
- `s3_minio.rs`는 **MinIO 컨테이너를 스스로 `docker run`으로 기동/정리**한다(테스트 host
  프로세스는 `127.0.0.1:<published-port>`로 접속, `mc` 사이드카는
  `host.docker.internal`(`--add-host …:host-gateway`)로 버킷 생성). 따라서 서비스
  컨테이너를 미리 띄우지 않는다 — Docker만 있으면 된다(ubuntu-latest 러너에 사전 설치).
- docker 미가용 시 테스트가 자동 skip(경고 출력)하도록 작성돼 있다.

### `postgres` (push main + nightly)
PG 증분(logical decoding/pgoutput)은 `wal_level=logical` 서버가 필요하다.
```bash
docker info >/dev/null          # Docker 가용성 확인(compose가 PG를 띄운다)
make scenario-pg                # = build + postgres-up(compose) + scripts/scenario-pg-e2e.sh
make postgres-down              # 정리(if: always())
```
- **서비스 컨테이너 대신 docker compose를 쓰는 이유:** GitHub 서비스 컨테이너는 컨테이너
  `command` 오버라이드를 지원하지 않아 `postgres -c wal_level=logical`로 띄울 수 없다.
  `docker/docker-compose.postgres.yaml`이 `command`로 `wal_level=logical`을 설정하고
  `--wait`로 healthy까지 대기한다.
- `scripts/scenario-pg-e2e.sh`는 **풀 → 증분(pgoutput) ×2 → 빈 슬라이스 → 구조 검증 →
  풀 복구 → PITR(전체/중간) → 시퀀스 재동기화**까지 21개 단언을 수행한다. 시드·검증은
  컨테이너 안 `psql`(`docker exec`)로 하므로 **호스트 psql은 불필요**하다.
- 성공 시 스크립트가 replication slot·테스트 DB를 정리한다(WAL 누수 방지). 실패 시
  산출물·컨테이너를 남기고, `make postgres-down`이 `if: always()`로 컨테이너를 정리한다.

### `musl` (모든 PR/push)
```bash
cargo install cross --version 0.2.5 --locked
cross build --release --target x86_64-unknown-linux-musl
```
- acceptance-report §5 방식: `cross 0.2.5` + `ghcr.io/cross-rs/x86_64-unknown-linux-musl`
  컨테이너가 musl 툴체인을 제공해 zstd C 빌드 + ring 어셈블리를 함께 컴파일한다.
- 산출물(static-pie ELF)을 `actions/upload-artifact@v4`로 업로드한다(14일 보존).

## 3. 캐시 / 토큰체인

- 캐시: `Swatinem/rust-cache@v2`(잡별 `key`로 분리).
- 토큰체인: `dtolnay/rust-toolchain@master`로 **1.93.0** 핀.
  - MSRV는 `Cargo.toml rust-version = "1.88"`(mongodb v3.7 요구).
  - 검증 토큰체인은 **1.93.0**(acceptance-report §1 실측). 정확히 1.88.0으로 돌리면
    clippy `uninlined_format_args` 등 버전별 린트 차이로 거짓 실패가 난다(아래 §4).

## 4. 검증 상태 (실측 / GitHub 첫 실행에서 확인 필요)

YAML은 `python3 -c "import yaml; yaml.safe_load(...)"`로 파싱 검증했고 `actionlint 1.7.12`로
린트 통과(shellcheck 내장 — `run:` 블록 포함). 각 잡 커맨드는 로컬에서 다음과 같이 교차 확인했다.

| 항목 | 로컬 실측(toolchain 1.93.0) | GitHub 첫 실행에서 확인 필요 |
|---|---|---|
| `cargo test --lib` | ✅ 343 passed | — |
| `cargo test --test exit_codes_e2e` | ✅ 7 passed | — |
| `cargo clippy --all-targets --all-features -D warnings` | ✅ 0 경고(1.93.0) | — |
| `cargo fmt --all -- --check` | ❌ **실패**(아래 주의) | 소스 포맷 정리 후 green |
| mongodb-database-tools URL+sha256 | ✅ URL 200·sha256 실측 일치 | tar 추출 경로/PATH 등록 |
| replica set fixture `up`/`down` | (로컬 미기동) | RS 기동·통합 테스트 첫 실행 |
| s3 MinIO 자체 기동 | (로컬 미기동) | `host.docker.internal` 게이트웨이 |
| postgres `make scenario-pg` | ✅ 로컬 21단언 PASS(PG16, slot·DB 정리) | GHA 러너 docker compose v2·python3 |
| `cross build … musl` | (acceptance-report §5에서 별도 실측) | 캐시 없는 첫 빌드 시간/ghcr pull |

### ⚠ 알려진 선결 조건: `cargo fmt --all -- --check` 실패
현재 커밋된 소스(`src/**`, `tests/**`)는 `cargo fmt --check`를 통과하지 **못한다**.
- 1.88.0·1.93.0 **양쪽 rustfmt에서 동일하게 실패**(rustfmt 버전 문제가 아님).
- PRD §12 수용 기준 #1은 `build + clippy`만 요구했고 `fmt --check`는 포함된 적이 없어,
  소스가 한 번도 rustfmt-clean이 아니었다(acceptance-report도 fmt를 돌리지 않음).
- 대표 차이: 테스트의 한 줄 배열(`"x-backup", "backup", …`)을 rustfmt가 멀티라인으로
  펼치려 하고, `use` 순서를 재정렬한다.
- **해결:** 코드 소유 측이 `cargo fmt --all`을 1회 실행해 커밋하면 이 step이 green이 된다.
  CI는 표준 위생 게이트로서 `fmt --check`를 **유지**한다(제거하면 회귀를 숨김).

### MSRV 회귀 감시(적용됨)
`msrv` 잡이 `toolchain: 1.88.0`으로 `cargo check --all-features --all-targets`를 돈다
(clippy/fmt는 1.88에서 거짓 실패하므로 check만). Cargo.toml의 `rust-version = "1.88"`
주장은 로컬 실측으로 검증됨 — 1.88로 컴파일되지 않는 변경이 들어오면 이 잡이 잡는다.
