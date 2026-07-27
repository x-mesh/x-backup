//! 잡 이력 — append-only NDJSON 두 갈래(잡별 이벤트 로그 + 요약 인덱스).
//!
//! ## 저장 형식
//! ```text
//! <state_dir>/jobs/index.ndjson   ← 모든 잡의 start/end 레코드(요약의 원재료)
//! <state_dir>/jobs/<job-id>.ndjson ← 그 잡 하나의 start/log/end 레코드
//! ```
//!
//! **인덱스는 잡별 로그에서 `log` 레코드만 걸러낸 사본이다.** 이 한 문장이 이 모듈 설계의
//! 축이다 — 재구축([`JobStore::rebuild_index`])이 "필터 + 복사"로 정의되고, 두 파일이
//! 어긋날 여지가 구조적으로 좁아진다. 인덱스가 없어도 잡별 로그만 있으면 이력 전체를 다시
//! 만들 수 있고, 잡별 로그가 로테이션으로 사라져도 인덱스에 요약은 남는다.
//!
//! ## 요약을 통째로 다시 쓰지 않고 `start`/`end` 두 레코드로 나눈 이유
//! 잡의 요약은 실행 중에 바뀐다(시작 → 종료). 바뀔 때마다 그 줄을 고치려면 파일 중간을
//! 덮어써야 하고, 그 순간 **append-only가 깨진다**(= 크래시가 이력을 손상시킬 수 있고,
//! "기존 줄은 절대 변하지 않는다"는 검증 가능한 성질을 잃는다). 그래서 상태 변화를 두
//! 개의 사건으로 나눠 각각 append하고, 읽을 때 같은 `id`끼리 접어서([`JobStore::list`])
//! 요약 하나([`JobSummary`])를 만든다. 로그 구조화(log-structured) 저장의 가장 작은 형태다.
//!
//! 그 결과 `finish` 경로가 **아무것도 읽지 않는다** — 종료 사건은 자기 정보만 담으므로
//! 시작 정보를 되찾을 필요가 없다. 잡이 끝나는 순간에 파일을 읽어야 했다면 그 읽기가
//! 실패했을 때 종료 기록을 못 남기는 사태가 생긴다.
//!
//! ## 잡 id — UUID v7, 새 의존성 없음
//! `uuid` 크레이트는 이미 `features = ["v7"]`로 트리에 있다(백업 ID가 UUID v7이다 —
//! `Cargo.toml`, [`crate::pipeline::backup`]). v7은 앞 48비트가 밀리초 단위 유닉스
//! 타임스탬프(big-endian)이므로 **문자열 사전순 = 생성순**이다. 여기서 그 성질이 두 번
//! 일한다:
//!
//! 1. 로테이션이 파일명만 정렬해 "오래된 것"을 고를 수 있다 — 파일을 열지 않는다.
//! 2. 인덱스를 접을 때 [`std::collections::BTreeMap`]에 id로 담으면 정렬이 공짜다.
//!
//! [`JobId`]는 id를 **문자열로 들고 다니지 않고** 파싱된 [`Uuid`]로 들고 있다가 표시할
//! 때 정규형(소문자 하이픈)으로 다시 만든다. 파일명·URL 경로가 이 값에서 나오므로, 밖에서
//! 들어온 문자열이 그대로 파일명이 되는 경로를 아예 없앤다([`JobId::parse`]).
//!
//! ## 로테이션 — 로그는 지우고 인덱스 항목은 남긴다
//! 기본 상한은 최근 [`DEFAULT_RETAIN_JOBS`]건이고, 넘치면 **오래된 잡의 로그 파일만**
//! 지운다. 인덱스 항목은 남긴다. 근거:
//!
//! - 무거운 것은 로그 본문이다(잡 하나가 수 MB까지 간다). 인덱스 한 줄은 수백 바이트다.
//!   디스크 문제를 만드는 쪽만 지우면 목적이 달성된다.
//! - 인덱스 한 줄이 곧 "누가 언제 무엇을 눌렀는지"이고, 그게 이 이력의 존재 이유다. 로그
//!   본문은 시간이 지나면 가치가 급감하지만 요약은 그렇지 않다.
//! - 인덱스에서 줄을 지우려면 파일을 다시 써야 한다 → append-only를 깨고, 재작성 중
//!   크래시로 **이력 전체**를 잃을 위험을 만든다. 얻는 것(수백 KB)에 비해 대가가 크다.
//!
//! **대가는 정직하게 밝힌다.** 인덱스는 무한히 자란다(잡 하나당 두 줄, 대략 400바이트).
//! 하루 200잡을 돌려도 1년에 30MB 수준이라 지금 규모에서는 실용적 문제가 아니지만, 영원히
//! 괜찮은 것은 아니다. 정리가 필요해지면 운영자가 인덱스 파일을 지우면 되고, 그러면 남아
//! 있는 로그 파일에서 최근 이력이 재구축된다([`JobStore::rebuild_index`]) — 즉 **정리 수단이
//! 이미 있고, 그 수단이 데이터 유실을 캐시 성격 안으로 묶는다**(로테이션으로 로그가 사라진
//! 잡은 재구축 대상이 아니므로 그때 요약도 함께 사라진다).
//!
//! ## 동시 append 안전성 — 인스턴스를 공유해야 한다
//! [`crate::web::audit`]와 같은 판단으로 `tokio::sync::Mutex`가 append 전체(열기→쓰기→
//! 필요 시 fsync)를 직렬화한다. `O_APPEND`의 원자성은 "한 번의 `write()`"에 대한 보장이지
//! 우리 줄이 항상 단일 `write()`로 나간다는 보장이 아니다 — 뮤텍스로 직렬화하면 그 세부와
//! 무관하게 "동시에 두 줄이 끼어들 수 없다"가 이 프로세스 안에서 확정된다.
//!
//! 감사 로그와 다른 점이 하나 있다: 여기는 파일이 여러 개다(인덱스 + 잡별 로그). 그럼에도
//! **뮤텍스는 하나만 둔다.** 파일별 뮤텍스는 "잡이 끝나면 그 엔트리를 언제 지우나"라는 새
//! 상태 관리를 만들고, 그 상태가 t12의 재부착과 엉킬 수 있다. 하나로 두면 서로 다른 파일에
//! 쓰는 두 잡이 잠깐 줄을 서지만, append 한 번은 open+write 수십 µs이고 로그 줄은 초당
//! 수십 줄 규모다.
//!
//! **그래서 쓰기 경로는 반드시 [`JobStore`] 인스턴스 하나를 공유해야 한다.** 뮤텍스는
//! 인스턴스에 붙어 있으므로, 요청마다 새 인스턴스를 만들어 쓰면 직렬화가 사라진다. 읽기
//! 전용 화면은 [`JobStore::attach`]로 매번 새로 붙어도 안전하다(읽기는 서로를 방해하지
//! 않는다).
//!
//! ## fsync는 인덱스에만 — 비대칭인 이유
//! 감사 로그는 매 줄 `fsync`한다(기록되지 않은 감사 항목은 존재 이유 자체를 부정한다).
//! 여기는 다르다. 잡 이력은 캐시이고, 로그 줄은 초당 수십 개다 — 줄마다 fsync를 걸면
//! 디스크가 잡보다 느려진다. 그래서 **인덱스 append(잡당 2회)만 fsync하고 로그 줄은
//! 커널에 맡긴다.** 크래시로 마지막 로그 몇 줄이 사라져도 "언제 무엇을 눌렀는지"는 남는다.
//!
//! ## 잘린 마지막 줄 — 양쪽에서 막는다
//! 쓰다가 죽으면 파일이 개행 없이 끝난다. 두 방향 모두 다룬다:
//!
//! - **쓸 때**: 파일 끝이 개행이 아니면 구분자 개행을 먼저 넣는다(t8과 같은 처리). 기존
//!   바이트는 건드리지 않으므로 append-only는 유지되고, 새 줄만 오염에서 분리된다.
//! - **읽을 때**: JSON으로 접히지 않는 줄은 건너뛰고 **개수를 센다**. 잘린 마지막 줄은
//!   대개 JSON이 닫히지 않아 여기서 걸린다. 조용히 무시하지 않는 이유는 화면이 "일부를
//!   못 읽었다"고 말할 수 있어야 하기 때문이다([`JobHistory::unreadable_lines`]).
//!
//! ## 마스킹은 이 모듈의 책임이 아니다(그러나 읽는 쪽은 다시 마스킹한다)
//! 이 모듈은 [`crate::web::mask`]를 import하지 않는다. 받은 문자열을 그대로 JSON 문자열로
//! 직렬화할 뿐이고 내용을 들여다보지 않는다 — 감사 로그와 같은 경계다(t8 헤더 참조).
//! 쓰는 쪽(t11)이 [`crate::web::job::JobRunner::secret_registry`]로 먼저 지우고, **읽어서
//! 화면에 그리는 쪽이 한 번 더 지운다**([`crate::web::view::jobs`]).
//!
//! 필드 이름을 `args_masked`(t8과 같은 계약 표기)로 두면서 로그 본문 쪽은 `text`로 둔
//! 이유: 인자는 우리가 만든 닫힌 어휘라 쓰는 쪽이 마스킹을 보장할 수 있지만, 로그 본문은
//! 자식 프로세스가 무엇을 찍을지 통제할 수 없다. 이름으로 보장할 수 없는 것을 이름으로
//! 약속하지 않는다 — 대신 **읽는 쪽의 마스킹을 무조건 경유하게** 만든다.
//!
//! ## t11·t12가 붙는 지점
//! 두 태스크가 동시에 진행 중이라 이 파일은 그쪽 모듈을 import하지 않는다. 이음매는 이
//! 세 개면 충분하다:
//!
//! | 쓰는 쪽 | 부르는 것 | 언제 |
//! |---|---|---|
//! | t11(자식 출력 적재) | [`JobStore::append_log`] | stdout/stderr 한 덩이마다 |
//! | t11/t12(종료 처리) | [`JobStore::finish`] | `RunningJob::wait`가 돌려준 [`JobOutcome`]으로 |
//! | t12(재부착 판정) | [`JobStore::list`] → [`JobSummary::is_running`] | 기동 직후 |
//!
//! 재부착에 필요한 3중 대조 값(pid·started_at·profile)이 [`JobSummary`]에 그대로 있다 —
//! pid만으로는 안 된다(재사용되므로). 그래서 [`JobStart`]는 `started_at`을 **호출자에게서
//! 받는다**(감사 로그가 시각을 스스로 찍는 것과 반대다). 여기서 `started_at`은 기록 시각이
//! 아니라 **프로세스 신원의 일부**이고, 그것을 관측한 것은 spawn한 러너다.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::error::{Result, XBackupError};
use crate::web::job::JobOutcome;

/// state 디렉터리 아래 잡 이력이 쌓이는 하위 디렉터리 이름.
pub const JOBS_DIR_NAME: &str = "jobs";

/// 요약 인덱스 파일 이름. 잡별 로그 파일과 같은 디렉터리에 있고, 파일명이 UUID가 아니므로
/// 로테이션 대상 판정([`JobId::parse`])에 걸리지 않는다 — 인덱스가 실수로 지워지지 않는
/// 성질이 파일명 규칙에서 나온다.
pub const JOB_INDEX_FILE_NAME: &str = "index.ndjson";

/// 잡별 로그 파일 확장자.
pub const JOB_LOG_FILE_SUFFIX: &str = ".ndjson";

