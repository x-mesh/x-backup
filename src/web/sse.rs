//! SSE(Server-Sent Events) 허브 — 잡 하나의 이벤트를 여러 뷰어에게 팬아웃한다.
//!
//! ## 이 파일이 아는 것 / 모르는 것
//! [`JobHub`]는 "잡 이벤트를 팬아웃한다"만 안다. 잡이 무엇인지, 어느 프로파일인지,
//! 잡 id로 허브를 어떻게 찾는지는 **모른다** — 그건 잡 레지스트리(t14 `state/jobs.rs`,
//! `routes/jobs.rs`)의 책임이다. 이 분리 덕분에 이 파일은 t14를 import하지 않고도
//! 완결된 단위로 테스트된다. 실제 배선(잡 id → `Arc<JobHub>` 조회 → 이 파일의
//! [`sse_response`] 호출)은 리더가 `server.rs`/`routes/jobs.rs`에서 완성한다.
//!
//! ## 뷰어 0명이어도 잡은 계속 돈다 — 팬아웃은 `broadcast` 채널
//! [`tokio::sync::broadcast`]는 구독자가 없어도 `send()`가 실패로 죽지 않는다(단지
//! 아무도 못 받을 뿐). 그래서 잡 실행([`super::job::stream`])과 뷰어의 존재 여부가
//! 완전히 분리된다 — 운영자가 브라우저 탭을 닫아도 백업은 멈추지 않고, 진행률은 허브
//! 안에서 계속 버려질 뿐이다(다음 섹션이 "버려짐"의 예외를 설명한다).
//!
//! ## 나중에 붙은 뷰어를 위한 스냅샷
//! `broadcast` 채널은 구독 **이후**의 메시지만 전달한다 — 운영자가 잡이 시작되고 한참
//! 뒤에 화면을 열면 그 시점까지의 진행률·상태를 전혀 못 본 채 다음 이벤트까지 빈
//! 화면을 봐야 한다. 그래서 [`JobHub`]는 가장 최근의 `progress`/`state`/`done` 이벤트를
//! 각각 하나씩만 별도로 보관하고, [`JobHub::subscribe`]가 그 스냅샷을 구독 스트림의
//! **맨 앞에 먼저 흘려보낸 뒤** 실시간 브로드캐스트로 이어 붙인다. `log`는 스냅샷에
//! 넣지 않는다 — 로그는 한 줄 한 줄이 서로 다른 정보라 "최신 하나"라는 개념이 없다
//! (놓친 로그를 보여주려면 전체 이력을 들고 있어야 하는데, 그건 잡 로그 파일(t14)의
//! 역할이지 이 휘발성 허브의 역할이 아니다).
//!
//! ## 시크릿 마스킹은 [`JobHub::publish`] 안에서 강제된다
//! 자식 stderr에 무엇이 찍힐지, stdout 요약 JSON에 무엇이 실릴지 이 프로세스는 통제할 수
//! 없다([`crate::web::mask`] 헤더 참고). 이벤트를 만드는 호출부(`super::job::stream`·
//! `super::routes::backup`)가 마스킹을 깜빡해도 SSE로 나가는 프레임이 새지 않도록, 마스킹을
//! **호출부 재량이 아니라 이 허브의 구조**로 못박는다 — `publish`는 브로드캐스트하기(그리고
//! 스냅샷에 담기) **직전에** 자신이 쥔 [`SecretRegistry`]로 이벤트를 한 번 더 통과시킨다
//! ([`JobHub::redact`]). 호출부가 이미 마스킹했더라도 이중 마스킹은 안전하다(치환 결과물인
//! `[REDACTED]`에는 등록된 시크릿 부분 문자열이 없다).
//!
//! **이 주장은 한때 사실이 아니었다.** 예전 `publish`는 `Log` variant만 골라 마스킹했고,
//! `Done.summary`(자식 stdout 원문 JSON)는 그대로 프레임으로 나갔다 — 헤더는 "모든 프레임"을
//! 주장하고 `tests/web_secret_leak.rs`는 SSE 커버리지를 그 주장에 위임했지만, 이 파일 테스트는
//! 전부 `summary: None`이어서 그 필드를 한 번도 검사하지 않았다. 지금은 [`JobHub::redact`]가
//! **와일드카드 없는 exhaustive match**로 모든 variant를 분해해 다시 조립한다 — variant나
//! 필드가 추가되면 컴파일이 깨지므로, 새 필드가 조용히 마스킹에서 빠지는 경로가 문법 수준에서
//! 없다. 그 함수 doc이 "어떤 필드를 왜 지우고 무엇은 왜 그대로 두는가"의 기준을 들고 있다.
//!
//! ## 왜 `broadcast`이고 뒤처진 뷰어는 어떻게 되는가
//! 채널 용량([`CHANNEL_CAPACITY`])을 넘어서도록 뒤처진 구독자는 오래된 이벤트를
//! 통째로 건너뛴다([`tokio::sync::broadcast::error::RecvError::Lagged`]). 이걸 막으려고
//! "느린 뷰어를 기다리는" 진짜 배압을 걸면, 뷰어 한 명의 브라우저 탭이 자식의 stdout/
//! stderr 파이프를 비우는 태스크까지 거슬러 올라가 막을 수 있다 — 그러면 뷰어 하나가
//! 잡 자체를 멈추는 사고가 된다([`super::job::stream`] 헤더의 "파이프를 비우지 않으면
//! 자식이 멈춘다"와 같은 위험). 그래서 이 허브는 **뷰어를 절대 기다리지 않는다**:
//! 느린 뷰어는 이벤트를 잃을 뿐이고(`log` 몇 줄을 못 볼 수 있다), 잡 실행에는 영향이
//! 없다. 완전한 이력이 필요하면 잡 로그 파일(t14)을 보라 — 이 허브는 "지금 보고 있는
//! 사람에게 실시간으로"가 계약이지 "언젠가 접속한 모든 사람에게 빠짐없이"가 아니다.

