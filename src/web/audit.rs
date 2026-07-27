//! 감사 로그 — append-only NDJSON.
//!
//! ## 이 로그의 목적 — 침해 방지가 아니라 사후 추적
//! [`crate::web`] 모듈 헤더가 밝히듯, `x-backup serve`는 age 개인키·프로덕션 DB 접속
//! 정보·config 쓰기 권한을 전부 쥔 상주 프로세스다. 이 프로세스가 침해되면 감사 로그도
//! 함께 조작될 수 있으므로, 이 로그가 **침해를 막지는 못한다.** 이 로그의 존재 이유는
//! 오직 하나 — "이 콘솔에서 무슨 일이 있었는지 운영자가 나중에 반드시 알 수 있게 하는 것"이다.
//! 그래서 설계 전반이 "삭제·수정 불가(append-only)"와 "기록 실패를 절대 삼키지 않음"에
//! 맞춰져 있다.
//!
//! ## 저장 형식
//! `<state_dir>/audit.ndjson` — 한 줄에 항목 하나(NDJSON: newline-delimited JSON).
//! 기존 줄은 절대 수정·삭제하지 않는다. 한 줄의 필드:
//! `timestamp`(RFC3339 UTC, 이 모듈이 기록 시점에 직접 채운다) · `actor` · `action` ·
//! `target`(프로파일 등) · `args_masked`(호출자가 이미 마스킹) · `outcome` · `exit_code`.
//!
//! `timestamp`를 [`AuditEvent`]가 아니라 [`AuditLog::record`]가 직접 채우는 이유: 호출자가
//! 임의 시각을 넣을 수 있으면 감사 기록의 시간 순서를 스스로 조작(백데이팅)할 수 있다.
//! append 순서 = 실제 발생 순서가 되도록, 시각의 유일한 출처를 이 모듈로 좁힌다.
//!
//! ## 핵심 불변식 — "기록 없이는 실행 불가"를 타입으로 강제
//! > 파괴적 작업은 감사 로그 append가 **성공한 뒤에만** 실행된다. append 실패 = 작업 거부.
//!
//! 파괴적 작업 핸들러(t21, t23~t25)는 아직 없다. 그래서 이 모듈은 그 핸들러가 반드시
//! 따를 수밖에 없는 **API 모양**만 미리 만든다: [`AuditLog::gate`]는 append가 성공했을
//! 때만 [`AuditReceipt`]를 반환하고, [`AuditReceipt`]는 이 모듈 밖에서 만들 방법이
//! 없다(필드 비공개, `pub` 생성자 없음). 파괴적 작업 함수가 `receipt: AuditReceipt`를
//! **값으로** 받는 파라미터로 시그니처에 두면, "기록을 잊고 작업하는" 코드는 애초에
//! 넘길 `AuditReceipt` 값을 만들 수 없어 컴파일되지 않는다. 이 패턴의 한계는
//! [`AuditReceipt`] doc을 보라.
//!
//! ## 마스킹은 이 모듈의 책임이 아니다
//! [`AuditEvent::args_masked`]는 이름 그대로 **호출자가 이미 마스킹을 마친** 문자열만
//! 받는다. 이 모듈은 `crate::web::mask`를 import하지 않는다(호출자가 동시에 만드는
//! 모듈이라 순환·타이밍 위험이 있고, 애초에 관심사가 다르다). 감사 모듈이 마스킹
//! 규칙까지 알면 두 가지가 섞인다 — 어떤 문자열이 시크릿인지 아는 것(마스킹의 일)과
//! 그 결과를 변경 없이 영구 기록하는 것(감사의 일). 마스킹 실패를 이 모듈이 떠안으면,
//! 마스킹 버그가 그대로 "기록 실패로 인한 작업 거부"로 번져 가용성 사고가 된다 — 반대로
//! 마스킹 성공 여부를 이 모듈이 검증하지 않으면 원문이 새는 일은 전적으로 호출자
//! 책임이 된다(이 모듈은 받은 문자열을 그대로 JSON 문자열로 직렬화할 뿐, 내용을
//! 들여다보지 않는다).
//!
//! ## 동시 append 안전성
//! 웹은 여러 잡을 동시에 돌리므로, 여러 tokio 태스크가 같은 파일에 동시에
//! [`AuditLog::record`]를 호출할 수 있다. 한 줄이 다른 줄과 섞이지 않도록 **프로세스
//! 내부 `tokio::sync::Mutex`로 append 전체(열기→쓰기→fsync)를 직렬화**한다. OS의
//! `O_APPEND` 원자성(POSIX가 보장하는 lseek+write 원자성)에 기대지 않고 굳이 Mutex를
//! 쓰는 이유: `O_APPEND`의 원자성은 "한 번의 `write()` 시스템 콜"이 파일 끝에 원자적으로
//! 붙는다는 보장이지, 우리 줄이 항상 단일 `write()`로 나간다는 보장은 아니다(버퍼 크기·
//! 파일시스템에 따라 나뉠 수 있음). Mutex로 직렬화하면 이런 세부사항과 무관하게
//! "동시에 두 줄이 끼어들 수 없다"는 사실을 이 프로세스 안에서 확정적으로 증명할 수
//! 있다. **한계:** 이 보장은 *이 프로세스가 이 파일의 유일한 writer*라는 전제 위에
//! 서 있다. 지금 아키텍처가 정확히 그렇다(자식 `x-backup <cmd>` 프로세스는 이 파일에
//! 쓰지 않는다 — [`crate::web`] 모듈 헤더 참조). 만약 나중에 다른 프로세스도 같은
//! 파일에 쓰게 된다면, 프로세스 간 직렬화에는 `flock` 같은 파일 잠금이 별도로
//! 필요하다 — 지금은 범위 밖이다.
//!
//! ## 잘린 마지막 줄 보호
//! 이전 실행이 쓰다가 죽으면(디스크 풀, kill -9 등) 파일이 개행 없이 끝날 수 있다.
//! 그 뒤에 새 줄을 그냥 이어 붙이면 죽은 줄과 새 줄이 한 줄로 합쳐져 **새 항목까지**
//! 깨진 JSON이 된다. 그래서 쓰기 전에 파일 끝이 개행인지 확인하고, 아니면 먼저
//! 개행 하나를 넣어 우리 줄을 분리한다. 기존 바이트는 절대 건드리지 않는다(추가만
//! 한다) — append-only 불변식은 유지하면서 새 항목의 무결성만 보장한다.
//!
//! ## 왜 `record()` 호출마다 파일을 다시 여는가
//! fd를 캐싱하지 않는다. 감사 이벤트는 잡 단위(시작/종료)로만 발생해 드물므로 매번
//! `open()`하는 비용은 무시할 만하고, 대신 두 가지를 얻는다: (1) 외부 도구가 로그
//! 로테이션(rename 후 새 파일 생성)을 하더라도 다음 기록이 항상 "지금 그 경로"에
//! 붙는다 — 캐싱된 fd가 이미 unlink된 inode에 계속 쓰는 놀람을 피한다. (2) 매 기록이
//! 독립적인 성공/실패로 딱 떨어져, "이전에 연 fd가 어느 시점부터 죽어 있었다" 같은
//! 상태를 추적할 필요가 없다.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::Serialize;
use tokio::sync::Mutex;