/// 인덱스에 실리는 레코드의 스키마 버전.
///
/// 잡별 로그의 `log` 레코드에는 이 필드를 넣지 않는다 — 그 줄은 잡별 파일 안에서만 의미가
/// 있고 초당 수십 줄이 쌓이므로 반복 필드를 최소화한다. 반면 `start`/`end`는 인덱스에도
/// 실려 **외부 도구가 읽는 문서**가 되므로 버전을 함께 남긴다(t4가 CLI JSON 문서에
/// `schema`를 붙인 것과 같은 이유).
pub const JOB_HISTORY_SCHEMA: u32 = 1;

/// 로그 파일을 보관하는 최근 잡 개수(기본 상한).
///
/// 200건의 근거: 하루 몇 건 도는 백업 운영에서 두 달 치가 남고, 잡당 로그가 넉넉히 1MB라
/// 해도 200MB로 묶인다. 이 값은 [`JobStore::with_retain`]으로 바꿀 수 있다.
pub const DEFAULT_RETAIN_JOBS: usize = 200;

/// 상세 화면용으로 메모리에 들고 있는 로그 줄 상한.
///
/// 파일 크기와 무관하게 메모리를 묶기 위한 값이다. 넘치면 **앞쪽(오래된) 줄을 버린다** —
/// 잡이 왜 실패했는지는 거의 항상 끝에 있다. 버린 개수는
/// [`JobDetail::dropped_leading_logs`]로 보고해 화면이 "앞부분 생략"을 말할 수 있게 한다.
const MAX_LOG_LINES_KEPT: usize = 2_000;

/// [`JobStore::finish_persistent`]가 종료 기록을 남기려 시도하는 횟수(첫 시도 포함).
///
/// 3의 근거: 이 경로의 실패는 대개 순간적이다(다른 프로세스가 디스크를 잠깐 채웠거나, 네트워크
/// 파일시스템의 일시적 EIO). 세 번이면 그런 순간을 넘길 만하고, 영구적인 실패(디렉터리 자체가
/// 사라짐·권한 박탈)라면 몇 번을 더 해도 결과가 같으므로 늘릴 이유가 없다. 실패 비용이 큰
/// 기록이지만(그 잡이 stuck이 된다) **무한 재시도는 하지 않는다** — 이 함수를 부르는 곳은
/// 잡 하나의 완료 처리 태스크이고, 거기서 영원히 도는 루프는 잡이 끝났는데도 서버 안에 태스크가
/// 영구히 남는다는 뜻이다.
const FINISH_ATTEMPTS: u32 = 3;

/// 종료 기록 재시도 사이의 대기.
///
/// 500ms는 "디스크 상황이 바뀔 틈은 주지만 사람이 화면 앞에서 기다리는 시간에 영향을 주지
/// 않는" 값이다 — 최악의 경우(3회 전부 실패) 총 1초를 더 쓰고 끝난다. 잡 완료 처리는 이미
/// 백그라운드 태스크라 이 대기가 응답을 막지 않는다.
const FINISH_RETRY_DELAY: Duration = Duration::from_millis(500);

// ---------------------------------------------------------------------------
// 잡 id
// ---------------------------------------------------------------------------

/// 잡 하나의 식별자 — UUID v7(시간 정렬 가능, 모듈 헤더 참조).
///
/// 내부를 [`Uuid`]로 들고 있는 것이 이 타입의 요점이다. 파일명과 URL 경로가 이 값에서
/// 나오므로, 외부 문자열이 검증을 거치지 않고 그 자리에 도달하는 경로를 없앤다 —
/// [`parse`](Self::parse)를 통과한 값만 존재할 수 있고, 표시는 항상 [`Uuid`]의 정규형
/// (소문자 하이픈)으로 다시 만들어진다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JobId(Uuid);

impl JobId {
    /// 새 id를 만든다 — UUID v7이므로 사전순이 생성순이다.
    ///
    /// `new()`가 아니라 `generate()`인 이유: `new()`는 인자 없는 생성자라는 관례상
    /// [`Default`]를 함께 요구받는데(clippy `new_without_default`), "기본값 잡 id"라는
    /// 개념은 존재하지 않는다. 이름이 부작용(새 값 생성)을 말하게 둔다.
    #[must_use]
    pub fn generate() -> Self {
        Self(Uuid::now_v7())
    }

    /// 문자열을 id로 접는다. URL 경로 조각·파일명 후보가 전부 이 문을 지난다.
    ///
    /// UUID 문법이 아닌 것은 모두 거부되므로 `..`·`/`·널 바이트 같은 경로 조작 문자는
    /// 애초에 통과할 수 없다(아래 `parse_rejects_path_traversal_and_garbage` 테스트).
    /// 버전(v7)은 검사하지 않는다 — 다른 버전 id로 기록된 오래된 이력도 열람은 되어야
    /// 하고, 안전성은 "UUID 문법"만으로 이미 확보된다(정렬만 보장되지 않는다).
    pub fn parse(raw: &str) -> Result<Self> {
        Uuid::parse_str(raw).map(Self).map_err(|_| {
            XBackupError::Usage(format!(
                "잡 id '{raw}'는 UUID 형식이 아닙니다 — 잡 이력 링크가 잘못되었거나 손상된 \
                 인덱스 항목입니다."
            ))
        })
    }

    /// 표시용 단축형(앞 8자) — 표 안에서 id 열이 화면을 다 먹지 않게 한다.
    ///
    /// v7의 앞부분은 타임스탬프이므로 앞 8자는 "언제 시작했는가"까지 담는다(뒤를 자르는
    /// 것보다 정보가 많다). 단축형은 **표시 전용**이고 조회 키로 쓰지 않는다.
    pub fn short(&self) -> String {
        let text = self.0.as_hyphenated().to_string();
        text.chars().take(8).collect()
    }
}

impl std::fmt::Display for JobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 하이픈 소문자 정규형 고정 — 파일명이 이 표기에서 나오므로 표기가 흔들리면
        // 같은 잡이 두 파일로 갈린다.
        write!(f, "{}", self.0.as_hyphenated())
    }
}

// ---------------------------------------------------------------------------
// 디스크 레코드 (NDJSON 한 줄)
// ---------------------------------------------------------------------------

/// 자식 출력의 어느 스트림에서 온 줄인지.
///
/// 문자열로 직렬화하되 **모르는 값은 [`LogStream::Unknown`]으로 접는다**(`from`/`into`
/// 변환). 나중 빌드가 스트림 종류를 추가했을 때 그 줄이 통째로 버려지지 않게 하려는
/// 것이다 — 로그 한 줄을 잃는 것보다 "종류 미상"으로 보여주는 편이 낫다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
#[non_exhaustive]
pub enum LogStream {
    /// 자식 stdout — `--json` 결과 문서가 여기 온다.
    Stdout,
    /// 자식 stderr — 진행률·경고·진단이 여기 온다.
    Stderr,
    /// 서버 자신이 남긴 메모(재부착·취소 요청 등). 자식이 찍은 것이 아니다.
    Note,
    /// 이 빌드가 모르는 스트림 종류.
    Unknown,
}

impl LogStream {
    /// 직렬화·화면에 쓰는 토큰. 기술용어이므로 영문 고정([`crate::i18n`] 규약).
    pub fn token(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
            Self::Note => "note",
            Self::Unknown => "unknown",
        }
    }
}

impl From<String> for LogStream {
    fn from(raw: String) -> Self {
        match raw.as_str() {
            "stdout" => Self::Stdout,
            "stderr" => Self::Stderr,
            "note" => Self::Note,
            _ => Self::Unknown,
        }
    }
}

impl From<LogStream> for String {
    fn from(stream: LogStream) -> Self {
        stream.token().to_string()
    }
}

/// NDJSON 한 줄의 모양. 인덱스와 잡별 로그가 **같은 타입**을 쓴다(모듈 헤더 "인덱스는
/// 잡별 로그에서 `log` 레코드만 걸러낸 사본이다").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum JobRecord {
    /// 잡이 시작됐다. 인덱스와 잡별 로그 양쪽에 들어간다.
    Start(StartRecord),
    /// 자식 출력 한 덩이. **잡별 로그에만** 들어간다.
    Log(LogRecord),
    /// 잡이 끝났다. 인덱스와 잡별 로그 양쪽에 들어간다.
    End(EndRecord),
}

/// 시작 레코드 — 재부착 판정에 필요한 3중 대조 값(pid·started_at·profile)을 담는다.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct StartRecord {
    schema: u32,
    id: String,
    command: String,
    profile: Option<String>,
    /// 호출자가 이미 마스킹을 마친 인자(t8 `args_masked`와 같은 계약).
    args_masked: Vec<String>,
    started_at: DateTime<Utc>,
    pid: Option<u32>,
}

/// 로그 레코드 — 잡별 파일 안에서만 의미가 있으므로 `schema`·`id`를 싣지 않는다.
///
/// `text`에 개행이 들어와도 파일 형식은 깨지지 않는다 — JSON 문자열 이스케이프가 `\n`으로
/// 접으므로 물리적으로는 항상 한 줄이다. 그래서 자식 출력을 "줄 단위로 쪼개서" 넘겨야 할
/// 의무가 호출자(t11)에게 없다.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct LogRecord {
    ts: DateTime<Utc>,
    stream: LogStream,
    text: String,
}

/// 종료 레코드. 시작 정보를 담지 않으므로 `finish` 경로가 아무것도 읽지 않는다(모듈 헤더).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct EndRecord {
    schema: u32,
    id: String,
    finished_at: DateTime<Utc>,
    /// [`JobOutcome::label`]이 내놓는 고정 어휘. 자유 문자열이 아니다.
    outcome: String,
    exit_code: Option<i32>,
}

// ---------------------------------------------------------------------------
// 공개 값 타입 (디스크 표현이 아니라 "접힌 결과")
// ---------------------------------------------------------------------------

/// 잡 시작을 기록할 때 넘기는 값.
///
/// `started_at`을 호출자가 준다 — 감사 로그가 시각을 스스로 찍는 것과 반대인 이유는 모듈
/// 헤더 "t11·t12가 붙는 지점" 마지막 문단 참조(여기서 시각은 기록 시점이 아니라 프로세스
/// 신원의 일부다).
#[derive(Debug, Clone)]
pub struct JobStart<'a> {
    /// 서브커맨드 이름(`backup`/`restore`/…) — [`crate::web::job::JobCommand::verb`].
    pub command: &'a str,
    /// 대상 프로파일(있으면).
    pub profile: Option<&'a str>,
    /// **호출자가 이미 마스킹한** 인자 목록
    /// ([`crate::web::job::JobSpec::masked_args`]).
    pub args_masked: &'a [String],
    /// 자식이 spawn된 시각(러너가 관측한 값).
    pub started_at: DateTime<Utc>,
    /// 자식 pid — t12의 재부착 판정용. 없으면 `None`.
    pub pid: Option<u32>,
}

/// 잡 하나의 요약 — 인덱스 레코드들을 접어서 만든다.
///
/// 디스크 표현이 아니다(직렬화하지 않는다). 저장 형식은 [`JobRecord`]이고, 이 타입은
/// "그 레코드들을 읽어 접은 결과"다. 둘을 갈라 두면 저장 형식을 바꿀 때 화면 코드가 흔들리지
/// 않는다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobSummary {
    /// 잡 id.
    pub id: JobId,
    /// 서브커맨드 이름.
    pub command: String,
    /// 대상 프로파일(있으면).
    pub profile: Option<String>,
    /// 마스킹된 인자 목록.
    pub args_masked: Vec<String>,
    /// 자식 spawn 시각.
    pub started_at: DateTime<Utc>,
    /// 자식 pid(기록됐으면). t12의 재부착 판정은 이 값 + `started_at` + `profile` 3중
    /// 대조로 한다 — pid는 재사용되므로 단독으로는 신원이 되지 못한다.
    pub pid: Option<u32>,
    /// 종료 시각. `None`이면 **아직 끝나지 않았거나 종료 기록을 남기지 못했다**(서버가
    /// 잡보다 먼저 죽은 경우) — 두 상황을 이 타입은 구분하지 않는다. 구분은 pid가 살아
    /// 있는지 보는 t12의 몫이다.
    pub finished_at: Option<DateTime<Utc>>,
    /// [`JobOutcome::label`] 어휘의 결과 문자열(끝났으면).
    pub outcome: Option<String>,
    /// 종료 코드(있으면). 시그널 종료는 `None`이다.
    pub exit_code: Option<i32>,
}

