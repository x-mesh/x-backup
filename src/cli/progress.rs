//! 진행 표시(progress) — indicatif 기반, **stderr** 타깃(PRD §FR-9/R16, pitfall 9-1).
//!
//! ## 설계
//! 파이프라인 본문은 진행 상태를 *공유 카운터*([`ProgressCounter`] = `Arc<AtomicU64>`)에만
//! 누산한다(바이트 통과 시 `fetch_add`). 표시 측([`ProgressReporter`])은 그 카운터를
//! 주기(200ms) 폴링해 화면을 갱신하는 **별도 task**로 분리된다 — 파이프라인 로직과
//! 표시 정책(바/스피너/JSON/무표시)이 결합되지 않는다.
//!
//! ```text
//! pipeline ──fetch_add(bytes)──▶ ProgressCounter(Arc<AtomicU64>) ◀──poll(200ms)── ProgressReporter task ──▶ stderr
//! ```
//!
//! ## 표시 정책(출력 모드별)
//! - **Progress(TTY/--progress):** indicatif 바/스피너를 stderr에.
//!   - 복구: 저장 크기(stored_size_bytes)는 알지만 **압축 백업은 복호화·압축해제 후
//!     입력량을 사전에 정확히 알 수 없어 부정형(spinner)** 으로 표시한다(t13 결정).
//!     비압축이면 저장 크기 = 복원 입력량이라 근사 % 바가 가능하다.
//!   - 백업: 총량을 사전에 모를 수 있어, dbStats 추정치가 있으면 **근사 %("추정")**,
//!     없으면 처리 바이트·속도 기반 **부정형(spinner)**.
//! - **Json(--json):** 화면 바 대신 진행 *이벤트*를 stderr에 JSON 라인으로 흘린다
//!   (`{"event":"progress","bytes":...}`) — stdout은 결과 전용.
//! - **Quiet/비-TTY:** 아무것도 표시하지 않는다([`ProgressReporter::disabled`]).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use tokio::task::JoinHandle;

use crate::cli::output::OutputMode;

/// 파이프라인이 누산하는 공유 진행 카운터(통과 바이트 수).
pub type ProgressCounter = Arc<AtomicU64>;

/// 새 진행 카운터를 만든다(0에서 시작).
pub fn new_counter() -> ProgressCounter {
    Arc::new(AtomicU64::new(0))
}

/// 진행 표시의 총량·라벨을 정하는 진행 종류.
#[derive(Debug, Clone)]
pub enum ProgressKind {
    /// 총량을 아는 정확 % 진행(복구 — stored_size_bytes 기지).
    Exact {
        /// 전체 바이트 수.
        total_bytes: u64,
        /// 진행 표시 라벨(예: "복구").
        label: String,
    },
    /// dbStats 추정치 기반 근사 % 진행(백업 — "추정" 명시).
    Estimated {
        /// 추정 총 바이트 수(dbStats).
        estimated_total_bytes: u64,
        /// 진행 표시 라벨(예: "백업").
        label: String,
    },
    /// 총량 미지의 부정형 진행(백업 — 처리 바이트·속도만 표시).
    Indeterminate {
        /// 진행 표시 라벨(예: "백업").
        label: String,
    },
}

impl ProgressKind {
    /// 라벨 텍스트.
    fn label(&self) -> &str {
        match self {
            Self::Exact { label, .. }
            | Self::Estimated { label, .. }
            | Self::Indeterminate { label } => label,
        }
    }
}

