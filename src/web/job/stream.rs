//! 자식 stdout/stderr → SSE 중계 — NDJSON 진행률을 합쳐서, 사람이 읽는 로그는 그대로.
//!
//! [`super::runner::RunningJob`]의 doc이 정한 자리다: 길게 도는 잡은 `take_stdout`/
//! `take_stderr`로 파이프를 꺼내 **독립 태스크에서 끝까지 읽으면서** `wait`를 기다려야
//! 한다(안 그러면 파이프가 차서 자식이 멈춘다). 이 파일이 그 "끝까지 읽는" 태스크의
//! 본체다 — [`relay_stdout`]와 [`relay_stderr`]를 호출부(t12 잡 생명주기)가 각각
//! `tokio::spawn`해 `RunningJob::wait()`와 나란히 돌린다.
//!
//! ## 왜 CLI(`src/cli/progress.rs`)가 아니라 여기서 다시 파싱하는가
//! `ProgressReporter::start_json`이 이미 200ms 간격으로 값이 바뀔 때만 stderr에 찍는다
//! (중복 억제). 그런데 그건 **그 CLI 프로세스 자신의** 정책이고, 이 파일은 신뢰할 수
//! 없는 자식의 출력을 받는 쪽이다 — 버그가 있거나 악의적인 자식이 그 절제를 지키지
//! 않고 초당 수만 줄을 뿜을 수 있다는 전제로 방어를 다시 쌓는다(아래 "배압" 절 참고).
//! 신뢰 경계를 넘어오는 데이터는 상대가 프로토콜을 지킬 거라고 가정하지 않는다.
//!
//! ## 라인 길이 상한 — [`MAX_LINE_BYTES`]
//! 정상적인 진행률 줄(`{"event":"progress","bytes":N,"total":T}`)은 수십~백 바이트,
//! 최종 요약 JSON(t4가 타입화한 `schema` 필드 있는 문서)도 프로파일 몇 개 규모라
//! 넉넉잡아 수십 KB를 넘지 않는다. 1 MiB는 그 정상 범위의 수백~수천 배 여유를 주면서도,
//! 자식이 개행 없이 무한정 바이트를 뿜는 상황(버그로 무한 루프에 빠진 로거, 또는
//! 적대적 입력)에서 이 태스크 하나가 붙잡는 메모리를 실용적 상한(동시 잡 수십 개여도
//! 수십 MB) 안에 묶어 둔다. [`read_capped_line`]은 상한을 넘는 바이트를 **버퍼에 담지
//! 않고** 계속 읽어 넘기기만 하므로(상한 이후 바이트는 저장하지 않는다), 줄이
//! 아무리 길어도 이 태스크의 메모리 사용량은 상한을 넘지 않는다 — 상한을 "이 이상은
//! 못 읽는다"가 아니라 "이 이상은 기억하지 않는다"로 구현했다는 뜻이다(파이프는 계속
//! 비워야 자식이 안 멈춘다는 [`super::runner`]의 제약을 지키기 위해서다).
//!
//! ## 파싱 실패는 스트림을 죽이지 않는다 — 로그로 격하
//! stdout의 한 줄이 progress 이벤트도 최종 요약도 아니면([`classify_stdout_line`]이
//! [`StdoutLine::Unrecognized`]로 판정) 에러로 취급해 중계를 멈추지 않는다. 대신 그
//! 줄을 [`crate::web::sse::JobEvent::Log`]로 격하해 그대로 내보낸다. 근거: 이 계층은
//! 자식이 낸 텍스트의 **형태를 신뢰하지 않는다** — 라이브러리가 실수로 stdout에 뭔가를
//! 찍거나, 잘린 JSON(파이프가 끊기는 시점 등), 향후 버전이 이 빌드가 모르는 새 이벤트
//! 종류를 추가하는 경우까지 전부 이 경로로 들어온다. 어느 경우든 "중계 자체가 죽는 것"이
//! "낯선 줄 하나를 로그로 보여주는 것"보다 훨씬 나쁘다 — 전자는 남은 진행률·요약을 전부
//! 잃고, 후자는 운영자가 화면에서 그 줄을 직접 보고 판단할 수 있다.
//!
//! ## 배압(backpressure) — progress는 합치고, log는 합치지 않는다
//! 진행률은 **최신 값만 의미가 있다**(누적 바이트 수는 단조 증가하고, 화면은 항상
//! "지금 얼마나 됐는가"만 보여주면 된다) — 그래서 [`ProgressCoalescer`]가
//! [`PROGRESS_COALESCE_INTERVAL`](200ms) 안에 들어온 값 중 최신 것 하나만 내보내고
//! 나머지는 버린다. 200ms를 고른 이유는 자의적이지 않다 — `src/cli/progress.rs`의
//! `ProgressReporter`도 화면 갱신을 200ms 간격으로 폴링한다. 그보다 촘촘히 SSE로
//! 내보내 봤자 브라우저가 그릴 수 있는 갱신 빈도의 원천(그 폴링 주기)보다 빠를 수
//! 없으므로 순수한 낭비다. 반대로 **로그 줄은 합치지 않는다** — 각 줄이 서로 다른
//! 정보를 담고 있어(진행률처럼 "최신 값이 이전 값을 대체"하는 성질이 없다) 임의로
//! 버리면 운영자가 못 보는 진단이 생긴다. 로그의 배압 정책은 이 파일이 아니라
//! [`crate::web::sse`]의 `broadcast` 채널이 진다 — 뷰어가 못 따라오면(채널 용량 초과)
//! 오래된 로그가 버려지지만, 그건 "느린 뷰어의 문제"이지 "이 릴레이가 자식을 막는
//! 문제"가 아니다(모듈 헤더 참고: 이 태스크는 파이프를 절대 멈추지 않고 계속 읽는다).
//!
//! ## stderr 적재 이음매 — [`JobLogSink`]
//! stderr는 SSE로 실시간 중계하는 동시에 잡 로그로도 남아야 한다. 그런데 "잡 로그를
//! 어떤 형식으로 어디에 쌓는가"는 이 태스크의 범위 밖이고 t14(`state/jobs.rs`)가
//! 아직 정의하지 않았다 — 그래서 이 파일은 t14를 기다리거나 import하지 않고, 대신
//! "문자열 한 줄을 적재한다"는 최소 계약만 [`JobLogSink`] trait으로 노출한다. t12(잡을
//! 스폰하는 쪽)가 [`relay_stderr`]를 호출할 때 t14의 구현을 `Arc<dyn JobLogSink>`로
//! 감싸 넘기면 연결된다. 지금 이 파일의 테스트는 `Mutex<Vec<String>>` 기반의 더미
//! 구현으로 이 이음매가 실제로 호출되는지만 확인한다.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, BufReader};