impl JobSummary {
    /// 종료 기록이 없는 상태인지 — 화면은 이 값을 "running"으로 표시한다.
    pub fn is_running(&self) -> bool {
        self.finished_at.is_none()
    }

    /// 시작~종료 경과 시간. 아직 끝나지 않았으면 `None`.
    ///
    /// 음수 방향(시계 되감기·수동 편집)도 그대로 돌려준다 — 화면이 "이상한 값"을 보고
    /// 판단할 수 있게 하고, 여기서 0으로 뭉개 감추지 않는다.
    pub fn duration(&self) -> Option<TimeDelta> {
        self.finished_at.map(|end| end - self.started_at)
    }

    /// 시작 레코드만으로 만드는 요약(종료 정보는 아직 없다).
    fn from_start(id: JobId, record: StartRecord) -> Self {
        Self {
            id,
            command: record.command,
            profile: record.profile,
            args_masked: record.args_masked,
            started_at: record.started_at,
            pid: record.pid,
            finished_at: None,
            outcome: None,
            exit_code: None,
        }
    }

    /// 종료 레코드를 얹는다.
    fn apply_end(&mut self, record: EndRecord) {
        self.finished_at = Some(record.finished_at);
        self.outcome = Some(record.outcome);
        self.exit_code = record.exit_code;
    }
}

/// [`JobStore::list`]의 결과 — 이력과 **못 읽은 것의 개수**를 함께 돌려준다.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobHistory {
    /// 최신순(잡 id 내림차순 = 생성 역순) 요약 목록.
    pub entries: Vec<JobSummary>,
    /// 이력으로 접지 못한 줄 수 — JSON 파싱 실패(잘린 마지막 줄 포함)와 짝 없는 종료
    /// 레코드를 합친 값이다. 화면이 "일부를 못 읽었다"고 말할 수 있게 하려고 센다.
    pub unreadable_lines: usize,
    /// 인덱스 파일이 존재하고 읽혔는지. `false`면 이력이 없는 것이 아니라 **인덱스가
    /// 없는 것**일 수 있다(재구축 안내를 화면이 띄울 근거).
    pub index_present: bool,
}

/// 잡 하나의 상세 — 요약 + 로그 본문.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobDetail {
    /// 조회한 잡 id.
    pub id: JobId,
    /// 요약(인덱스 또는 로그 파일에서 복원). 둘 다 없으면 `None` = 모르는 잡.
    pub summary: Option<JobSummary>,
    /// 로그 줄(오래된 것부터). [`MAX_LOG_LINES_KEPT`]로 묶인다.
    pub logs: Vec<JobLogLine>,
    /// 상한을 넘겨 버린 앞쪽 줄 수.
    pub dropped_leading_logs: usize,
    /// 로그 파일이 있었는지. `false`면 로테이션으로 정리됐거나 애초에 없던 잡이다 —
    /// **별도의 "정리됨" 마커를 남기지 않는다**(파일의 부재가 그 마커다).
    pub log_file_present: bool,
    /// 읽지 못한 줄 수(잘린 마지막 줄 등).
    pub unreadable_lines: usize,
}

/// 로그 한 줄.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobLogLine {
    /// 기록 시각(이 모듈이 append 시점에 찍는다).
    pub ts: DateTime<Utc>,
    /// 어느 스트림에서 왔는지.
    pub stream: LogStream,
    /// 본문. **마스킹되어 있다고 가정하지 않는다** — 화면이 다시 마스킹한다(모듈 헤더).
    pub text: String,
}

/// [`JobStore::rebuild_index`]의 결과.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RebuildReport {
    /// 재구축에 쓰인 잡 로그 파일 수.
    pub jobs: usize,
    /// 새 인덱스에 쓴 레코드 수.
    pub records: usize,
    /// 읽지 못한 줄 수.
    pub unreadable_lines: usize,
}

/// append 한 번의 내구성 정책 — 호출부에서 `true`/`false`가 무엇을 뜻하는지 보이게 하려고
/// 열거형으로 둔다(모듈 헤더 "fsync는 인덱스에만").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Durability {
    /// 커널에 맡긴다(로그 줄).
    Buffered,
    /// `fsync`까지 한다(인덱스).
    Synced,
}

// ---------------------------------------------------------------------------
// 저장소
// ---------------------------------------------------------------------------

/// 잡 이력 저장소.
///
/// **쓰기 경로는 이 인스턴스를 공유해야 한다**(모듈 헤더 "동시 append 안전성"). 읽기
/// 전용 화면은 [`attach`](Self::attach)로 매번 새로 붙어도 된다.
#[derive(Debug)]
pub struct JobStore {
    /// `<state_dir>/jobs`.
    dir: PathBuf,
    /// 로그 파일을 보관할 최근 잡 개수.
    retain: usize,
    /// append 직렬화용. 파일이 여러 개여도 하나만 둔다(모듈 헤더).
    write_lock: Mutex<()>,
}

impl JobStore {
    /// 쓰기 경로용으로 연다 — 디렉터리를 만들려고 **시도**한다.
    ///
    /// 실패해도 `Self`를 돌려준다(경고만 남긴다). 감사 로그의 `open`이 실패를 전파해
    /// 서버 기동을 막는 것과 정반대이고, 근거는 [`super`] 모듈 헤더의 "캐시 성격" 규칙
    /// 1번이다 — 잡 이력을 못 쓴다고 콘솔 전체를 못 띄우면 운영자는 장애 대응 중에
    /// 터미널로 돌아가야 한다. 개별 쓰기는 각자 [`Result`]로 실패를 알린다.
    pub fn open(state_dir: &Path) -> Self {
        let dir = state_dir.join(JOBS_DIR_NAME);
        if let Err(e) = ensure_dir(&dir) {
            tracing::warn!(
                dir = %dir.display(),
                error = %e,
                "잡 이력 디렉터리를 준비할 수 없습니다 — 이력 기록 없이 계속합니다(백업 동작에는 영향 없음)"
            );
        }
        Self::attach(state_dir)
    }

    /// 읽기 전용으로 붙는다 — **파일시스템을 만들지 않는다.**
    ///
    /// GET 화면이 상태를 만들지 않게 하는 것이 목적이다([`super`] 모듈 헤더 규칙 2).
    pub fn attach(state_dir: &Path) -> Self {
        Self {
            dir: state_dir.join(JOBS_DIR_NAME),
            retain: DEFAULT_RETAIN_JOBS,
            write_lock: Mutex::new(()),
        }
    }

    /// 로그 파일 보관 상한을 바꾼다. 0은 1로 올린다 — "전부 지운다"는 정책은 이력을
    /// 남기려는 이 모듈의 목적과 모순이므로 실수로 설정될 여지를 남기지 않는다.
    #[must_use]
    pub fn with_retain(mut self, retain: usize) -> Self {
        self.retain = retain.max(1);
        self
    }

    /// 이력 디렉터리(`<state_dir>/jobs`).
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 요약 인덱스 파일 경로.
    pub fn index_path(&self) -> PathBuf {
        self.dir.join(JOB_INDEX_FILE_NAME)
    }

    /// 잡별 로그 파일 경로. 파일명은 [`JobId`]의 정규형에서만 나온다.
    pub fn log_path(&self, id: &JobId) -> PathBuf {
        self.dir.join(format!("{id}{JOB_LOG_FILE_SUFFIX}"))
    }

    /// 잡 시작을 기록하고 새 id를 돌려준다.
    ///
    /// 잡별 로그 파일에 **먼저** 쓴다. 인덱스는 그 파일들에서 재구축 가능한 사본이므로,
    /// 둘 중 하나만 남는 크래시에서는 원본 쪽이 남아야 한다.
    ///
    /// 로테이션 실패는 삼킨다(경고만) — 디스크 정리에 실패했다고 잡 시작을 막을 이유가
    /// 없다. 반대로 기록 실패는 전파한다: 호출자가 "이력에 남지 않는 잡"을 인지해야 한다.
    pub async fn start(&self, start: JobStart<'_>) -> Result<JobId> {
        let id = JobId::generate();
        let record = JobRecord::Start(StartRecord {
            schema: JOB_HISTORY_SCHEMA,
            id: id.to_string(),
            command: start.command.to_string(),
            profile: start.profile.map(str::to_string),
            args_masked: start.args_masked.to_vec(),
            started_at: start.started_at,
            pid: start.pid,
        });

        self.append(self.log_path(&id), &record, Durability::Buffered)
            .await?;
        self.append(self.index_path(), &record, Durability::Synced)
            .await?;

        if let Err(e) = self.rotate().await {
            tracing::warn!(error = %e, "오래된 잡 로그 정리에 실패했습니다");
        }
        Ok(id)
    }

    /// 자식 출력 한 덩이를 그 잡의 로그 파일에 append한다 — **t11의 진입점**.
    ///
    /// `text`에 개행이 있어도 파일 형식은 깨지지 않는다([`LogRecord`] doc). fsync하지
    /// 않는다(모듈 헤더 "fsync는 인덱스에만").
    pub async fn append_log(&self, id: &JobId, stream: LogStream, text: &str) -> Result<()> {
        let record = JobRecord::Log(LogRecord {
            ts: Utc::now(),
            stream,
            text: text.to_string(),
        });
        self.append(self.log_path(id), &record, Durability::Buffered)
            .await
    }

    /// 잡 종료를 기록한다 — **t11/t12의 진입점**.
    ///
    /// [`JobOutcome`]을 값으로 받는다(문자열이 아니라). 결과 어휘의 단일 출처가 t10이어야
    /// exit 4(경고 동반 **성공**)와 exit 5(락 충돌, 재시도 가능)를 화면이 실패로 접는
    /// 사고가 이 경계에서 생기지 않는다.
    ///
    /// 시작 정보를 되찾지 않으므로 **아무 파일도 읽지 않는다**(모듈 헤더).
    pub async fn finish(&self, id: &JobId, outcome: JobOutcome) -> Result<()> {
        let record = JobRecord::End(EndRecord {
            schema: JOB_HISTORY_SCHEMA,
            id: id.to_string(),
            finished_at: Utc::now(),
            outcome: outcome.label().to_string(),
            exit_code: outcome.exit_code(),
        });
        self.append(self.log_path(id), &record, Durability::Buffered)
            .await?;
        self.append(self.index_path(), &record, Durability::Synced)
            .await
    }

