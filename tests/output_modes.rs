//! 통합 테스트 — 출력 모드(FR-9/R16): 비-TTY에서 진행 표시가 화면을 오염시키지 않는다.
//!
//! 바이너리를 **파이프로** 실행(stdout/stderr 캡처 = 비-TTY)해, 진행 바(indicatif)의
//! 재그리기 제어 시퀀스가 stderr에 나오지 않는지 검사한다(pitfall 9-1: progress=stderr,
//! 비-TTY 자동 quiet). DB·mongodump 없이 동작하도록 `serverSelectionTimeoutMS`를 짧게
//! 줘 메타 질의가 빠르게 실패하게 만든다 — 진행 표시기는 시작되지만(비-TTY→disabled)
//! 아무것도 그리지 않아야 한다.
//!
//! 기본 CI 단위 테스트에 포함된다(feature 게이트 없음) — 외부 의존성이 없다.

use std::io::Write;
use std::process::Command;

/// 임시 config.toml(local destination + 짧은 타임아웃 URI를 가리키는 env)을 만든다.
fn write_temp_config(dir: &std::path::Path) -> std::path::PathBuf {
    let dest = dir.join("backups");
    std::fs::create_dir_all(&dest).unwrap();
    let cfg = format!(
        r#"
default_profile = "t"

[profiles.t.mode]
output = "progress"

[profiles.t.source]
uri_env = "XB_TEST_FAST_URI"

[profiles.t.destination]
type = "local"
path = "{}"

[profiles.t.features.encryption]
enabled = false
"#,
        dest.display()
    );
    let path = dir.join("config.toml");
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(cfg.as_bytes()).unwrap();
    path
}

/// 비-TTY(파이프) 백업 실행 시 stderr에 진행 바 재그리기 시퀀스가 없어야 한다.
///
/// config는 output=progress지만, 파이프 실행(비-TTY)이므로 OutputMode는 자동 quiet로
/// 낮아져 진행 바가 그려지지 않는다. 연결은 짧은 타임아웃으로 빠르게 실패한다(작업 미완료).
#[test]
fn non_tty_backup_emits_no_progress_bar_on_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_temp_config(dir.path());

    // 라우팅 불가 주소 + 짧은 server selection 타임아웃 → 메타 질의가 빠르게 실패.
    let fast_uri = "mongodb://127.0.0.1:1/?serverSelectionTimeoutMS=200&connectTimeoutMS=200";

    let output = Command::new(env!("CARGO_BIN_EXE_x-backup"))
        .args([
            "backup",
            "--profile",
            "t",
            "--skip-precheck", // precheck를 건너뛰고 곧장 백업 경로로(진행 표시기 시작 지점).
        ])
        .env("XB_CONFIG", &config)
        .env("XB_TEST_FAST_URI", fast_uri)
        // RUST_LOG를 끄면 tracing 로그도 최소화되나, 진행 바 검사에는 영향 없다.
        .env("RUST_LOG", "off")
        .output()
        .expect("바이너리 실행 실패");

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);

    // 1) 진행 바(indicatif) 재그리기 시퀀스가 없어야 한다.
    //    - 캐리지 리턴(\r)으로 같은 줄을 덮어쓰는 바, 또는 ANSI 커서 이동 시퀀스.
    assert!(
        !stderr.contains('\r'),
        "비-TTY stderr에 진행 바 재그리기(\\r)가 있으면 안 됨:\n{stderr:?}"
    );
    assert!(
        !stderr.contains("\x1b["),
        "비-TTY stderr에 ANSI 커서 제어 시퀀스가 있으면 안 됨:\n{stderr:?}"
    );
    // 2) 진행 바 템플릿 토막(예: '[===' / 'bytes/sec' 표기)이 없어야 한다.
    assert!(
        !stderr.contains("[==") && !stderr.contains("[> "),
        "비-TTY stderr에 진행 바 모양이 있으면 안 됨:\n{stderr:?}"
    );
    // 3) 연결 실패로 작업은 미완료여야 한다(성공 종료 0이 아님).
    assert!(
        !output.status.success(),
        "DB 없이 백업이 성공할 수 없음(상태: {:?})\nstdout: {stdout}\nstderr: {stderr}",
        output.status.code()
    );
    // 4) stdout에는 진행 이벤트가 섞이지 않는다(결과 전용 채널 — 여기선 실패라 비어 있음).
    assert!(
        !stdout.contains("progress") && !stdout.contains('\r'),
        "stdout(결과 채널)에 진행 출력이 섞이면 안 됨:\n{stdout:?}"
    );
}

/// 비-TTY에서 `init` 마법사는 대화형 전용 가드로 즉시 거부된다(EOF/파이프 안전).
#[test]
fn non_tty_init_is_rejected() {
    let output = Command::new(env!("CARGO_BIN_EXE_x-backup"))
        .args(["init"])
        // stdin을 파이프(빈 입력)로 — 비-TTY.
        .stdin(std::process::Stdio::piped())
        .output()
        .expect("바이너리 실행 실패");

    // 대화형 전용 가드 → Usage(exit 2).
    assert_eq!(
        output.status.code(),
        Some(2),
        "비-TTY init은 exit 2여야 함\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