use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use axum::response::sse::{Event, KeepAlive, Sse};
use futures::stream::{self, Stream, StreamExt};
use serde::Serialize;
use tokio::sync::broadcast;

use crate::web::mask::SecretRegistry;
use crate::web::view::components::Level;

/// 브로드캐스트 채널 용량(이벤트 개수).
///
/// [`super::job::stream`]의 진행률 합치기 주기(200ms)를 그대로 따르면 진행률은 초당
/// 최대 5개, 로그는 자식이 몰아 찍는 순간(사전 점검 실패 등 여러 줄을 한꺼번에 내는
/// 경우)에 수십 줄이 튈 수 있다. 256은 그런 순간적인 몰림을 뷰어가 한 폴링 주기
/// 안에서 소화할 수 있는 여유이지, "이 이상은 절대 안 온다"는 상한이 아니다 — 더
/// 뒤처지면 위 모듈 헤더대로 오래된 이벤트가 버려질 뿐 채널이 죽지는 않는다.
const CHANNEL_CAPACITY: usize = 256;

/// 로그 한 줄의 출처 — 화면이 "이게 자식이 낸 텍스트인지, 우리 중계 계층이 만든
/// 안내인지"를 구분해 다르게 표시할 수 있게 한다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogStream {
    /// 자식 stderr — 사람이 읽는 진단(`tracing` 로그, 경고).
    Stderr,
    /// 자식 stdout인데 NDJSON(progress/summary)으로 파싱되지 않은 줄. 정상 프로토콜
    /// 밖의 텍스트이므로 화면에는 보이되 "예상 밖"이라는 신호로 구분해 둔다.
    Stdout,
    /// 이 중계 계층(`super::job::stream`) 자신이 만든 메시지(라인 절단 알림 등) —
    /// 자식이 낸 텍스트가 아니다.
    Internal,
}

/// 한 잡에 대해 SSE로 내보내는 이벤트 4종.
///
/// `#[serde(tag = "event")]`로 JSON 본문에도 종류를 싣는다 — SSE 프레임의 `event:`
/// 필드([`to_sse_event`]가 채운다)와 별개로, 본문만 보고 파싱하는 클라이언트(예:
/// `EventSource.onmessage`로 전부 받아 자기가 분기하는 코드)도 종류를 알 수 있게
/// 하기 위해서다. 두 표현이 같은 값을 담아 중복이지만, 소비자 구현 방식에 대한
/// 가정을 하나 줄이는 쪽을 택했다.
///
/// ## 레벨을 왜 이벤트에 싣는가
/// `state`/`done`은 화면의 상태 배지를 바꾸는 이벤트다. 그 배지의 색을 **받는 쪽(브라우저
/// 스크립트)이 정하게 두면** 서버 렌더와 갈라진다 — 실제로 갈라졌고, 종료 시 무조건
/// `warn`을 박아 **exit 0 성공이 경고 배지로 떴다**([`super::view::backup`] 헤더 "상태
/// 배지의 레벨은 서버가 정한다"). 결과→레벨 매핑은 이 크레이트에서
/// [`super::job::exit::level_for_label`] 하나뿐이어야 하므로, 그 값을 서버가 계산해 이벤트에
/// 실어 보내고 스크립트는 `data-level`에 반영만 한다. [`Level`]은 [`Level::token`]으로
/// 직렬화되므로(그 타입 doc) JSON에 나가는 문자열도 `data-level`과 같은 어휘다.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum JobEvent {
    /// 진행률 갱신. `total`은 자식이 총량을 모르면(`Indeterminate`) 없다
    /// ([`crate::cli::progress::progress_json_line`]과 같은 모양).
    ///
    /// 여러 개가 빠르게 들어와도 이 허브에 도달하는 시점에는 이미
    /// [`super::job::stream`]의 합치기를 거친 뒤다 — 이 허브 자신은 합치지 않는다
    /// (합치기는 "무엇을 보낼지" 판단이고 자식 출력을 읽는 쪽이 결정할 정보를 더
    /// 갖고 있다).
    Progress {
        bytes: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        total: Option<u64>,
    },
    /// 사람이 읽는 로그 한 줄. `line`은 [`JobHub::publish`]가 반드시 마스킹한
    /// 뒤에만 내보낸다(모듈 헤더 "시크릿 마스킹" 참고) — 이 variant를 만드는
    /// 시점에는 아직 원문일 수 있다.
    Log { stream: LogStream, line: String },
    /// 잡 생명주기 상태 전이. 어휘(`"running"`/`"cancelling"` 등)는 호출부가 정한다
    /// — 상태 기계 자체는 잡 생명주기(t12)의 도메인이고, 이 허브는 문자열을 그대로
    /// 실어 나를 뿐 의미를 해석하지 않는다.
    State {
        state: String,
        /// 이 상태를 화면에서 어떤 `data-level`로 보여줄지 — **서버가 정한다**(아래
        /// "레벨을 왜 이벤트에 싣는가" 참고).
        level: Level,
    },
    /// 잡 종료. `exit_code`가 `None`이면 시그널 종료([`super::job::runner::JobOutcome`]
    /// 참고). `summary`는 자식이 stdout 맨 끝에 낸 최종 요약 JSON(`schema` 필드를 가진
    /// 문서) — 자식이 그걸 찍기 전에 죽었으면 `None`이다.
    Done {
        outcome: String,
        exit_code: Option<i32>,
        /// 이 결과의 화면 레벨 — 서버가 [`super::job::exit::level_for_label`]로 계산해
        /// 싣는다(아래 참고).
        level: Level,
        #[serde(skip_serializing_if = "Option::is_none")]
        summary: Option<serde_json::Value>,
    },
}

