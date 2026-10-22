//! `GET /monitor` · `GET /monitor/events` — 라이브 모니터.
//!
//! 화면 하나가 `status --watch --json --all` 자식 **하나**를 공유한다. 뷰어 수와 프레임
//! 배급은 [`crate::web::state::monitor::LiveMonitor`]가 알고, 이 파일은 **자식의 수명**만
//! 맡는다 — 언제 띄우고 언제 죽이는가.
//!
//! ## 자식을 띄우는 순간과 죽이는 순간
//! - **띄운다**: 뷰어가 0에서 1이 될 때. [`LiveMonitor::claim_child`]가 CAS라 뷰어 둘이
//!   동시에 붙어도 자식은 하나다.
//! - **죽인다**: 마지막 뷰어가 떠나고 [`VIEWER_GRACE`]가 지났는데 여전히 0명일 때.
//!   grace가 필요한 이유는 그 모듈 헤더에 있다(새로고침 = 끊고 즉시 다시 붙기).
//!
//! 감시 태스크는 자식을 띄운 쪽이 함께 띄운다. 그 태스크가 하는 일은 하나다: 주기적으로
//! 뷰어 수를 보고, 0이 grace만큼 지속되면 자식을 끝낸다.
//!
//! ## 왜 SSE 응답 안에서 뷰어 수를 줄이지 않는가
//! [`Subscription`]이 드롭될 때 자동으로 줄어든다. 브라우저 탭 닫힘·네트워크 끊김·프록시
//! 타임아웃은 전부 **응답 스트림이 드롭되는** 것으로 나타나는데, 그 경로를 손으로 세면
//! 하나라도 빠뜨렸을 때 자식이 영원히 산다. 소멸자에 걸어 두면 빠뜨릴 자리가 없다.
//!
//! ## 이 화면이 대시보드와 다른 점
//! [`crate::web::routes::dashboard`]는 **요청할 때마다** 프로파일마다 자식을 띄우고 TTL
//! 캐시로 그 빈도를 묶는다(요청 기반). 이 화면은 **자식 하나가 계속 돌면서** 밀어준다
//! (스트림 기반). 둘의 부하 성질이 달라서 방어도 다르다 — 저쪽은 캐시, 이쪽은 뷰어 수다.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::stream::{self, Stream, StreamExt};
use maud::Markup;
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::web::job::{JobCount, JobFlag, JobSpec};
use crate::web::state::monitor::{LiveMonitor, Subscription, VIEWER_GRACE};
use crate::web::view::{layout, monitor as view};
use crate::web::ServeConfig;

/// 화면 경로.
pub const MONITOR_PATH: &str = "/monitor";
/// SSE 스트림 경로.
pub const MONITOR_EVENTS_PATH: &str = "/monitor/events";
/// `<title>`·내비게이션 라벨. 기술용어이므로 영문 고정([`crate::i18n`] 규약).
pub const MONITOR_TITLE: &str = "Monitor";

/// 자식의 갱신 주기(초).
///
/// CLI 기본값은 1초지만 웹은 2초를 쓴다. 이 화면의 프레임은 네트워크를 건너 브라우저까지
/// 가고, 사람이 숫자 변화를 눈으로 좇는 데 1초는 오히려 산만하다. 무엇보다 **매 틱이
/// 프로덕션 DB 왕복**이라 주기를 두 배로 늘리는 것이 곧 부하 절반이다.
const WATCH_INTERVAL_SECS: u32 = 2;

/// 뷰어 수를 확인하는 주기 — grace 판정의 해상도.
const VIEWER_CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// 자식 stdout 한 줄의 상한(바이트).
///
/// 프레임은 프로파일 수에 비례하는데, 프로파일 하나당 네임스페이스 목록이 붙으므로
/// 상한이 없으면 병리적인 config에서 한 줄이 메모리를 통째로 먹을 수 있다. 1MiB면
/// 프로파일 수백 개 × 네임스페이스 수백 개를 덮는다.
const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// `GET /monitor` — 화면(스크립트가 아래 SSE에 붙는다).
pub async fn page(State(ctx): State<Arc<ServeConfig>>) -> Markup {
    layout::shell(
        ctx.lang,
        MONITOR_TITLE,
        view::body(ctx.lang, WATCH_INTERVAL_SECS),
    )
}