use crate::web::sse::{JobEvent, JobHub, LogStream};

/// 한 줄이 이 바이트 수를 넘으면 그 이후는 버퍼에 담지 않고 절단한다.
/// 근거는 모듈 헤더 "라인 길이 상한" 참고.
pub const MAX_LINE_BYTES: usize = 1024 * 1024; // 1 MiB

/// 진행률 합치기 주기. 근거는 모듈 헤더 "배압" 참고 — CLI 자신의 폴링 주기(200ms)와
/// 맞춘다.
pub const PROGRESS_COALESCE_INTERVAL: Duration = Duration::from_millis(200);

// ---------------------------------------------------------------------------
// 길이 상한이 있는 라인 읽기
// ---------------------------------------------------------------------------

/// [`read_capped_line`] 한 번 호출의 결과.
struct CappedLine {
    /// 상한까지만 담긴 바이트(원본이 상한보다 길었으면 뒤가 잘려 있다).
    bytes: Vec<u8>,
    /// 원본이 `bytes.len()`보다 길었는지(= 절단이 실제로 일어났는지).
    truncated: bool,
}

/// `\n` 기준으로 한 줄을 읽되, [`MAX_LINE_BYTES`]를 넘는 바이트는 **버퍼에 담지 않고**
/// 계속 읽어 넘긴다(파이프를 비우는 것이 목적이지, 무한정 담아 두는 것이 목적이
/// 아니다 — 모듈 헤더 참고).
///
/// 반환값:
/// - `Ok(None)` — 더 읽을 데이터가 전혀 없다(직전 호출이 마지막 줄까지 다 소비했다).
/// - `Ok(Some(line))` — 한 줄을 읽었다(개행으로 끝났든, 개행 없이 EOF로 끝났든).
/// - `Err(_)` — 하부 I/O 오류.
///
/// `RD: AsyncBufRead`를 직접 받는다(내부에서 새로 감싸지 않는다) — 호출부가 이미
/// [`BufReader`]로 감싸 둔 것을 여러 번 이 함수에 넘겨 줄 단위로 반복 호출하는
/// 용법이기 때문이다.
async fn read_capped_line<RD>(reader: &mut RD, cap: usize) -> std::io::Result<Option<CappedLine>>
where
    RD: AsyncBufRead + Unpin,
{
    let mut buf: Vec<u8> = Vec::new();
    // 버퍼에 담은 것과 별개로 "실제로 몇 바이트를 봤는지"를 추적해야 절단 여부를
    // 정확히 판정할 수 있다(buf 자체는 cap에서 더 안 자란다).
    let mut total_seen: usize = 0;

    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            // EOF. 지금까지 아무 바이트도 못 봤으면 "더 읽을 게 없다"(None) — 봤으면
            // 개행 없이 끝난 마지막 줄이다.
            return Ok(if total_seen == 0 {
                None
            } else {
                let truncated = total_seen > buf.len();
                Some(CappedLine {
                    bytes: buf,
                    truncated,
                })
            });
        }

        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
            let chunk = &available[..pos];
            total_seen += chunk.len();
            if buf.len() < cap {
                let take = chunk.len().min(cap - buf.len());
                buf.extend_from_slice(&chunk[..take]);
            }
            reader.consume(pos + 1); // 개행까지 함께 소비한다.
            let truncated = total_seen > buf.len();
            return Ok(Some(CappedLine {
                bytes: buf,
                truncated,
            }));
        }

        // 이 청크 안에 개행이 없다 — 전부 소비하고(파이프를 비우고) 다음 청크를 본다.
        let consumed = available.len();
        total_seen += consumed;
        if buf.len() < cap {
            let take = consumed.min(cap - buf.len());
            buf.extend_from_slice(&available[..take]);
        }
        reader.consume(consumed);
    }
}

