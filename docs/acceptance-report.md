# 수용 기준 최종 검증 보고서 (Acceptance Report)

> 대상: x-build PRD `§12 Acceptance Criteria` 10개 항목
> 작성: 2026-06-13 · 태스크: t14(통합 테스트 스위트 완성 + musl 빌드) · 단계 7/7
> 기준 커밋: t13(`fa7817a`) 위에서 t14 변경 적용

---

## 1. 실측 환경

| 구성 | 버전/값 |
|---|---|
| rustc / cargo | **1.93.0** (2026-01-19) |
| 호스트 | macOS (darwin/arm64) |
| mongod (fixture) | **7.0.35** (Docker `mongo:7`, single-node replica set) |
| mongodump / mongorestore | **100.16.1** (공식 tarball, spike §2 경로) |
| MinIO | `minio/minio:latest` (S3 통합 테스트, docker) |
| musl 빌드 | `cross 0.2.5` + `ghcr.io/cross-rs/x86_64-unknown-linux-musl` |

도구 설치: mongodump/mongorestore는 macOS에 패키지로 설치돼 있지 않아, spike 보고서(`docs/spike-oplog-archive.md` §2)의 공식 tarball을
내려받아 사용했다. CI에서는 패키지 매니저 또는 동일 tarball pin으로 설치한다(spike §7).

---

## 2. 테스트 스위트 실행 결과 (수치)

모든 스위트를 실제 실행했다. 통합/S3는 Docker(replica set ×2, MinIO)와 mongo 도구를 실제로 띄워 검증했다.

| 스위트 | 명령 | 결과 |
|---|---|---|
| 단위(lib) | `cargo test --lib` | **282 passed / 0 failed** |
| E2E 종료 코드 | `cargo test --test exit_codes_e2e` | **7 passed / 0 failed** |
| 통합(replica set) | `cargo test --features integration-tests -- --include-ignored` | **전 스위트 통과**(아래 분해) |
| S3(MinIO) | `cargo test --features s3-integration --test s3_minio` | **3 passed / 0 failed** |
| clippy | `cargo clippy --all-targets --all-features -- -D warnings` | **0 경고** |
| release 빌드 | `cargo build --release` | **성공** |
| musl 빌드 | `cross build --release --target x86_64-unknown-linux-musl` | **성공**(static-pie ELF, alpine 실행 확인) |

### 통합 스위트 분해(`--features integration-tests`, replica set 기동 후)

| 테스트 파일 | passed | 핵심 검증 |
|---|---|---|
| `full_backup.rs` | 1 | 산출물 3종 존재·체크섬·oplog_range(SC1 일부) |
| `full_restore.rs` | 3 | backup→restore round-trip 문서 수·해시 일치(SC1) |
| `incremental_backup.rs` | 3 | 증분 체인·applyOps 캡처·**gap→exit 4 승격(SC2)** |
| `pitr_integration.rs` | 2 | **PITR 후 목표 ts 이후 문서 부재(SC4)** + dry-run 무부작용 |
| `status_integration.rs` | 3 | status 전 점검 항목 + precheck 서브셋 |
| `lock_integration.rs` | 4 | 동시 잠금 exit 5·Drop 해제·stale 회수 |
| `verify_integration.rs` | 6 | 변조 탐지·암호문 산출물·키 없는 deep 거부(SC3) |
| `prune_integration.rs` | 6 | 체인 안전 prune |
| `output_modes.rs` | 2 | 비-TTY quiet·JSON |
| `status_decisions.rs` | 9 | 신호등 판정 |

### 실행 중 발견·수정한 결함 1건

- **증상:** `status_integration`의 `full_status_emits_all_check_items`·`precheck_subset_passes_on_healthy_replica_set`가
  `report.overall == Fail`로 실패. 원인은 권한 점검(`check_privileges`).
- **근본 원인:** fixture replica set은 **인증 비활성**(no-auth)이라 `connectionStatus`의
  `authenticatedUserPrivileges`가 비어 있다. 기존 코드는 이를 "필요 권한 누락"으로 보고 `Fail` 처리 →
  인증을 끈 서버에서는 접속 주체가 사실상 전권인데도 거짓 음성(false negative).