use crate::error::{Result, XBackupError};

/// 감사 로그 파일 이름(state 디렉터리 기준).
const AUDIT_LOG_FILE_NAME: &str = "audit.ndjson";

/// 감사 로그 한 줄의 결과 상태.
///
/// 자유 문자열 대신 고정 어휘를 쓰는 이유: 감사 로그는 사람이 아니라 나중에 `grep`/
/// 스크립트가 훑는 것을 전제한다. 자유 문자열이면 같은 뜻이 "성공"/"success"/"ok"로
/// 흩어져 검색이 깨진다. `#[non_exhaustive]`로 열어 두어, 후속 태스크(t21/t23~25)가
/// 도메인에 필요한 상태(예: 대화형 취소)를 이 enum에 변형으로 추가할 수 있게 한다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AuditOutcome {
    /// 파괴적 작업 실행 **직전**에 남기는 상태 — [`AuditLog::gate`] 전용.
    /// 이 줄이 있다는 것 자체가 "게이트를 통과했다(= 실행이 시작됐다)"는 증거다.
    Requested,
    /// 작업이 성공적으로 끝남.
    Success,
    /// 작업이 실패로 끝남(exit code는 `exit_code` 필드에 별도로 남는다).
    Failure,
}

/// 한 번의 append 호출에 실리는 내용.
///
/// `timestamp`는 여기 없다 — [`AuditLog::record`]가 기록 시점에 직접 채운다(모듈 헤더
/// "저장 형식" 참조, 호출자의 시각 조작을 막기 위함).
#[derive(Debug, Clone)]
pub struct AuditEvent<'a> {
    /// 이 동작을 수행한 주체(예: 세션 사용자, 향후 t6 인증 계층이 채움).
    pub actor: &'a str,
    /// 수행한 동작(예: `"prune.delete"`, `"backup.start"`).
    pub action: &'a str,
    /// 대상(예: 프로파일명). 시크릿은 여기 담지 않는다 — 프로파일명 같은 식별자용이다.
    pub target: &'a str,
    /// **호출자가 이미 마스킹을 마친 인자만** 담는다 — 모듈 헤더 "마스킹은 이 모듈의
    /// 책임이 아니다" 참조. 이름을 `args_masked`로 둔 것 자체가 계약이다: 마스킹되지
    /// 않은 원문을 넘기면 이 계약을 어기는 것이고, 이 모듈은 그것을 검증하지 않는다.
    pub args_masked: &'a [String],
    /// 이 항목의 결과 상태.
    pub outcome: AuditOutcome,
    /// 연관된 종료 코드(있으면). [`AuditOutcome::Requested`]는 아직 끝나지 않았으므로
    /// 보통 `None`이다.
    pub exit_code: Option<i32>,
}

