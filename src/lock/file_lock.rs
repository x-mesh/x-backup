//! 로컬 lock 파일 — 동시 실행 직렬화(PRD §FR-12, 리서치 architecture §6).
//!
//! 동일 프로파일(동일 destination)에 대한 `backup`/`restore`/`prune` 동시 실행을
//! 막는다. 풀 백업 장기화 중 증분 cron이 중복 기동해 증분 체인 정합성이 깨지는
//! 시나리오가 대표적이다(FR-12).
//!
//! ## 설계 (리서치 §6)
//! - **경로:** `$XDG_RUNTIME_DIR/x-backup/<profile>.lock`, 없으면 `/tmp/x-backup`로
//!   폴백한다. lock은 *프로파일 단위*다(destination이 아니라 프로파일명으로 키잉 —
//!   같은 프로파일의 두 인스턴스만 직렬화하면 충분하고, 운영자가 프로파일명만으로
//!   stale lock을 식별·정리할 수 있다).
//! - **원자 생성:** `O_CREAT | O_EXCL`로 lock 파일을 만든다. 이미 있으면 충돌 후보다.
//! - **내용:** [`LockData`] JSON(pid, started_at, profile, hostname). 충돌 시 누가
//!   잡고 있는지(pid·시작 시각)를 운영자에게 보여주기 위함이다.
//! - **해제:** [`LockGuard`]가 Drop될 때 best-effort로 파일을 삭제한다(RAII).
//!
//! ## stale lock 정책 (pitfall 9-5 — 보수적)
//! 비정상 종료로 남은 lock을 어떻게 다룰지는 안전과 편의의 트레이드오프다. 본
//! 구현은 다음 보수적 정책을 택한다(주석으로 근거 명시):
//!
//! 1. **PID 부재(가장 확실):** lock의 pid가 더 이상 살아있지 않으면(`kill(pid, 0)`이
//!    `ESRCH`) 죽은 프로세스의 잔재가 확실하므로 **자동 회수**한다 — 경고 로그 후
//!    lock을 제거하고 1회 재시도한다. 단, **hostname이 다르면** 다른 호스트의
//!    프로세스일 수 있어(이 호스트에 그 pid가 우연히 없을 뿐) 자동 회수하지 않는다.
//! 2. **PID 생존:** 정상적으로 다른 인스턴스가 실행 중이다 → [`LockConflict`](exit 5).
//!    pid·시작 시각을 안내한다.
//! 3. **연령 초과(24h) + PID 생존:** 비정상적으로 오래 잡고 있으나 프로세스는 살아
//!    있다 → **자동 해제하지 않는다**. 24h 초과라도 살아있는 프로세스를 강제로
//!    밀어내면 그 프로세스의 작업과 충돌해 체인을 깰 수 있다. 충돌로 처리하되
//!    "오래된 lock으로 의심됨 — 수동 확인/해제 방법"을 안내한다(보수적).
//!
//! 즉 *자동 회수는 PID 부재가 확실할 때만* 허용하고, 그 외에는 안내 후 거부한다.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Result, XBackupError};

/// 살아있는 PID라도 이 시간을 초과해 잡고 있으면 "오래된 lock"으로 의심한다.
///
/// 단, **자동 해제하지 않는다**(살아있는 프로세스를 강제로 밀어내면 위험 — 위 정책 3).
/// 충돌로 처리하되 안내 메시지에 이 사실을 덧붙인다.
const STALE_AGE_SECS: i64 = 24 * 60 * 60;

/// lock 파일에 기록하는 보유자 메타데이터.
///
/// 시크릿(URI·키 등)은 절대 담지 않는다 — 충돌 진단에 필요한 식별 정보만 담는다.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockData {
    /// lock을 잡은 프로세스 ID.
    pub pid: u32,
    /// lock 획득 시각(UTC RFC3339). 연령 판정과 안내 메시지에 쓴다.
    pub started_at: String,
    /// 프로파일 이름(lock 키 — 디버깅·교차 확인용).
    pub profile: String,
    /// 호스트명(다른 호스트 lock의 자동 회수를 막는 보수 장치).
    pub hostname: String,
}