/// JSON 진행 이벤트를 한 줄 직렬화한다(stderr 출력용). `total`이 있으면 함께 담는다.
///
/// 순수 함수로 분리해 단위 테스트가 출력 형식을 직접 검증할 수 있게 한다(I/O 없음).
pub fn progress_json_line(bytes: u64, total: Option<u64>) -> String {
    match total {
        Some(t) => format!(r#"{{"event":"progress","bytes":{bytes},"total":{t}}}"#),
        None => format!(r#"{{"event":"progress","bytes":{bytes}}}"#),
    }
}

/// 진행 표시기 — 백그라운드 폴링 task를 들고 있다가 [`ProgressReporter::finish`]에서 정리한다.
///
/// 표시 모드가 아니면([`ProgressReporter::disabled`]) 아무 task도 띄우지 않는 무동작 핸들이다.
pub struct ProgressReporter {
    counter: ProgressCounter,
    task: Option<JoinHandle<()>>,
    bar: Option<ProgressBar>,
}

impl ProgressReporter {
    /// 출력 모드·진행 종류에 맞는 표시기를 시작한다(공유 카운터 폴링 task 스폰).
    ///
    /// - `Progress`: indicatif 바/스피너를 stderr에 그린다.
    /// - `Json`: 진행 이벤트를 stderr JSON 라인으로 흘린다(화면 바 없음).
    /// - `Quiet`: 무표시([`Self::disabled`]).
    pub fn start(mode: OutputMode, kind: ProgressKind, counter: ProgressCounter) -> Self {
        match mode {
            OutputMode::Progress => Self::start_bar(kind, counter),
            OutputMode::Json => Self::start_json(kind, counter),
            OutputMode::Quiet => Self::disabled(counter),
        }
    }

    /// 무표시 핸들(quiet/비-TTY) — 카운터만 들고 아무것도 그리지 않는다.
    pub fn disabled(counter: ProgressCounter) -> Self {
        Self {
            counter,
            task: None,
            bar: None,
        }
    }

    /// indicatif 바/스피너를 stderr에 그리는 표시기.
    fn start_bar(kind: ProgressKind, counter: ProgressCounter) -> Self {
        // 모든 ProgressBar는 명시적으로 stderr 타깃에 그린다(pitfall 9-1).
        let bar = match &kind {
            ProgressKind::Exact { total_bytes, label }
            | ProgressKind::Estimated {
                estimated_total_bytes: total_bytes,
                label,
            } => {
                let bar =
                    ProgressBar::with_draw_target(Some(*total_bytes), ProgressDrawTarget::stderr());
                // 추정 진행은 % 뒤에 "(추정)"을 붙여 정확치 아님을 명시한다(PRD §FR-9).
                let suffix = if matches!(kind, ProgressKind::Estimated { .. }) {
                    " (추정)"
                } else {
                    ""
                };
                bar.set_style(
                    ProgressStyle::with_template(&format!(
                        "{{msg}} [{{bar:30}}] {{bytes}}/{{total_bytes}} ({{percent}}%{suffix}) {{bytes_per_sec}} {{elapsed}}"
                    ))
                    .unwrap_or_else(|_| ProgressStyle::default_bar())
                    .progress_chars("=> "),
                );
                bar.set_message(label.clone());
                bar
            }
            ProgressKind::Indeterminate { label } => {
                // 총량 미지 → 스피너(부정형). 처리 바이트·속도·경과시간만 표시.
                let bar = ProgressBar::with_draw_target(None, ProgressDrawTarget::stderr());
                bar.set_style(
                    ProgressStyle::with_template(
                        "{spinner} {msg} {bytes} {bytes_per_sec} {elapsed}",
                    )
                    .unwrap_or_else(|_| ProgressStyle::default_spinner()),
                );
                bar.set_message(label.clone());
                bar
            }
        };

        // 폴링 task: 200ms마다 공유 카운터를 읽어 바 위치를 갱신한다.
        let poll_counter = Arc::clone(&counter);
        let poll_bar = bar.clone();
        let task = tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(200));
            loop {
                tick.tick().await;
                poll_bar.set_position(poll_counter.load(Ordering::SeqCst));
                if poll_bar.is_finished() {
                    break;
                }
            }
        });

        Self {
            counter,
            task: Some(task),
            bar: Some(bar),
        }
    }

    /// 진행 이벤트를 stderr JSON 라인으로 흘리는 표시기(--json).
    fn start_json(kind: ProgressKind, counter: ProgressCounter) -> Self {
        let total = match &kind {
            ProgressKind::Exact { total_bytes, .. }
            | ProgressKind::Estimated {
                estimated_total_bytes: total_bytes,
                ..
            } => Some(*total_bytes),
            ProgressKind::Indeterminate { .. } => None,
        };
        let _ = kind.label(); // 라벨은 JSON 모드에선 쓰지 않는다(이벤트 최소화).

        let poll_counter = Arc::clone(&counter);
        let task = tokio::spawn(async move {
            use std::io::Write;
            let mut tick = tokio::time::interval(Duration::from_millis(200));
            let mut last = u64::MAX;
            loop {
                tick.tick().await;
                let bytes = poll_counter.load(Ordering::SeqCst);
                // 변화가 있을 때만 한 줄씩 stderr로(중복 억제).
                if bytes != last {
                    last = bytes;
                    let line = progress_json_line(bytes, total);
                    // 진행 이벤트는 stderr 전용(stdout은 결과). 실패는 무시.
                    let _ = writeln!(std::io::stderr(), "{line}");
                }
            }
        });

        Self {
            counter,
            task: None, // JSON task는 finish에서 abort로 정리한다(아래 별도 보관).
            bar: None,
        }
        .with_json_task(task)
    }

    /// JSON 폴링 task를 보관한다(finish에서 abort 정리). 내부 헬퍼.
    fn with_json_task(mut self, task: JoinHandle<()>) -> Self {
        self.task = Some(task);
        self
    }

    /// 진행 표시를 종료한다 — 최종 위치를 반영하고 폴링 task를 정리한다.
    ///
    /// 파이프라인이 끝난 뒤 호출한다. 바 모드면 최종 카운터로 바를 채우고 finish,
    /// JSON/무표시 모드면 폴링 task를 abort한다.
    pub async fn finish(mut self) {
        let final_bytes = self.counter.load(Ordering::SeqCst);
        if let Some(bar) = self.bar.take() {
            bar.set_position(final_bytes);
            bar.finish_and_clear();
        }
        if let Some(task) = self.task.take() {
            // 바 task는 is_finished로 스스로 멈추지만, 확실히 정리하기 위해 abort 후 join.
            task.abort();
            let _ = task.await;
        }
    }

    /// 대화형 프롬프트를 안전하게 출력하기 위한 suspend 핸들을 만든다.
    ///
    /// 진행 표시 도중 사용자 확인(예: 복구 덮어쓰기 [y/N])을 받아야 할 때, 이 핸들의
    /// [`ProgressSuspend::run`]으로 프롬프트를 감싸면 바를 잠시 비우고 프롬프트를 그린 뒤
    /// 다시 그린다. 핸들은 [`finish`](Self::finish) 호출과 독립적으로 살아 있어,
    /// `run_restore` 같은 곳에 confirm 콜백으로 넘긴 뒤 바깥에서 `finish`해도 안전하다.
    pub fn suspend_handle(&self) -> ProgressSuspend {
        ProgressSuspend {
            bar: self.bar.clone(),
        }
    }
}