/// `GET /monitor/events` — 프레임 SSE.
///
/// 구독을 먼저 만든 뒤에 자식을 확인한다. 순서를 뒤집으면 자식이 첫 프레임을 발행하는
/// 순간과 구독 사이에 창이 생겨, 첫 뷰어가 한 틱을 통째로 놓친다.
pub async fn events(State(ctx): State<Arc<ServeConfig>>) -> Response {
    let subscription = ctx.monitor.subscribe();
    ensure_child(&ctx);
    sse_response(subscription).into_response()
}

/// 자식이 없으면 띄우고, 그 자식을 지킬 감시 태스크도 함께 띄운다.
fn ensure_child(ctx: &Arc<ServeConfig>) {
    if !ctx.monitor.claim_child() {
        return; // 이미 돌고 있다.
    }

    let spec = JobSpec::new(crate::web::job::JobCommand::Status, ctx.lang)
        .with_flag(JobFlag::All)
        .with_flag(JobFlag::Watch)
        .with_count(JobCount::Interval, WATCH_INTERVAL_SECS);

    let mut running = match ctx.jobs.spawn(&spec) {
        Ok(running) => running,
        Err(e) => {
            tracing::warn!(error = %e, "라이브 모니터 자식을 띄우지 못했습니다");
            ctx.monitor.release_child();
            return;
        }
    };

    let Some(stdout) = running.take_stdout() else {
        tracing::warn!("라이브 모니터 자식의 stdout을 열지 못했습니다");
        ctx.monitor.release_child();
        return;
    };

    // 1) 프레임 읽기 — 자식 stdout을 줄 단위로 브로드캐스트한다.
    let monitor = Arc::clone(&ctx.monitor);
    let registry = ctx.jobs.secret_registry().clone();
    let reader = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    if line.len() > MAX_FRAME_BYTES {
                        tracing::warn!(bytes = line.len(), "모니터 프레임이 상한을 넘어 버립니다");
                        continue;
                    }
                    // 자식이 만든 문자열이므로 무조건 마스킹을 한 번 더 통과시킨다
                    // (다른 화면과 같은 2차 방어).
                    monitor.publish(&registry.mask(&line));
                }
                Ok(None) => break, // 자식이 stdout을 닫았다 = 끝났다.
                Err(e) => {
                    tracing::warn!(error = %e, "모니터 자식 stdout 읽기 실패");
                    break;
                }
            }
        }
    });

    // 2) 감시 — 아무도 보지 않게 되면 자식을 끝낸다.
    let monitor = Arc::clone(&ctx.monitor);
    tokio::spawn(async move {
        wait_until_unwatched(&monitor).await;

        // 아무도 보지 않는다 — 프로덕션을 그만 두드린다.
        tracing::info!("라이브 모니터 뷰어가 없어 자식을 종료합니다");
        if let Err(e) = crate::web::job::lifecycle::terminate_just_spawned(
            running,
            crate::web::job::lifecycle::CANCEL_GRACE_DEFAULT,
        )
        .await
        {
            tracing::warn!(error = %e, "모니터 자식 종료 실패");
        }
        reader.abort();

        // **표시는 마지막에 내린다.** 이 줄이 자식 종료보다 먼저 오면, 그 사이에 들어온
        // 뷰어가 `claim_child`에 성공해 자식이 둘이 된다 — 이 화면의 존재 이유가 깨진다.
        monitor.release_child();
    });
}

/// 뷰어가 0인 상태가 [`VIEWER_GRACE`]만큼 **연속으로** 이어질 때까지 기다린다.
///
/// ## 왜 즉시 죽이지 않는가
/// 새로고침은 브라우저에서 "끊고 곧바로 다시 붙기"로 나타난다. 그 찰나에 자식을 죽이면
/// 새로고침 한 번마다 자식이 죽고 다시 뜨고, 그때마다 델타가 `null`로 되돌아간다 — 화면이
/// 계속 "아직 모른다"로 리셋되는 셈이다.
///
/// ## 왜 중간 복귀에서 경과를 0으로 되돌리는가
/// 되돌리지 않고 누적만 하면, 몇 초씩 끊겼다 붙기를 반복하는 불안정한 회선에서 **뷰어가
/// 붙어 있는 순간에 자식이 죽는다.** 판정 대상은 "합쳐서 5초 비었나"가 아니라 "지금까지
/// **쭉** 5초 비었나"다.
///
/// 자식 소유권을 갖지 않는 순수 대기 함수로 떼어 둔 이유는 테스트다 — 이 판정만 있으면
/// 실제 자식 프로세스 없이 가상 시계로 grace 의미론 전체를 고정할 수 있다.
async fn wait_until_unwatched(monitor: &LiveMonitor) {
    let mut empty_for = Duration::ZERO;
    loop {
        tokio::time::sleep(VIEWER_CHECK_INTERVAL).await;

        if monitor.viewers() > 0 {
            empty_for = Duration::ZERO;
            continue;
        }
        empty_for += VIEWER_CHECK_INTERVAL;
        if empty_for >= VIEWER_GRACE {
            return;
        }
    }
}