impl LockData {
    /// 현재 프로세스 기준으로 lock 메타를 만든다.
    fn for_current(profile: &str) -> Self {
        Self {
            pid: std::process::id(),
            started_at: chrono::Utc::now().to_rfc3339(),
            profile: profile.to_string(),
            hostname: hostname(),
        }
    }
}

/// 잡고 있는 lock — Drop 시 파일을 제거한다(RAII).
///
/// 핸들러는 작업 전체 동안 이 가드를 변수로 살려 두어야 한다. 함수가 반환하거나
/// `?`로 조기 종료(또는 패닉)하면 가드가 Drop되며 lock이 해제된다.
#[derive(Debug)]
#[must_use = "LockGuard를 즉시 버리면 lock이 곧바로 해제됩니다 — 작업 동안 변수로 유지하세요"]
pub struct LockGuard {
    /// lock 파일 경로(Drop 시 삭제 대상).
    path: PathBuf,
    /// 이 가드가 기록한 메타(검증·테스트용).
    data: LockData,
    /// Drop에서 삭제를 시도할지 여부(해제 후 비활성화).
    armed: bool,
}

impl LockGuard {
    /// 이 가드가 보유한 lock의 메타데이터.
    pub fn data(&self) -> &LockData {
        &self.data
    }

    /// lock 파일 경로(테스트·진단용).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 명시적으로 lock을 해제한다(Drop을 기다리지 않음). 멱등.
    pub fn release(mut self) {
        self.unlock();
    }

    fn unlock(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;
        if let Err(e) = std::fs::remove_file(&self.path) {
            // 이미 사라졌으면(다른 경로에서 정리) 정상. 그 외엔 경고만 남긴다 —
            // lock 해제 실패가 작업 결과를 뒤집을 만큼 치명적이지는 않다.
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %self.path.display(), "lock 파일 제거 실패: {e}");
            }
        }
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        self.unlock();
    }
}

/// 프로파일 단위 lock을 획득한다(기본 lock 디렉터리 사용).
///
/// 충돌 시 [`XBackupError::LockConflict`](exit 5)를 반환한다. stale lock은 위 정책에
/// 따라 처리한다(PID 부재면 자동 회수·1회 재시도).
pub fn acquire(profile: &str) -> Result<LockGuard> {
    let dir = lock_dir();
    acquire_in(&dir, profile)
}

/// 지정한 디렉터리 아래에서 프로파일 lock을 획득한다(테스트에서 경로 주입).
///
/// 동시성·stale 분기의 단위 테스트가 임시 디렉터리를 주입할 수 있게 분리했다.
pub fn acquire_in(dir: &Path, profile: &str) -> Result<LockGuard> {
    let path = lock_path(dir, profile);

    // lock 디렉터리를 준비한다(없으면 생성).
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            XBackupError::Failure(crate::tr!(
                "failed to prepare the lock directory ({}): {e}",
                "lock 디렉터리 준비 실패({}): {e}",
                parent.display()
            ))
        })?;
    }

    // 1차 시도 → 충돌이면 stale 판정 → (회수 가능 시) 1회 재시도.
    match try_create(&path, profile)? {
        Some(guard) => Ok(guard),
        None => {
            // 이미 lock이 존재한다 — 보유자를 읽어 stale 여부를 판정한다.
            let holder = read_holder(&path);
            match classify(&holder) {
                StaleDecision::Reclaim(reason) => {
                    tracing::warn!(
                        path = %path.display(),
                        "{reason} — stale lock으로 판단해 자동 회수합니다"
                    );
                    // 죽은 보유자의 lock을 제거하고 1회만 재시도한다.
                    let _ = std::fs::remove_file(&path);
                    match try_create(&path, profile)? {
                        Some(guard) => Ok(guard),
                        None => {
                            // 재시도 사이 다른 프로세스가 끼어들었다 → 정상 충돌.
                            Err(conflict_error(&path, &read_holder(&path), None))
                        }
                    }
                }
                StaleDecision::Conflict(extra) => Err(conflict_error(&path, &holder, extra)),
            }
        }
    }
}