- **수정:** `auth_is_disabled()`(인증된 사용자 없음 판정)을 추가하고, 인증 비활성이면 권한을 충분으로 보되
  운영 권고를 담은 **Warn**으로 처리(백업을 막지 않음). 단위 테스트 2개 추가
  (`auth_disabled_when_no_authenticated_users`, `auth_enabled_when_user_present`).
- **재실행:** 수정 후 status_integration **3/3 통과**, lib 단위 280→282.

---

## 3. 수용 기준 체크리스트 (PRD §12, 10항목)

| # | 기준 | 판정 | 근거(테스트명·실행 결과) |
|---|---|:---:|---|
| 1 | `cargo build --release` + `clippy --all-targets -- -D warnings` 0 경고 | **충족** | release 빌드 성공(36s) · clippy `--all-targets --all-features` 0 경고 |
| 2 | `cargo test`(단위) + `cargo test --features integration-tests`(replica set) 통과 | **충족** | 단위 282 passed · 통합 전 스위트 통과(§2 분해표) |
| 3 | 통합: backup→restore 후 문서 수·해시 일치(SC1) | **충족** | `full_restore.rs::backup_then_restore_matches_counts_and_hash`(+`restore_half_matches_with_real_mongorestore`) 통과 |
| 4 | 통합: oplog 롤오버 유도 → exit 4 + 풀 승격(SC2) | **충족** | `incremental_backup.rs::gap_promotes_to_full_with_exit_4` 통과. 롤오버는 base 기준점 ts를 oplog 최소보다 과거로 조작해 동일 gap 코드 경로를 강제(방식은 테스트 doc에 명시) |
| 5 | 단위/통합: 산출물 헥사 검사로 암호문 확인 + 개인키 없는 verify --deep 거부(SC3) | **충족** | 암호문: `crypto/age.rs::age_round_trip`(평문≠암호문 + `age-encryption.org` 매직 검사), `verify_integration.rs::encrypted_backup_structural_ok_deep_needs_key`(실 압축→암호 산출물 평문 불일치). 키 없는 deep 거부: 동 테스트가 `exit 2` + 키 격리 안내 검증 |
| 6 | 통합: PITR 후 목표 ts 이후 문서 부재(SC4) | **충족** | `pitr_integration.rs::pitr_recovers_to_point_between_writes`(쓰기A 존재·쓰기B 부재·내림 매핑 검증) 통과 |
| 7 | `cargo build --target x86_64-unknown-linux-musl` 성공(SC5) | **충족** | `cross build --release --target x86_64-unknown-linux-musl` 성공(zstd C 빌드 + ring/rustls 포함). 산출물 = **static-pie x86_64 ELF**, alpine:3(glibc 없음)에서 `--version` 정상 실행 확인 |
| 8 | exit code 시나리오: 점검 실패=3 / gap=4 / 잠금=5 / 설정 오류=2(SC6) | **충족** | **실 바이너리 E2E**(`exit_codes_e2e.rs`): 설정 오류=2(`unknown_profile`, `unresolved_uri_env`, `pitr_with_only`, `invalid_flag`), 점검 실패=3(`precheck_unreachable_uri`), 잠금 충돌=5(`lock_conflict`). gap=4는 DB 필요 경로라 통합 `gap_promotes_to_full_with_exit_4`가 커버(중복 작성 회피). 매핑 SoT는 `error.rs` 단위 테스트 |
| 9 | `ps` 검사: 자식 프로세스 argv에 URI/시크릿 부재(C3) | **충족** | mongodump: `engine/mongo/dump.rs::spawned_child_argv_has_no_uri_only_config_path`(실 spawn argv 파일 기록 검사) + `argv_never_contains_uri`. mongorestore: `engine/mongo/restore.rs::argv_includes_flags_and_never_uri`. 양쪽 모두 URI는 0600 `--config` 파일로만 전달 확인 |
| 10 | Slice 0 스파이크 보고서가 docs/에 기록되고 아키텍처 분기 반영(A4) | **충족** | `docs/spike-oplog-archive.md`(실측 기반, archive 모드 채택 결정 §4) — t1에서 작성, 아키텍처(archive 스트리밍) 채택됨 |

