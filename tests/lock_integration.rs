//! 통합 테스트 — 동시 실행 잠금(FR-12) E2E.
//!
//! lock 파일 경로를 tempdir로 주입해([`acquire_in`]) 다음을 검증한다:
//! - **동시 획득:** 한쪽이 잡고 있으면 두 번째는 LockConflict(exit 5).
//! - **Drop 해제:** 첫 가드를 떨어뜨리면 재획득이 된다.
//! - **stale 회수(실프로세스):** 짧게 사는 자식 프로세스의 PID로 위조한 lock은,
//!   자식이 살아 있는 동안은 충돌이지만 자식 종료(=PID 부재) 후에는 자동 회수된다.
//!
//! 외부 도구·DB 없이 동작한다(기본 단위 테스트로 실행).

use std::process::Command;

use x_backup::lock::{acquire_in, LockData};

/// 같은 프로파일을 동시에 잡으면 두 번째는 exit 5(LockConflict)다.
#[test]
fn concurrent_acquire_second_is_exit_5() {
    let dir = tempfile::tempdir().unwrap();
    let g1 = acquire_in(dir.path(), "prof").expect("첫 획득 성공");
    let err = acquire_in(dir.path(), "prof").expect_err("두 번째는 충돌해야 함");
    assert_eq!(err.exit_code(), 5, "두 번째 획득은 exit 5: {err}");
    // 가드를 명시적으로 살려 둔다(조기 Drop 방지).
    drop(g1);
}

/// 첫 가드를 Drop한 뒤에는 같은 프로파일을 다시 잡을 수 있다(Drop 해제).
#[test]
fn lock_released_on_drop_allows_reacquire() {
    let dir = tempfile::tempdir().unwrap();
    {
        let _g = acquire_in(dir.path(), "prof").unwrap();
        // 잡은 동안은 충돌.
        assert!(acquire_in(dir.path(), "prof").is_err());
    } // Drop → 해제.
    assert!(
        acquire_in(dir.path(), "prof").is_ok(),
        "Drop 후 재획득 실패"
    );
}

/// 실프로세스 stale 시나리오: 자식 프로세스 PID로 위조한 lock은
/// 자식 생존 중에는 충돌, 자식 종료(PID 부재) 후에는 자동 회수된다.
///
/// 위조 lock의 hostname은 **실제 acquire가 기록한 값**을 그대로 재사용한다 — 내부
/// `hostname()` 규칙과 정확히 일치시켜 "같은 호스트"로 인식되게 한다(자동 회수 전제).
#[test]
fn stale_reclaim_after_holder_process_exits() {
    let dir = tempfile::tempdir().unwrap();

    // 1) 실제 lock을 한 번 잡아 이 호스트의 hostname 값을 그대로 얻는다.
    let host = {
        let g = acquire_in(dir.path(), "prof").unwrap();
        let h = g.data().hostname.clone();
        g.release(); // lock 해제(파일 제거).
        h
    };

    // 2) 잠깐 사는 자식(sleep)을 띄워 그 PID로 lock을 위조한다(같은 호스트).
    let mut child = Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("sleep 자식 spawn 실패");
    let child_pid = child.id();

    let lock_path = dir.path().join("prof.lock");
    let forged = LockData {
        pid: child_pid,
        started_at: chrono::Utc::now().to_rfc3339(),
        profile: "prof".to_string(),
        hostname: host,
    };
    std::fs::write(&lock_path, serde_json::to_vec_pretty(&forged).unwrap()).unwrap();

    // 3) 자식이 살아 있는 동안은 충돌(exit 5)이어야 한다.
    let err = acquire_in(dir.path(), "prof").expect_err("살아있는 PID는 충돌");
    assert_eq!(err.exit_code(), 5);

    // 4) 자식을 종료해 PID를 부재 상태로 만든다.
    child.kill().expect("자식 kill 실패");
    child.wait().expect("자식 wait 실패"); // 좀비 회수 → PID 완전 부재.

    // 5) 이제 PID 부재 → 자동 회수 후 우리가 획득해야 한다.
    let guard = acquire_in(dir.path(), "prof").expect("stale 회수 후 획득 성공");
    assert_eq!(guard.data().pid, std::process::id());
}

/// 다른 프로파일은 같은 디렉터리에서도 서로 막지 않는다.
#[test]
fn different_profiles_independent() {
    let dir = tempfile::tempdir().unwrap();
    let _a = acquire_in(dir.path(), "alpha").unwrap();
    let _b = acquire_in(dir.path(), "beta").unwrap();
}