impl JobEvent {
    /// SSE 프레임의 `event:` 필드에 쓸 이름. `#[serde(tag = "event")]`가 만드는 JSON
    /// 키 이름과 반드시 같은 어휘여야 하므로(본문/프레임 이중 표기가 어긋나면 소비자가
    /// 헷갈린다), 아래 `kind_matches_serialized_tag` 테스트가 둘을 묶어 둔다.
    fn kind(&self) -> &'static str {
        match self {
            JobEvent::Progress { .. } => "progress",
            JobEvent::Log { .. } => "log",
            JobEvent::State { .. } => "state",
            JobEvent::Done { .. } => "done",
        }
    }
}

/// 가장 최근 `progress`/`state`/`done` 이벤트 스냅샷 — 나중에 붙는 뷰어에게 재생한다
/// (모듈 헤더 참고). `log`는 성질이 달라 여기 없다.
#[derive(Default)]
struct Snapshot {
    state: Option<JobEvent>,
    progress: Option<JobEvent>,
    done: Option<JobEvent>,
}

/// 한 잡의 이벤트를 여러 뷰어에게 팬아웃하는 허브.
///
/// 잡 하나마다 하나씩 만들어(t12/t14가 잡을 시작할 때) `Arc`로 감싸 공유한다 —
/// [`super::job::stream`]의 릴레이 태스크(publish하는 쪽)와 여러 SSE 핸들러(subscribe하는
/// 쪽)가 동시에 같은 값을 들고 있어야 하기 때문이다.
pub struct JobHub {
    tx: broadcast::Sender<JobEvent>,
    snapshot: Mutex<Snapshot>,
    secrets: SecretRegistry,
}

impl JobHub {
    /// 새 허브를 만든다. `secrets`는 이 잡의 자식에게 주입한 시크릿 레지스트리
    /// ([`crate::web::job::JobRunner::secret_registry`]) — [`publish`](Self::publish)가
    /// `Log` 이벤트를 내보내기 전에 이걸로 마스킹한다.
    pub fn new(secrets: SecretRegistry) -> Self {
        let (tx, _rx) = broadcast::channel(CHANNEL_CAPACITY);
        Self {
            tx,
            snapshot: Mutex::new(Snapshot::default()),
            secrets,
        }
    }

    /// 등록된 시크릿을 기준으로 텍스트를 마스킹한다. `publish`가 `Log` 이벤트에
    /// 자동으로 적용하는 것과 같은 레지스트리를 쓴다 — 잡 로그 파일(t14)처럼 SSE가
    /// 아닌 다른 목적지에도 "같은 기준으로 지운 텍스트"가 필요할 때 이 함수를 쓴다
    /// ([`super::job::stream`]의 `JobLogSink` 연동이 그 예다).
    pub fn mask(&self, text: &str) -> String {
        self.secrets.mask(text)
    }