    /// [`finish`](Self::finish)를 실패 시 짧게 재시도한다 — **종료 기록이 빠지면 그 잡은
    /// 영구히 "실행 중"이 된다.**
    ///
    /// ## 왜 이 함수가 따로 있는가 — 조용한 stuck이 결함의 뿌리였다
    /// 종료 기록이 없으면 [`JobSummary::is_running`]이 영원히 참이고, 그러면
    /// [`crate::web::routes::backup`]의 중복 실행 사전 거부가 **그 프로파일의 모든 새 백업을
    /// 영구히 409로 막는다.** 그런데 호출부들은 오랫동안 `if let Err(e) = finish(..) { warn }`
    /// 한 줄로 끝냈다 — 디스크가 잠깐 꽉 찼거나 권한이 흔들린 한 순간이 그 프로파일의 백업을
    /// 통째로 잠그고, 남는 흔적은 `-v` 없이는 보이지도 않는 warn 한 줄이었다.
    ///
    /// 그래서 (1) 짧게 재시도하고, (2) 그래도 실패하면 **`error!` 레벨로** 무엇이 막히는지·
    /// 어떻게 푸는지를 함께 남긴다. 재시도가 의미 있는 이유: 이 경로의 실패는 대개 순간적인
    /// 것(디스크 여유·잠깐의 EIO)이고, 영구적인 것(디렉터리가 사라짐)이면 재시도 몇 번의
    /// 비용이 무의미할 뿐 해가 없다.
    ///
    /// ## 영구 실패는 어떻게 회복되는가
    /// 다음 서버 기동의 재부착 스캔([`crate::web::reattach`])이 pid를 조회해 "그 프로세스는
    /// 없다"를 확인하고 그 항목을 마감한다. 즉 이 함수의 실패는 **재시작까지만** stuck을
    /// 만든다 — 그래도 그 사이 백업이 막히므로 조용히 지나가서는 안 된다.
    pub async fn finish_persistent(&self, id: &JobId, outcome: JobOutcome) -> Result<()> {
        let mut last_error = None;
        for attempt in 1..=FINISH_ATTEMPTS {
            match self.finish(id, outcome).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    tracing::warn!(
                        job_id = %id,
                        attempt,
                        attempts = FINISH_ATTEMPTS,
                        error = %e,
                        "잡 종료 기록에 실패했습니다 — 재시도합니다"
                    );
                    last_error = Some(e);
                    if attempt < FINISH_ATTEMPTS {
                        tokio::time::sleep(FINISH_RETRY_DELAY).await;
                    }
                }
            }
        }
        let error = last_error.expect("최소 한 번은 시도했으므로 에러가 있다");
        tracing::error!(
            job_id = %id,
            outcome = outcome.label(),
            error = %error,
            "잡 종료 기록을 {FINISH_ATTEMPTS}번 시도해도 남기지 못했습니다 — 이 잡은 이력에서 \
             계속 '실행 중'으로 보이고, 같은 프로파일의 새 백업이 409(AlreadyRunning)로 \
             막힙니다. 디스크 여유·권한을 확인하세요. 서버를 재시작하면 재부착 스캔이 이 \
             항목을 정리합니다."
        );
        Err(error)
    }

    /// 재구축할 잡 로그 파일이 하나라도 있는지.
    ///
    /// 기동 시 "인덱스가 없다"를 두 상황으로 가르는 데 쓴다([`crate::web::reattach`]):
    /// **복구가 필요한 상태**(로그는 있는데 인덱스가 사라졌다 → [`rebuild_index`](Self::rebuild_index))와
    /// **아직 아무 잡도 돌지 않은 상태**(지울 것도 되살릴 것도 없다). 후자에서 재구축을
    /// 부르면 빈 인덱스 파일만 만들어 놓고 화면의 "인덱스 없음" 안내를 무의미하게 지운다.
    ///
    /// 실패를 반환하지 않는다 — 디렉터리를 읽을 수 없으면 "없다"로 본다(캐시 성격).
    pub async fn has_log_files(&self) -> bool {
        let dir = self.dir.clone();
        spawn_read(move || {
            collect_log_ids(&dir)
                .map(|ids| !ids.is_empty())
                .unwrap_or(false)
        })
        .await
    }

    /// 이력 목록 — 최신순. **실패를 반환하지 않는다.**
    ///
    /// 인덱스가 없거나 읽을 수 없으면 빈 이력을 돌려준다(캐시 성격). 못 읽은 줄은
    /// [`JobHistory::unreadable_lines`]로 센다 — 조용히 사라지지 않는다.
    pub async fn list(&self) -> JobHistory {
        let path = self.index_path();
        let read = spawn_read(move || read_records(&path)).await;
        fold_index(read)
    }

    /// 잡 하나의 상세(요약 + 로그). **실패를 반환하지 않는다.**
    ///
    /// 요약은 인덱스에서 먼저 찾고, 없으면 로그 파일의 시작 레코드로 복원한다. 순서가 이
    /// 방향인 이유: 인덱스에는 종료 레코드까지 접힌 결과가 있고, 로그 파일에는 로테이션
    /// 이후 아무것도 없을 수 있다.
    ///
    /// 인덱스를 통째로 접는 비용을 상세 화면 한 번에 지불한다. 인덱스 한 줄이 수백
    /// 바이트이므로 수천 건 규모에서는 문제가 없다 — 이 비용이 문제가 될 규모라면 그때는
    /// 인덱스 압축(모듈 헤더의 "대가")이 먼저 필요한 상황이다.
    pub async fn detail(&self, id: &JobId) -> JobDetail {
        let indexed = self
            .list()
            .await
            .entries
            .into_iter()
            .find(|entry| entry.id == *id);

        let path = self.log_path(id);
        let read = spawn_read(move || read_log_file(&path)).await;

        let summary = indexed.or_else(|| {
            let start = read.start.clone()?;
            let mut summary = JobSummary::from_start(*id, start);
            if let Some(end) = read.end.clone() {
                summary.apply_end(end);
            }
            Some(summary)
        });

        JobDetail {
            id: *id,
            summary,
            logs: read
                .logs
                .into_iter()
                .map(|record| JobLogLine {
                    ts: record.ts,
                    stream: record.stream,
                    text: record.text,
                })
                .collect(),
            dropped_leading_logs: read.dropped,
            log_file_present: read.present,
            unreadable_lines: read.unreadable,
        }
    }

    /// 인덱스를 잡별 로그 파일들로부터 다시 만든다.
    ///
    /// 인덱스가 없거나 로그와 어긋날 때의 복구 경로다. 동작은 모듈 헤더의 한 문장 그대로 —
    /// 각 로그 파일에서 `log`가 아닌 레코드만 걸러 새 인덱스에 쓴다.
    ///
    /// **이 모듈에서 유일하게 append가 아닌 쓰기다.** 임시 파일에 다 쓰고 `rename`으로
    /// 갈아치우므로(같은 디렉터리 안이라 원자적), 중간 상태가 관측되지 않고 실패 시 기존
    /// 인덱스가 그대로 남는다. append-only 규칙의 예외를 여기 하나로 묶어 두는 것이
    /// 요점이다 — 예외가 여러 곳에 흩어지면 "기존 줄은 변하지 않는다"를 더 이상 말할 수
    /// 없다.
    ///
    /// **한계:** 로테이션으로 로그 파일이 지워진 잡은 재구축 대상이 아니다. 그래서 인덱스를
    /// 지우고 재구축하면 그 잡들의 요약은 사라진다(모듈 헤더 "대가" 참조).
    pub async fn rebuild_index(&self) -> Result<RebuildReport> {
        let dir = self.dir.clone();
        let index = self.index_path();
        // 재구축 중에는 append가 끼어들지 못하게 잠근다 — rename으로 갈아치우는 사이에
        // 들어온 append는 그 순간 사라진 파일에 쓰게 된다.
        let _guard = self.write_lock.lock().await;
        tokio::task::spawn_blocking(move || rebuild_index_blocking(&dir, &index))
            .await
            .map_err(|e| {
                XBackupError::Failure(format!(
                    "잡 인덱스 재구축 태스크가 비정상 종료했습니다: {e}"
                ))
            })?
    }

    /// 오래된 잡의 **로그 파일만** 지운다(인덱스 항목은 남긴다 — 모듈 헤더).
    ///
    /// 파일명(=UUID v7)만 정렬하므로 파일을 열지 않고, 인덱스 크기와 무관하게 비용이
    /// 보관 개수에 묶인다. 반환값은 지운 파일 수다.
    ///
    /// append 잠금을 잡지 않는다 — 삭제 대상은 "가장 오래된" 파일이고 append 대상은
    /// 방금 만든 파일이라 겹치지 않는다. 동시에 200건이 넘게 돌아 겹치는 극단적 경우에도
    /// 손상은 없다: append는 `create(true)`이므로 파일이 다시 생기고, 그 잡의 로그는
    /// 지워진 앞부분만큼 짧아진다(이력 요약은 인덱스에 그대로 있다).
    pub async fn rotate(&self) -> Result<usize> {
        let dir = self.dir.clone();
        let retain = self.retain;
        tokio::task::spawn_blocking(move || rotate_blocking(&dir, retain))
            .await
            .map_err(|e| {
                XBackupError::Failure(format!("잡 로그 정리 태스크가 비정상 종료했습니다: {e}"))
            })?
    }

    /// 레코드 하나를 NDJSON 한 줄로 append한다 — 이 모듈의 유일한 쓰기 지점.
    async fn append(
        &self,
        path: PathBuf,
        record: &JobRecord,
        durability: Durability,
    ) -> Result<()> {
        let mut line = serde_json::to_string(record)
            .map_err(|e| XBackupError::Failure(format!("잡 이력 레코드 직렬화 실패: {e}")))?;
        line.push('\n');

        // 여기서부터 파일 끝까지가 임계 구역이다(모듈 헤더 "동시 append 안전성").
        let _guard = self.write_lock.lock().await;
        tokio::task::spawn_blocking(move || append_line(&path, line.as_bytes(), durability))
            .await
            .map_err(|e| {
                XBackupError::Failure(format!("잡 이력 기록 태스크가 비정상 종료했습니다: {e}"))
            })?
    }
}

// ---------------------------------------------------------------------------
// 읽기 (순수 — 파일시스템만 만진다, 실패를 값으로 돌려준다)
// ---------------------------------------------------------------------------

/// 파일 하나를 읽은 결과.
#[derive(Default)]
struct ReadRecords {
    records: Vec<JobRecord>,
    /// 접지 못한 줄 수(잘린 마지막 줄·손상·미지 `kind`).
    unreadable: usize,
    /// 파일이 있고 열렸는지.
    present: bool,
}

/// 잡별 로그 파일을 메모리 상한 안에서 읽은 결과.
#[derive(Default)]
struct ReadLogFile {
    start: Option<StartRecord>,
    end: Option<EndRecord>,
    logs: Vec<LogRecord>,
    dropped: usize,
    unreadable: usize,
    present: bool,
}

/// 블로킹 읽기를 tokio 워커 밖으로 넘긴다. 태스크가 패닉하면 "못 읽었다"로 접는다 —
/// 읽기 실패로 화면을 500으로 떨어뜨리지 않는다(캐시 성격).
async fn spawn_read<T, F>(f: F) -> T
where
    T: Default + Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(value) => value,
        Err(e) => {
            tracing::warn!(error = %e, "잡 이력 읽기 태스크가 비정상 종료했습니다");
            T::default()
        }
    }
}

/// NDJSON 파일을 레코드 목록으로 읽는다. **어떤 입력에도 실패를 반환하지 않는다.**
///
/// 줄 단위로 읽으므로 파일 전체를 문자열로 올리지 않고, 접히지 않는 줄은 건너뛰며 센다.
fn read_records(path: &Path) -> ReadRecords {
    let Some(reader) = open_for_read(path) else {
        return ReadRecords::default();
    };
    let mut out = ReadRecords {
        records: Vec::new(),
        unreadable: 0,
        present: true,
    };
    for line in reader.lines() {
        match parse_line(line) {
            Ok(Some(record)) => out.records.push(record),
            Ok(None) => {}
            Err(()) => out.unreadable += 1,
        }
    }
    out
}