/// 감사 로그 append가 성공했다는 증표.
///
/// ## 이 타입이 강제하는 것
/// 이 모듈 밖에서는 만들 수 없다(필드 비공개, `pub` 생성자 없음). 유일한 생성 경로는
/// [`AuditLog::gate`]가 append에 **성공**했을 때뿐이다. 파괴적 작업 함수(t21, t23~t25
/// 몫)가 시그니처에 `receipt: AuditReceipt`를 **값으로 받는** 필수 파라미터로 두면:
/// - `Clone`/`Copy`를 파생하지 않았으므로, 받은 receipt를 소비(move)하면 그걸로
///   끝이다 — 같은 receipt로 두 번째 파괴적 작업을 또 실행하는 실수도 타입 수준에서
///   막힌다(각 파괴적 작업은 자신만의 `gate()` 호출로 새 receipt를 받아야 한다).
/// - "기록을 건너뛰고 작업하는" 코드는 넘길 `AuditReceipt` 값 자체를 만들 방법이
///   없으므로 컴파일되지 않는다.
///
/// ## 한계
/// - **"기록된 그 작업 = 실행되는 그 작업"까지는 보장하지 않는다.** 이 타입은 "직전에
///   무언가가 기록됐다"만 증명한다. `gate()` 직후 바로 그 작업을 실행하는 것은 호출부의
///   관례로 지켜야 한다(같은 함수 스코프 안에서, 다른 receipt와 뒤섞이지 않게).
/// - Rust 가시성 규칙 안에서의 보장이다 — `unsafe`나 `std::mem::transmute` 같은 우회를
///   막지는 못한다(이 크레이트 안에서 그런 우회를 쓰지 않는다는 것은 코드 리뷰의 몫이다).
/// - receipt를 만들어 놓고 **작업을 실행하지 않고 버리는 것**은 막지 못한다(과소
///   실행). 이건 안전 방향의 실패라 허용한다 — "기록됐지만 실행 안 됨"은 감사관이
///   보기에 이상하지 않지만, "실행됐는데 기록 안 됨"은 이 모듈이 존재하는 이유 그
///   자체를 무너뜨린다.
#[must_use = "AuditReceipt를 버리면 파괴적 작업을 실행할 방법이 없어집니다 — gate() 직후 바로 소비하세요"]
pub struct AuditReceipt {
    _private: (),
}