    /// 이벤트 하나를 모든 현재 뷰어에게 내보내고, `progress`/`state`/`done`이면
    /// 스냅샷을 갱신한다.
    ///
    /// 구독자가 0명이어도 실패하지 않는다 — `broadcast::Sender::send`의 반환값(받은
    /// 구독자 수)을 의도적으로 버린다. 이게 이 허브의 핵심 계약이다(모듈 헤더 "뷰어
    /// 0명이어도 잡은 계속 돈다").
    pub fn publish(&self, event: JobEvent) {
        // 마스킹은 **스냅샷 갱신보다 먼저** 한다 — 스냅샷은 나중에 붙는 뷰어에게 그대로
        // 재생되므로, 원문을 스냅샷에 담으면 지금 구독자에게는 지워진 값을 보내면서
        // 나중 구독자에게는 원문을 보내는 최악의 조합이 된다.
        let event = self.redact(event);
        {
            // lock을 짧게 쥔다 — 스냅샷 갱신은 포인터 대입 수준이라 broadcast send와
            // 겹칠 이유가 없다(그리고 아래 send는 lock 밖에서 수행해 구독자 콜백이
            // 이 lock을 기다리는 동안 이 스레드가 lock을 쥔 채 블록되는 상황을 피한다).
            let mut snapshot = self.snapshot.lock().unwrap_or_else(|e| e.into_inner());
            match &event {
                JobEvent::Progress { .. } => snapshot.progress = Some(event.clone()),
                JobEvent::State { .. } => snapshot.state = Some(event.clone()),
                JobEvent::Done { .. } => snapshot.done = Some(event.clone()),
                JobEvent::Log { .. } => {}
            }
        }
        let _ = self.tx.send(event);
    }

    /// 이벤트 하나를 내보내기 직전에 마스킹한다 — **모듈 헤더의 "이 허브를 거친 모든
    /// 프레임은 마스킹을 거쳤다"를 코드로 성립시키는 함수.**
    ///
    /// ## 와일드카드 arm(`_ =>`)이 없는 것이 이 함수의 설계다
    /// 예전 구현은 `if let JobEvent::Log { line, .. } = &mut event` 한 줄이었다. 그래서
    /// **`JobEvent::Done.summary`(자식 stdout 원문 JSON)가 마스킹을 통째로 우회했다** —
    /// 모듈 헤더는 모든 프레임이 마스킹을 거친다고 주장했고, `tests/web_secret_leak.rs`는
    /// SSE 커버리지를 그 주장에 위임했지만, 실제로는 그 필드를 아무도 검사하지 않았다
    /// (이 파일 테스트도 전부 `summary: None`이었다).
    ///
    /// 그래서 "골라서 마스킹"을 "전부 분해해서 다시 조립"으로 바꿨다. 모든 variant를 명시적으로
    /// 분해하고 와일드카드를 두지 않으므로, [`JobEvent`]에 variant나 필드가 추가되면 **컴파일이
    /// 깨진다.** 새 필드가 조용히 마스킹에서 빠지는 경로가 문법 수준에서 없다 — 다음 사람은
    /// 반드시 arm을 쓰고 "이 필드에 남의 텍스트가 실리는가"를 판단하게 된다.
    ///
    /// ## 모든 문자열을 무조건 지우지는 않는다 — 판단의 기준
    /// 마스킹 대상은 **이 프로세스가 내용을 통제하지 못하는 텍스트**다:
    ///
    /// - [`JobEvent::Log::line`] — 자식 stderr. 무엇이 찍힐지 통제 불가.
    /// - [`JobEvent::Done::summary`] — 자식 stdout의 최종 요약 JSON. 지금 CLI `--json`
    ///   요약에서 시크릿 값을 싣는 필드는 없지만, `peek --json`은 **DB 문서 원문**을 싣고
    ///   `status`의 `items[].message`는 드라이버·외부 도구의 에러 문자열을 담는다 — 그 두
    ///   경로가 나중에 이 허브를 타면 실제 누출이 된다. 2차 방어선은 그때 있어야 한다.
    ///
    /// 반대로 `state`·`outcome`·`level`·`bytes`·`total`은 우리가 만든 닫힌 어휘이거나
    /// 숫자다. 이것들까지 마스킹하면 얻는 것이 없고 잃는 것이 있다: 등록된 시크릿이 우연히
    /// `"succeeded"` 같은 라벨과 같으면 배지 글자가 `[REDACTED]`가 되어 화면이 결과를 말하지
    /// 못한다. 각 arm에 그 판단을 주석으로 남겨 두는 것이 이 함수의 나머지 절반이다.
    fn redact(&self, event: JobEvent) -> JobEvent {
        match event {
            // 자식 stderr — 반드시 지운다.
            JobEvent::Log { stream, line } => JobEvent::Log {
                stream,
                line: self.secrets.mask(&line),
            },
            // 자식 stdout의 요약 JSON — 반드시 지운다(위 doc 참조).
            JobEvent::Done {
                outcome,
                exit_code,
                level,
                summary,
            } => JobEvent::Done {
                // 우리가 만든 닫힌 어휘(`JobOutcome::label`) — 지우지 않는다.
                outcome,
                exit_code,
                level,
                summary: summary.map(|value| mask_json(&self.secrets, value)),
            },
            // 숫자뿐이다 — 지울 문자열이 없다.
            JobEvent::Progress { bytes, total } => JobEvent::Progress { bytes, total },
            // 호출부가 정하는 어휘지만 잡 생명주기의 닫힌 상태 이름이다(`"running"`/
            // `"cancelling"`) — 자식이 만든 텍스트가 아니므로 지우지 않는다.
            JobEvent::State { state, level } => JobEvent::State { state, level },
        }
    }

