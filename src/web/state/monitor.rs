//! 라이브 모니터의 서버 상태 — **자식 하나를 여러 뷰어가 나눠 본다.**
//!
//! ## 왜 자식이 하나여야 하는가
//! `status --watch`는 끝나지 않는 스트림이고, 매 틱마다 모든 프로파일의 DB에 붙어 문서
//! 수와 크기를 센다. 뷰어마다 자식을 띄우면 그 부하가 **뷰어 수만큼 곱해진다** — 장애
//! 대응 중에 운영자 다섯이 각자 모니터를 열면 프로덕션에 붙는 연결이 다섯 배가 되고,
//! 그건 [`crate::web::cache`]가 막으려던 바로 그 상황이다(그 모듈 헤더 "아무도 막아주지
//! 않는 부하").
//!
//! 그래서 이 타입은 자식을 **하나만** 띄우고 그 stdout을 브로드캐스트로 나눈다. 뷰어가
//! 1명이든 20명이든 프로덕션이 받는 부하는 같다.
//!
//! ## 뷰어가 0이 되면 왜 바로 끄지 않는가 — grace
//! 새로고침 한 번은 "구독 해제 → 즉시 재구독"이다. 0을 보는 순간 자식을 죽이면 새로고침
//! 때마다 자식이 죽고 다시 뜨고, 그때마다 델타 기준(이전 틱)이 사라져 화면이 첫 틱으로
//! 되돌아간다. [`VIEWER_GRACE`]는 그 깜빡임을 흡수한다 — 그 시간 안에 누군가 다시 붙으면
//! 자식은 계속 산다.
//!
//! grace가 지나도 0이면 자식을 끝낸다. **아무도 보지 않는데 프로덕션을 계속 두드리는 것이
//! 이 화면의 유일한 실패 모드**이므로, 그 자리는 확실히 닫는다.
//!
//! ## 늦게 붙은 뷰어에게는 마지막 프레임을 먼저 준다
//! 브로드캐스트는 구독 이후의 값만 준다. 갱신 주기가 몇 초이므로, 방금 붙은 뷰어가 다음
//! 틱까지 **빈 화면**을 보게 된다. 그래서 마지막 프레임을 따로 들고 있다가 새 구독자에게
//! 먼저 흘려보낸다([`Subscription::snapshot`]) — [`crate::web::sse::JobHub`]가 같은 이유로
//! 같은 일을 한다.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::broadcast;

/// 마지막 뷰어가 떠난 뒤 자식을 살려 두는 시간.
///
/// 5초를 고른 이유: 새로고침·탭 이동으로 생기는 재구독 공백은 보통 1초 안쪽이고, 사람이
/// "실수로 닫았다가 다시 여는" 시간도 몇 초다. 반대로 이 값이 길면 아무도 안 보는 동안
/// 프로덕션을 두드리는 시간이 그만큼 길어진다 — 깜빡임을 흡수하는 최소한으로 잡는다.
pub const VIEWER_GRACE: Duration = Duration::from_secs(5);

/// 브로드캐스트 채널이 들고 있는 프레임 수.
///
/// 느린 뷰어가 밀리면 오래된 프레임부터 버려진다(`broadcast`의 동작). 라이브 화면에서
/// **오래된 프레임은 버리는 것이 맞다** — 놓친 틱을 뒤늦게 그리면 화면이 과거를 보여준다.
const FRAME_CHANNEL_CAPACITY: usize = 8;

/// 라이브 모니터 한 대(= 이 서버 인스턴스의 `status --watch` 자식 하나).
///
/// 자식의 수명 관리(spawn/kill)는 이 타입이 하지 않는다 — 그건
/// [`crate::web::routes::monitor`]의 몫이고, 이 타입은 **뷰어 수와 프레임 배급**만 안다.
/// 갈라 둔 이유: 자식을 띄우는 코드는 `JobRunner`·`JobSpec`을 알아야 하는데, 그 지식이
/// 여기 들어오면 이 타입을 자식 없이 단위 테스트할 수 없다.
#[derive(Debug)]
pub struct LiveMonitor {
    tx: broadcast::Sender<Arc<str>>,
    last_frame: Mutex<Option<Arc<str>>>,
    viewers: AtomicUsize,
    /// 자식이 도는 중인가 — 라우트가 spawn/kill을 결정할 때 본다.
    running: AtomicUsize,
}