/// append-only 감사 로그 핸들.
///
/// `write_lock`은 같은 프로세스 안의 여러 tokio 태스크가 동시에 append할 때 줄이
/// 섞이지 않게 직렬화한다(모듈 헤더 "동시 append 안전성" 참조).
#[derive(Debug)]
pub struct AuditLog {
    path: PathBuf,
    write_lock: Mutex<()>,
}

impl AuditLog {
    /// 감사 로그를 연다 — `<state_dir>/audit.ndjson`.
    ///
    /// `state_dir`은 이미 존재하는 디렉터리여야 한다(생성은 상위 모듈([`crate::web`]의
    /// state 디렉터리 준비 로직) 소관 — 이 함수는 그 결과를 전제로만 쓴다). 호출 즉시
    /// 한 번 실제로 열어(없으면 생성) 쓰기 가능 여부를 부팅 시점에 확인한다 — 감사
    /// 로그를 못 쓰는 상태로 서버가 떴다가, 첫 파괴적 요청이 들어온 순간에야 실패를
    /// 발견하는 것을 막는다(fail-closed, [`crate::web`] 모듈 헤더의 바인딩 검증과 같은
    /// 철학).
    pub fn open(state_dir: &Path) -> Result<Self> {
        let path = state_dir.join(AUDIT_LOG_FILE_NAME);
        open_append_raw(&path).map_err(|e| {
            XBackupError::Config(format!(
                "감사 로그 파일을 열 수 없습니다({}): {e} — state 디렉터리 권한을 확인하세요. \
                 감사 로그 없이는 파괴적 작업을 기록·실행할 수 없으므로 서버를 기동하지 \
                 않습니다.",
                path.display()
            ))
        })?;
        Ok(Self {
            path,
            write_lock: Mutex::new(()),
        })
    }

    /// 감사 로그 파일 경로(진단·테스트용).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 항목 하나를 append한다. `outcome`/`exit_code`를 자유롭게 지정할 수 있는
    /// 일반 기록 경로다 — 파괴적 작업의 "완료" 기록(성공/실패)은 물론, 파괴적 작업과
    /// 무관한 이벤트(예: 향후 인증 실패)도 이 경로로 남길 수 있다.
    ///
    /// 실패(디스크 풀, 권한 상실 등)는 절대 삼키지 않고 그대로 호출자에게 전파한다 —
    /// 삼키면 "기록 실패를 호출자가 작업 거부로 이어야 한다"는 상위 규약을 이 함수가
    /// 스스로 깨는 셈이다.
    pub async fn record(&self, event: AuditEvent<'_>) -> Result<()> {
        let record = SerializedRecord {
            timestamp: Utc::now().to_rfc3339(),
            actor: event.actor,
            action: event.action,
            target: event.target,
            args_masked: event.args_masked,
            outcome: event.outcome,
            exit_code: event.exit_code,
        };
        let mut line = serde_json::to_string(&record)
            .map_err(|e| XBackupError::Failure(format!("감사 로그 항목 직렬화 실패: {e}")))?;
        line.push('\n');

        // 여기서부터 파일 끝까지가 임계 구역이다 — 동시 append 직렬화(모듈 헤더 참조).
        let _guard = self.write_lock.lock().await;
        let path = self.path.clone();
        // 파일 I/O(open/seek/write/fsync)는 블로킹이므로 spawn_blocking으로 넘겨 tokio
        // 실행기 스레드를 막지 않는다. Mutex 가드는 spawn_blocking의 await 동안 계속
        // 잡혀 있어(tokio::sync::Mutex는 await 경계를 넘겨 들고 있도록 설계됨) 직렬화가
        // 유지된다.
        tokio::task::spawn_blocking(move || append_line(&path, line.as_bytes()))
            .await
            .map_err(|e| {
                XBackupError::Failure(format!("감사 로그 기록 태스크가 비정상 종료했습니다: {e}"))
            })??;
        Ok(())
    }