    /// 이 잡의 이벤트 스트림을 새로 연다.
    ///
    /// 먼저 스냅샷(state → progress → done 순, 있는 것만)을 흘려보낸 뒤 실시간
    /// 브로드캐스트로 이어 붙인다 — 그래서 이 스트림을 처음부터 소비하는 쪽은 "지금
    /// 상태"를 즉시 알 수 있다. `done`까지 스냅샷에 있다는 것은 이미 끝난 잡에
    /// 뒤늦게 붙은 뷰어도 "끝났다"는 사실을 즉시 받는다는 뜻이다(잡이 끝난 채로
    /// 얼마나 오래 허브가 살아있는지는 레지스트리를 들고 있는 쪽(t14)의 정책이다).
    pub fn subscribe(&self) -> impl Stream<Item = JobEvent> {
        let replay: Vec<JobEvent> = {
            let snapshot = self.snapshot.lock().unwrap_or_else(|e| e.into_inner());
            [&snapshot.state, &snapshot.progress, &snapshot.done]
                .into_iter()
                .filter_map(|slot| slot.clone())
                .collect()
        };
        stream::iter(replay).chain(broadcast_stream(self.tx.subscribe()))
    }
}

/// JSON 값 안의 **모든 문자열**을 마스킹한다 — [`JobHub::redact`]가 `summary`에 쓴다.
///
/// ## 왜 재귀적으로 전부 훑는가
/// `summary`는 자식이 stdout 맨 끝에 낸 문서이고, 이 프로세스는 그 **모양을 모른다**(자식이
/// 어느 서브커맨드인지에 따라 스키마가 다르고, `peek --json`은 DB 문서 원문을 중첩해 싣는다).
/// 특정 키만 골라 지우려면 그 스키마를 여기서 다시 알아야 하고, 그 지식은 CLI가 필드를 하나
/// 추가하는 순간 낡는다 — 낡은 화이트리스트는 조용히 새는 화이트리스트다. 그래서 구조를
/// 가정하지 않고 문자열이 나오는 모든 자리를 지운다.
///
/// **객체 키도 지운다.** 키가 우리 어휘인 경우가 대부분이지만, `peek`가 싣는 DB 문서에서는
/// 키가 곧 그 데이터베이스의 필드 이름이다 — 남의 데이터라는 점에서 값과 다르지 않다.
///
/// 숫자·불리언·null은 그대로 둔다. 시크릿은 문자열로만 등록되므로([`SecretRegistry::register`])
/// 숫자를 문자열로 바꿔 검사하는 것은 오탐만 만든다.
fn mask_json(secrets: &SecretRegistry, value: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::String(text) => Value::String(secrets.mask(&text)),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| mask_json(secrets, item))
                .collect(),
        ),
        Value::Object(fields) => Value::Object(
            fields
                .into_iter()
                .map(|(key, item)| (secrets.mask(&key), mask_json(secrets, item)))
                .collect(),
        ),
        // 숫자·불리언·null에는 지울 것이 없다.
        other => other,
    }
}

/// [`broadcast::Receiver`]를 [`Stream`]으로 편다.
///
/// `tokio-stream`(별도 의존성) 없이 `futures::stream::unfold`만으로 구현한다 —
/// `recv()`가 이미 `Future`를 돌려주므로 반복해서 그 결과를 상태로 접으면 된다.
/// [`broadcast::error::RecvError::Lagged`]는 스트림을 끊지 않고 건너뛴다(모듈 헤더
/// "왜 broadcast이고 뒤처진 뷰어는 어떻게 되는가" 참고) — `Closed`(송신측 drop, 즉
/// 허브 자체가 사라짐)일 때만 스트림을 끝낸다.
fn broadcast_stream(rx: broadcast::Receiver<JobEvent>) -> impl Stream<Item = JobEvent> {
    stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(event) => return Some((event, rx)),
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(
                        skipped,
                        "SSE 뷰어가 이벤트 처리 속도를 따라가지 못해 일부를 건너뛰었습니다"
                    );
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    })
}

/// [`JobEvent`]를 실제 SSE 와이어 프레임([`Event`])으로 접는다.
fn to_sse_event(event: JobEvent) -> Event {
    let kind = event.kind();
    // 직렬화 실패는 사실상 일어나지 않는다(모든 필드가 원시 타입이거나 이미 유효한
    // `serde_json::Value`다). 그래도 `unwrap` 대신 방어한다 — 이벤트 하나의 직렬화
    // 실패로 이 태스크가 panic하면 스트림 전체가 끊기고, 뷰어의 `EventSource`가
    // 자동 재연결을 반복하며 애꿎은 재구독 스톰을 만든다. 빈 객체 하나를 잃는 편이 낫다.
    let payload = serde_json::to_string(&event).unwrap_or_else(|e| {
        tracing::error!(error = %e, kind, "SSE 이벤트 직렬화 실패 — 빈 객체로 대체합니다");
        "{}".to_string()
    });
    Event::default().event(kind).data(payload)
}