/// 잡별 로그 파일을 읽되 **로그 줄만 상한으로 묶는다**([`MAX_LOG_LINES_KEPT`]).
///
/// 시작·종료 레코드는 개수가 유한하므로 항상 보존하고, 로그 줄은 뒤쪽 N개만 남긴다 —
/// 실패 원인은 거의 항상 끝에 있다.
fn read_log_file(path: &Path) -> ReadLogFile {
    let Some(reader) = open_for_read(path) else {
        return ReadLogFile::default();
    };
    let mut out = ReadLogFile {
        present: true,
        ..ReadLogFile::default()
    };
    let mut ring: VecDeque<LogRecord> = VecDeque::with_capacity(64);
    for line in reader.lines() {
        match parse_line(line) {
            Ok(Some(JobRecord::Start(record))) => out.start = Some(record),
            Ok(Some(JobRecord::End(record))) => out.end = Some(record),
            Ok(Some(JobRecord::Log(record))) => {
                if ring.len() == MAX_LOG_LINES_KEPT {
                    ring.pop_front();
                    out.dropped += 1;
                }
                ring.push_back(record);
            }
            Ok(None) => {}
            Err(()) => out.unreadable += 1,
        }
    }
    out.logs = ring.into();
    out
}

/// 읽기용으로 파일을 연다. 없거나 열 수 없으면 `None`(경고만 남긴다).
fn open_for_read(path: &Path) -> Option<BufReader<File>> {
    match File::open(path) {
        Ok(file) => Some(BufReader::new(file)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            // 권한·ENOTDIR 등. 화면은 "이력 없음"을 보여주고, 원인은 서버 로그에 남는다.
            tracing::warn!(path = %path.display(), error = %e, "잡 이력 파일을 읽을 수 없습니다");
            None
        }
    }
}

/// 한 줄을 레코드로 접는다.
///
/// - `Ok(Some(record))`: 정상.
/// - `Ok(None)`: 빈 줄(형식상 무해하므로 세지 않는다).
/// - `Err(())`: 접히지 않는 줄 — 잘린 마지막 줄, 비-UTF8, 미지 `kind`.
fn parse_line(line: std::io::Result<String>) -> std::result::Result<Option<JobRecord>, ()> {
    // 비-UTF8 줄은 `lines()`가 여기서 에러로 준다. 그 줄만 버리고 다음 줄로 간다 —
    // 우리가 쓴 줄은 항상 UTF-8 JSON이므로, 이런 줄은 외부 오염이다.
    let line = line.map_err(|_| ())?;
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    // 위 주석의 "외부 오염"을 끝까지 밀면 깊이 폭탄도 포함된다. 여기서 걸러도 안 걸러도
    // 결과는 같은 `Err(())`다 — `serde_json`이 재귀 상한으로 이미 거부하기 때문이다
    // (`crate::web::jsonguard` 헤더: 이 관문은 크래시 방어가 아니다). 그래도 부르는 이유는
    // 깊이 판정을 한 곳에 모아 두기 위해서다.
    if crate::web::jsonguard::check_depth(trimmed).is_err() {
        return Err(());
    }
    serde_json::from_str(trimmed).map(Some).map_err(|_| ())
}

/// 인덱스 레코드들을 요약 목록으로 접는다 — **순수 함수**(파일시스템을 모른다).
///
/// [`BTreeMap`]에 [`JobId`]로 담으므로 정렬이 공짜다(UUID v7 = 시간순). 마지막에 뒤집어
/// 최신순으로 낸다.
fn fold_index(read: ReadRecords) -> JobHistory {
    let mut by_id: BTreeMap<JobId, JobSummary> = BTreeMap::new();
    let mut unreadable = read.unreadable;

    for record in read.records {
        match record {
            JobRecord::Start(start) => match JobId::parse(&start.id) {
                Ok(id) => {
                    // 같은 id의 start가 두 번 오면 나중 것이 이긴다(중복 기록·재구축 흔적).
                    // 그 잡에 이미 접힌 종료 정보가 있으면 보존한다.
                    let previous = by_id.remove(&id);
                    let mut summary = JobSummary::from_start(id, start);
                    if let Some(previous) = previous {
                        summary.finished_at = previous.finished_at;
                        summary.outcome = previous.outcome;
                        summary.exit_code = previous.exit_code;
                    }
                    by_id.insert(id, summary);
                }
                // 인덱스는 손으로 편집될 수 있는 파일이다. 파싱되지 않는 id는 화면 링크와
                // 파일명으로 흘러갈 값이므로 여기서 버린다.
                Err(_) => unreadable += 1,
            },
            JobRecord::End(end) => match JobId::parse(&end.id) {
                Ok(id) => match by_id.get_mut(&id) {
                    Some(summary) => summary.apply_end(end),
                    // 짝 없는 종료 레코드 — 인덱스를 지운 뒤 진행 중이던 잡이 끝난 경우다.
                    // 재구축이 로그 파일에서 시작 레코드를 되살리면 회복된다.
                    None => unreadable += 1,
                },
                Err(_) => unreadable += 1,
            },
            // 인덱스에 로그 줄이 있으면 형식 위반이다(쓰는 경로가 없다). 세어서 드러낸다.
            JobRecord::Log(_) => unreadable += 1,
        }
    }

    JobHistory {
        entries: by_id.into_values().rev().collect(),
        unreadable_lines: unreadable,
        index_present: read.present,
    }
}

// ---------------------------------------------------------------------------
// 쓰기 (동기 — spawn_blocking 안에서만 부른다)
// ---------------------------------------------------------------------------

/// 디렉터리를 만들고(없으면) 소유자 전용 권한을 준다.
///
/// 새로 만들 때만 0700으로 조인다 — 잡 로그에는 마스킹을 통과한 뒤에도 네임스페이스·경로
/// 같은 운영 정보가 남는다([`crate::web`]의 state 디렉터리 정책과 같은 판단).
fn ensure_dir(dir: &Path) -> std::io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // 실패는 치명적이지 않다(권한을 지원하지 않는 파일시스템일 수 있다).
        if let Err(e) = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)) {
            tracing::warn!(dir = %dir.display(), error = %e, "잡 이력 디렉터리 권한을 0700으로 조이지 못했습니다");
        }
    }
    Ok(())
}

/// 한 줄을 실제로 append한다.
///
/// 잘린 마지막 줄 보호(구분자 개행 선행 삽입)는 t8 [`crate::web::audit`]와 같은 처리다.
/// 같은 로직을 두 파일이 각자 들고 있는 것은 그 함수들이 비공개이고 `audit.rs`가 다른
/// 태스크의 소유 파일이기 때문이다 — 공용화는 두 파일이 다 안정된 뒤의 정리 대상이다.
/// (정책도 완전히 같지 않다: 여기는 fsync가 선택이고 실패의 의미가 다르다.)
fn append_line(path: &Path, line: &[u8], durability: Durability) -> Result<()> {
    let mut file = open_append(path).map_err(|e| {
        XBackupError::Failure(format!(
            "잡 이력 파일을 열 수 없습니다({}): {e}",
            path.display()
        ))
    })?;

    if !ends_with_newline_or_empty(&mut file).map_err(|e| {
        XBackupError::Failure(format!(
            "잡 이력 파일 상태 확인 실패({}): {e}",
            path.display()
        ))
    })? {
        file.write_all(b"\n").map_err(|e| {
            XBackupError::Failure(format!("잡 이력 구분자 기록 실패({}): {e}", path.display()))
        })?;
    }

    file.write_all(line).map_err(|e| {
        XBackupError::Failure(format!("잡 이력 기록 실패({}): {e}", path.display()))
    })?;
    if durability == Durability::Synced {
        file.sync_data().map_err(|e| {
            XBackupError::Failure(format!("잡 이력 fsync 실패({}): {e}", path.display()))
        })?;
    }
    Ok(())
}

/// append 모드로 연다(없으면 생성, 생성 시 0600).
///
/// `.read(true)`도 켠다 — [`ends_with_newline_or_empty`]가 파일 끝 1바이트를 읽어야 하고,
/// write-only fd로는 그 읽기가 `EBADF`로 거부된다.
fn open_append(path: &Path) -> std::io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.create(true).read(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

/// 파일이 비어 있거나 마지막 바이트가 개행인지. `O_APPEND`이므로 이 `seek`은 다음 쓰기
/// 위치에 영향을 주지 않는다.
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

/// 디렉터리에 있는 잡 로그 파일의 id를 오름차순(= 시간순)으로 모은다.
///
/// 파일명이 [`JobId::parse`]를 통과하는 것만 담으므로 `index.ndjson`은 자연히 빠진다 —
/// 로테이션이 인덱스를 지울 수 없는 이유가 이 필터다.
fn collect_log_ids(dir: &Path) -> std::io::Result<Vec<JobId>> {
    let mut ids = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(stem) = name.strip_suffix(JOB_LOG_FILE_SUFFIX) else {
            continue;
        };
        if let Ok(id) = JobId::parse(stem) {
            ids.push(id);
        }
    }
    ids.sort_unstable();
    Ok(ids)
}

/// [`JobStore::rotate`]의 동기 본체.
fn rotate_blocking(dir: &Path, retain: usize) -> Result<usize> {
    let ids = match collect_log_ids(dir) {
        Ok(ids) => ids,
        // 디렉터리가 아직 없으면 지울 것도 없다.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => {
            return Err(XBackupError::Failure(format!(
                "잡 이력 디렉터리를 읽을 수 없습니다({}): {e}",
                dir.display()
            )))
        }
    };
    if ids.len() <= retain {
        return Ok(0);
    }
    let mut removed = 0;
    // 오름차순이므로 앞쪽이 오래된 것이다.
    for id in &ids[..ids.len() - retain] {
        let path = dir.join(format!("{id}{JOB_LOG_FILE_SUFFIX}"));
        match std::fs::remove_file(&path) {
            Ok(()) => removed += 1,
            // 이미 없어졌으면 목적은 달성된 것이다.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "오래된 잡 로그를 지우지 못했습니다");
            }
        }
    }
    Ok(removed)
}

/// [`JobStore::rebuild_index`]의 동기 본체 — 임시 파일에 쓰고 `rename`으로 갈아치운다.
fn rebuild_index_blocking(dir: &Path, index: &Path) -> Result<RebuildReport> {
    ensure_dir(dir).map_err(|e| {
        XBackupError::Failure(format!(
            "잡 이력 디렉터리를 준비할 수 없습니다({}): {e}",
            dir.display()
        ))
    })?;
    let ids = collect_log_ids(dir).map_err(|e| {
        XBackupError::Failure(format!(
            "잡 이력 디렉터리를 읽을 수 없습니다({}): {e}",
            dir.display()
        ))
    })?;

    let mut report = RebuildReport {
        jobs: 0,
        records: 0,
        unreadable_lines: 0,
    };
    let mut body = String::new();
    for id in &ids {
        let read = read_records(&dir.join(format!("{id}{JOB_LOG_FILE_SUFFIX}")));
        report.unreadable_lines += read.unreadable;
        if !read.present {
            continue;
        }
        report.jobs += 1;
        for record in read.records {
            // 로그 줄은 인덱스로 넘기지 않는다 — 이 필터가 재구축의 정의 그 자체다.
            if matches!(record, JobRecord::Log(_)) {
                continue;
            }
            let line = serde_json::to_string(&record)
                .map_err(|e| XBackupError::Failure(format!("잡 이력 레코드 직렬화 실패: {e}")))?;
            body.push_str(&line);
            body.push('\n');
            report.records += 1;
        }
    }

    // 같은 디렉터리 안의 임시 파일 → rename. 다른 디렉터리(예: /tmp)에 쓰면 rename이
    // 파일시스템 경계를 넘어 실패하거나 복사로 바뀐다(원자성이 사라진다).
    let tmp = index.with_extension("ndjson.rebuild");
    write_private(&tmp, body.as_bytes()).map_err(|e| {
        XBackupError::Failure(format!(
            "잡 인덱스 임시 파일을 쓸 수 없습니다({}): {e}",
            tmp.display()
        ))
    })?;
    std::fs::rename(&tmp, index).map_err(|e| {
        // 실패해도 기존 인덱스는 그대로다. 임시 파일만 정리한다.
        let _ = std::fs::remove_file(&tmp);
        XBackupError::Failure(format!(
            "잡 인덱스를 교체할 수 없습니다({}): {e}",
            index.display()
        ))
    })?;
    Ok(report)
}