// ---------------------------------------------------------------------------
// stdout 분류 — progress / summary / 그 외
// ---------------------------------------------------------------------------

/// 파싱된 stdout 한 줄의 분류.
#[derive(Debug)]
enum StdoutLine {
    /// `{"event":"progress","bytes":N,"total":T?}` —
    /// [`crate::cli::progress::progress_json_line`]과 같은 모양.
    Progress { bytes: u64, total: Option<u64> },
    /// 최상위 `schema` 필드를 가진 JSON — 자식이 낸 최종 요약 문서(t4).
    Summary(serde_json::Value),
    /// 위 둘 다 아니다(JSON이 아니거나, JSON이지만 모르는 모양) — 로그로 격하한다.
    Unrecognized,
}

/// stdout 한 줄(개행 제외, 앞뒤 공백 제거됨)을 분류한다. 순수 함수 — I/O 없음.
///
/// ## 파싱 전에 깊이를 먼저 잰다
/// 병리적으로 깊은 줄은 [`crate::web::jsonguard`]가 먼저 걸러
/// [`StdoutLine::Unrecognized`]로 격하한다 — 즉 **로그로는 남고 진행률·요약으로는
/// 해석되지 않는다.**
///
/// 안전 때문은 아니다. `serde_json`이 이미 재귀 상한을 갖고 있어 깊은 입력도 그냥
/// `Err`로 접힌다(그 모듈 헤더). 그러면 이 경로의 결과는 관문이 있든 없든 똑같이
/// `Unrecognized`다. 관문을 두는 이유는 **판정을 한 곳에 모으기 위해서**다 — 이 파일이
/// 깊이를 다루지 않으면 "여기는 왜 예외인가"를 나중에 누군가 다시 따져야 한다.
///
/// 잡을 실패시키지 않는 것은 별개의 판단이다: 줄 하나가 병리적이라고 백업 자체가
/// 잘못됐다는 뜻은 아니고, 진행률 한 줄을 못 읽은 것으로 도는 백업을 끊는 것이 더 나쁘다.
fn classify_stdout_line(line: &str) -> StdoutLine {
    if crate::web::jsonguard::check_depth(line).is_err() {
        return StdoutLine::Unrecognized;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return StdoutLine::Unrecognized;
    };
    if value.get("event").and_then(serde_json::Value::as_str) == Some("progress") {
        return match value.get("bytes").and_then(serde_json::Value::as_u64) {
            Some(bytes) => {
                let total = value.get("total").and_then(serde_json::Value::as_u64);
                StdoutLine::Progress { bytes, total }
            }
            // "event":"progress"인데 필수 필드가 없다 — 계약 위반이지만 죽이지 않는다.
            None => StdoutLine::Unrecognized,
        };
    }
    if value.get("schema").is_some() {
        return StdoutLine::Summary(value);
    }
    StdoutLine::Unrecognized
}

// ---------------------------------------------------------------------------
// 진행률 합치기
// ---------------------------------------------------------------------------

/// 일정 간격 안에 들어온 진행률 중 **최신 값만** 내보내도록 판단하는 상태 기계.
///
/// `std::time::Instant`(실제 벽시계)를 쓴다 — `tokio::time`의 가상 시계
/// (`pause`/`advance`)는 `test-util` feature가 있어야 하는데, 그건 이 태스크가
/// Cargo.toml에 손댈 이유로 삼기엔 과하다(이 파일이 실제로 필요로 하는 것은 axum SSE
/// 지원뿐이고, 그것도 결국 새 feature 없이 해결됐다 — 완료 보고 참고). 대신 테스트는
/// 간격을 짧게(수십 ms) 잡고 그보다 넉넉히 긴 실제 `sleep`으로 경계를 넘긴다 — 값
/// 자체(정확히 몇 ms)를 검증하는 게 아니라 "간격 전엔 보류, 후엔 방출"이라는 정성적
/// 동작만 확인하면 충분하므로 실시간 오차 수십 ms는 결과에 영향을 주지 않는다.
struct ProgressCoalescer {
    interval: Duration,
    last_emitted_at: Option<Instant>,
    pending: Option<(u64, Option<u64>)>,
}

impl ProgressCoalescer {
    fn new(interval: Duration) -> Self {
        Self {
            interval,
            last_emitted_at: None,
            pending: None,
        }
    }

    /// 새 값을 알린다. 지금 내보내야 하면(첫 값이거나 마지막 발행 이후 간격이 지났으면)
    /// `Some`으로 그 값을 돌려주고, 아니면 보류만 하고 `None`을 돌려준다.
    fn offer(&mut self, bytes: u64, total: Option<u64>) -> Option<(u64, Option<u64>)> {
        self.pending = Some((bytes, total));
        let now = Instant::now();
        let due = match self.last_emitted_at {
            None => true,
            Some(last) => now.saturating_duration_since(last) >= self.interval,
        };
        if due {
            self.last_emitted_at = Some(now);
            self.pending.take()
        } else {
            None
        }
    }