/// `O_CREAT | O_EXCL`로 lock 파일을 원자 생성하고 메타를 기록한다.
///
/// - 생성 성공: lock 획득 → [`LockGuard`].
/// - 이미 존재(`AlreadyExists`): `Ok(None)`(상위에서 stale 판정).
/// - 그 외 I/O 오류: [`XBackupError`].
fn try_create(path: &Path, profile: &str) -> Result<Option<LockGuard>> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true).mode(0o600); // create_new = O_CREAT|O_EXCL.
    match opts.open(path) {
        Ok(mut file) => {
            let data = LockData::for_current(profile);
            let bytes = serde_json::to_vec_pretty(&data).map_err(|e| {
                XBackupError::Failure(crate::tr!(
                    "failed to serialize lock metadata: {e}",
                    "lock 메타 직렬화 실패: {e}"
                ))
            })?;
            // 기록에 실패하면 막 만든 파일을 정리하고 에러를 올린다(부분 lock 방지).
            if let Err(e) = file.write_all(&bytes).and_then(|_| file.flush()) {
                let _ = std::fs::remove_file(path);
                return Err(XBackupError::Failure(crate::tr!(
                    "failed to write the lock metadata ({}): {e}",
                    "lock 메타 기록 실패({}): {e}",
                    path.display()
                )));
            }
            Ok(Some(LockGuard {
                path: path.to_path_buf(),
                data,
                armed: true,
            }))
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
        Err(e) => Err(XBackupError::Failure(crate::tr!(
            "failed to create the lock file ({}): {e}",
            "lock 파일 생성 실패({}): {e}",
            path.display()
        ))),
    }
}

/// 기존 lock 파일에서 보유자 메타를 읽는다(파싱 실패·읽기 실패는 None).
fn read_holder(path: &Path) -> Option<LockData> {
    let mut buf = String::new();
    File::open(path).ok()?.read_to_string(&mut buf).ok()?;
    serde_json::from_str(&buf).ok()
}

/// stale 판정 결과.
enum StaleDecision {
    /// 자동 회수 가능(죽은 보유자) — 사유 문자열.
    Reclaim(String),
    /// 충돌(살아있거나 판단 불가) — 안내에 덧붙일 추가 메시지(있으면).
    Conflict(Option<String>),
}

/// 보유자 메타로 stale 여부를 판정한다(위 정책 1~3).
fn classify(holder: &Option<LockData>) -> StaleDecision {
    let Some(h) = holder else {
        // lock 파일은 있는데 메타를 못 읽었다(손상/경쟁). 보수적으로 충돌 처리하되
        // 손상 가능성을 안내한다 — 운영자가 직접 확인·삭제하도록.
        return StaleDecision::Conflict(Some(crate::tr!(
            "the lock file could not be read (it may be corrupt, or another process is \
             writing it right now). Check it by hand and delete the lock file if it's safe to",
            "lock 파일을 읽을 수 없습니다(손상되었거나 다른 프로세스가 쓰는 중일 수 있음). \
             직접 확인 후 안전하다면 lock 파일을 삭제하세요"
        )));
    };

    // 다른 호스트의 lock은 이 호스트에서 PID 생존을 판정할 수 없다(같은 pid가 다른
    // 호스트의 다른 프로세스일 수 있음). 보수적으로 회수하지 않는다.
    if h.hostname != hostname() {
        return StaleDecision::Conflict(Some(crate::tr!(
            "this is another host's ('{}') lock — this host will not reclaim it \
             automatically. Check whether that host's operation has finished",
            "다른 호스트('{}')의 lock입니다 — 이 호스트에서는 자동 회수하지 않습니다. \
             해당 호스트에서 작업이 끝났는지 확인하세요",
            h.hostname
        )));
    }

    // 정책 1: PID 부재가 확실하면 자동 회수.
    if !pid_alive(h.pid) {
        return StaleDecision::Reclaim(crate::tr!(
            "the process holding the lock (pid {}) no longer exists",
            "lock 보유 프로세스(pid {})가 더 이상 존재하지 않습니다",
            h.pid
        ));
    }

    // 정책 3: 살아있으나 24h 초과 — 자동 해제 금지, 안내만.
    if lock_age_secs(&h.started_at).is_some_and(|age| age > STALE_AGE_SECS) {
        return StaleDecision::Conflict(Some(crate::tr!(
            "the lock has been held for over 24 hours (pid {} is still alive) — it may be \
             abnormally old, but since the process is alive it will not be released \
             automatically. If that operation is stuck, kill the process and then delete \
             the lock file",
            "lock이 24시간 넘게(pid {} 생존 중) 유지되고 있습니다 — 비정상적으로 오래된 \
             lock일 수 있으나 프로세스가 살아 있어 자동 해제하지 않습니다. 해당 작업이 \
             멈춰 있다면 프로세스를 종료한 뒤 lock 파일을 삭제하세요",
            h.pid
        )));
    }

    // 정책 2: 정상적으로 다른 인스턴스 실행 중.
    StaleDecision::Conflict(None)
}