/// 파일을 새로 쓴다(있으면 잘라내고), 생성 시 0600 + fsync.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut opts = OpenOptions::new();
    opts.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    file.write_all(bytes)?;
    // rename 전에 내용을 디스크로 내린다 — 순서를 뒤집으면 크래시 후 "이름은 새것,
    // 내용은 비어 있음"이 될 수 있다.
    file.sync_data()
}

// ---------------------------------------------------------------------------
// 프로세스 전역 공유 인스턴스 — 쓰기 경로가 뮤텍스를 실제로 공유하게 만든다 (t15)
// ---------------------------------------------------------------------------

/// `state_dir` 경로별 쓰기 공유 [`JobStore`] 인스턴스 레지스트리.
///
/// 모듈 헤더 "동시 append 안전성"이 요구하는 전제("쓰기 경로는 반드시 인스턴스 하나를
/// 공유해야 한다")를 실제로 지키려면, 그 인스턴스가 요청마다 새로 만들어지지 않고 서버
/// 수명 동안 하나로 고정돼야 한다. 이 인스턴스를 axum state([`crate::web::ServeConfig`])에
/// 필드로 얹는 것이 정공법이지만, 그 구조체는 이 태스크(t15)가 손댈 수 없는 배선
/// 지점이다(리더가 이미 다른 필드들을 채워 둔 상태이고, 이 태스크의 소유 파일 목록에
/// `src/web/mod.rs`가 없다). 그래서 이 파일 안에서 완결되는 지연 초기화 레지스트리로
/// 대신한다 — 인스턴스를 만들 수 있는 유일한 진입점은 [`shared`]뿐이다.
///
/// 경로별로 나누는 이유(전역 싱글턴 하나가 아닌 이유)는 테스트 격리다.
/// [`crate::web::ServeConfig::for_test`]는 호출마다 다른 임시 `state_dir`을 만든다 — 이
/// 프로세스 안에서 `#[tokio::test]`가 전부 같은 바이너리로 도는데, 만약 레지스트리가
/// "처음 부른 경로 하나"만 기억한다면 두 번째 테스트부터는 자기 것이 아닌 첫 테스트의
/// `JobStore`를 얻게 되어 잡 이력이 테스트 사이로 샌다. 경로를 키로 쓰면 각 테스트가
/// 자기 임시 디렉터리에 대해서만 유일한 인스턴스를 얻으므로 격리가 그대로 유지된다.
fn shared_registry() -> &'static std::sync::Mutex<HashMap<PathBuf, Arc<JobStore>>> {
    // `std::sync::Mutex`를 명시적으로 쓴다 — 이 파일은 이미 `tokio::sync::Mutex`를
    // `Mutex`라는 이름으로 들여와 있고(`write_lock` 필드), 이 레지스트리는 async 경계를
    // 넘겨 들고 있을 이유가 없는 짧은 임계 구역(HashMap 조회/삽입뿐)이라 굳이 async
    // 뮤텍스를 쓸 이유가 없다. 같은 이름을 두 가지로 쓰면 헷갈리므로 전체 경로로 못박는다.
    static REGISTRY: OnceLock<std::sync::Mutex<HashMap<PathBuf, Arc<JobStore>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// 이 `state_dir`에 대한 쓰기 공유 [`JobStore`]를 얻는다(없으면 [`JobStore::open`]으로
/// 만들어 등록한다).
///
/// **쓰기 경로(`start`·`append_log`·`finish`)는 반드시 이 함수로 얻은 인스턴스를 써야
/// 한다.** 직접 `JobStore::open`을 다시 부르면 append 직렬화 뮤텍스가 새로 생겨 "동시
/// 두 줄이 끼어들 수 없다"는 보장이 깨진다(모듈 헤더 참조). 읽기 전용 화면(`GET
/// /jobs`·`GET /jobs/{id}`)은 이 함수를 쓸 필요가 없다 — `JobStore::attach`가 매번 새로
/// 붙어도 안전하다는 것은 그대로다.
///
/// lock이 오염(poison)돼도 계속 잡는다 — 보호 대상이 `HashMap` 자체이고, 패닉이 일어난
/// 지점은 클로저 안(`or_insert_with`)의 `JobStore::open`뿐인데 그 함수는 실패해도
/// panic하지 않는다(내부에서 만든 실패는 경고만 남기고 `Self`를 돌려준다 — 그 doc
/// 참조). 그래도 방어적으로 poison을 무시한다.
pub fn shared(state_dir: &Path) -> Arc<JobStore> {
    let mut registry = shared_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Arc::clone(
        registry
            .entry(state_dir.to_path_buf())
            .or_insert_with(|| Arc::new(JobStore::open(state_dir))),
    )
}

// ---------------------------------------------------------------------------
// JobLogSink 구현 (t35 흡수) — stderr 중계(t11)를 이 잡의 로그 파일로 잇는다
// ---------------------------------------------------------------------------

/// [`crate::web::job::stream::JobLogSink`] 구현 — stderr 중계 태스크가 이미 마스킹까지
/// 끝낸 한 줄을 이 잡의 로그 파일에 적재한다.
///
/// ## 왜 줄마다 태스크를 스폰하지 않는가 — 순서 보장
/// `JobLogSink::append`는 **동기** 함수다(`relay_stderr`의 핫 루프가 그때그때 직접
/// 부르는 이음매라 async가 아니다 — [`crate::web::job::stream`] 헤더 참조). 반면
/// [`JobStore::append_log`]는 파일 I/O를 `spawn_blocking`으로 넘기는 **비동기** 함수다.
/// 가장 단순한 다리는 "`append`가 불릴 때마다 `tokio::spawn`으로 그 줄 하나를 위한
/// 새 태스크를 띄워 그 안에서 `append_log`를 부른다"이지만, 이건 **순서를 보장하지
/// 못한다** — 여러 독립 태스크가 [`JobStore`]의 내부 `write_lock`을 다투는 순서는
/// 스폰 순서와 무관하게 스케줄러가 정한다(특히 멀티스레드 러너에서). stderr 로그는
/// 순서 자체가 정보다(진행 메시지가 뒤섞이면 진단이 오히려 헷갈린다).
///
/// 그래서 이 구현은 **생성 시점에 소비자 태스크를 하나만** 띄운다([`Self::spawn`]).
/// `append`는 그 태스크로 이어지는 채널에 줄을 밀어 넣기만 하고, 실제 파일 쓰기는
/// 그 단일 소비자가 도착 순서 그대로 처리한다 — 소비자가 하나뿐이므로 "채널에 넣은
/// 순서 = 파일에 적힌 순서"가 항상 성립한다.
///
/// ## 왜 bounded 채널 + `try_send`인가
/// `append`는 동기 함수라 `await`할 수 없고, [`relay_stderr`](crate::web::job::stream::relay_stderr)의
/// 핫 루프 안에서 불린다 — 여기서 블록하면 stderr 파이프를 비우는 것 자체가 멈추고,
/// 결국 자식이 멈춘다([`crate::web::job::runner`] 헤더 "파이프를 비우지 않으면 자식이
/// 멈춘다"). 그래서 절대 블록하지 않는 `try_send`만 쓴다. 채널을 unbounded로 두지
/// 않는 이유는 [`crate::web::job::stream`] 헤더가 이미 경고하는 적대적 시나리오
/// ("버그가 있거나 악의적인 자식이 초당 수만 줄을 뿜을 수 있다")에서 메모리가 무한정
/// 자라는 것을 막기 위해서다. 채널이 가득 차면 그 줄은 **파일 적재에서만** 빠진다 —
/// SSE 중계는 이 sink와 별개 경로([`JobHub::publish`])라 영향받지 않으므로, 운영자는
/// 그 줄을 실시간으로는 보고 이력에서만 놓친다(디스크를 못 따라가는 것보다 훨씬 나은
/// 실패다).
pub struct JobLogStoreSink {
    id: JobId,
    tx: tokio::sync::mpsc::Sender<String>,
}

/// [`JobLogStoreSink`]의 채널 용량(줄 개수).
///
/// [`crate::web::sse`]의 `CHANNEL_CAPACITY`(256, progress 코얼레싱 뒤 초당 수 개 +
/// 로그 몰림을 흡수하는 값)보다 넉넉히 잡는다 — 이 채널은 디스크 쓰기(파이프 비우기보다
/// 느릴 수 있다)를 소비 속도로 두므로, SSE 브로드캐스트보다 더 큰 순간 몰림을 흡수할
/// 여유가 필요하다. 그렇다고 무제한은 아니다(모듈 헤더의 "적대적 시나리오" 방어).
const LOG_SINK_CHANNEL_CAPACITY: usize = 4096;

impl JobLogStoreSink {
    /// 새 sink를 만들고, 채널을 순서대로 비워 실제 적재를 수행하는 소비자 태스크를
    /// 함께 띄운다.
    ///
    /// `store`는 반드시 [`shared`]로 얻은 인스턴스여야 한다(직접 `JobStore::open`을
    /// 새로 만들면 이 잡의 로그 append가 다른 잡의 append와 같은 파일 락 없이 동시에
    /// 일어날 여지가 생긴다 — 잡마다 로그 파일이 다르므로 실제 충돌은 드물지만, 인덱스
    /// append는 전 잡이 공유하는 파일이라 반드시 같은 뮤텍스를 거쳐야 한다).
    pub fn spawn(store: Arc<JobStore>, id: JobId) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(LOG_SINK_CHANNEL_CAPACITY);
        tokio::spawn(async move {
            // 채널의 송신측(이 구조체의 `tx`)이 전부 drop되면(잡이 끝나 sink가 버려지면)
            // `recv()`가 `None`을 돌려주고 이 루프가 자연히 끝난다 — 별도 종료 신호가
            // 필요 없다.
            while let Some(line) = rx.recv().await {
                if let Err(e) = store.append_log(&id, LogStream::Stderr, &line).await {
                    tracing::warn!(
                        job_id = %id,
                        error = %e,
                        "잡 로그 적재 실패 — SSE 중계는 영향받지 않고 계속됩니다"
                    );
                }
            }
        });
        Self { id, tx }
    }
}