    /// 보류 중인 값을 무조건 꺼낸다 — 스트림이 끝나는 시점(EOF·요약 직전)에 마지막
    /// 값이 간격에 걸려 버려지는 것을 막는다.
    fn flush(&mut self) -> Option<(u64, Option<u64>)> {
        self.pending.take()
    }
}

// ---------------------------------------------------------------------------
// stdout 릴레이
// ---------------------------------------------------------------------------

/// [`relay_stdout`]가 끝나고 돌려주는 요약.
///
/// 호출부(t12)가 [`super::runner::JobOutcome`](자식 종료 코드)과 `summary`를 합쳐
/// [`crate::web::sse::JobEvent::Done`]을 만드는 데 쓴다 — `Done`을 언제 내보낼지는
/// "자식이 실제로 끝났는가"(= `wait()`)를 아는 쪽의 결정이라 이 함수 자신은 `Done`을
/// 내보내지 않는다.
#[derive(Debug, Default, Clone)]
pub struct StdoutRelayResult {
    /// 자식이 stdout 끝에 낸 최종 요약 JSON. 자식이 그걸 찍기 전에 죽었으면(크래시·
    /// 시그널) `None`이다. 요약이 두 번 이상 나오면(있어서는 안 되지만) 마지막 것이
    /// 남는다 — 자식의 프로토콜 위반을 우리가 판정하지 않고 "가장 최근 것"을 신뢰한다.
    pub summary: Option<serde_json::Value>,
    /// 길이 상한에서 절단된 줄 수.
    pub truncated_lines: u64,
    /// progress도 summary도 아니어서 로그로 격하된 줄 수.
    pub unrecognized_lines: u64,
}

/// 자식 stdout을 끝까지 읽어 progress는 합쳐서, 요약은 모아서 [`JobHub`]로 중계한다.
///
/// **이 함수는 파이프가 닫힐 때까지(자식이 stdout을 닫을 때까지) 반환하지 않는다.**
/// 호출부는 [`super::runner::RunningJob::wait`]와 나란히 돌려야 한다(모듈 헤더 참고).
pub async fn relay_stdout<R>(stdout: R, hub: Arc<JobHub>) -> StdoutRelayResult
where
    R: AsyncRead + Unpin,
{
    let mut reader = BufReader::new(stdout);
    let mut coalescer = ProgressCoalescer::new(PROGRESS_COALESCE_INTERVAL);
    let mut result = StdoutRelayResult::default();

    loop {
        let capped = match read_capped_line(&mut reader, MAX_LINE_BYTES).await {
            Ok(Some(c)) => c,
            Ok(None) => break,
            Err(e) => {
                hub.publish(JobEvent::Log {
                    stream: LogStream::Internal,
                    line: format!("stdout 읽기 오류(중계를 멈춥니다): {e}"),
                });
                break;
            }
        };

        if capped.truncated {
            result.truncated_lines += 1;
            hub.publish(JobEvent::Log {
                stream: LogStream::Internal,
                line: format!(
                    "stdout 한 줄이 {MAX_LINE_BYTES} 바이트 상한에서 잘렸습니다(개행 없이 더 \
                     길게 이어진 줄로 추정됩니다)"
                ),
            });
            // 잘린 줄은 온전한 JSON일 수 없다 — 파싱을 시도하지 않는다.
            continue;
        }

        let line = String::from_utf8_lossy(&capped.bytes).into_owned();
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        match classify_stdout_line(trimmed) {
            StdoutLine::Progress { bytes, total } => {
                if let Some((bytes, total)) = coalescer.offer(bytes, total) {
                    hub.publish(JobEvent::Progress { bytes, total });
                }
            }
            StdoutLine::Summary(value) => {
                // 요약 직전의 마지막 진행률을 흘려보낸다 — 100%를 못 보고 요약만 뜨면
                // 화면이 "언제 끝난 진행률인지" 끊긴 인상을 준다.
                if let Some((bytes, total)) = coalescer.flush() {
                    hub.publish(JobEvent::Progress { bytes, total });
                }
                result.summary = Some(value);
            }
            StdoutLine::Unrecognized => {
                result.unrecognized_lines += 1;
                hub.publish(JobEvent::Log {
                    stream: LogStream::Stdout,
                    line: trimmed.to_string(),
                });
            }
        }
    }

    // 마지막 progress가 간격에 걸려 아직 안 나갔을 수 있다 — 스트림 종료 시 유실 방지.
    if let Some((bytes, total)) = coalescer.flush() {
        hub.publish(JobEvent::Progress { bytes, total });
    }

    result
}

// ---------------------------------------------------------------------------
// stderr 릴레이 + 로그 적재 이음매
// ---------------------------------------------------------------------------

/// stderr 한 줄을 잡 로그로 적재할 대상.
///
/// **구현은 이 파일의 책임이 아니다.** 파일 저장 스키마는 t14(`state/jobs.rs`)가
/// 정하고, 이 trait은 그 스키마를 몰라도 되게 하는 최소 계약("문자열 한 줄을 받아
/// 어딘가에 쌓는다")만 표현한다(모듈 헤더 "stderr 적재 이음매" 참고).
pub trait JobLogSink: Send + Sync {
    /// 이미 마스킹까지 끝난(SSE로 나가는 것과 같은 기준의) 한 줄을 적재한다.
    /// [`relay_stderr`]가 [`JobHub::mask`]를 거친 문자열만 넘기므로 구현체가 다시
    /// 마스킹할 필요는 없다.
    fn append(&self, line: &str);
}