**요약: 10/10 충족.**

---

## 4. exit code E2E 매핑 (SC6 상세)

`exit_codes_e2e.rs`는 빌드된 바이너리를 실제 스폰해(`assert_cmd`) `main`의 `ExitCode` 변환까지 끝단으로 검증한다.
DB·Docker 없이 결정론적으로 재현 가능한 시나리오만 담았다.

| 종료 코드 | 시나리오 | 테스트 |
|:---:|---|---|
| 2 | 알 수 없는 프로파일 | `unknown_profile_is_exit_2` |
| 2 | uri_env 미해석(env 부재) | `unresolved_uri_env_is_exit_2` |
| 2 | PITR(`--at`) + `--only` 병용 | `pitr_with_only_is_exit_2` |
| 2 | 잘못된 플래그(clap) | `invalid_flag_is_exit_2` |
| 3 | 도달 불가 URI 사전 점검 실패 | `precheck_unreachable_uri_is_exit_3` |
| 5 | 동일 프로파일 잠금 충돌(생존 PID 위조) | `lock_conflict_is_exit_5` |
| 4 | (DB 경로) gap 승격 | `incremental_backup.rs::gap_promotes_to_full_with_exit_4` |
| 0 | (DB 경로) backup/restore 성공 | `full_restore.rs` round-trip |

잠금 충돌 E2E는 lock 디렉터리를 `XDG_RUNTIME_DIR`, 호스트명을 `HOSTNAME` env로 격리해(둘 다 lock 모듈이 참조)
다른 테스트·실제 사용자 lock과 섞이지 않게 한다. 생존 PID는 짧게 사는 `sleep` 자식의 것을 위조한다.

---

## 5. musl 빌드 검증 (SC5)

```bash
rustup target add x86_64-unknown-linux-musl
cross build --release --target x86_64-unknown-linux-musl   # 2m 04s, 성공

file target/x86_64-unknown-linux-musl/release/x-backup
# → ELF 64-bit LSB pie executable, x86-64, static-pie linked

docker run --rm --platform linux/amd64 \
  -v "$PWD/target/x86_64-unknown-linux-musl/release/x-backup":/x-backup:ro \
  alpine:3 /x-backup --version
# → x-backup 0.1.0   (glibc 없는 alpine에서 정적 실행 확인)
```

- **macOS 호스트 직접 링크 대신 `cross`(Docker) 사용:** macOS에서 musl 타깃을 직접 링크하려면 musl-gcc/cross
  C 툴체인이 필요하다. `cross`가 컨테이너 안에서 musl 툴체인을 제공해 **zstd C 빌드**와 **ring 어셈블리**를 함께
  컴파일한다(reqwest→rustls→ring, age, object_store 전 의존 트리 포함).
- **CI 검증 방법:** Linux 러너에서는 `apt install musl-tools` 후
  `cargo build --release --target x86_64-unknown-linux-musl` 직접 빌드가 가능하다. 컨테이너 빌드를 선호하면
  본 보고서와 동일하게 `cross`를 쓴다. 산출물은 정적 바이너리라 distroless/alpine 이미지에 그대로 담을 수 있다.

---

## 6. PRD 정합 보정 (FR-9)

t13 결정 반영: 복구 progress는 압축 산출물의 복호화·압축해제 후 입력량을 사전에 알 수 없어
**부정형(spinner)** 으로 구현됐다. PRD `FR-9`의 "복구는 ... 정확한 퍼센트 진행 가능" 문장을 실제 구현
(압축=부정형, 비압축=근사 가능)에 맞게 보정했다(`docs/PRD.md` FR-9). 코드 doc(`src/cli/progress.rs` 모듈
주석)의 동일 문구도 함께 정정했다.
