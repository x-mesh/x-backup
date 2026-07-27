//! 테스트 전용 보조 도구. **`cfg(test)`에서만 컴파일된다.**
//!
//! 여기 있는 것은 제품 동작이 아니라 **테스트 하네스의 환경 문제**를 다룬다. 제품 코드가
//! 이 모듈을 부르는 일은 없어야 한다(그럴 수도 없다 — `cfg(test)`로 막혀 있다).

use std::path::Path;
use std::time::Duration;

/// 방금 만든 실행 파일이 실제로 `exec` 가능해질 때까지 잠깐 기다린다.
///
/// ## 왜 필요한가 — Linux의 `ETXTBSY`
/// 여러 테스트가 임시 셸 스크립트를 만들어 "가짜 mongodump" 같은 외부 도구로 쓴다. 그
/// 스크립트를 만든 뒤 `exec`하면 Linux가 간헐적으로 `ETXTBSY`(Text file busy)를 낸다 —
/// **어떤 프로세스든 그 파일을 쓰기로 열고 있으면** exec이 거부되기 때문이다.
///
/// 우리 쪽 쓰기 핸들은 이미 닫았는데도 나는 이유는 **다른 테스트 스레드의 fork**다:
///
/// ```text
/// 스레드 A: 스크립트 파일을 쓰기로 열고 내용을 쓴다        ← 쓰기 fd 열림
/// 스레드 B: 자기 자식을 fork() 한다                        ← 자식이 A의 fd를 상속
/// 스레드 A: 쓰기 fd를 닫는다(부모에서는 닫혔다)
/// 스레드 A: 스크립트를 exec 한다  →  ETXTBSY               ← B의 자식이 아직 물고 있다
/// 스레드 B: 자식이 execve → CLOEXEC로 상속된 fd가 닫힌다   ← 이 시점 이후에는 성공한다
/// ```
///
/// 그래서 **테스트를 하나만 돌리면 절대 재현되지 않고**(실측: 단독 6회 전부 통과), 전량을
/// 병렬로 돌리면 간헐적으로 실패한다(실측: 3회 중 3개/1개/0개 실패). macOS에서는 나지
/// 않는다 — 그래서 로컬 개발 중에는 보이지 않고 Linux CI에서만 터진다.
///
/// ## 제품에는 이 문제가 없다
/// x-backup이 exec하는 대상은 **자기가 방금 쓴 파일이 아니다**: 외부 도구(`mongodump`)는
/// config가 가리키는 기존 경로이고, 웹이 띄우는 자식은 이미 설치된 자기 자신
/// (`current_exe()`)이다. 쓰기 직후 exec하는 조합은 테스트 하네스에만 있다.
///
/// ## 왜 재시도인가
/// 이 창을 없애려면 다른 테스트가 fork하지 않도록 전량을 직렬화해야 하는데
/// (`--test-threads=1`), 그건 1400개 테스트의 실행 시간을 대가로 낸다. 창이 매우 짧으므로
/// (다른 쪽 자식이 execve에 도달할 때까지) 몇 번 기다리는 편이 싸다.
pub fn wait_until_executable(path: &Path) {
    /// 재시도 상한 — 넘기면 그냥 돌려준다(진짜 문제라면 호출부가 그 실패를 보게 한다).
    const ATTEMPTS: usize = 50;
    /// 매 시도 사이 대기. 창은 fork→execve 사이라 보통 첫 시도에 풀린다.
    const BACKOFF: Duration = Duration::from_millis(20);

    for _ in 0..ATTEMPTS {
        // 스크립트를 한 번 실제로 돌려 본다. 여기서 쓰는 가짜 도구들은 부작용이 없고
        // (자기 stdout/stderr에만 쓴다) 출력은 버린다.
        match std::process::Command::new(path).output() {
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => std::thread::sleep(BACKOFF),
            // 성공이든 다른 오류든, `ETXTBSY`가 아니면 이 함수가 할 일은 끝났다.
            _ => return,
        }
    }
}