/// 자식 stderr를 끝까지 읽어 [`JobHub`]로 중계하고, 있으면 [`JobLogSink`]에도 적재한다.
///
/// [`relay_stdout`]과 마찬가지로 파이프가 닫힐 때까지 반환하지 않는다. stdout과 달리
/// stderr는 파싱하지 않는다 — 전부 사람이 읽는 진단(`tracing` 로그)이므로 한 줄 = 로그
/// 한 줄이다.
pub async fn relay_stderr<R>(stderr: R, hub: Arc<JobHub>, sink: Option<Arc<dyn JobLogSink>>)
where
    R: AsyncRead + Unpin,
{
    let mut reader = BufReader::new(stderr);
    loop {
        let capped = match read_capped_line(&mut reader, MAX_LINE_BYTES).await {
            Ok(Some(c)) => c,
            Ok(None) => break,
            Err(e) => {
                publish_and_store(
                    &hub,
                    sink.as_deref(),
                    LogStream::Internal,
                    format!("stderr 읽기 오류(중계를 멈춥니다): {e}"),
                );
                break;
            }
        };

        if capped.truncated {
            publish_and_store(
                &hub,
                sink.as_deref(),
                LogStream::Internal,
                format!(
                    "stderr 한 줄이 {MAX_LINE_BYTES} 바이트 상한에서 잘렸습니다(개행 없이 더 \
                     길게 이어진 줄로 추정됩니다)"
                ),
            );
            continue;
        }

        let line = String::from_utf8_lossy(&capped.bytes).into_owned();
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        publish_and_store(
            &hub,
            sink.as_deref(),
            LogStream::Stderr,
            trimmed.to_string(),
        );
    }
}