impl crate::web::job::stream::JobLogSink for JobLogStoreSink {
    fn append(&self, line: &str) {
        match self.tx.try_send(line.to_string()) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!(
                    job_id = %self.id,
                    "잡 로그 적재 채널이 가득 차 이 줄을 이력에서 건너뜁니다(SSE 중계는 그대로 나갑니다)"
                );
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                // 소비 태스크가 이미 끝났다(정상 종료 또는 패닉) — 조용히 버린다. 잡
                // 실행 자체를 막을 이유가 없다(모듈 헤더의 "캐시 성격" 판단과 같다).
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 테스트용 시작 명세를 만든다.
    fn start_spec<'a>(
        command: &'a str,
        profile: Option<&'a str>,
        args: &'a [String],
    ) -> JobStart<'a> {
        JobStart {
            command,
            profile,
            args_masked: args,
            started_at: Utc::now(),
            pid: Some(4242),
        }
    }

    /// 파일의 각 줄을 `serde_json::Value`로 읽는다(빈 줄 제외).
    fn json_lines(path: &Path) -> Vec<serde_json::Value> {
        let content = std::fs::read_to_string(path).unwrap();
        content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                serde_json::from_str(l)
                    .unwrap_or_else(|e| panic!("유효하지 않은 JSON 줄 '{l}': {e}"))
            })
            .collect()
    }

    // ---- 잡 id ----

    /// 생성된 id는 사전순 = 생성순이다(UUID v7). 이 성질이 로테이션·정렬의 전제다.
    #[test]
    fn generated_ids_sort_by_creation_order() {
        let mut previous = JobId::generate();
        for _ in 0..50 {
            // v7의 시간 해상도는 밀리초이므로 같은 밀리초 안에서는 무작위 부분이 순서를
            // 정한다 — 그래서 "감소하지 않는다"까지만 단정한다(엄격한 증가는 v7 규격이
            // 보장하지 않는다).
            let next = JobId::generate();
            assert!(
                next.to_string() >= previous.to_string(),
                "id가 시간 역순으로 생성됐다: {previous} → {next}"
            );
            previous = next;
        }
    }

    /// 표시 형태는 소문자 하이픈 정규형이고, 어떤 표기로 파싱해도 같은 값이 된다.
    #[test]
    fn parse_normalizes_representation() {
        let id = JobId::generate();
        let canonical = id.to_string();
        assert_eq!(canonical.len(), 36, "하이픈 정규형이 아니다: {canonical}");
        assert_eq!(canonical, canonical.to_lowercase());
        // 대문자·중괄호·URN 표기도 같은 id로 접힌다 → 파일명은 언제나 정규형 하나뿐이다.
        for variant in [
            canonical.to_uppercase(),
            format!("{{{canonical}}}"),
            format!("urn:uuid:{canonical}"),
            canonical.replace('-', ""),
        ] {
            assert_eq!(
                JobId::parse(&variant).unwrap_or_else(|e| panic!("'{variant}' 파싱 실패: {e}")),
                id
            );
            assert_eq!(JobId::parse(&variant).unwrap().to_string(), canonical);
        }
        assert_eq!(id.short().len(), 8);
    }

    /// 경로 조작·쓰레기 문자열은 전부 거부된다 — 이 값이 파일명이 되므로 여기가 관문이다.
    #[test]
    fn parse_rejects_path_traversal_and_garbage() {
        for hostile in [
            "../../etc/passwd",
            "..",
            "/etc/shadow",
            "index",
            "index.ndjson",
            "",
            "0190f0a2-1a2b-7c3d-8e4f-aabbccddeeff/../../x",
            "0190f0a2-1a2b-7c3d-8e4f-aabbccddeeff\0",
            "job id with spaces",
        ] {
            let err = JobId::parse(hostile).expect_err(&format!("'{hostile}'가 통과했다"));
            assert_eq!(err.exit_code(), crate::error::exit_codes::USAGE);
        }
    }

    // ---- 기록 ----

    /// 시작·로그·종료가 두 파일에 유효한 NDJSON으로 남는다.
    #[tokio::test]
    async fn recording_writes_valid_ndjson_to_both_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::open(dir.path());
        let args = vec![
            "backup".to_string(),
            "--profile".to_string(),
            "prod".to_string(),
        ];

        let id = store
            .start(start_spec("backup", Some("prod"), &args))
            .await
            .unwrap();
        store
            .append_log(&id, LogStream::Stderr, "dumping collection users")
            .await
            .unwrap();
        store
            .finish(&id, JobOutcome::SucceededWithWarnings)
            .await
            .unwrap();

        // 인덱스: start + end 두 줄(로그 줄은 오지 않는다).
        let index = json_lines(&store.index_path());
        assert_eq!(index.len(), 2, "인덱스 줄 수가 다르다: {index:?}");
        assert_eq!(index[0]["kind"], "start");
        assert_eq!(index[0]["schema"], JOB_HISTORY_SCHEMA);
        assert_eq!(index[0]["id"], id.to_string());
        assert_eq!(index[0]["command"], "backup");
        assert_eq!(index[0]["profile"], "prod");
        assert_eq!(index[0]["pid"], 4242);
        assert_eq!(index[0]["args_masked"], serde_json::json!(args));
        assert_eq!(index[1]["kind"], "end");
        assert_eq!(index[1]["outcome"], "succeeded-with-warnings");
        assert_eq!(index[1]["exit_code"], 4);

        // 잡별 로그: start + log + end 세 줄.
        let log = json_lines(&store.log_path(&id));
        assert_eq!(log.len(), 3);
        assert_eq!(log[1]["kind"], "log");
        assert_eq!(log[1]["stream"], "stderr");
        assert_eq!(log[1]["text"], "dumping collection users");
        // 로그 줄에는 schema/id를 싣지 않는다(모듈 헤더의 근거를 고정한다).
        assert!(log[1]["schema"].is_null());
        assert!(log[1]["id"].is_null());
        // 시각은 RFC3339로 파싱된다.
        chrono::DateTime::parse_from_rfc3339(log[1]["ts"].as_str().unwrap()).unwrap();
    }

    /// 여러 번 기록해도 기존 줄은 **바이트 단위로 불변**이다 — append-only 핵심 보장.
    #[tokio::test]
    async fn repeated_records_leave_existing_bytes_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::open(dir.path());
        let id = store.start(start_spec("verify", None, &[])).await.unwrap();

        let index_after_start = std::fs::read(store.index_path()).unwrap();
        let log_after_start = std::fs::read(store.log_path(&id)).unwrap();

        store
            .append_log(&id, LogStream::Stdout, "{}")
            .await
            .unwrap();
        store.finish(&id, JobOutcome::Succeeded).await.unwrap();

        let index_now = std::fs::read(store.index_path()).unwrap();
        let log_now = std::fs::read(store.log_path(&id)).unwrap();

        assert!(
            index_now.starts_with(&index_after_start),
            "인덱스 기존 바이트가 변경됐다 — append-only 위반"
        );
        assert!(
            log_now.starts_with(&log_after_start),
            "로그 기존 바이트가 변경됐다 — append-only 위반"
        );
        assert!(index_now.len() > index_after_start.len());
    }

    /// 잘린 마지막 줄이 있는 파일을 읽어도 나머지가 정상 파싱되고, 이어 쓴 줄은 오염되지
    /// 않는다(읽기·쓰기 양쪽 보호).
    #[tokio::test]
    async fn torn_last_line_is_skipped_and_does_not_corrupt_new_entries() {
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::open(dir.path());
        // 정상 잡 하나를 먼저 남긴다.
        let first = store.start(start_spec("list", None, &[])).await.unwrap();
        store.finish(&first, JobOutcome::Succeeded).await.unwrap();

        // 인덱스 끝에 개행 없이 잘린 줄을 붙인다(크래시 흔적).
        let torn = br#"{"kind":"start","schema":1,"id":"0190f0a2-1a2b-7c3d-8e4f-aab"#;
        {
            let mut f = OpenOptions::new()
                .append(true)
                .open(store.index_path())
                .unwrap();
            f.write_all(torn).unwrap();
        }

        // 읽기: 잘린 줄은 세어지고 나머지는 살아 있다.
        let history = store.list().await;
        assert_eq!(history.entries.len(), 1, "정상 항목이 유실됐다");
        assert_eq!(history.unreadable_lines, 1, "잘린 줄이 집계되지 않았다");
        assert_eq!(history.entries[0].id, first);

        // 쓰기: 새 잡을 기록하면 잘린 줄과 합쳐지지 않는다.
        let second = store.start(start_spec("status", None, &[])).await.unwrap();
        let raw = std::fs::read(store.index_path()).unwrap();
        assert!(
            raw.windows(torn.len()).any(|w| w == torn),
            "손상된 기존 바이트가 보존되지 않았다"
        );
        let history = store.list().await;
        assert_eq!(history.entries.len(), 2, "새 항목이 오염됐다");
        assert_eq!(history.entries[0].id, second, "최신순이 아니다");
        assert_eq!(history.unreadable_lines, 1);
    }

    /// 인덱스를 지워도 잡별 로그로부터 재구축된다.
    #[tokio::test]
    async fn index_can_be_rebuilt_after_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::open(dir.path());

        let mut ids = Vec::new();
        for command in ["backup", "verify", "prune"] {
            let id = store
                .start(start_spec(command, Some("prod"), &[]))
                .await
                .unwrap();
            store
                .append_log(&id, LogStream::Stderr, "noise")
                .await
                .unwrap();
            store.finish(&id, JobOutcome::Succeeded).await.unwrap();
            ids.push(id);
        }
        let before = store.list().await;
        assert_eq!(before.entries.len(), 3);

        std::fs::remove_file(store.index_path()).unwrap();
        let gone = store.list().await;
        assert!(gone.entries.is_empty(), "인덱스 없이 항목이 나왔다");
        assert!(!gone.index_present, "인덱스 부재가 보고되지 않았다");

        let report = store.rebuild_index().await.unwrap();
        assert_eq!(report.jobs, 3);
        assert_eq!(report.records, 6, "잡당 start+end 두 줄이어야 함");
        assert_eq!(report.unreadable_lines, 0);

        let after = store.list().await;
        assert_eq!(after.entries, before.entries, "재구축 결과가 원본과 다르다");
        // 재구축된 인덱스에는 로그 줄이 없다.
        for line in json_lines(&store.index_path()) {
            assert_ne!(line["kind"], "log", "로그 줄이 인덱스로 넘어갔다");
        }
    }

    /// 재구축은 기존 인덱스가 로그와 어긋날 때도 로그를 기준으로 갈아치운다.
    #[tokio::test]
    async fn rebuild_replaces_a_stale_index() {
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::open(dir.path());
        let id = store.start(start_spec("backup", None, &[])).await.unwrap();
        store.finish(&id, JobOutcome::Failed).await.unwrap();

        // 인덱스에 존재하지 않는 잡을 손으로 심는다(어긋난 상태).
        let ghost = JobId::generate();
        {
            let mut f = OpenOptions::new()
                .append(true)
                .open(store.index_path())
                .unwrap();
            let line = serde_json::to_string(&JobRecord::Start(StartRecord {
                schema: JOB_HISTORY_SCHEMA,
                id: ghost.to_string(),
                command: "restore".to_string(),
                profile: None,
                args_masked: Vec::new(),
                started_at: Utc::now(),
                pid: None,
            }))
            .unwrap();
            writeln!(f, "{line}").unwrap();
        }
        assert_eq!(store.list().await.entries.len(), 2);

        store.rebuild_index().await.unwrap();
        let entries = store.list().await.entries;
        assert_eq!(entries.len(), 1, "로그에 없는 항목이 남았다");
        assert_eq!(entries[0].id, id);
    }

    /// 로테이션 상한이 적용되고, 오래된 **로그 파일만** 지워진다(인덱스 항목은 남는다).
    #[tokio::test]
    async fn rotation_trims_old_log_files_but_keeps_index_entries() {
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::open(dir.path()).with_retain(3);

        let mut ids = Vec::new();
        for _ in 0..6 {
            let id = store.start(start_spec("backup", None, &[])).await.unwrap();
            store.finish(&id, JobOutcome::Succeeded).await.unwrap();
            ids.push(id);
        }

        // 최근 3건의 로그만 남는다.
        for id in &ids[..3] {
            assert!(
                !store.log_path(id).exists(),
                "오래된 로그가 지워지지 않았다: {id}"
            );
        }
        for id in &ids[3..] {
            assert!(store.log_path(id).exists(), "최근 로그가 지워졌다: {id}");
        }
        // 인덱스는 6건 전부 유지한다.
        let history = store.list().await;
        assert_eq!(history.entries.len(), 6, "인덱스 항목이 함께 지워졌다");
        assert_eq!(history.unreadable_lines, 0);

        // 인덱스 파일 자체는 절대 로테이션 대상이 아니다.
        assert!(store.index_path().exists());
    }

    /// 기본 상한은 200이고, `with_retain(0)`은 1로 올린다(전부 지우는 정책 금지).
    #[test]
    fn retain_defaults_and_floor() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(JobStore::attach(dir.path()).retain, DEFAULT_RETAIN_JOBS);
        assert_eq!(DEFAULT_RETAIN_JOBS, 200);
        assert_eq!(JobStore::attach(dir.path()).with_retain(0).retain, 1);
    }

    /// 로테이션으로 로그가 사라진 잡도 요약은 조회된다(로그는 "정리됨"으로 보고).
    #[tokio::test]
    async fn detail_of_rotated_job_keeps_summary_without_logs() {
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::open(dir.path()).with_retain(1);
        let old = store
            .start(start_spec("backup", Some("prod"), &[]))
            .await
            .unwrap();
        store.finish(&old, JobOutcome::Succeeded).await.unwrap();
        let _new = store.start(start_spec("verify", None, &[])).await.unwrap();

        assert!(!store.log_path(&old).exists(), "로테이션이 동작하지 않았다");
        let detail = store.detail(&old).await;
        assert!(!detail.log_file_present, "로그 부재가 보고되지 않았다");
        assert!(detail.logs.is_empty());
        let summary = detail.summary.expect("요약이 사라졌다");
        assert_eq!(summary.command, "backup");
        assert_eq!(summary.profile.as_deref(), Some("prod"));
        assert!(!summary.is_running());
    }

    /// 상태 디렉터리가 없거나 파일로 점유돼도 읽기가 패닉 없이 "이력 없음"을 낸다.
    #[tokio::test]
    async fn missing_or_broken_state_dir_reads_as_empty() {
        // 1) 존재하지 않는 경로.
        let store = JobStore::attach(Path::new("/nonexistent/x-backup-state-9f2b"));
        let history = store.list().await;
        assert!(history.entries.is_empty());
        assert!(!history.index_present);
        let detail = store.detail(&JobId::generate()).await;
        assert!(detail.summary.is_none());
        assert!(!detail.log_file_present);

        // 2) jobs 디렉터리 자리를 파일이 차지한 경우(ENOTDIR).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(JOBS_DIR_NAME), b"not a directory").unwrap();
        let store = JobStore::open(dir.path());
        assert!(store.list().await.entries.is_empty());
        // 쓰기는 실패를 **전파한다**(읽기와 달리 조용히 성공한 척하지 않는다).
        assert!(store.start(start_spec("backup", None, &[])).await.is_err());
    }

    /// 손상된 인덱스(JSON이 아닌 줄·미지 kind·잘못된 id)도 나머지를 살려서 읽는다.
    #[tokio::test]
    async fn corrupt_index_lines_are_counted_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::open(dir.path());
        let id = store.start(start_spec("backup", None, &[])).await.unwrap();
        store.finish(&id, JobOutcome::Succeeded).await.unwrap();

        {
            let mut f = OpenOptions::new()
                .append(true)
                .open(store.index_path())
                .unwrap();
            writeln!(f, "not json at all").unwrap();
            writeln!(f, r#"{{"kind":"nope","id":"x"}}"#).unwrap();
            writeln!(f, r#"{{"kind":"start","schema":1,"id":"../../etc/passwd","command":"c","profile":null,"args_masked":[],"started_at":"2026-07-25T00:00:00Z","pid":null}}"#).unwrap();
            writeln!(f, r#"{{"kind":"end","schema":1,"id":"0190f0a2-1a2b-7c3d-8e4f-aabbccddeeff","finished_at":"2026-07-25T00:00:00Z","outcome":"succeeded","exit_code":0}}"#).unwrap();
            // 빈 줄은 손상이 아니다.
            writeln!(f).unwrap();
        }

        let history = store.list().await;
        assert_eq!(history.entries.len(), 1, "정상 항목이 유실됐다");
        assert_eq!(
            history.unreadable_lines, 4,
            "손상 줄 집계가 다르다: {history:?}"
        );
    }

    /// 여러 잡을 동시에 기록해도 줄이 섞이지 않고 하나도 유실되지 않는다.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_recording_does_not_interleave() {
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        // 하나의 인스턴스를 공유한다 — 뮤텍스가 인스턴스에 붙어 있으므로 이것이 전제다
        // (모듈 헤더 "동시 append 안전성").
        let store = Arc::new(JobStore::open(dir.path()));
        const N: usize = 40;

        let mut handles = Vec::with_capacity(N);
        for i in 0..N {
            let store = Arc::clone(&store);
            handles.push(tokio::spawn(async move {
                let profile = format!("p{i}");
                let args = vec!["backup".to_string(), profile.clone()];
                let id = store
                    .start(JobStart {
                        command: "backup",
                        profile: Some(&profile),
                        args_masked: &args,
                        started_at: Utc::now(),
                        pid: Some(i as u32),
                    })
                    .await
                    .unwrap();
                for line in 0..5 {
                    store
                        .append_log(&id, LogStream::Stderr, &format!("{profile} line {line}"))
                        .await
                        .unwrap();
                }
                store.finish(&id, JobOutcome::Succeeded).await.unwrap();
                id
            }));
        }
        let mut ids = Vec::with_capacity(N);
        for h in handles {
            ids.push(h.await.unwrap());
        }

        let history = store.list().await;
        assert_eq!(history.unreadable_lines, 0, "줄이 섞였거나 손상됐다");
        assert_eq!(history.entries.len(), N, "항목이 유실됐다");
        let mut profiles: Vec<String> = history
            .entries
            .iter()
            .map(|e| e.profile.clone().unwrap())
            .collect();
        profiles.sort();
        let mut expected: Vec<String> = (0..N).map(|i| format!("p{i}")).collect();
        expected.sort();
        assert_eq!(profiles, expected);

        // 각 잡의 로그도 온전하다(5줄 + start + end).
        for id in &ids {
            let detail = store.detail(id).await;
            assert_eq!(detail.unreadable_lines, 0, "{id}의 로그가 손상됐다");
            assert_eq!(detail.logs.len(), 5, "{id}의 로그 줄 수가 다르다");
        }
    }

    /// 종료 결과가 [`JobOutcome`] 어휘로 그대로 저장된다 — 특히 4·5가 뭉개지지 않는다.
    #[tokio::test]
    async fn outcome_vocabulary_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::open(dir.path());
        let cases = [
            (JobOutcome::Succeeded, "succeeded", Some(0)),
            (
                JobOutcome::SucceededWithWarnings,
                "succeeded-with-warnings",
                Some(4),
            ),
            (JobOutcome::LockConflict, "lock-conflict", Some(5)),
            (JobOutcome::Failed, "failed", Some(1)),
            (JobOutcome::Signaled(15), "signaled", None),
        ];
        for (outcome, label, exit) in cases {
            let id = store.start(start_spec("backup", None, &[])).await.unwrap();
            store.finish(&id, outcome).await.unwrap();
            let summary = store.detail(&id).await.summary.unwrap();
            assert_eq!(summary.outcome.as_deref(), Some(label));
            assert_eq!(summary.exit_code, exit);
            assert!(!summary.is_running());
            assert!(summary.duration().is_some());
        }
    }

    /// 끝나지 않은 잡은 `is_running`이고 duration이 없다 — t12의 재부착 판정 입력이다.
    #[tokio::test]
    async fn unfinished_jobs_are_reported_as_running_with_identity() {
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::open(dir.path());
        let started_at = Utc::now();
        let id = store
            .start(JobStart {
                command: "backup",
                profile: Some("prod"),
                args_masked: &[],
                started_at,
                pid: Some(31337),
            })
            .await
            .unwrap();

        let entries = store.list().await.entries;
        assert_eq!(entries.len(), 1);
        let summary = &entries[0];
        assert!(summary.is_running());
        assert_eq!(summary.duration(), None);
        // 3중 대조 값이 그대로 보존된다(pid 단독으로는 신원이 아니다).
        assert_eq!(summary.pid, Some(31337));
        assert_eq!(summary.profile.as_deref(), Some("prod"));
        assert_eq!(
            summary.started_at.timestamp_millis(),
            started_at.timestamp_millis(),
            "started_at은 호출자가 관측한 값이 그대로 보존되어야 한다"
        );
        assert_eq!(summary.id, id);
    }

    /// 개행·제어문자·초장문이 섞인 로그도 한 줄 NDJSON으로 남고 그대로 되읽힌다.
    #[tokio::test]
    async fn multiline_and_hostile_log_text_stays_one_physical_line() {
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::open(dir.path());
        let id = store.start(start_spec("backup", None, &[])).await.unwrap();

        let nasty = "first\nsecond\r\n\u{0}\u{7}\u{1b}[31mred\u{1b}[0m {\"kind\":\"end\"}";
        let long = "A".repeat(20_000);
        store
            .append_log(&id, LogStream::Stderr, nasty)
            .await
            .unwrap();
        store
            .append_log(&id, LogStream::Stdout, &long)
            .await
            .unwrap();

        // 물리적으로는 start + 2줄이다(본문의 개행이 줄을 늘리지 않는다).
        assert_eq!(json_lines(&store.log_path(&id)).len(), 3);
        let detail = store.detail(&id).await;
        assert_eq!(detail.unreadable_lines, 0);
        assert_eq!(detail.logs.len(), 2);
        assert_eq!(detail.logs[0].text, nasty, "본문이 변형됐다");
        assert_eq!(detail.logs[1].text.len(), 20_000);
    }

    /// 로그 줄 상한을 넘으면 **앞쪽**을 버리고 버린 개수를 보고한다.
    #[tokio::test]
    async fn log_reading_is_bounded_and_reports_dropped_lines() {
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::open(dir.path());
        let id = store.start(start_spec("backup", None, &[])).await.unwrap();
        let total = MAX_LOG_LINES_KEPT + 5;
        for i in 0..total {
            store
                .append_log(&id, LogStream::Stderr, &format!("line {i}"))
                .await
                .unwrap();
        }
        let detail = store.detail(&id).await;
        assert_eq!(detail.logs.len(), MAX_LOG_LINES_KEPT);
        assert_eq!(detail.dropped_leading_logs, 5);
        // 남은 것은 뒤쪽이다 — 실패 원인은 끝에 있다.
        assert_eq!(
            detail.logs.last().unwrap().text,
            format!("line {}", total - 1)
        );
        assert_eq!(detail.logs[0].text, "line 5");
    }

    /// 모르는 스트림 종류는 줄을 버리지 않고 `Unknown`으로 접힌다.
    #[test]
    fn unknown_stream_token_folds_to_unknown_variant() {
        let line =
            r#"{"kind":"log","ts":"2026-07-25T00:00:00Z","stream":"future-stream","text":"hi"}"#;
        let record: JobRecord = serde_json::from_str(line).expect("줄이 버려졌다");
        match record {
            JobRecord::Log(log) => {
                assert_eq!(log.stream, LogStream::Unknown);
                assert_eq!(log.text, "hi");
            }
            other => panic!("log 레코드가 아니다: {other:?}"),
        }
        // 알려진 토큰은 그대로 왕복한다.
        for stream in [LogStream::Stdout, LogStream::Stderr, LogStream::Note] {
            let json = serde_json::to_string(&stream).unwrap();
            assert_eq!(json, format!("\"{}\"", stream.token()));
            assert_eq!(
                serde_json::from_str::<LogStream>(&json).unwrap(),
                stream,
                "{stream:?} 왕복 실패"
            );
        }
    }

    /// 읽기 경로는 파일시스템을 만들지 않는다 — GET 화면이 상태를 만들지 않는 성질.
    #[tokio::test]
    async fn attach_never_creates_anything() {
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::attach(dir.path());
        let _ = store.list().await;
        let _ = store.detail(&JobId::generate()).await;
        let _ = store.rotate().await;
        assert!(
            !store.dir().exists(),
            "읽기 전용 경로가 디렉터리를 만들었다"
        );
    }
}