/// 구독을 프레임 스트림으로 편다 — **스냅샷 먼저, 그다음 라이브.**
///
/// SSE 포장과 분리해 둔 이유는 테스트다. `Sse<S>`는 내부 스트림을 다시 꺼내주지 않고,
/// HTTP 본문으로 내려가면 청크 경계·keep-alive 주석이 섞여 "무엇이 몇 번째로 나왔는가"를
/// 보기 어려워진다. 순서가 이 함수의 유일한 계약이므로 그 계약만 따로 볼 수 있게 둔다
/// (`crate::web::sse`의 테스트가 `JobEvent` 스트림을 직접 보는 것과 같은 방식).
fn frame_stream(mut subscription: Subscription) -> impl Stream<Item = Arc<str>> {
    // 구독 시점의 마지막 프레임을 먼저 흘려보낸다 — 없으면 다음 틱까지(최대 몇 초) 표가
    // 비어 있고, 그 사이 사용자는 "고장인가"와 "아직인가"를 구분할 수 없다.
    let snapshot = subscription.take_snapshot();
    stream::unfold(
        (snapshot, subscription),
        |(snapshot, mut subscription)| async move {
            if let Some(frame) = snapshot {
                return Some((frame, (None, subscription)));
            }
            let frame = subscription.next_frame().await?;
            Some((frame, (None, subscription)))
        },
    )
}