    /// 파괴적 작업 게이트 — append에 성공해야만 [`AuditReceipt`]를 반환한다.
    ///
    /// outcome은 항상 [`AuditOutcome::Requested`]로 고정한다(호출자가 임의 outcome으로
    /// "게이트"를 위장해 기록하는 것을 막기 위해, 이 메서드는 `AuditEvent`를 그대로
    /// 받지 않고 조각을 받아 내부에서 직접 조립한다). 작업이 끝난 뒤의 성공/실패는
    /// 별도로 [`record`](Self::record)를 호출해 남긴다(모듈 헤더 "핵심 불변식" 참조).
    pub async fn gate(
        &self,
        actor: &str,
        action: &str,
        target: &str,
        args_masked: &[String],
    ) -> Result<AuditReceipt> {
        self.record(AuditEvent {
            actor,
            action,
            target,
            args_masked,
            outcome: AuditOutcome::Requested,
            exit_code: None,
        })
        .await?;
        Ok(AuditReceipt { _private: () })
    }
}

/// 파일에 실제로 직렬화되는 한 줄의 전체 모양(항목 + [`AuditLog`]가 채우는 `timestamp`).
#[derive(Serialize)]
struct SerializedRecord<'a> {
    timestamp: String,
    actor: &'a str,
    action: &'a str,
    target: &'a str,
    args_masked: &'a [String],
    outcome: AuditOutcome,
    exit_code: Option<i32>,
}

/// 실제 append 수행 — 동기 함수. [`AuditLog::record`]의 `spawn_blocking` 안에서만
/// 호출한다(직접 async 컨텍스트에서 부르지 않는다).
fn append_line(path: &Path, line: &[u8]) -> Result<()> {
    let mut file = open_append_raw(path).map_err(|e| {
        XBackupError::Failure(format!(
            "감사 로그 파일을 열 수 없습니다({}): {e} — 이 실패는 반드시 호출자에게 전파되어 \
             해당 작업을 막아야 합니다(모듈 헤더의 핵심 불변식 참조)",
            path.display()
        ))
    })?;

    // 잘린 마지막 줄 보호 — 모듈 헤더 참조. 기존 바이트는 건드리지 않고 필요하면
    // 구분자 개행만 먼저 추가한다.
    if !ends_with_newline_or_empty(&mut file).map_err(|e| {
        XBackupError::Failure(format!("감사 로그 상태 확인 실패({}): {e}", path.display()))
    })? {
        file.write_all(b"\n").map_err(|e| {
            XBackupError::Failure(format!(
                "감사 로그 구분자 기록 실패({}): {e}",
                path.display()
            ))
        })?;
    }

    file.write_all(line).map_err(|e| {
        XBackupError::Failure(format!("감사 로그 기록 실패({}): {e}", path.display()))
    })?;
    // 감사 로그의 존재 이유가 "나중에 반드시 알 수 있게"이므로, 커널 페이지 캐시에만
    // 머물다 크래시로 증발하지 않게 fsync까지 한다. 호출 빈도가 낮아(잡 시작/종료당
    // 수 회) 이 비용은 감수한다.
    file.sync_data().map_err(|e| {
        XBackupError::Failure(format!("감사 로그 fsync 실패({}): {e}", path.display()))
    })?;
    Ok(())
}

