//! E2E 종료 코드 시나리오 — 실제로 빌드된 `x-backup` 바이너리를 스폰해 PRD §9 종료
//! 코드 규약(SC6)을 검증한다.
//!
//! ## 왜 별도 바이너리 E2E인가
//! 종료 코드 *매핑* 자체는 [`x_backup::XBackupError::exit_code`] 단위 테스트가
//! 단일 진실 공급원으로 검증한다(`src/error.rs`). 이 파일은 그 매핑이 **실제 프로세스
//! 종료 코드까지 끝단(end-to-end)으로 전달되는지**를 확인한다 — `main`의
//! `ExitCode` 변환·clap 파싱·핸들러 라우팅을 통째로 거친다.
//!
//! ## DB·Docker 비의존 원칙
//! 여기 담는 시나리오는 **MongoDB·mongodump·Docker 없이** 결정론적으로 재현 가능한
//! 것만 다룬다:
//! - 설정 오류 → **exit 2**(알 수 없는 프로파일 / 미지원 destination)
//! - PITR(`--at`) + `--only` 병용 → **exit 2**(pitfall 1-4)
//! - 사전 점검 실패(도달 불가 URI) → **exit 3**
//! - 잠금 충돌(동일 프로파일 보유자 생존) → **exit 5**(FR-12)
//!
//! DB가 필요한 경로는 의도적으로 제외하고 기존 통합 테스트가 커버한다(아래 표 참조):
//! - **gap 승격 → exit 4**: `tests/incremental_backup.rs::gap_promotes_to_full_with_exit_4`
//!   (`--features integration-tests`, replica set 필요).
//! - **성공 → exit 0**(backup/restore round-trip): `tests/full_restore.rs`(통합).
//!
//! 잠금 충돌 시나리오는 lock 디렉터리를 `XDG_RUNTIME_DIR`로, 호스트명을 `HOSTNAME`으로
//! 격리해(둘 다 lock 모듈이 참조하는 env) 다른 테스트·실제 사용자 lock과 섞이지 않게 한다.

use std::io::Write;
use std::process::Command as StdCommand;

use assert_cmd::cargo::cargo_bin;
use assert_cmd::Command;

/// 유효한 최소 config.toml을 임시 파일로 만든다. `uri_env`는 호출자가 채운 env 변수명.
/// destination은 local(tempdir)로 둬 config 해석이 destination 단계까지 통과하게 한다.
fn write_config(uri_env: &str, dest_path: &str) -> tempfile::NamedTempFile {
    let mut f = tempfile::Builder::new()
        .prefix("x-backup-e2e-")
        .suffix(".toml")
        .tempfile()
        .unwrap();
    write!(
        f,
        r#"
default_profile = "p"

[profiles.p.mode]
backup_type = "full"
output      = "quiet"
precheck    = true

[profiles.p.source]
uri_env = "{uri_env}"

[profiles.p.destination]
type = "local"
path = "{dest_path}"

[profiles.p.features.compression]
algorithm = "zstd"
level     = 3

[profiles.p.features.encryption]
enabled = false
"#
    )
    .unwrap();
    f.flush().unwrap();
    f
}

/// `x-backup` 바이너리 커맨드. 테스트 격리를 위해 lock·tracing 관련 env를 항상 초기화한다.
fn xbackup() -> Command {
    let mut cmd = Command::cargo_bin("x-backup").expect("x-backup 바이너리 빌드 필요");
    // tracing 노이즈 억제(결과 판정은 exit code로만).
    cmd.env("RUST_LOG", "off");
    cmd
}

/// 설정 오류 → exit 2: 존재하지 않는 프로파일을 지정하면 Config 에러(exit 2)로 끊긴다.
/// (DB 연결 전에 config 해석에서 실패한다 — 무부작용.)
#[test]
fn unknown_profile_is_exit_2() {
    let dir = tempfile::tempdir().unwrap();
    let lock_dir = tempfile::tempdir().unwrap();
    let cfg = write_config("XB_E2E_URI_UNUSED", dir.path().to_str().unwrap());

    xbackup()
        .args(["--config", cfg.path().to_str().unwrap()])
        .args(["backup", "--profile", "does-not-exist"])
        .env("XDG_RUNTIME_DIR", lock_dir.path())
        .assert()
        .code(2);
}

/// 설정 오류 → exit 2: source.uri_env가 가리키는 env가 비어 있으면(해석 불가)
/// Config 에러(exit 2). 백업이 시작되기 전 단계다.
#[test]
fn unresolved_uri_env_is_exit_2() {
    let dir = tempfile::tempdir().unwrap();
    // lock을 테스트별 격리 디렉터리로 둔다 — backup 핸들러는 config 해석 *이전에* lock을
    // 잡으므로, 공유 기본 lock 경로를 쓰면 다른 테스트의 위조 lock과 충돌(exit 5)할 수 있다.
    let lock_dir = tempfile::tempdir().unwrap();
    let cfg = write_config("XB_E2E_MISSING_URI_ENV", dir.path().to_str().unwrap());

    xbackup()
        .args(["--config", cfg.path().to_str().unwrap()])
        .args(["backup", "--profile", "p"])
        .env("XDG_RUNTIME_DIR", lock_dir.path())
        // uri_env가 가리키는 변수를 일부러 제거(미설정).
        .env_remove("XB_E2E_MISSING_URI_ENV")
        .assert()
        .code(2);
}