/// 충돌 에러를 만든다(pid·시작 시각·추가 안내 포함, exit 5).
fn conflict_error(path: &Path, holder: &Option<LockData>, extra: Option<String>) -> XBackupError {
    let mut msg = match holder {
        Some(h) => crate::tr!(
            "another instance of the same profile '{}' is running (pid {}, started {}). \
             Wait for it to finish, or stop that operation",
            "동일 프로파일 '{}'의 다른 인스턴스가 실행 중입니다(pid {}, 시작 {}). \
             완료를 기다리거나 해당 작업을 종료하세요",
            h.profile,
            h.pid,
            h.started_at
        ),
        None => crate::tr!(
            "another instance holds the lock '{}'",
            "lock '{}'을 다른 인스턴스가 점유하고 있습니다",
            path.display()
        ),
    };
    if let Some(extra) = extra {
        msg.push_str(". ");
        msg.push_str(&extra);
    }
    msg.push_str(&crate::tr!(
        " (lock file: {})",
        " (lock 파일: {})",
        path.display()
    ));
    XBackupError::LockConflict(msg)
}

/// `kill(pid, 0)`로 프로세스 생존을 확인한다.
///
/// - `0` 반환: 살아 있음(또는 권한 없음 = 존재함 = EPERM → 살아 있는 것으로 본다).
/// - `ESRCH`: 그런 프로세스 없음 → 죽음.
///
/// `kill(.., 0)`은 시그널을 보내지 않고 권한/존재만 검사한다(POSIX). 따라서 안전하다.
fn pid_alive(pid: u32) -> bool {
    // pid 0/1 등 경계: 0은 "프로세스 그룹 전체"라 오판 위험 → 살아있는 것으로 본다.
    if pid == 0 {
        return true;
    }
    // SAFETY: kill(pid, 0)은 메모리에 영향이 없고 시그널도 보내지 않는 존재 검사다.
    let ret = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if ret == 0 {
        return true;
    }
    // errno == ESRCH(없음)일 때만 죽음으로 판단. EPERM(권한)은 존재함을 의미한다.
    let err = std::io::Error::last_os_error();
    err.raw_os_error() != Some(libc::ESRCH)
}

/// `started_at`(RFC3339) 이후 경과 초. 파싱 실패면 None(연령 판정 생략).
fn lock_age_secs(started_at: &str) -> Option<i64> {
    let started = chrono::DateTime::parse_from_rfc3339(started_at).ok()?;
    Some(
        chrono::Utc::now()
            .signed_duration_since(started)
            .num_seconds(),
    )
}

/// lock 파일 경로: `<dir>/<profile>.lock`. 프로파일명의 경로 구분자는 무해화한다.
fn lock_path(dir: &Path, profile: &str) -> PathBuf {
    // 프로파일명에 '/'가 들어가면 의도치 않은 하위 경로가 되므로 치환한다.
    let safe: String = profile
        .chars()
        .map(|c| if c == '/' || c == '\\' { '_' } else { c })
        .collect();
    dir.join(format!("{safe}.lock"))
}