/// append 모드로 파일을 연다(없으면 생성). 생성 시에만 0600으로 조인다(파일 소유자
/// 전용 — 상위 모듈이 state 디렉터리에 적용하는 정책과 같은 이유).
///
/// `.read(true)`도 함께 켠다 — 쓰기만 할 뿐인데 읽기 권한이 왜 필요한가 하면,
/// [`ends_with_newline_or_empty`]가 "잘린 마지막 줄" 보호를 위해 파일 끝 1바이트를
/// 읽어야 하기 때문이다(write-only fd로는 그 읽기 자체가 `EBADF`로 거부된다).
fn open_append_raw(path: &Path) -> std::io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.create(true).read(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

/// 파일이 비어 있거나 마지막 바이트가 개행인지 확인한다. append 위치에 영향을 주지
/// 않는다 — 파일이 `O_APPEND`로 열려 있으므로(유닉스) 다음 write는 이 함수의 `seek`
/// 위치와 무관하게 항상 파일 끝에 붙는다.
fn ends_with_newline_or_empty(file: &mut File) -> std::io::Result<bool> {
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(true);
    }
    let mut buf = [0u8; 1];
    file.seek(SeekFrom::End(-1))?;
    file.read_exact(&mut buf)?;
    Ok(buf[0] == b'\n')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 파일에서 각 줄을 `serde_json::Value`로 파싱해 돌려준다(빈 줄은 건너뜀).
    fn read_lines(path: &Path) -> Vec<serde_json::Value> {
        let content = std::fs::read_to_string(path).unwrap();
        content
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| {
                serde_json::from_str(l)
                    .unwrap_or_else(|e| panic!("유효하지 않은 JSON 줄 '{l}': {e}"))
            })
            .collect()
    }

    /// 기록 하나가 유효한 NDJSON 한 줄로 들어가고, 요구된 7개 필드를 모두 담는다.
    #[tokio::test]
    async fn record_appends_one_valid_ndjson_line() {
        let dir = tempfile::tempdir().unwrap();
        let log = AuditLog::open(dir.path()).unwrap();
        let args = vec!["--profile".to_string(), "***masked***".to_string()];

        log.record(AuditEvent {
            actor: "cli",
            action: "prune.delete",
            target: "nightly",
            args_masked: &args,
            outcome: AuditOutcome::Success,
            exit_code: Some(0),
        })
        .await
        .unwrap();

        let lines = read_lines(log.path());
        assert_eq!(lines.len(), 1);
        let v = &lines[0];
        assert_eq!(v["actor"], "cli");
        assert_eq!(v["action"], "prune.delete");
        assert_eq!(v["target"], "nightly");
        assert_eq!(v["args_masked"], serde_json::json!(args));
        assert_eq!(v["outcome"], "success");
        assert_eq!(v["exit_code"], 0);
        // timestamp가 RFC3339로 파싱 가능해야 한다(시각의 유일한 출처가 이 모듈이라는
        // 계약의 회귀 방지).
        let ts = v["timestamp"].as_str().unwrap();
        chrono::DateTime::parse_from_rfc3339(ts)
            .unwrap_or_else(|e| panic!("timestamp '{ts}'가 RFC3339가 아님: {e}"));
    }

    /// `Requested` outcome은 exit_code 없이(`null`) 기록된다 — gate() 시점엔 아직
    /// 결과를 모른다.
    #[tokio::test]
    async fn requested_outcome_has_no_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let log = AuditLog::open(dir.path()).unwrap();
        let _receipt = log
            .gate("cli", "prune.delete", "nightly", &[])
            .await
            .unwrap();

        let lines = read_lines(log.path());
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["outcome"], "requested");
        assert!(lines[0]["exit_code"].is_null());
    }

    /// 여러 번 기록해도 기존 줄은 바이트 단위로 불변이다 — append-only 핵심 보장.
    #[tokio::test]
    async fn repeated_records_leave_existing_bytes_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let log = AuditLog::open(dir.path()).unwrap();

        log.record(AuditEvent {
            actor: "a",
            action: "first",
            target: "t1",
            args_masked: &[],
            outcome: AuditOutcome::Success,
            exit_code: Some(0),
        })
        .await
        .unwrap();
        let after_first = std::fs::read(log.path()).unwrap();

        log.record(AuditEvent {
            actor: "b",
            action: "second",
            target: "t2",
            args_masked: &[],
            outcome: AuditOutcome::Failure,
            exit_code: Some(1),
        })
        .await
        .unwrap();
        let after_second = std::fs::read(log.path()).unwrap();

        assert!(
            after_second.starts_with(&after_first),
            "기존 바이트가 변경됐다 — append-only 위반"
        );
        assert!(after_second.len() > after_first.len());

        let lines = read_lines(log.path());
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["action"], "first");
        assert_eq!(lines[1]["action"], "second");
    }

    /// 쓰기 불가 디렉터리(생성 자체가 막힘)는 open()에서 에러로 전파된다(삼키지 않음).
    #[cfg(unix)]
    #[test]
    fn open_fails_when_directory_is_not_writable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();

        let result = AuditLog::open(dir.path());

        // 원상복구(tempdir Drop이 삭제할 수 있도록) 먼저 하고 나서 단정한다.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();

        assert!(
            result.is_err(),
            "쓰기 불가 디렉터리에서 open()이 성공하면 안 됨"
        );
    }

    /// state 디렉터리 경로 자리를 파일이 점유하고 있으면(ENOTDIR) open()이 에러로
    /// 전파된다.
    #[test]
    fn open_fails_when_state_dir_path_is_occupied_by_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let occupied = dir.path().join("state-dir-but-a-file");
        std::fs::write(&occupied, b"not a directory").unwrap();

        let result = AuditLog::open(&occupied);
        assert!(
            result.is_err(),
            "부모 경로가 파일로 점유된 경우 open()이 성공하면 안 됨"
        );
    }

    /// 부팅 시점엔 쓰기가 됐지만 그 뒤 로그 파일이 읽기 전용이 되면(예: 운영 실수),
    /// 이후 record() 호출은 에러로 전파되고 삼켜지지 않는다.
    #[cfg(unix)]
    #[tokio::test]
    async fn record_fails_when_log_file_becomes_read_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let log = AuditLog::open(dir.path()).unwrap();
        std::fs::set_permissions(log.path(), std::fs::Permissions::from_mode(0o400)).unwrap();

        let result = log
            .record(AuditEvent {
                actor: "cli",
                action: "x",
                target: "y",
                args_masked: &[],
                outcome: AuditOutcome::Failure,
                exit_code: Some(1),
            })
            .await;

        // 원상복구 후 단정(tempdir 정리를 위해).
        std::fs::set_permissions(log.path(), std::fs::Permissions::from_mode(0o600)).unwrap();

        assert!(
            result.is_err(),
            "읽기 전용 로그 파일에 record()가 성공하면 안 됨"
        );
    }

    /// 여러 tokio 태스크가 동시에 append해도 줄이 섞이지 않고, 전부 유효한 JSON이며
    /// 하나도 유실되지 않는다.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_appends_do_not_interleave() {
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(AuditLog::open(dir.path()).unwrap());
        const N: usize = 100;

        let mut handles = Vec::with_capacity(N);
        for i in 0..N {
            let log = Arc::clone(&log);
            handles.push(tokio::spawn(async move {
                log.record(AuditEvent {
                    actor: "concurrent",
                    action: "write",
                    target: &format!("task-{i}"),
                    args_masked: &[],
                    outcome: AuditOutcome::Success,
                    exit_code: Some(0),
                })
                .await
                .unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        let lines = read_lines(log.path());
        assert_eq!(
            lines.len(),
            N,
            "줄 수가 태스크 수와 다르다 — 유실 또는 병합 의심"
        );

        let mut targets: Vec<String> = lines
            .iter()
            .map(|v| v["target"].as_str().unwrap().to_string())
            .collect();
        targets.sort();
        let mut expected: Vec<String> = (0..N).map(|i| format!("task-{i}")).collect();
        expected.sort();
        assert_eq!(targets, expected, "일부 항목이 섞이거나 손상됐다");
    }

    /// 이전 실행이 개행 없이 죽어(잘린 마지막 줄) 남긴 파일에 이어 써도, 새 줄은
    /// 오염되지 않고 독립된 유효한 JSON으로 들어간다. 기존(손상된) 바이트도 그대로
    /// 보존된다(append-only).
    #[tokio::test]
    async fn append_after_torn_last_line_does_not_corrupt_new_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(AUDIT_LOG_FILE_NAME);
        let torn = br#"{"actor":"crashed","action":"half_writ"#.to_vec();
        std::fs::write(&path, &torn).unwrap();

        let log = AuditLog::open(dir.path()).unwrap();
        log.record(AuditEvent {
            actor: "recovered",
            action: "after_crash",
            target: "t",
            args_masked: &[],
            outcome: AuditOutcome::Success,
            exit_code: Some(0),
        })
        .await
        .unwrap();

        let content = std::fs::read(&path).unwrap();
        assert!(
            content.starts_with(&torn),
            "손상된 기존 바이트가 보존되지 않았다"
        );

        let text = String::from_utf8(content).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "잘린 줄 뒤에 새 줄이 분리되어 붙지 않았다");
        assert_eq!(lines[0], String::from_utf8(torn).unwrap());
        let new_entry: serde_json::Value = serde_json::from_str(lines[1])
            .unwrap_or_else(|e| panic!("새 줄이 오염됐다('{}'): {e}", lines[1]));
        assert_eq!(new_entry["action"], "after_crash");
    }

    /// gate()는 성공 시에만 AuditReceipt를 반환하고, 그 receipt는 (예시로 만든) 파괴적
    /// 작업 함수에 값으로 소비되어야 호출할 수 있다 — 컴파일 시점 강제를 실제 사용
    /// 패턴으로 보여주는 테스트다(모듈 헤더 "핵심 불변식" 참조. trybuild 같은
    /// 컴파일-실패 테스트는 이 repo에 없는 새 dev-dependency라 추가하지 않았다 —
    /// 완료 보고의 "한계" 참조).
    #[tokio::test]
    async fn gate_receipt_is_required_to_call_destructive_shaped_fn() {
        // 파괴적 작업 함수의 모양을 흉내낸다: receipt를 값으로 요구한다.
        fn run_destructive_op(_receipt: AuditReceipt, label: &str) -> String {
            format!("executed: {label}")
        }

        let dir = tempfile::tempdir().unwrap();
        let log = AuditLog::open(dir.path()).unwrap();
        let receipt = log
            .gate("cli", "prune.delete", "nightly", &[])
            .await
            .expect("정상 경로에서 gate()는 성공해야 함");

        let result = run_destructive_op(receipt, "nightly");
        assert_eq!(result, "executed: nightly");

        // "requested" 한 줄만 남아 있어야 한다(작업 완료 기록은 호출부가 별도 record()를
        // 부르는 몫이라 여기선 남기지 않는다).
        let lines = read_lines(log.path());
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["outcome"], "requested");
    }

    /// gate()가 실패하면(기록 자체가 안 됨) AuditReceipt 값을 얻을 방법이 없다 —
    /// `Result<AuditReceipt>`의 `Err` 분기에는 receipt가 들어 있지 않으므로, 호출부가
    /// `?`로 조기 반환하든 직접 매치하든 파괴적 작업으로 이어지는 경로가 원천적으로
    /// 없다는 것을 확인한다(기록 성공 → receipt 존재의 순서를 코드로 단정).
    #[cfg(unix)]
    #[tokio::test]
    async fn gate_failure_yields_no_receipt() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let log = AuditLog::open(dir.path()).unwrap();
        std::fs::set_permissions(log.path(), std::fs::Permissions::from_mode(0o400)).unwrap();

        let outcome = log.gate("cli", "prune.delete", "nightly", &[]).await;

        std::fs::set_permissions(log.path(), std::fs::Permissions::from_mode(0o600)).unwrap();

        match outcome {
            Ok(_receipt) => panic!("쓰기 불가 상태에서 gate()가 receipt를 반환하면 안 됨"),
            Err(_) => {
                // 기대한 경로: Err에는 AuditReceipt가 없다 — 여기서 파괴적 작업을 호출할
                // 방법이 없다(호출부 코드가 컴파일되려면 Ok 분기에서 얻은 receipt가
                // 있어야 하는데, 이 분기엔 애초에 그 값이 존재하지 않는다).
            }
        }
    }
}