/// 구독을 SSE 응답으로 만든다.
fn sse_response(
    subscription: Subscription,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let stream = frame_stream(subscription)
        .map(|frame| Ok(Event::default().event("frame").data(frame.as_ref())));
    // keep-alive는 axum 기본값(15초) — 앞단 프록시의 유휴 타임아웃보다 촘촘하다
    // (`crate::web::sse::sse_response`와 같은 판단).
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::state::monitor::LiveMonitor;

    /// **늦게 붙은 뷰어도 빈 화면을 보지 않는다** — 구독 시점의 마지막 프레임이 먼저 온다.
    ///
    /// 이것이 없으면 자식이 이미 돌고 있는데도 새 뷰어는 다음 틱까지 빈 표를 본다.
    #[tokio::test]
    async fn a_late_viewer_gets_the_last_frame_before_any_new_one() {
        let monitor = Arc::new(LiveMonitor::new());
        monitor.publish(r#"{"tick":1}"#);

        let mut stream = Box::pin(frame_stream(monitor.subscribe()));

        let first = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("스냅샷을 기다리다 시간이 다 됐다")
            .expect("스트림이 바로 끝났다");
        assert!(
            first.contains(r#""tick":1"#),
            "스냅샷이 먼저 오지 않았다: {first}"
        );

        // 그다음부터는 새로 발행되는 프레임이 이어진다.
        monitor.publish(r#"{"tick":2}"#);
        let second = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("라이브 프레임을 기다리다 시간이 다 됐다")
            .expect("스트림이 끝났다");
        assert!(
            second.contains(r#""tick":2"#),
            "라이브 프레임이 아니다: {second}"
        );
    }

    /// 첫 뷰어에게는 스냅샷이 없다 — 그렇다고 스트림이 끝나면 안 된다(다음 틱을 기다린다).
    #[tokio::test]
    async fn the_first_viewer_waits_instead_of_seeing_the_stream_end() {
        let monitor = Arc::new(LiveMonitor::new());
        let mut stream = Box::pin(frame_stream(monitor.subscribe()));

        // 아직 아무것도 발행되지 않았다 — 스트림은 끝나지 않고 기다려야 한다.
        let idle = tokio::time::timeout(Duration::from_millis(200), stream.next()).await;
        assert!(idle.is_err(), "프레임이 없는데 스트림이 무언가를 내놓았다");

        monitor.publish(r#"{"tick":7}"#);
        let frame = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("발행 후에도 프레임이 오지 않았다")
            .expect("스트림이 끝났다");
        assert!(frame.contains(r#""tick":7"#));
    }

    /// 뷰어가 붙어 있는 동안에는 감시 태스크가 자식을 끝내지 않는다는 성질을,
    /// 상태 타입 수준에서 확인한다(자식 없이).
    #[tokio::test]
    async fn a_live_viewer_keeps_the_child_claimed() {
        let monitor = Arc::new(LiveMonitor::new());
        assert!(monitor.claim_child());

        let _viewer = monitor.subscribe();
        assert_eq!(monitor.viewers(), 1);
        assert!(monitor.child_running(), "뷰어가 있는데 자식이 내려갔다");
    }

    /// 감시를 백그라운드로 띄우고 핸들을 돌려준다.
    fn watch(monitor: &Arc<LiveMonitor>) -> tokio::task::JoinHandle<()> {
        let monitor = Arc::clone(monitor);
        tokio::spawn(async move { wait_until_unwatched(&monitor).await })
    }

    /// 가상 시계를 흘린 뒤, 방금 깨어난 태스크가 실제로 진행할 틈을 준다.
    ///
    /// 이 양보가 없으면 `is_finished()`가 "아직 스케줄되지 않았을 뿐"인 상태를
    /// "아직 안 끝났다"로 잘못 읽어, 테스트가 우연히 통과한다.
    async fn advance(by: Duration) {
        tokio::time::sleep(by).await;
        tokio::task::yield_now().await;
    }

    /// **아무도 안 볼 때만, 그리고 grace를 다 채운 뒤에만 끝난다.**
    #[tokio::test(start_paused = true)]
    async fn the_child_survives_exactly_the_grace_period_after_the_last_viewer() {
        let monitor = Arc::new(LiveMonitor::new());
        let started = tokio::time::Instant::now();

        wait_until_unwatched(&monitor).await;

        assert!(
            started.elapsed() >= VIEWER_GRACE,
            "grace를 다 채우지 않고 자식을 죽였다: {:?}",
            started.elapsed()
        );
    }

    /// **뷰어가 있는 한 자식은 산다** — grace의 몇 배가 지나도.
    #[tokio::test(start_paused = true)]
    async fn a_watching_viewer_keeps_the_child_alive_indefinitely() {
        let monitor = Arc::new(LiveMonitor::new());
        let _viewer = monitor.subscribe();
        let task = watch(&monitor);

        advance(VIEWER_GRACE * 10).await;

        assert!(
            !task.is_finished(),
            "보고 있는 사람이 있는데 자식을 종료하려 했다"
        );
        task.abort();
    }

    /// **새로고침은 자식을 죽이지 않는다** — grace 안에 다시 붙으면 카운트가 처음으로
    /// 되돌아간다.
    ///
    /// 이 테스트가 잡는 회귀: `empty_for`를 0으로 되돌리지 않고 누적만 하면, 끊겼다 붙기를
    /// 반복하는 회선에서 **뷰어가 붙어 있는 순간에** 자식이 죽는다.
    #[tokio::test(start_paused = true)]
    async fn a_reconnect_within_the_grace_period_resets_the_countdown() {
        let monitor = Arc::new(LiveMonitor::new());
        let viewer = monitor.subscribe();
        let task = watch(&monitor);

        // 떠났다 — grace가 끝나기 직전까지 기다린다.
        drop(viewer);
        advance(VIEWER_GRACE - VIEWER_CHECK_INTERVAL).await;
        assert!(!task.is_finished(), "grace가 끝나기 전에 자식을 죽였다");

        // 새로고침으로 다시 붙었다. 여기서부터는 **처음부터** 다시 세야 한다.
        let refreshed = monitor.subscribe();
        advance(VIEWER_CHECK_INTERVAL * 2).await;
        assert!(
            !task.is_finished(),
            "새로고침으로 뷰어가 돌아왔는데 자식을 종료했다"
        );

        // 다시 떠난다 — 직전에 쌓인 경과가 살아 있었다면 여기서 곧바로 끝나 버린다.
        drop(refreshed);
        advance(VIEWER_GRACE - VIEWER_CHECK_INTERVAL * 2).await;
        assert!(
            !task.is_finished(),
            "복귀 전 경과가 남아 있다 — grace가 처음부터 다시 세어지지 않았다"
        );

        // 이번에는 끝까지 비어 있으므로 종료된다.
        advance(VIEWER_CHECK_INTERVAL * 3).await;
        assert!(task.is_finished(), "끝까지 비었는데 자식이 살아 있다");
    }
}