/// SSE로 내보내고(마스킹은 [`JobHub::publish`]가 구조적으로 강제한다), 적재소가 있으면
/// 같은 마스킹 기준으로 만든 문자열을 그쪽에도 넣는다 — 두 목적지가 서로 다른 내용을
/// 갖지 않게 한 곳에서 처리한다.
fn publish_and_store(hub: &JobHub, sink: Option<&dyn JobLogSink>, stream: LogStream, line: String) {
    if let Some(sink) = sink {
        sink.append(&hub.mask(&line));
    }
    hub.publish(JobEvent::Log { stream, line });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::mask::SecretRegistry;
    use futures::stream::StreamExt;
    use std::sync::Mutex;
    use tokio::io::AsyncWriteExt;

    // ---- ProgressCoalescer(화이트박스) ----

    /// 첫 값은 즉시 내보낸다(간격을 기다리지 않는다) — 뷰어가 잡 시작 직후 아무것도
    /// 못 보고 간격만큼 기다리게 하면 안 된다.
    #[tokio::test]
    async fn coalescer_emits_first_value_immediately() {
        let mut c = ProgressCoalescer::new(Duration::from_millis(200));
        assert_eq!(c.offer(10, Some(100)), Some((10, Some(100))));
    }

    /// 간격 안에 들어온 후속 값은 보류되고, 간격이 지나면 **가장 최근** 값이 나간다.
    ///
    /// 실제 시계를 쓰므로 간격을 짧게(20ms) 잡고 그 5배(100ms)를 실제로 재운다 —
    /// 정확한 타이밍이 아니라 "간격 전엔 보류, 후엔 방출"이라는 동작만 확인한다
    /// ([`ProgressCoalescer`] doc 참고).
    #[tokio::test]
    async fn coalescer_suppresses_within_interval_and_emits_latest_after() {
        let mut c = ProgressCoalescer::new(Duration::from_millis(20));
        assert!(c.offer(10, Some(100)).is_some());
        assert_eq!(c.offer(20, Some(100)), None, "간격 안이라 보류돼야 함");
        assert_eq!(c.offer(30, Some(100)), None, "이것도 간격 안");

        tokio::time::sleep(Duration::from_millis(100)).await;

        // 간격이 지난 뒤 새 값을 알리면 그 값이 바로 나간다(가장 최근 값).
        assert_eq!(c.offer(40, Some(100)), Some((40, Some(100))));
    }

    /// 보류 중인 값은 `flush`로 강제로 꺼낼 수 있다(스트림 종료 시 유실 방지).
    #[tokio::test]
    async fn coalescer_flush_returns_pending_value() {
        let mut c = ProgressCoalescer::new(Duration::from_millis(200));
        c.offer(10, None);
        assert_eq!(c.offer(20, None), None, "보류 중");
        assert_eq!(c.flush(), Some((20, None)), "보류값이 flush로 나와야 함");
        assert_eq!(c.flush(), None, "이미 비었으면 다시 꺼낼 게 없음");
    }

    // ---- classify_stdout_line ----

    #[test]
    fn classifies_progress_summary_and_unrecognized() {
        assert!(matches!(
            classify_stdout_line(r#"{"event":"progress","bytes":10,"total":100}"#),
            StdoutLine::Progress {
                bytes: 10,
                total: Some(100)
            }
        ));
        assert!(matches!(
            classify_stdout_line(r#"{"event":"progress","bytes":10}"#),
            StdoutLine::Progress {
                bytes: 10,
                total: None
            }
        ));
        assert!(matches!(
            classify_stdout_line(r#"{"schema":1,"backup_id":"x"}"#),
            StdoutLine::Summary(_)
        ));
        for weird in [
            "not json",
            r#"{"event":"progress"}"#, // bytes 없음 — 계약 위반
            r#"{"hello":"world"}"#,    // schema도 event도 없음
            "",
        ] {
            assert!(
                matches!(classify_stdout_line(weird), StdoutLine::Unrecognized),
                "'{weird}'가 Unrecognized가 아니다"
            );
        }
    }

    /// 병리적으로 깊은 줄은 `serde_json`에 닿기 전에 격하된다 — 관문을 거치든
    /// `serde_json`이 거부하든 결과는 `Unrecognized`로 같아야 한다는 고정.
    #[test]
    fn pathologically_deep_line_is_demoted_not_parsed() {
        let bomb = "[".repeat(50_000);
        assert!(
            matches!(classify_stdout_line(&bomb), StdoutLine::Unrecognized),
            "깊은 줄이 파싱 경로로 들어갔다"
        );

        // 상한 안쪽의 중첩된 요약은 여전히 요약으로 인식된다.
        let nested = format!(
            r#"{{"schema":1,"backup_id":"x","deep":{}{}}}"#,
            "[".repeat(60),
            "]".repeat(60)
        );
        assert!(
            matches!(classify_stdout_line(&nested), StdoutLine::Summary(_)),
            "상한 안쪽 요약이 격하됐다"
        );
    }

    // ---- read_capped_line(경계 조건) ----

    /// duplex 파이프로 stdout 흉내를 낸다 — 실제 프로세스 없이 백프레셔가 있는 비동기
    /// 스트림을 만들 수 있다(작은 버퍼로 여러 청크에 걸친 읽기도 자연히 재현된다).
    fn fake_pipe(buffer: usize) -> (tokio::io::DuplexStream, tokio::io::DuplexStream) {
        tokio::io::duplex(buffer)
    }

    #[tokio::test]
    async fn read_capped_line_returns_none_on_clean_eof() {
        let (writer, reader) = fake_pipe(64);
        drop(writer); // 즉시 EOF, 아무 데이터도 없음.
        let mut reader = BufReader::new(reader);
        assert!(read_capped_line(&mut reader, 100).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn read_capped_line_reads_exact_cap_without_truncation() {
        let (mut writer, reader) = fake_pipe(1024);
        let write = tokio::spawn(async move {
            writer.write_all(b"12345678\n").await.unwrap(); // 정확히 8바이트
        });
        let mut reader = BufReader::new(reader);
        let line = read_capped_line(&mut reader, 8).await.unwrap().unwrap();
        write.await.unwrap();
        assert_eq!(line.bytes, b"12345678");
        assert!(!line.truncated, "상한과 정확히 같은 길이는 절단이 아니다");
    }

    #[tokio::test]
    async fn read_capped_line_truncates_and_stream_survives_for_next_line() {
        // 작은 버퍼(4바이트)로 강제 분할 읽기를 유도하면서, cap=8을 넘는 줄 뒤에
        // 정상 줄이 이어져도 다음 줄을 온전히 읽는지 확인한다.
        let (mut writer, reader) = fake_pipe(4);
        let write = tokio::spawn(async move {
            writer.write_all(b"0123456789ABCDEF\n").await.unwrap(); // 16바이트, cap=8
            writer.write_all(b"next\n").await.unwrap();
        });
        let mut reader = BufReader::new(reader);

        let first = read_capped_line(&mut reader, 8).await.unwrap().unwrap();
        assert_eq!(first.bytes, b"01234567", "상한까지만 담겨야 함");
        assert!(first.truncated);

        let second = read_capped_line(&mut reader, 8).await.unwrap().unwrap();
        assert_eq!(second.bytes, b"next");
        assert!(
            !second.truncated,
            "다음 줄은 절단되지 않아야 함(스트림 생존)"
        );

        write.await.unwrap();
    }

    #[tokio::test]
    async fn read_capped_line_handles_trailing_line_without_newline() {
        let (mut writer, reader) = fake_pipe(64);
        let write = tokio::spawn(async move {
            writer.write_all(b"no-trailing-newline").await.unwrap();
            // writer가 drop되며 EOF.
        });
        let mut reader = BufReader::new(reader);
        let line = read_capped_line(&mut reader, 100).await.unwrap().unwrap();
        write.await.unwrap();
        assert_eq!(line.bytes, b"no-trailing-newline");
        assert!(!line.truncated);
        assert!(read_capped_line(&mut reader, 100).await.unwrap().is_none());
    }

    // ---- relay_stdout(통합) ----

    /// relay 하나를 백프레셔가 있는 duplex 파이프로 돌리고, 결과와 수신된 이벤트를
    /// 함께 돌려준다. write와 relay를 **동시에** 스폰해야 한다 — duplex 버퍼가
    /// 작으면(여기서는 64KB) 다 쓰기 전에 리더가 비워 줘야 write가 끝난다.
    async fn run_relay_stdout(input: Vec<u8>) -> (StdoutRelayResult, Vec<JobEvent>) {
        let (mut writer, reader) = fake_pipe(64 * 1024);
        let hub = Arc::new(JobHub::new(SecretRegistry::new()));
        let mut events = Box::pin(hub.subscribe());

        let write_task = tokio::spawn(async move {
            writer.write_all(&input).await.expect("write 실패");
        });
        let relay_hub = Arc::clone(&hub);
        let relay_task = tokio::spawn(relay_stdout(reader, relay_hub));

        let (write_res, relay_res) = tokio::join!(write_task, relay_task);
        write_res.expect("write 태스크가 패닉했다");
        let result = relay_res.expect("relay 태스크가 패닉했다");

        let mut collected = Vec::new();
        while let Ok(Some(event)) =
            tokio::time::timeout(Duration::from_millis(200), events.next()).await
        {
            collected.push(event);
        }
        (result, collected)
    }

    /// 정상 NDJSON 스트림: progress 값들이 순서대로 이어지고(합쳐졌더라도 순서가
    /// 뒤바뀌지 않는다), 마지막 값이 유실 없이 나오며, 요약이 캡처된다.
    #[tokio::test]
    async fn ndjson_progress_then_summary_is_relayed_in_order() {
        let input = concat!(
            r#"{"event":"progress","bytes":10,"total":100}"#,
            "\n",
            r#"{"event":"progress","bytes":55,"total":100}"#,
            "\n",
            r#"{"event":"progress","bytes":100,"total":100}"#,
            "\n",
            r#"{"schema":1,"backup_id":"abc","stored_size_bytes":100}"#,
            "\n",
        )
        .as_bytes()
        .to_vec();

        let (result, events) = run_relay_stdout(input).await;

        let progress_bytes: Vec<u64> = events
            .iter()
            .filter_map(|e| match e {
                JobEvent::Progress { bytes, .. } => Some(*bytes),
                _ => None,
            })
            .collect();
        assert!(!progress_bytes.is_empty(), "progress 이벤트가 하나도 없다");
        // 합쳐지더라도(코얼레싱) 받은 값들은 오름차순이어야 한다 — 순서가 안 뒤집힌다.
        assert!(
            progress_bytes.windows(2).all(|w| w[0] <= w[1]),
            "progress 순서가 뒤집혔다: {progress_bytes:?}"
        );
        // 마지막 값(100)은 유실되지 않는다 — summary 직전 flush가 보장한다.
        assert_eq!(*progress_bytes.last().unwrap(), 100);

        assert_eq!(
            result
                .summary
                .as_ref()
                .and_then(|s| s.get("backup_id"))
                .and_then(|v| v.as_str()),
            Some("abc")
        );
        assert_eq!(result.truncated_lines, 0);
        assert_eq!(result.unrecognized_lines, 0);
    }

    /// 개행 없는 8MB 줄이 상한에서 절단되고, 그 뒤에 이어지는 정상 줄은 온전히
    /// 처리된다(스트림이 죽지 않는다).
    #[tokio::test]
    async fn oversized_line_is_truncated_and_stream_stays_alive() {
        let mut input = vec![b'a'; 8 * 1024 * 1024]; // 8 MiB, 개행 없음
        input.push(b'\n');
        input.extend_from_slice(br#"{"event":"progress","bytes":1,"total":2}"#);
        input.push(b'\n');

        let (result, events) = run_relay_stdout(input).await;

        assert_eq!(
            result.truncated_lines, 1,
            "절단이 정확히 한 번 카운트돼야 함"
        );
        assert!(
            events.iter().any(
                |e| matches!(e, JobEvent::Log { stream: LogStream::Internal, line } if line.contains("잘렸습니다"))
            ),
            "절단 알림 로그가 없다"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, JobEvent::Progress { bytes: 1, .. })),
            "절단된 줄 뒤의 정상 progress가 처리되지 않았다: {events:?}"
        );
    }

    /// 잘린 JSON·비-NDJSON 텍스트·비-UTF8 바이트가 섞여도 죽지 않고, 각각 로그로
    /// 격하되며, 그 뒤의 정상 요약은 그대로 캡처된다.
    #[tokio::test]
    async fn malformed_and_non_utf8_input_does_not_kill_the_relay() {
        let mut input = Vec::new();
        input.extend_from_slice(br#"{"schema":1,"#); // 잘린 JSON
        input.push(b'\n');
        input.extend_from_slice(b"just some plain log text, not json at all");
        input.push(b'\n');
        input.extend_from_slice(&[0xFF, 0xFE, 0x00, b'x']); // 비-UTF8 바이트
        input.push(b'\n');
        input.extend_from_slice(br#"{"schema":1,"backup_id":"survived"}"#);
        input.push(b'\n');

        let (result, events) = run_relay_stdout(input).await;

        assert_eq!(result.unrecognized_lines, 3, "세 줄 모두 격하돼야 함");
        assert_eq!(
            result
                .summary
                .as_ref()
                .and_then(|s| s.get("backup_id"))
                .and_then(|v| v.as_str()),
            Some("survived"),
            "적대적 줄들 이후에도 진짜 요약은 캡처돼야 함"
        );
        let log_count = events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    JobEvent::Log {
                        stream: LogStream::Stdout,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(log_count, 3);
    }

    /// 초당 수만 줄이 몰려도(합치기가 없다면 SSE가 그 수만큼 프레임을 냈을 상황)
    /// 상한 시간 안에 끝나고, 실제로 나가는 progress 프레임 수는 합치기 덕분에
    /// 훨씬 적다.
    #[tokio::test]
    async fn burst_of_many_progress_lines_is_coalesced_into_few_frames() {
        const LINES: u64 = 50_000;
        let mut input = String::new();
        for i in 0..LINES {
            input.push_str(&format!(
                r#"{{"event":"progress","bytes":{i},"total":{LINES}}}"#
            ));
            input.push('\n');
        }

        let (result, events) = tokio::time::timeout(
            Duration::from_secs(10),
            run_relay_stdout(input.into_bytes()),
        )
        .await
        .expect("10초 안에 끝나야 한다(배압 없이 자식을 막지 않는다는 증거)");

        let progress_count = events
            .iter()
            .filter(|e| matches!(e, JobEvent::Progress { .. }))
            .count();
        assert!(
            (progress_count as u64) < LINES / 100,
            "합치기가 동작하지 않았다 — {LINES}줄에 {progress_count}개 프레임"
        );
        assert!(progress_count >= 1, "최소 하나는 나가야 한다");
        assert_eq!(result.unrecognized_lines, 0);
        // 마지막 값(LINES-1)이 flush로 유실 없이 나온다.
        let last = events
            .iter()
            .filter_map(|e| match e {
                JobEvent::Progress { bytes, .. } => Some(*bytes),
                _ => None,
            })
            .next_back()
            .unwrap();
        assert_eq!(last, LINES - 1, "마지막 진행률 값이 유실됐다");
    }

    // ---- relay_stderr + JobLogSink ----

    /// 테스트용 더미 적재소 — 호출된 줄을 그대로 모은다.
    struct CollectingSink {
        lines: Mutex<Vec<String>>,
    }
    impl CollectingSink {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                lines: Mutex::new(Vec::new()),
            })
        }
        fn snapshot(&self) -> Vec<String> {
            self.lines.lock().unwrap().clone()
        }
    }
    impl JobLogSink for CollectingSink {
        fn append(&self, line: &str) {
            self.lines.lock().unwrap().push(line.to_string());
        }
    }

    /// stderr에 심어둔 시크릿이 SSE로 나가는 `Log` 이벤트와 [`JobLogSink`] 적재본
    /// **양쪽 모두**에서 마스킹된다.
    #[tokio::test]
    async fn stderr_secret_is_masked_in_both_sse_and_log_sink() {
        const SECRET: &str = "sup3rSecretPassw0rdXYZ";
        let mut registry = SecretRegistry::new();
        assert!(registry.register(SECRET));
        let hub = Arc::new(JobHub::new(registry));
        let mut events = Box::pin(hub.subscribe());
        let sink = CollectingSink::new();

        let (mut writer, reader) = fake_pipe(4096);
        let write_task = tokio::spawn(async move {
            writer
                .write_all(format!("auth failed: {SECRET}\n").as_bytes())
                .await
                .unwrap();
        });
        let relay_hub = Arc::clone(&hub);
        let sink_dyn: Arc<dyn JobLogSink> = sink.clone();
        let relay_task = tokio::spawn(relay_stderr(reader, relay_hub, Some(sink_dyn)));
        let (w, r) = tokio::join!(write_task, relay_task);
        w.unwrap();
        r.unwrap();

        let event = tokio::time::timeout(Duration::from_secs(1), events.next())
            .await
            .expect("타임아웃")
            .expect("이벤트가 없다");
        let JobEvent::Log { line, .. } = event else {
            panic!("Log 이벤트가 아니다");
        };
        assert!(
            !line.contains(SECRET),
            "SSE 프레임에 시크릿 원문이 남았다: {line}"
        );
        assert!(line.contains(crate::web::mask::REDACTED_PLACEHOLDER));

        let stored = sink.snapshot();
        assert_eq!(stored.len(), 1);
        assert!(
            !stored[0].contains(SECRET),
            "적재된 로그에 시크릿 원문이 남았다: {}",
            stored[0]
        );
        assert!(stored[0].contains(crate::web::mask::REDACTED_PLACEHOLDER));
    }

    /// 적재소가 없어도(`None`) relay_stderr는 패닉 없이 SSE 중계만 수행한다.
    #[tokio::test]
    async fn relay_stderr_without_sink_still_relays_to_sse() {
        let hub = Arc::new(JobHub::new(SecretRegistry::new()));
        let mut events = Box::pin(hub.subscribe());
        let (mut writer, reader) = fake_pipe(4096);
        let write_task = tokio::spawn(async move {
            writer.write_all(b"hello from stderr\n").await.unwrap();
        });
        let relay_hub = Arc::clone(&hub);
        let relay_task = tokio::spawn(relay_stderr(reader, relay_hub, None));
        let (w, r) = tokio::join!(write_task, relay_task);
        w.unwrap();
        r.unwrap();

        let event = tokio::time::timeout(Duration::from_secs(1), events.next())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(event, JobEvent::Log { line, .. } if line == "hello from stderr"));
    }
}