/// 이 서브경로 조각 — 잡 id는 상위 라우터가 이미 추출해 왔다는 전제다.
///
/// 전체 경로(예: `/jobs/{id}/events`)를 정하고 `server.rs`에 실제로 등록하는 것은
/// 이 태스크가 손댈 수 없는 파일이라 리더/t14의 몫이다(모듈 헤더 "이 파일이 아는 것 /
/// 모르는 것" 참고). 이 상수는 그 배선 코드가 참조할 이름 하나를 여기 박아 두어,
/// 경로 문자열이 여러 곳에 손으로 복사되는 것을 막는 용도다.
pub const JOB_EVENTS_ROUTE_SUFFIX: &str = "/events";

/// 허브 하나를 실제 SSE 응답으로 조립한다.
///
/// `axum::routing::get`에 바로 걸 수 있는 완성된 핸들러는 **아니다** — 잡 id로부터
/// `Arc<JobHub>`를 찾는 일(상태 추출)은 이 모듈이 모르는 레지스트리의 몫이다. 그 레지스트리는
/// [`crate::web::state::live::LiveJobs`]이고 [`crate::web::ServeConfig::live`]가 들고 있다.
/// 호출부는 이런 모양이다([`crate::web::routes::backup::events`]가 실제 예):
///
/// ```ignore
/// async fn events_handler(
///     State(ctx): State<Arc<ServeConfig>>,
///     Path(id): Path<JobId>,
/// ) -> Result<impl IntoResponse, StatusCode> {
///     let hub = ctx.live.hub_for(&id).ok_or(StatusCode::NOT_FOUND)?;
///     Ok(sse_response(hub))
/// }
/// ```
///
/// keep-alive는 axum 기본값(15초 간격의 빈 주석 프레임)을 그대로 쓴다 — 앞단 리버스
/// 프록시의 유휴 타임아웃(흔히 60초, [`crate::web::routes::doctor`] 헤더의 같은 판단)
/// 보다 충분히 촘촘해 중간 프록시가 연결을 끊지 않는다.
pub fn sse_response(hub: Arc<JobHub>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = hub.subscribe().map(|event| Ok(to_sse_event(event)));
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn hub() -> JobHub {
        JobHub::new(SecretRegistry::new())
    }

    async fn collect_n(
        mut stream: impl Stream<Item = JobEvent> + Unpin,
        n: usize,
    ) -> Vec<JobEvent> {
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            match tokio::time::timeout(Duration::from_secs(5), stream.next()).await {
                Ok(Some(event)) => out.push(event),
                Ok(None) => panic!(
                    "스트림이 {n}개를 채우기 전에 끝났다(받은 개수: {})",
                    out.len()
                ),
                Err(_) => panic!("5초 안에 이벤트가 오지 않았다(받은 개수: {})", out.len()),
            }
        }
        out
    }

    /// `#[serde(tag = "event")]`가 만드는 JSON의 `"event"` 값과 [`JobEvent::kind`]가
    /// 같은 어휘를 쓴다 — 어긋나면 SSE 프레임의 `event:` 필드와 본문의 `"event"` 키가
    /// 서로 다른 이름을 말하게 된다.
    #[test]
    fn kind_matches_serialized_tag() {
        let samples = [
            JobEvent::Progress {
                bytes: 1,
                total: None,
            },
            JobEvent::Log {
                stream: LogStream::Stderr,
                line: "x".into(),
            },
            JobEvent::State {
                state: "running".into(),
                level: Level::Ok,
            },
            JobEvent::Done {
                outcome: "succeeded".into(),
                exit_code: Some(0),
                level: Level::Ok,
                summary: None,
            },
        ];
        for event in samples {
            let json: serde_json::Value =
                serde_json::from_str(&serde_json::to_string(&event).unwrap()).unwrap();
            assert_eq!(
                json["event"],
                event.kind(),
                "{event:?}의 태그와 kind()가 다르다"
            );
        }
    }

    /// `total` 없는 progress는 JSON에 `total` 키 자체가 없다(CLI의
    /// `progress_json_line`과 같은 절약 — 모듈 헤더 참고).
    #[test]
    fn progress_without_total_omits_the_field() {
        let event = JobEvent::Progress {
            bytes: 42,
            total: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(!json.contains("total"), "total 키가 없어야 하는데: {json}");
        assert!(json.contains("42"));
    }

    /// 구독자가 없어도 `publish`는 패닉·블록 없이 끝난다 — 뷰어 0명 시나리오의 핵심.
    #[tokio::test]
    async fn publish_without_subscribers_does_not_panic() {
        let hub = hub();
        hub.publish(JobEvent::Progress {
            bytes: 10,
            total: Some(100),
        });
        hub.publish(JobEvent::Log {
            stream: LogStream::Stderr,
            line: "still running".into(),
        });
        // 패닉하지 않고 여기까지 왔으면 성공.
    }

    /// 뷰어가 없는 동안 진행률이 여러 번 갱신된 뒤, **나중에** 구독한 뷰어는 가장
    /// 최근 진행률(과 state)을 즉시 받는다 — 중간값들을 다시 보내는 게 아니라 최신
    /// 하나만 재생한다.
    #[tokio::test]
    async fn late_subscriber_receives_latest_snapshot_only() {
        let hub = hub();
        hub.publish(JobEvent::State {
            state: "running".into(),
            level: Level::Ok,
        });
        hub.publish(JobEvent::Progress {
            bytes: 10,
            total: Some(100),
        });
        hub.publish(JobEvent::Progress {
            bytes: 50,
            total: Some(100),
        });
        hub.publish(JobEvent::Progress {
            bytes: 90,
            total: Some(100),
        });

        let stream = hub.subscribe();
        let replayed = collect_n(Box::pin(stream), 2).await;
        assert!(
            matches!(&replayed[0], JobEvent::State { state, .. } if state == "running"),
            "첫 재생 이벤트가 state가 아니다: {replayed:?}"
        );
        assert!(
            matches!(&replayed[1], JobEvent::Progress { bytes: 90, .. }),
            "중간값이 아니라 최신 진행률(90)이 와야 한다: {replayed:?}"
        );
    }

    /// 스냅샷에 없던 종류(`done`)는 재생되지 않는다 — 아직 끝나지 않은 잡이므로.
    #[tokio::test]
    async fn subscribe_before_done_replays_only_whats_known() {
        let hub = hub();
        hub.publish(JobEvent::Progress {
            bytes: 1,
            total: None,
        });
        let stream = hub.subscribe();
        let replayed = collect_n(Box::pin(stream), 1).await;
        assert!(matches!(replayed[0], JobEvent::Progress { .. }));
    }

    /// 이미 끝난 잡에 뒤늦게 붙어도 `done` 스냅샷을 즉시 받는다.
    #[tokio::test]
    async fn late_subscriber_after_done_still_learns_the_job_finished() {
        let hub = hub();
        hub.publish(JobEvent::State {
            state: "running".into(),
            level: Level::Ok,
        });
        hub.publish(JobEvent::Done {
            outcome: "succeeded".into(),
            exit_code: Some(0),
            level: Level::Ok,
            summary: None,
        });
        let stream = hub.subscribe();
        let replayed = collect_n(Box::pin(stream), 2).await;
        assert!(matches!(replayed[1], JobEvent::Done { .. }), "{replayed:?}");
    }

    /// 동시 뷰어 20명이 전부 같은 실시간 이벤트를 받는다(팬아웃).
    #[tokio::test]
    async fn fans_out_to_twenty_concurrent_viewers() {
        let hub = Arc::new(hub());
        const VIEWERS: usize = 20;
        let mut streams: Vec<_> = (0..VIEWERS).map(|_| Box::pin(hub.subscribe())).collect();

        hub.publish(JobEvent::Log {
            stream: LogStream::Stderr,
            line: "hello everyone".into(),
        });

        for s in &mut streams {
            let event = tokio::time::timeout(Duration::from_secs(5), s.next())
                .await
                .expect("타임아웃")
                .expect("스트림이 끝났다");
            assert!(
                matches!(&event, JobEvent::Log { line, .. } if line == "hello everyone"),
                "뷰어가 다른 내용을 받았다: {event:?}"
            );
        }
    }

    /// `state`·`progress`·`done`을 등록된 시크릿이 섞인 `log` 한 줄과 함께 발행하면,
    /// `log`의 원문만 마스킹되고 다른 이벤트는 손대지 않는다.
    #[tokio::test]
    async fn publish_masks_log_events_with_the_hubs_registry() {
        let mut registry = SecretRegistry::new();
        assert!(registry.register("sup3rSecretPassw0rd"));
        let hub = JobHub::new(registry);

        let stream = hub.subscribe();
        hub.publish(JobEvent::Log {
            stream: LogStream::Stderr,
            line: "auth failed: sup3rSecretPassw0rd".into(),
        });

        let received = collect_n(Box::pin(stream), 1).await;
        let JobEvent::Log { line, .. } = &received[0] else {
            panic!("log 이벤트가 아니다: {received:?}");
        };
        assert!(
            !line.contains("sup3rSecretPassw0rd"),
            "시크릿 원문이 남았다: {line}"
        );
        assert!(line.contains(crate::web::mask::REDACTED_PLACEHOLDER));
    }

    /// **`Done.summary`도 마스킹을 거친다** — 이 필드는 자식 stdout 원문 JSON이다.
    ///
    /// 예전에는 `publish`가 `Log`만 골라 마스킹해서 이 필드가 통째로 우회했고, 이 파일 테스트가
    /// 전부 `summary: None`이어서 그 사실이 드러나지 않았다. 중첩 객체·배열·객체 **키**까지
    /// 훑는지 함께 확인한다 — `peek --json`은 DB 문서 원문을 중첩해 싣고, 그 문서에서는 키도
    /// 남의 데이터다.
    #[tokio::test]
    async fn publish_masks_the_done_summary_json() {
        const SECRET: &str = "NOT-A-REAL-SECRET-sse-summary-8fd21a";
        let mut registry = SecretRegistry::new();
        assert!(registry.register(SECRET));
        let hub = JobHub::new(registry);

        let stream = hub.subscribe();
        hub.publish(JobEvent::Done {
            outcome: "succeeded".into(),
            exit_code: Some(0),
            level: Level::Ok,
            summary: Some(serde_json::json!({
                "schema": 1,
                "uri": format!("mongodb://u:{SECRET}@h/db"),
                "items": [
                    { "message": format!("driver error: {SECRET}") },
                    { "bytes": 42, "ok": true, "nothing": null },
                ],
                // 키 자체가 남의 데이터인 경우(peek의 문서 필드명).
                SECRET: "value",
            })),
        });

        let received = collect_n(Box::pin(stream), 1).await;
        let JobEvent::Done { summary, .. } = &received[0] else {
            panic!("done 이벤트가 아니다: {received:?}");
        };
        let rendered = serde_json::to_string(summary.as_ref().expect("summary 누락")).unwrap();
        assert!(
            !rendered.contains(SECRET),
            "요약 JSON에 시크릿 원문이 남았다: {rendered}"
        );
        assert!(
            rendered.contains(crate::web::mask::REDACTED_PLACEHOLDER),
            "치환 흔적이 없다: {rendered}"
        );
        // 숫자·불리언·null은 건드리지 않는다(오탐 없이 그대로 남는다).
        assert!(rendered.contains("42"), "{rendered}");
        assert!(rendered.contains("true"), "{rendered}");
        assert!(rendered.contains("null"), "{rendered}");
    }

    /// **나중에 붙는 뷰어에게 재생되는 스냅샷도 마스킹된 것이어야 한다.**
    ///
    /// 마스킹이 스냅샷 갱신보다 뒤에 있으면 "지금 구독자에게는 지워진 값, 나중 구독자에게는
    /// 원문"이라는 최악의 조합이 된다 — 그 순서를 이 테스트가 고정한다(구독을 publish
    /// **뒤에** 한다).
    #[tokio::test]
    async fn replayed_snapshot_is_also_masked() {
        const SECRET: &str = "NOT-A-REAL-SECRET-sse-snapshot-3b7c40";
        let mut registry = SecretRegistry::new();
        assert!(registry.register(SECRET));
        let hub = JobHub::new(registry);

        hub.publish(JobEvent::Done {
            outcome: "failed".into(),
            exit_code: Some(1),
            level: Level::Fail,
            summary: Some(serde_json::json!({ "error": SECRET })),
        });

        // 이벤트가 이미 지나간 뒤에 붙는다 — 받는 것은 스냅샷 재생뿐이다.
        let received = collect_n(Box::pin(hub.subscribe()), 1).await;
        let rendered = serde_json::to_string(&received[0]).unwrap();
        assert!(
            !rendered.contains(SECRET),
            "스냅샷 재생에 시크릿 원문이 남았다: {rendered}"
        );
    }

    /// 결과 라벨·상태 어휘는 **지우지 않는다** — 우리가 만든 닫힌 어휘이고, 지우면 화면이
    /// 결과를 말하지 못한다(등록된 시크릿이 우연히 라벨과 같을 때 배지가 `[REDACTED]`가 된다).
    #[tokio::test]
    async fn our_own_vocabulary_is_left_alone() {
        let mut registry = SecretRegistry::new();
        // 라벨과 **같은 문자열**을 시크릿으로 등록해 최악의 경우를 만든다.
        assert!(registry.register("succeeded"));
        let hub = JobHub::new(registry);

        let stream = hub.subscribe();
        hub.publish(JobEvent::Done {
            outcome: "succeeded".into(),
            exit_code: Some(0),
            level: Level::Ok,
            summary: None,
        });
        let received = collect_n(Box::pin(stream), 1).await;
        let JobEvent::Done { outcome, .. } = &received[0] else {
            panic!("done 이벤트가 아니다: {received:?}");
        };
        assert_eq!(
            outcome, "succeeded",
            "결과 라벨이 마스킹돼 화면이 결과를 말할 수 없게 됐다"
        );
    }

    /// `JobHub::mask`는 `publish`와 같은 레지스트리를 쓴다 — 잡 로그 파일(t14) 같은
    /// SSE 밖 목적지도 같은 기준으로 지울 수 있다.
    #[test]
    fn mask_helper_uses_the_same_registry_as_publish() {
        let mut registry = SecretRegistry::new();
        assert!(registry.register("anotherLongSecretValue"));
        let hub = JobHub::new(registry);
        let masked = hub.mask("leaked: anotherLongSecretValue");
        assert!(!masked.contains("anotherLongSecretValue"));
        assert!(masked.contains(crate::web::mask::REDACTED_PLACEHOLDER));
    }

    /// SSE 응답 조립 자체가 패닉 없이 끝난다(스트림을 실제로 소비하는 것은 axum의
    /// 몫이라 여기서는 타입이 맞물리는지만 확인한다).
    ///
    /// `#[tokio::test]`여야 한다 — `keep_alive`가 내부적으로 `tokio::time::Sleep`을
    /// 만들어(axum의 `KeepAliveStream`) 그 시점에 이미 Tokio 런타임 컨텍스트를
    /// 요구한다(런타임 밖에서 부르면 "no reactor running" panic).
    #[tokio::test]
    async fn sse_response_builds_without_panicking() {
        let hub = Arc::new(hub());
        let _response = sse_response(hub);
    }
}