impl Default for LiveMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveMonitor {
    /// 뷰어도 자식도 없는 상태로 만든다.
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(FRAME_CHANNEL_CAPACITY);
        Self {
            tx,
            last_frame: Mutex::new(None),
            viewers: AtomicUsize::new(0),
            running: AtomicUsize::new(0),
        }
    }

    /// 뷰어 하나를 등록하고 구독을 돌려준다.
    ///
    /// 돌려주는 [`Subscription`]이 드롭되면 뷰어 수가 자동으로 줄어든다 — SSE 응답
    /// 스트림이 끊기는 자리(브라우저 탭 닫힘·네트워크 끊김)를 라우트가 일일이 잡지 않아도
    /// 되게 하려는 것이다. 그 자리를 손으로 관리하면 **한 경로라도 빠뜨렸을 때 자식이
    /// 영원히 산다.**
    pub fn subscribe(self: &Arc<Self>) -> Subscription {
        self.viewers.fetch_add(1, Ordering::SeqCst);
        Subscription {
            monitor: Arc::clone(self),
            rx: self.tx.subscribe(),
            snapshot: self
                .last_frame
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        }
    }

    /// 지금 보고 있는 뷰어 수.
    pub fn viewers(&self) -> usize {
        self.viewers.load(Ordering::SeqCst)
    }

    /// 자식이 도는 중이라고 표시한다. 이미 표시돼 있으면 `false`(= 띄우면 안 된다).
    ///
    /// compare-and-swap으로 **한 번만** 성공한다 — 뷰어 둘이 동시에 붙어 둘 다
    /// "자식이 없네"를 보는 경합에서 자식이 두 개 뜨는 것을 막는다.
    pub fn claim_child(&self) -> bool {
        self.running
            .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    /// 자식이 끝났다고 표시한다.
    pub fn release_child(&self) {
        self.running.store(0, Ordering::SeqCst);
        // 다음 뷰어가 붙었을 때 죽은 자식의 마지막 프레임을 "지금 상태"로 보지 않게 한다.
        *self
            .last_frame
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    /// 자식이 도는 중인가.
    pub fn child_running(&self) -> bool {
        self.running.load(Ordering::SeqCst) == 1
    }

    /// 프레임 한 장을 배급한다(그리고 마지막 프레임으로 기억한다).
    pub fn publish(&self, frame: &str) {
        let frame: Arc<str> = Arc::from(frame);
        *self
            .last_frame
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::clone(&frame));
        // 구독자가 0명이어도 실패로 보지 않는다 — grace 구간에는 정상적으로 0명이다.
        let _ = self.tx.send(frame);
    }
}

/// 뷰어 하나의 구독. **드롭되면 뷰어 수가 줄어든다.**
#[derive(Debug)]
pub struct Subscription {
    monitor: Arc<LiveMonitor>,
    rx: broadcast::Receiver<Arc<str>>,
    snapshot: Option<Arc<str>>,
}

impl Subscription {
    /// 구독 시점에 이미 있던 마지막 프레임(있으면) — 첫 화면을 비우지 않기 위한 것.
    pub fn take_snapshot(&mut self) -> Option<Arc<str>> {
        self.snapshot.take()
    }