/// 진행 바를 잠시 비운(clear) 상태로 클로저를 실행한 뒤 다시 그리는 핸들.
///
/// 대화형 프롬프트가 스피너 프레임에 덮여 보이지 않는 문제(폴링 task가 200ms마다 stderr를
/// 다시 그림)를 막는다. [`ProgressBar::suspend`]는 바의 상태 뮤텍스를 잡은 채 클로저를
/// 실행하므로, 폴링 task의 `set_position`이 프롬프트 도중 끼어들지 못한다. 바 모드가
/// 아니면(JSON/quiet) 덮어쓸 바가 없으므로 클로저를 그대로 실행한다.
#[derive(Clone)]
pub struct ProgressSuspend {
    bar: Option<ProgressBar>,
}

impl ProgressSuspend {
    /// 바를 잠시 비운 상태에서 `f`를 실행하고 그 반환값을 돌려준다.
    pub fn run<F, R>(&self, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        match &self.bar {
            Some(bar) => bar.suspend(f),
            None => f(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_line_with_total() {
        assert_eq!(
            progress_json_line(1024, Some(4096)),
            r#"{"event":"progress","bytes":1024,"total":4096}"#
        );
    }

    #[test]
    fn json_line_without_total() {
        assert_eq!(
            progress_json_line(512, None),
            r#"{"event":"progress","bytes":512}"#
        );
    }

    #[test]
    fn json_line_is_valid_json() {
        // 직렬화 라인이 실제로 파싱 가능한 JSON인지(스크립트 연동 안전성).
        let line = progress_json_line(100, Some(200));
        let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["event"], "progress");
        assert_eq!(parsed["bytes"], 100);
        assert_eq!(parsed["total"], 200);
    }

    /// quiet 모드는 무표시 — task/bar 없이 카운터만 보관한다.
    #[tokio::test]
    async fn quiet_mode_is_disabled() {
        let counter = new_counter();
        counter.store(42, Ordering::SeqCst);
        let reporter = ProgressReporter::start(
            OutputMode::Quiet,
            ProgressKind::Indeterminate {
                label: "백업".into(),
            },
            Arc::clone(&counter),
        );
        assert!(reporter.bar.is_none(), "quiet은 바를 그리지 않아야 함");
        assert!(reporter.task.is_none(), "quiet은 폴링 task가 없어야 함");
        reporter.finish().await;
    }

    /// progress 모드는 바를 생성하고, finish가 카운터 최종값을 반영한다.
    #[tokio::test]
    async fn progress_mode_creates_bar_and_finishes() {
        let counter = new_counter();
        let reporter = ProgressReporter::start(
            OutputMode::Progress,
            ProgressKind::Exact {
                total_bytes: 1000,
                label: "복구".into(),
            },
            Arc::clone(&counter),
        );
        assert!(reporter.bar.is_some(), "progress는 바를 생성해야 함");
        // 카운터를 끝까지 채우고 finish — 패닉 없이 정리되어야 한다.
        counter.store(1000, Ordering::SeqCst);
        reporter.finish().await;
    }

    /// suspend 핸들(quiet 모드): 바가 없어도 클로저를 실행하고 반환값을 그대로 돌려준다.
    #[tokio::test]
    async fn suspend_handle_runs_closure_without_bar() {
        let counter = new_counter();
        let reporter = ProgressReporter::disabled(Arc::clone(&counter));
        let suspend = reporter.suspend_handle();
        let ran = std::cell::Cell::new(false);
        let ret = suspend.run(|| {
            ran.set(true);
            7
        });
        assert!(ran.get(), "바가 없어도 클로저는 실행돼야 함");
        assert_eq!(ret, 7, "클로저 반환값을 그대로 돌려줘야 함");
        reporter.finish().await;
    }

    /// suspend 핸들(progress 모드): 바가 있어도 클로저를 실행하고 반환값을 돌려준다.
    /// (바를 비웠다 다시 그리는 경로가 패닉 없이 통과하는지 함께 확인)
    #[tokio::test]
    async fn suspend_handle_runs_closure_with_bar() {
        let counter = new_counter();
        let reporter = ProgressReporter::start(
            OutputMode::Progress,
            ProgressKind::Indeterminate {
                label: "복구".into(),
            },
            Arc::clone(&counter),
        );
        let suspend = reporter.suspend_handle();
        let ret = suspend.run(|| 42);
        assert_eq!(ret, 42);
        reporter.finish().await;
    }

    /// kind 라벨 접근자(내부 일관성).
    #[test]
    fn kind_label_accessor() {
        assert_eq!(
            ProgressKind::Exact {
                total_bytes: 1,
                label: "복구".into()
            }
            .label(),
            "복구"
        );
        assert_eq!(
            ProgressKind::Indeterminate {
                label: "백업".into()
            }
            .label(),
            "백업"
        );
    }
}