/// PITR(`--at`) + `--only` 병용 → exit 2: 핸들러가 즉시 거부한다(pitfall 1-4).
/// config·DB 해석 이전에 인자 조합만으로 끊기는 경로다.
#[test]
fn pitr_with_only_is_exit_2() {
    let dir = tempfile::tempdir().unwrap();
    let lock_dir = tempfile::tempdir().unwrap();
    let cfg = write_config("XB_E2E_URI_UNUSED", dir.path().to_str().unwrap());

    xbackup()
        .args(["--config", cfg.path().to_str().unwrap()])
        .args([
            "restore",
            "--profile",
            "p",
            "--at",
            "2026-06-12T13:00:00Z",
            "--only",
            "testdb.items",
        ])
        .env("XDG_RUNTIME_DIR", lock_dir.path())
        .assert()
        .code(2);
}

/// 잘못된 플래그 → exit 2: clap이 사용법 오류로 종료한다(표준 clap 규약).
#[test]
fn invalid_flag_is_exit_2() {
    xbackup()
        .args(["backup", "--profile", "p", "--totally-unknown-flag"])
        .assert()
        .code(2);
}

/// 사전 점검 실패(도달 불가 URI) → exit 3: 유효한 config + 해석된 URI지만 대상이
/// 도달 불가하면 backup 자동 precheck가 PrecheckFailed(exit 3)로 작업을 미시작한다.
///
/// `--skip-precheck`를 주지 않는다(점검 경로 검증). DB가 떠 있지 않은 포트(127.0.0.1:1)로
/// 짧은 serverSelectionTimeout을 줘 빠르게 실패시킨다.
#[test]
fn precheck_unreachable_uri_is_exit_3() {
    let dir = tempfile::tempdir().unwrap();
    let lock_dir = tempfile::tempdir().unwrap();
    let cfg = write_config("XB_E2E_UNREACHABLE_URI", dir.path().to_str().unwrap());

    // 도달 불가 + 빠른 타임아웃. 닫힌 포트(1)로 즉시 연결 거부되게 한다.
    let unreachable = "mongodb://127.0.0.1:1/?serverSelectionTimeoutMS=800&connectTimeoutMS=800";

    xbackup()
        .args(["--config", cfg.path().to_str().unwrap()])
        .args(["backup", "--profile", "p"])
        .env("XB_E2E_UNREACHABLE_URI", unreachable)
        // lock을 테스트 격리 디렉터리로(다른 테스트·실제 lock과 분리).
        .env("XDG_RUNTIME_DIR", lock_dir.path())
        .assert()
        .code(3);
}

/// 잠금 충돌 → exit 5: 동일 프로파일 lock을 **생존 중인 PID**로 위조해 두면, 바이너리는
/// 보유자가 살아 있다고 판단해 LockConflict(exit 5)로 끊긴다(FR-12).
///
/// 위조 lock의 hostname은 바이너리가 보는 값과 같아야 자동 회수가 안 일어난다 — 바이너리에
/// `HOSTNAME` env를 주입해 양쪽을 동일 값으로 고정한다(lock 모듈은 HOSTNAME을 먼저 읽음).
/// PID는 짧게 사는 `sleep` 자식의 것을 쓴다(테스트 종료 시 정리).
#[test]
fn lock_conflict_is_exit_5() {
    let dir = tempfile::tempdir().unwrap();
    let lock_root = tempfile::tempdir().unwrap();
    let cfg = write_config("XB_E2E_URI_UNUSED", dir.path().to_str().unwrap());

    // 고정 hostname(바이너리 lock 모듈과 위조 lock 양쪽에 동일하게 적용).
    let fake_host = "x-backup-e2e-host";

    // 살아 있는 보유자 PID를 만들기 위해 sleep 자식을 띄운다.
    let mut holder = StdCommand::new("sleep")
        .arg("30")
        .spawn()
        .expect("sleep 자식 spawn 실패");
    let holder_pid = holder.id();

    // lock 디렉터리/파일을 바이너리가 보게 될 경로와 동일하게 만든다.
    //   기본 lock_dir = $XDG_RUNTIME_DIR/x-backup, 파일 = <profile>.lock
    let lock_dir = lock_root.path().join("x-backup");
    std::fs::create_dir_all(&lock_dir).unwrap();
    let lock_file = lock_dir.join("p.lock");
    let forged = serde_json::json!({
        "pid": holder_pid,
        "started_at": "2026-06-12T00:00:00Z",
        "profile": "p",
        "hostname": fake_host,
    });
    std::fs::write(&lock_file, serde_json::to_vec_pretty(&forged).unwrap()).unwrap();

    let result = xbackup()
        .args(["--config", cfg.path().to_str().unwrap()])
        .args(["backup", "--profile", "p"])
        .env("XDG_RUNTIME_DIR", lock_root.path())
        .env("HOSTNAME", fake_host)
        .assert();

    // 보유자를 정리(좀비 방지) 후 단정.
    let _ = holder.kill();
    let _ = holder.wait();

    result.code(5);
}

/// 빌드된 바이너리가 실제로 존재하고 실행 가능한지(스모크). `--version`은 항상 0이어야 한다.
#[test]
fn binary_runs_and_reports_version() {
    let bin = cargo_bin("x-backup");
    assert!(
        bin.exists(),
        "x-backup 바이너리가 빌드되지 않음: {}",
        bin.display()
    );
    xbackup().arg("--version").assert().success();
}