    /// 다음 프레임을 기다린다. 채널이 닫히면 `None`.
    ///
    /// 밀려서 프레임을 놓친 경우(`Lagged`)는 **끊지 않고 계속 받는다** — 라이브 화면에서
    /// 놓친 틱은 버리는 것이 맞고, 거기서 스트림을 끊으면 느린 뷰어가 화면을 잃는다.
    pub async fn next_frame(&mut self) -> Option<Arc<str>> {
        loop {
            match self.rx.recv().await {
                Ok(frame) => return Some(frame),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.monitor.viewers.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor() -> Arc<LiveMonitor> {
        Arc::new(LiveMonitor::new())
    }

    #[test]
    fn viewers_count_up_and_down_with_subscriptions() {
        let m = monitor();
        assert_eq!(m.viewers(), 0);

        let a = m.subscribe();
        let b = m.subscribe();
        assert_eq!(m.viewers(), 2);

        drop(a);
        assert_eq!(m.viewers(), 1, "구독 하나가 끊겼는데 반영되지 않았다");
        drop(b);
        assert_eq!(m.viewers(), 0, "마지막 구독이 끊겼는데 0이 아니다");
    }

    /// **자식은 한 번만 잡힌다** — 뷰어 여럿이 동시에 붙어도 둘째부터는 실패한다.
    #[test]
    fn only_one_caller_can_claim_the_child() {
        let m = monitor();
        assert!(m.claim_child(), "첫 요청이 자식을 잡지 못했다");
        assert!(
            !m.claim_child(),
            "두 번째 요청도 자식을 잡았다 — 자식이 둘 뜬다"
        );
        assert!(m.child_running());

        m.release_child();
        assert!(!m.child_running());
        assert!(m.claim_child(), "자식이 끝났는데 다시 띄울 수 없다");
    }

    /// 늦게 붙은 뷰어는 마지막 프레임을 즉시 받는다 — 다음 틱까지 빈 화면을 보지 않는다.
    #[tokio::test]
    async fn a_late_viewer_gets_the_last_frame_immediately() {
        let m = monitor();
        m.publish(r#"{"tick":1}"#);

        let mut late = m.subscribe();
        let snapshot = late.take_snapshot().expect("마지막 프레임이 없다");
        assert!(snapshot.contains(r#""tick":1"#));
        assert!(late.take_snapshot().is_none(), "스냅샷이 두 번 나온다");
    }

    /// 구독 **전에** 붙은 뷰어는 이후 프레임을 받는다.
    #[tokio::test]
    async fn subscribers_receive_frames_published_after_they_join() {
        let m = monitor();
        let mut sub = m.subscribe();
        m.publish(r#"{"tick":7}"#);

        let frame = sub.next_frame().await.expect("프레임을 못 받았다");
        assert!(frame.contains(r#""tick":7"#));
    }

    /// **뷰어 20명이 같은 프레임을 받는다** — 자식 하나로 팬아웃한다는 것의 실증.
    #[tokio::test]
    async fn twenty_viewers_all_receive_the_same_frame() {
        let m = monitor();
        let mut subs: Vec<Subscription> = (0..20).map(|_| m.subscribe()).collect();
        assert_eq!(m.viewers(), 20);

        m.publish(r#"{"tick":42,"profiles":[]}"#);

        for (i, sub) in subs.iter_mut().enumerate() {
            let frame = sub
                .next_frame()
                .await
                .unwrap_or_else(|| panic!("{i}번 뷰어가 프레임을 못 받았다"));
            assert!(
                frame.contains(r#""tick":42"#),
                "{i}번 뷰어가 다른 프레임을 받았다"
            );
        }
    }

    /// 자식이 끝나면 마지막 프레임을 버린다 — 죽은 자식의 값을 "지금 상태"로 보여주지 않는다.
    #[test]
    fn releasing_the_child_drops_the_stale_frame() {
        let m = monitor();
        m.claim_child();
        m.publish(r#"{"tick":1}"#);

        m.release_child();

        let mut fresh = m.subscribe();
        assert!(
            fresh.take_snapshot().is_none(),
            "자식이 죽었는데 옛 프레임이 지금 상태로 나온다"
        );
    }

    /// 구독자가 0명일 때 발행해도 패닉하지 않는다 — grace 구간의 정상 상태다.
    #[test]
    fn publishing_with_no_viewers_is_fine() {
        let m = monitor();
        m.publish(r#"{"tick":1}"#);
        assert_eq!(m.viewers(), 0);
    }
}