/// 기본 lock 디렉터리: `$XDG_RUNTIME_DIR/x-backup`, 없으면 `/tmp/x-backup`.
fn lock_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(rt) if !rt.is_empty() => PathBuf::from(rt).join("x-backup"),
        _ => std::env::temp_dir().join("x-backup"),
    }
}

/// 호스트명을 얻는다(실패 시 "unknown"). 다른 호스트 lock 회수 방지에 쓴다.
fn hostname() -> String {
    // std에 hostname API가 없어 환경변수·uname 폴백을 쓴다. 정확한 호스트명이
    // 아니어도(같은 호스트에서 일관되기만 하면) 자동 회수 안전성에는 충분하다.
    if let Some(h) = std::env::var_os("HOSTNAME").filter(|h| !h.is_empty()) {
        return h.to_string_lossy().into_owned();
    }
    read_uname_nodename().unwrap_or_else(|| "unknown".to_string())
}

/// `uname(2)`의 nodename(호스트명)을 읽는다.
fn read_uname_nodename() -> Option<String> {
    // SAFETY: uname은 호출자가 준 buf에 구조체를 채울 뿐 별도 부작용이 없다.
    unsafe {
        let mut uts: libc::utsname = std::mem::zeroed();
        if libc::uname(&mut uts) != 0 {
            return None;
        }
        let bytes: &[libc::c_char] = &uts.nodename;
        // c_char(i8/u8)을 u8로 보고 NUL까지 읽는다.
        let nul = bytes.iter().position(|&c| c == 0).unwrap_or(bytes.len());
        let slice: Vec<u8> = bytes[..nul].iter().map(|&c| c as u8).collect();
        String::from_utf8(slice).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 첫 acquire는 성공하고 lock 파일이 생긴다.
    #[test]
    fn first_acquire_creates_lock_file() {
        let dir = tempfile::tempdir().unwrap();
        let guard = acquire_in(dir.path(), "prof").unwrap();
        assert!(guard.path().exists());
        assert_eq!(guard.data().pid, std::process::id());
        assert_eq!(guard.data().profile, "prof");
    }

    /// 같은 프로파일을 살아있는 프로세스(이 테스트 프로세스)가 잡고 있으면 두 번째는
    /// LockConflict(exit 5)다.
    #[test]
    fn second_acquire_same_profile_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        let _g1 = acquire_in(dir.path(), "prof").unwrap();
        let err = acquire_in(dir.path(), "prof").unwrap_err();
        assert_eq!(err.exit_code(), 5, "두 번째 획득은 exit 5여야 함: {err}");
    }

    /// 다른 프로파일은 서로 막지 않는다(프로파일 단위 lock).
    #[test]
    fn different_profiles_do_not_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let _a = acquire_in(dir.path(), "a").unwrap();
        let _b = acquire_in(dir.path(), "b").unwrap();
        // 둘 다 성공해야 함(여기 도달하면 통과).
    }

    /// Drop(스코프 종료)으로 lock이 해제되어 재획득이 가능하다.
    #[test]
    fn drop_releases_lock() {
        let dir = tempfile::tempdir().unwrap();
        {
            let _g = acquire_in(dir.path(), "prof").unwrap();
        } // 여기서 Drop.
          // 같은 프로파일을 다시 잡을 수 있어야 한다.
        let g2 = acquire_in(dir.path(), "prof");
        assert!(g2.is_ok(), "Drop 후 재획득 실패: {:?}", g2.err());
    }

    /// release()로 명시적 해제 후 재획득 가능.
    #[test]
    fn explicit_release_allows_reacquire() {
        let dir = tempfile::tempdir().unwrap();
        let g = acquire_in(dir.path(), "prof").unwrap();
        g.release();
        assert!(acquire_in(dir.path(), "prof").is_ok());
    }

    /// stale: 죽은 PID가 같은 호스트로 기록된 lock은 자동 회수된다(정책 1).
    #[test]
    fn stale_dead_pid_same_host_is_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let path = lock_path(dir.path(), "prof");
        std::fs::create_dir_all(dir.path()).unwrap();
        // 존재하지 않을 PID(매우 큰 값)로 lock을 위조한다 — 같은 호스트명.
        let dead = LockData {
            pid: u32::MAX - 1,
            started_at: chrono::Utc::now().to_rfc3339(),
            profile: "prof".to_string(),
            hostname: hostname(),
        };
        std::fs::write(&path, serde_json::to_vec_pretty(&dead).unwrap()).unwrap();

        // 죽은 PID이므로 자동 회수 후 우리가 획득해야 한다.
        let guard = acquire_in(dir.path(), "prof").unwrap();
        assert_eq!(guard.data().pid, std::process::id());
    }

    /// stale: 다른 호스트로 기록된 lock은 (죽은 PID라도) 자동 회수하지 않고 충돌한다.
    #[test]
    fn lock_from_other_host_is_not_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let path = lock_path(dir.path(), "prof");
        std::fs::create_dir_all(dir.path()).unwrap();
        let other = LockData {
            pid: u32::MAX - 1, // 죽은 PID지만,
            started_at: chrono::Utc::now().to_rfc3339(),
            profile: "prof".to_string(),
            hostname: "some-other-host-xyz".to_string(), // 다른 호스트.
        };
        std::fs::write(&path, serde_json::to_vec_pretty(&other).unwrap()).unwrap();

        let err = acquire_in(dir.path(), "prof").unwrap_err();
        assert_eq!(err.exit_code(), 5);
        let msg = err.to_string();
        assert!(
            msg.contains("another host") || msg.contains("다른 호스트"),
            "missing other-host guidance: {err}"
        );
    }

    /// stale: 살아있는 PID + 24h 초과는 자동 해제하지 않고 충돌(안내 포함)한다(정책 3).
    #[test]
    fn alive_pid_over_age_conflicts_with_guidance() {
        let dir = tempfile::tempdir().unwrap();
        let path = lock_path(dir.path(), "prof");
        std::fs::create_dir_all(dir.path()).unwrap();
        // 살아있는 PID(이 프로세스) + 25시간 전 시작.
        let old = LockData {
            pid: std::process::id(),
            started_at: (chrono::Utc::now() - chrono::Duration::hours(25)).to_rfc3339(),
            profile: "prof".to_string(),
            hostname: hostname(),
        };
        std::fs::write(&path, serde_json::to_vec_pretty(&old).unwrap()).unwrap();

        let err = acquire_in(dir.path(), "prof").unwrap_err();
        assert_eq!(err.exit_code(), 5);
        let msg = err.to_string();
        assert!(
            msg.contains("24 hours") || msg.contains("24시간"),
            "missing stale-lock guidance: {err}"
        );
    }

    /// 손상된 lock 파일(파싱 불가)은 보수적으로 충돌 처리하고 안내한다.
    #[test]
    fn corrupt_lock_conflicts_conservatively() {
        let dir = tempfile::tempdir().unwrap();
        let path = lock_path(dir.path(), "prof");
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(&path, b"not-json{{").unwrap();

        let err = acquire_in(dir.path(), "prof").unwrap_err();
        assert_eq!(err.exit_code(), 5);
        let msg = err.to_string();
        assert!(
            msg.contains("could not be read") || msg.contains("읽을 수 없"),
            "missing corruption guidance: {err}"
        );
    }

    /// pid_alive: 현재 프로세스는 살아 있다.
    #[test]
    fn current_pid_is_alive() {
        assert!(pid_alive(std::process::id()));
    }

    /// pid_alive: 존재하지 않는 매우 큰 PID는 죽음으로 본다.
    #[test]
    fn nonexistent_pid_is_dead() {
        assert!(!pid_alive(u32::MAX - 1));
    }

    /// lock_path: 프로파일명의 경로 구분자를 무해화한다.
    #[test]
    fn lock_path_sanitizes_separators() {
        let p = lock_path(Path::new("/tmp/x"), "a/b\\c");
        assert_eq!(p, PathBuf::from("/tmp/x/a_b_c.lock"));
    }

    /// hostname()은 호출 간 일관된 값을 준다(자동 회수 안전성의 전제).
    #[test]
    fn hostname_is_consistent() {
        assert_eq!(hostname(), hostname());
        assert!(!hostname().is_empty());
    }
}
