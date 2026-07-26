//! `GET /lock` — 현재 프로파일 락 보유 현황(읽기 전용, 인증 뒤 화면).
//!
//! ## 이 화면이 존재하는 이유
//! 웹이 띄운 자식과 cron이 띄운 CLI는 같은 파일 락([`crate::lock::file_lock`])을 공유한다
//! ([`crate::web::job::runner`] 모듈 헤더 "XDG_RUNTIME_DIR" 참고) — 그래서 둘은 같은
//! 프로파일에 동시에 못 돈다. 그 자체는 옳은 설계지만, 웹 화면만 보는 운영자에게는 "내가
//! 누른 버튼이 왜 exit 5(락 충돌)로 끊기는가"가 미스터리로 남는다. 이 화면은 그 미스터리를
//! 없앤다 — "지금 누가 잡고 있는가"를 보여준다. [`crate::web::job::exit`]의 락 충돌
//! 안내문이 이 경로([`LOCK_PATH`])를 가리킨다.
//!
//! ## 절대 락을 잡거나 풀지 않는다
//! 이 모듈은 [`crate::lock::file_lock::acquire`]·`acquire_in`을 **한 번도 부르지 않는다.**
//! 그 함수들은 "락이 없으면 내가 잡는다"는 부작용이 있고(원자적 `O_CREAT|O_EXCL` 생성),
//! stale로 판정되면 **파일을 지운다**(자동 회수). 조회 화면이 그 경로를 타면 열람 한 번이
//! 실제로 락을 뺏거나 다른 프로세스의 stale lock을 지워 버릴 수 있다 — "보기만 했는데
//! 상태가 바뀌었다"는 사고다. 그래서 이 모듈은 **읽기 전용 원시 함수만** 쓴다
//! ([`file_lock::lock_dir`]·[`file_lock::pid_alive`]·[`file_lock::hostname`]) — 어느 것도
//! 파일을 만들거나 지우지 않는다. 락 파일 자체는 [`std::fs::read`]로만 연다(쓰기 모드로
//! 열지 않는다).
//!
//! ## stale 판정을 다시 구현하지 않는다 — 그러나 표시 정책은 다르다
//! `file_lock.rs`의 `classify()`(private)는 **회수 여부**를 정하는 함수라 "충돌로
//! 처리하되 안내"(24시간 초과는 살아있어도 의심) 같은 운영 판단까지 섞여 있고, 애초에
//! 이 크레이트 밖에서 부를 수 없다. 이 화면은 회수를 하지 않으므로 그 정책을 가져올
//! 필요가 없다 — 여기서는 딱 두 가지 원시 사실만 본다: **pid가 살아있는가**, **호스트명이
//! 이 서버와 같은가**. 그 위에 이 모듈만의 표시 정책([`LockStatus`] 네 갈래)을 얹는다.
//! `classify()`의 로직을 복붙하지 않고, 그 파일이 이미 검증해 둔 원시 함수만 재사용한다 —
//! 락 재구현과 표시 정책은 다른 층이다.
//!
//! ## 네 가지 상태 — 그리고 "판정 불가"가 "락 없음"이 되면 안 되는 이유
//! - **없음(행 자체가 없음):** `.lock` 파일이 하나도 없다. 어떤 작업이든 즉시 시작할 수
//!   있다.
//! - **실행 중([`LockStatus::Held`]):** pid 생존 + 같은 호스트. 다른 인스턴스가 정상적으로
//!   돌고 있다는 뜻이다.
//! - **회수 가능([`LockStatus::Stale`]):** pid 부재 + 같은 호스트. 이전 프로세스가 락을
//!   풀지 못하고 죽었다는 뜻이다 — 다음 `acquire` 시도에서 `file_lock.rs`가 자동으로
//!   정리한다(이 화면은 그 정리를 수행하지 않는다. 관찰만 한다).
//! - **판정 불가([`LockStatus::Indeterminate`]):** 손상된 JSON, 0바이트, 시각이 미래이거나
//!   해석 불가, 또는 다른 호스트의 락. 넷 다 "이 파일이 무엇을 뜻하는지 확신할 수 없다"는
//!   공통점이 있다. **이걸 "락 없음"으로 보여주면 안 된다** — 운영자가 안전하다고 믿고
//!   같은 프로파일로 백업을 또 돌리면, 실제로 다른 무언가가 그 destination을 만지고 있을
//!   가능성을 배제하지 못한 채 이중으로 돌리게 된다. 그래서 네 원인 모두 배지가
//!   [`Level::Error`]로 뜨고, 문구가 "이것이 락 없음을 뜻하지 않는다"를 명시적으로 말한다
//!   ([`IndeterminateReason::explain`]). 락 디렉터리 자체를 못 읽는 경우
//!   ([`Scan::DirUnavailable`])도 같은 이유로 "락 없음"과 시각적으로 다른 화면을 낸다.
//!
//! ## 락 파일의 절대 경로는 화면에 싣지 않는다
//! `pid`·`hostname`은 운영 정보이므로 보여준다(누가 잡고 있는지 알아야 판단할 수 있다).
//! 하지만 락 파일의 **절대 경로**(`$XDG_RUNTIME_DIR/x-backup/<profile>.lock`)는 싣지
//! 않는다 — 그 경로의 존재 자체가 `XDG_RUNTIME_DIR`의 실제 값(예: `/run/user/1000`)을
//! 드러내고, 이는 `doctor.rs`가 config·state 경로를 감추는 것과 같은 판단이다(그 모듈
//! 헤더 "시크릿" 절 참조). 프로파일 이름은 이미 운영자가 CLI에서 늘 보는 식별자라 경로와
//! 다르게 취급한다 — 파일 시스템 읽기 실패 메시지([`IndeterminateReason::Unreadable`])도
//! `std::io::Error`의 `Display`만 담는데, 그 값은 원인(권한 없음 등)만 말하고 경로는
//! 포함하지 않는다(`std::fs::read`가 돌려주는 에러는 경로를 자동으로 얹지 않는다).
//!
//! ## `file_lock.rs`에 가한 유일한 변경 — 가시성만
//! [`file_lock::lock_dir`]·[`file_lock::pid_alive`]·[`file_lock::hostname`] 세 함수를
//! `pub(crate)`로 열었다(원래 크레이트 비공개 `fn`이었다). **로직은 한 글자도 바꾸지
//! 않았다** — 이 화면이 읽기 전용으로 필요로 하는 원시 정보(락 디렉터리 위치, pid 생존,
//! 이 호스트의 이름)를 크레이트 내부에서 재사용하기 위한 가시성 조정뿐이다. 대안(락 파일
//! 파싱·pid 생존 판정을 이 파일에서 다시 짜는 것)은 오히려 "파일 락을 재구현하지
//! 마라"는 원칙을 더 크게 어긴다 — 같은 판정 로직이 두 곳에 있으면 한쪽만 고쳐졌을 때
//! 조용히 어긋난다. `read_holder`/`lock_path`/`classify`는 건드리지 않았다 — 이 화면은
//! 그것들이 하는 일(보유자 판독을 단일 `Option`으로 뭉개는 것, stale 회수 결정)을 그대로
//! 쓰지 않고 스스로 네 가지 손상 원인을 구분해야 하기 때문이다(아래 [`classify_file`]).

use std::path::Path;

use axum::extract::State;
use chrono::{DateTime, Utc};
use maud::{html, Markup};
use serde::Deserialize;
use std::sync::Arc;

use crate::i18n::Lang;
use crate::lock::file_lock;
use crate::web::view::components::{self, Level};
use crate::web::view::layout;
use crate::web::ServeConfig;

/// 화면 경로. 라우터·[`crate::web::job::exit`]의 락 충돌 안내문·테스트가 이 상수를
/// 공유한다.
pub const LOCK_PATH: &str = "/lock";

/// `<title>`·화면 제목·내비게이션 라벨이 공유하는 이름. 기술용어이므로 영문 고정
/// ([`crate::i18n`] 규약).
pub const LOCK_TITLE: &str = "Lock";

// ---------------------------------------------------------------------------
// 락 파일 하나의 판독 결과 (순수 자료구조)
// ---------------------------------------------------------------------------

/// 락을 잡고 있(었)던 프로세스의 최소 정보 — `Held`·`Stale` 공통.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LockHolder {
    /// 락 파일에 기록된 pid.
    pid: u32,
    /// 지금(수집 시각) 기준 경과 초. 음수가 될 수 없다 — 미래 시각은 [`classify_file`]이
    /// 이 구조체를 만들기 전에 [`LockStatus::Indeterminate`]로 걸러낸다.
    age_secs: i64,
}

/// 락 파일 하나를 읽은 결과 — 화면이 그릴 최종 상태.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LockStatus {
    /// pid 생존 + 같은 호스트. 다른 인스턴스가 정상 실행 중이다.
    Held(LockHolder),
    /// pid 부재 + 같은 호스트. 죽은 프로세스의 잔재 — 다음 acquire에서 자동 회수된다.
    Stale(LockHolder),
    /// 이 파일이 무엇을 뜻하는지 확신할 수 없다(모듈 헤더 "판정 불가" 참조).
    Indeterminate(IndeterminateReason),
}

/// [`LockStatus::Indeterminate`]의 구체적 사유 — 손상 4종 + 읽기 실패.
///
/// 네 가지(`Empty`·`Malformed`·`ImplausibleTimestamp`·`ForeignHost`)는 t13 지시서가 명시한
/// 손상 표본과 1:1로 맞춘다. `Unreadable`은 그 넷에는 없지만 `std::fs::read` 자체가 실패할
/// 수 있으므로(권한·경합) 다섯 번째로 추가했다 — 패닉 없이 처리해야 하는 입력의 범위를
/// "명시된 넷"으로 좁히면 실제 운영 중 마주칠 다섯 번째 사례에서 다시 패닉 위험이 생긴다.
#[derive(Debug, Clone, PartialEq, Eq)]
enum IndeterminateReason {
    /// `std::fs::read`가 실패했다(권한·경합 등). 에러 문자열은 경로를 포함하지 않는다.
    Unreadable(String),
    /// 파일이 0바이트다.
    Empty,
    /// JSON이 아니거나 [`crate::lock::LockData`] 모양이 아니다.
    Malformed,
    /// `started_at`을 RFC3339로 해석할 수 없거나, 해석은 됐지만 미래 시각이다.
    ImplausibleTimestamp,
    /// 다른 호스트명의 락이다 — 이 호스트에서는 pid 생존을 판정할 수 없다.
    ForeignHost(String),
}

impl IndeterminateReason {
    /// 오류 화면에 그대로 들어가는 설명 문장. **모든 분기가 "판정 불가 ≠ 락 없음"을
    /// 먼저 말한다** — 이 문장 하나가 이 파일 전체의 이유다(모듈 헤더 참조).
    fn explain(&self, lang: Lang) -> String {
        let prefix = lang.sel(
            "Undetermined — a lock file exists but its status cannot be verified. \
             This does NOT mean the profile is free.",
            "판정 불가 — 락 파일은 있지만 상태를 확인할 수 없습니다. \
             프로파일이 비어 있다는 뜻이 아닙니다.",
        );
        let cause = match self {
            IndeterminateReason::Unreadable(detail) => format!(
                "{} {detail}",
                lang.sel(
                    "Could not read the lock file:",
                    "락 파일을 읽을 수 없습니다:"
                )
            ),
            IndeterminateReason::Empty => lang
                .sel(
                    "The lock file is empty (0 bytes) — likely caught mid-write or truncated.",
                    "락 파일이 비어 있습니다(0바이트) — 쓰는 도중이었거나 잘렸을 수 있습니다.",
                )
                .to_string(),
            IndeterminateReason::Malformed => lang
                .sel(
                    "The lock file is not valid JSON in the expected shape.",
                    "락 파일이 기대한 모양의 JSON이 아닙니다.",
                )
                .to_string(),
            IndeterminateReason::ImplausibleTimestamp => lang
                .sel(
                    "The recorded start time could not be parsed, or is in the future — the \
                     file cannot be trusted.",
                    "기록된 시작 시각을 해석할 수 없거나 미래입니다 — 이 파일을 신뢰할 수 \
                     없습니다.",
                )
                .to_string(),
            IndeterminateReason::ForeignHost(host) => format!(
                "{} '{host}'.",
                lang.sel("Held by a different host:", "다른 호스트가 보유:")
            ),
        };
        format!("{prefix} {cause}")
    }
}

/// 락 디렉터리 안의 파일 하나 = 한 프로파일.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LockRow {
    /// 프로파일 이름(파일명에서 `.lock`을 뗀 것 — [`crate::lock::file_lock::lock_path`]가
    /// 이미 경로 구분자를 무해화해 뒀으므로 그대로 표시해도 안전하다).
    profile: String,
    status: LockStatus,
}

/// 락 디렉터리 전체를 훑은 결과.
#[derive(Debug)]
enum Scan {
    /// 디렉터리를 읽었다(비어 있을 수 있다 — 그러면 락이 하나도 없다는 뜻이다).
    Rows(Vec<LockRow>),
    /// 디렉터리 자체를 읽지 못했다. **"락 없음"과 다르다** — 모듈 헤더 참조.
    DirUnavailable(String),
}

// ---------------------------------------------------------------------------
// 수집 — 이 모듈의 유일한 부작용 지점 (읽기 전용)
// ---------------------------------------------------------------------------

/// [`file_lock::lock_dir`]를 스캔한다.
async fn collect() -> Scan {
    collect_in(&file_lock::lock_dir())
}

/// [`collect`]의 본체 — 디렉터리를 주입받아 테스트가 임시 디렉터리로 검증할 수 있게
/// 분리했다(`file_lock.rs`의 `acquire`/`acquire_in` 분리와 같은 패턴).
fn collect_in(dir: &Path) -> Scan {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        // 디렉터리가 없다 = 지금까지 아무도 락을 잡은 적이 없다 = 정상적인 "락 없음".
        // acquire_in()도 같은 경로에서 create_dir_all로 처음 만든다 — 존재하지 않는
        // 것은 손상이 아니라 "아직 아무도 쓰지 않은 상태"다.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Scan::Rows(Vec::new()),
        Err(e) => return Scan::DirUnavailable(e.to_string()),
    };

    let now_host = file_lock::hostname();
    let mut rows = Vec::new();
    for entry in entries {
        // 개별 entry 하나를 못 읽어도(경합 등) 디렉터리 전체를 못 읽은 것으로 취급하지
        // 않는다 — 다른 락 파일들은 여전히 유효한 정보다.
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("lock") {
            continue; // 락 디렉터리에 다른 파일이 섞여 있어도 무시한다.
        }
        let profile = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        rows.push(LockRow {
            profile,
            status: classify_file(&path, &now_host),
        });
    }
    rows.sort_by(|a, b| a.profile.cmp(&b.profile));
    Scan::Rows(rows)
}

/// 락 파일 하나를 읽어 [`LockStatus`]로 접는다 — **패닉 없이**, 어떤 바이트가 와도 값을
/// 돌려준다.
///
/// 판정 순서가 중요하다: 시각의 신뢰성(파싱 가능·과거)을 먼저 보고, 그 다음 호스트를
/// 보고, **같은 호스트일 때만** pid 생존을 확인한다. `file_lock.rs`의 `classify()`가
/// 호스트를 pid 생존보다 먼저 보는 것과 같은 순서다 — 다른 호스트의 pid는 이 머신의 pid
/// 네임스페이스와 무관하므로, 먼저 걸러내지 않으면 우연히 같은 번호의 로컬 프로세스를
/// 검사해 틀린 답을 낼 수 있다.
fn classify_file(path: &Path, now_host: &str) -> LockStatus {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => return LockStatus::Indeterminate(IndeterminateReason::Unreadable(e.to_string())),
    };
    if bytes.is_empty() {
        return LockStatus::Indeterminate(IndeterminateReason::Empty);
    }
    let data: LockDataView = match serde_json::from_slice(&bytes) {
        Ok(d) => d,
        Err(_) => return LockStatus::Indeterminate(IndeterminateReason::Malformed),
    };

    let Some(started) = parse_started_at(&data.started_at) else {
        return LockStatus::Indeterminate(IndeterminateReason::ImplausibleTimestamp);
    };
    let age_secs = Utc::now().signed_duration_since(started).num_seconds();
    if age_secs < 0 {
        // 시작 시각이 미래다 — 시계 왜곡이든 손상이든, 이 기록은 신뢰할 수 없다.
        return LockStatus::Indeterminate(IndeterminateReason::ImplausibleTimestamp);
    }

    if data.hostname != now_host {
        return LockStatus::Indeterminate(IndeterminateReason::ForeignHost(data.hostname));
    }

    let holder = LockHolder {
        pid: data.pid,
        age_secs,
    };
    if file_lock::pid_alive(data.pid) {
        LockStatus::Held(holder)
    } else {
        LockStatus::Stale(holder)
    }
}

/// [`crate::lock::LockData`]와 같은 모양의 역직렬화 전용 뷰.
///
/// `LockData`를 직접 쓰지 않는 이유: 그 타입을 그대로 쓰면 필드가 늘어날 때(예: 다른
/// 태스크가 진단용 필드를 추가) 이 파일이 조용히 그 필드를 무시하며 컴파일이 계속
/// 통과한다. 여기서 쓰는 필드(`pid`·`started_at`·`hostname`)만 명시적으로 뽑아 두면,
/// `LockData`의 필드 이름이 바뀌었을 때(예: `started_at` → `acquired_at`) 이 구조체가
/// 역직렬화에 실패하는 것이 아니라 **`serde(deny_unknown_fields)` 없이도 필드명 자체가
/// 안 맞아 파싱이 깨져** 드러난다. `profile` 필드는 쓰지 않는다 — 프로파일 이름은 파일명
/// (이미 무해화됨)에서 얻는 것이 더 신뢰할 수 있다(모듈 파일이 손상됐을 때도 파일명은
/// 여전히 유효하다).
#[derive(Debug, Deserialize)]
struct LockDataView {
    pid: u32,
    started_at: String,
    hostname: String,
}

/// RFC3339 `started_at`을 파싱한다. 실패하면 `None`(파싱 불가 = 판정 불가).
fn parse_started_at(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

// ---------------------------------------------------------------------------
// 렌더 (순수 함수)
// ---------------------------------------------------------------------------

/// `GET /lock` 핸들러. 인증은 상위 라우터가 이미 처리했다고 가정한다(모듈 헤더
/// "인증 뒤 화면" — 배선은 `server.rs`의 몫이다).
pub async fn page(State(ctx): State<Arc<ServeConfig>>) -> Markup {
    let scan = collect().await;
    let body = render(ctx.lang, &scan);
    layout::shell(ctx.lang, LOCK_TITLE, body)
}

/// 화면 본문을 만든다 — **순수 함수**(자식도, 파일시스템도 건드리지 않는다. `Scan`은 이미
/// 수집이 끝난 값이다).
fn render(lang: Lang, scan: &Scan) -> Markup {
    let subtitle = lang.sel(
        "Who currently holds a profile lock — read-only; this screen never acquires or \
         releases anything.",
        "지금 어떤 프로파일 락을 누가 잡고 있는지 — 읽기 전용이며 이 화면은 락을 잡거나 \
         풀지 않습니다.",
    );
    html! {
        (components::page_head(LOCK_TITLE, Some(subtitle)))
        @match scan {
            Scan::DirUnavailable(detail) => {
                (components::notice(Level::Error, lang.sel("Could not list locks", "락 목록을 읽을 수 없습니다"), html! {
                    p {
                        (lang.sel(
                            "The lock directory could not be read. This does not mean no locks \
                             are held — it means the status is unknown.",
                            "락 디렉터리를 읽을 수 없습니다. '락이 없다'는 뜻이 아니라 '상태를 \
                             모른다'는 뜻입니다.",
                        ))
                    }
                    p class="muted" { (detail) }
                }))
            }
            Scan::Rows(rows) => {
                @if rows.is_empty() {
                    (components::notice(Level::Ok, lang.sel("No locks held", "잡힌 락 없음"), html! {
                        p {
                            (lang.sel(
                                "No profile is currently locked. Backup, restore, and prune can \
                                 all start immediately.",
                                "현재 잠긴 프로파일이 없습니다. 백업·복구·정리를 바로 시작할 \
                                 수 있습니다.",
                            ))
                        }
                    }))
                } @else {
                    (summary(rows))
                    (rows_table(lang, rows))
                }
            }
        }
    }
}

/// 상단 요약 — "몇 개나 잡혀 있고, 그중 몇 개가 정상/의심스러운가"를 한눈에.
fn summary(rows: &[LockRow]) -> Markup {
    let mut held = 0usize;
    let mut stale = 0usize;
    let mut indeterminate = 0usize;
    for row in rows {
        match row.status {
            LockStatus::Held(_) => held += 1,
            LockStatus::Stale(_) => stale += 1,
            LockStatus::Indeterminate(_) => indeterminate += 1,
        }
    }
    components::meta_list(&[
        ("locks", rows.len().to_string()),
        ("held", held.to_string()),
        ("stale", stale.to_string()),
        ("indeterminate", indeterminate.to_string()),
    ])
}

/// 락 행 표. 행마다 `data-level`을 실어 CSS가 행 전체를 물들일 수 있게 한다
/// (`doctor.rs`의 `items_table`과 같은 패턴).
fn rows_table(lang: Lang, rows: &[LockRow]) -> Markup {
    html! {
        div class="dtable-scroll" {
            table class="dtable" {
                thead {
                    tr {
                        th scope="col" { "Status" }
                        th scope="col" { "Profile" }
                        th scope="col" { "Holder" }
                        th scope="col" { "Age" }
                        th scope="col" { "Detail" }
                    }
                }
                tbody {
                    @for row in rows {
                        @let level = level_of(&row.status);
                        tr data-level=(level.token()) {
                            td { (components::badge(level)) }
                            td class="mono key" { (row.profile) }
                            td class="mono" { (holder_text(&row.status)) }
                            td class="mono" { (age_text(&row.status)) }
                            td class="msg" { (detail_text(lang, &row.status)) }
                        }
                    }
                }
            }
        }
    }
}

/// 상태를 화면 레벨로 접는다.
///
/// `Held`가 [`Level::Warn`]인 이유: 락 그 자체는 이상 상태가 아니다(다른 인스턴스가
/// 정상적으로 일하고 있을 뿐이다) — 그러나 "이 프로파일로 지금 뭔가를 시작하면 exit
/// 5로 끊긴다"는 것을 운영자가 알아야 하므로 `Ok`가 아니라 `Warn`이다. `Stale`이
/// [`Level::Fail`]인 이유: 이전 프로세스가 락을 정상적으로 풀지 못하고 죽었다는 뜻이라
/// 원인(비정상 종료)을 살펴볼 가치가 있다 — 시스템이 다음 실행에서 자동으로 정리하긴
/// 하지만, "왜 죽었는가"는 이 화면이 답할 수 없는 질문이라 눈에 띄게 표시한다.
fn level_of(status: &LockStatus) -> Level {
    match status {
        LockStatus::Held(_) => Level::Warn,
        LockStatus::Stale(_) => Level::Fail,
        LockStatus::Indeterminate(_) => Level::Error,
    }
}

/// "누가"(pid) 열. 판정 불가 상태는 pid 자체를 신뢰할 수 없으므로 자리표시자만 낸다.
fn holder_text(status: &LockStatus) -> String {
    match status {
        LockStatus::Held(h) | LockStatus::Stale(h) => format!("pid {}", h.pid),
        LockStatus::Indeterminate(_) => "-".to_string(),
    }
}

/// "얼마나 오래" 열. 초 단위를 사람이 스캔하기 쉬운 `d`/`h`/`m`/`s` 표기로 접는다.
/// 라벨이므로 언어와 무관하게 같은 표기를 쓴다(`meta_list`의 키가 항상 영문인 것과 같은
/// 판단 — 숫자+단위 축약형은 번역할 대상이 아니다).
fn age_text(status: &LockStatus) -> String {
    match status {
        LockStatus::Held(h) | LockStatus::Stale(h) => format!("{} ago", format_age(h.age_secs)),
        LockStatus::Indeterminate(_) => "-".to_string(),
    }
}

/// 초를 `1d2h`/`3h12m`/`5m`/`9s` 형태로 접는다. 음수(미래 시각)는 이 함수에 도달하지
/// 않는다 — [`classify_file`]이 그 경우를 이미 `Indeterminate`로 걸렀다.
fn format_age(secs: i64) -> String {
    let secs = u64::try_from(secs).unwrap_or(0);
    if secs < 60 {
        return format!("{secs}s");
    }
    let minutes = secs / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let hours = minutes / 60;
    let rem_minutes = minutes % 60;
    if hours < 24 {
        return format!("{hours}h{rem_minutes}m");
    }
    let days = hours / 24;
    let rem_hours = hours % 24;
    format!("{days}d{rem_hours}h")
}

/// "무엇을 해야 하나" 열 — 상태별 안내문. [`IndeterminateReason::explain`]은 항상
/// "판정 불가 ≠ 락 없음"을 먼저 말한다.
fn detail_text(lang: Lang, status: &LockStatus) -> String {
    match status {
        LockStatus::Held(_) => lang
            .sel(
                "Running — wait for it to finish before starting the same profile.",
                "실행 중 — 같은 프로파일 작업을 시작하려면 끝나기를 기다리세요.",
            )
            .to_string(),
        LockStatus::Stale(_) => lang
            .sel(
                "Reclaimable — the holding process no longer exists. The next run on this \
                 profile will clear it automatically; no action is required here.",
                "회수 가능 — 보유 프로세스가 더 이상 존재하지 않습니다. 이 프로파일의 다음 \
                 실행에서 자동으로 정리됩니다 — 여기서 할 일은 없습니다.",
            )
            .to_string(),
        LockStatus::Indeterminate(reason) => reason.explain(lang),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock::LockData;

    fn write_lock(dir: &Path, profile: &str, data: &LockData) {
        std::fs::write(
            dir.join(format!("{profile}.lock")),
            serde_json::to_vec_pretty(data).unwrap(),
        )
        .unwrap();
    }

    fn sample_data(pid: u32, hostname: &str) -> LockData {
        // `LockData`는 `crate::lock`이 이미 공개해 둔 타입이다(pub 필드) — 시험용
        // 레코드를 직접 조립할 수 있다. `profile` 필드는 이 파일이 읽지 않으므로
        // 아무 값이나 넣는다.
        LockData {
            pid,
            started_at: Utc::now().to_rfc3339(),
            profile: "unused-in-this-file".to_string(),
            hostname: hostname.to_string(),
        }
    }

    // ---- 수집: 없음 / 실행 중 / stale 3가지 구분 ----------------------------------

    /// 디렉터리가 아예 없으면(락을 한 번도 잡은 적 없음) 빈 목록이다 — 오류가 아니다.
    #[test]
    fn missing_directory_is_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist-yet");
        match collect_in(&missing) {
            Scan::Rows(rows) => assert!(rows.is_empty()),
            Scan::DirUnavailable(e) => panic!("디렉터리 부재를 오류로 취급했다: {e}"),
        }
    }

    /// 살아있는 락(이 테스트 프로세스 자신의 pid)과 죽은 락(존재하지 않는 pid)이 서로
    /// 다른 상태로 갈린다 — DoD "락 없음/실행 중/stale 3가지 구분"의 핵심.
    #[test]
    fn held_and_stale_locks_are_distinguished() {
        let tmp = tempfile::tempdir().unwrap();
        let host = file_lock::hostname();
        write_lock(
            tmp.path(),
            "alive-profile",
            &sample_data(std::process::id(), &host),
        );
        write_lock(
            tmp.path(),
            "dead-profile",
            &sample_data(u32::MAX - 1, &host),
        );

        let Scan::Rows(rows) = collect_in(tmp.path()) else {
            panic!("스캔이 실패했다")
        };
        assert_eq!(rows.len(), 2);
        let alive = rows.iter().find(|r| r.profile == "alive-profile").unwrap();
        let dead = rows.iter().find(|r| r.profile == "dead-profile").unwrap();
        assert!(
            matches!(alive.status, LockStatus::Held(_)),
            "{:?}",
            alive.status
        );
        assert!(
            matches!(dead.status, LockStatus::Stale(_)),
            "{:?}",
            dead.status
        );
        assert_ne!(level_of(&alive.status), level_of(&dead.status));
    }

    /// 렌더된 화면에서도 없음/실행 중/stale 셋이 서로 다른 `data-level`로 나간다.
    #[test]
    fn rendered_page_distinguishes_three_states() {
        // 1) 없음.
        let empty = render(Lang::En, &Scan::Rows(Vec::new())).into_string();
        assert!(empty.contains("No locks held"));
        assert!(!empty.contains("data-level=\"warn\""));
        assert!(!empty.contains("data-level=\"fail\""));

        // 2) 실행 중 + stale이 섞인 화면.
        let host = file_lock::hostname();
        let rows = vec![
            LockRow {
                profile: "a".to_string(),
                status: LockStatus::Held(LockHolder {
                    pid: 1,
                    age_secs: 5,
                }),
            },
            LockRow {
                profile: "b".to_string(),
                status: LockStatus::Stale(LockHolder {
                    pid: 2,
                    age_secs: 5,
                }),
            },
        ];
        let _ = host; // 이 렌더 테스트는 상태 값을 직접 조립하므로 hostname은 쓰지 않는다.
        let out = render(Lang::En, &Scan::Rows(rows)).into_string();
        assert!(out.contains(r#"data-level="warn""#), "실행 중 배지 누락");
        assert!(out.contains(r#"data-level="fail""#), "stale 배지 누락");
    }

    // ---- 손상 4종: 패닉 없이 판정 불가 ---------------------------------------------

    /// 깨진 JSON은 패닉 없이 판정 불가로 접힌다.
    #[test]
    fn corrupt_json_is_indeterminate_without_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("p.lock");
        std::fs::write(&path, b"not-json{{").unwrap();
        let status = classify_file(&path, "any-host");
        assert!(matches!(
            status,
            LockStatus::Indeterminate(IndeterminateReason::Malformed)
        ));
    }

    /// 0바이트 파일도 패닉 없이 판정 불가로 접힌다.
    #[test]
    fn zero_byte_file_is_indeterminate_without_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("p.lock");
        std::fs::write(&path, b"").unwrap();
        let status = classify_file(&path, "any-host");
        assert!(matches!(
            status,
            LockStatus::Indeterminate(IndeterminateReason::Empty)
        ));
    }

    /// 미래 시각도 패닉 없이 판정 불가로 접힌다(pid는 살아있어도).
    #[test]
    fn future_timestamp_is_indeterminate_without_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("p.lock");
        let host = file_lock::hostname();
        let mut data = sample_data(std::process::id(), &host);
        data.started_at = (Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        std::fs::write(&path, serde_json::to_vec_pretty(&data).unwrap()).unwrap();

        let status = classify_file(&path, &host);
        assert!(
            matches!(
                status,
                LockStatus::Indeterminate(IndeterminateReason::ImplausibleTimestamp)
            ),
            "{status:?}"
        );
    }

    /// 다른 호스트명도 패닉 없이 판정 불가로 접힌다 — **stale이 아니다**(이 호스트에서는
    /// 그 pid의 생존을 판정할 수 없으므로, 죽었다고 단정하면 안 된다).
    #[test]
    fn foreign_host_is_indeterminate_not_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("p.lock");
        let data = sample_data(u32::MAX - 1, "some-other-host-xyz");
        std::fs::write(&path, serde_json::to_vec_pretty(&data).unwrap()).unwrap();

        let status = classify_file(&path, &file_lock::hostname());
        match status {
            LockStatus::Indeterminate(IndeterminateReason::ForeignHost(host)) => {
                assert_eq!(host, "some-other-host-xyz");
            }
            other => panic!("다른 호스트 락이 판정 불가로 접히지 않았다: {other:?}"),
        }
    }

    /// 손상 4종 전부가 화면에서 "판정 불가"를 말하고, **"락 없음"으로 오해될 문구를
    /// 담지 않는다** — t13 지시서의 핵심 요구사항.
    #[test]
    fn all_four_corruptions_say_undetermined_not_no_lock() {
        let cases = [
            IndeterminateReason::Empty,
            IndeterminateReason::Malformed,
            IndeterminateReason::ImplausibleTimestamp,
            IndeterminateReason::ForeignHost("other-host".to_string()),
        ];
        for reason in cases {
            for lang in [Lang::En, Lang::Ko] {
                let text = reason.explain(lang);
                match lang {
                    Lang::En => {
                        assert!(
                            text.to_lowercase().contains("undetermined")
                                || text.to_lowercase().contains("cannot"),
                            "{reason:?} en 설명에 판정 불가 문구가 없다: {text}"
                        );
                        assert!(
                            text.contains("does NOT mean the profile is free"),
                            "{reason:?} en 설명이 '락 없음 아님'을 말하지 않는다: {text}"
                        );
                    }
                    Lang::Ko => {
                        assert!(text.contains("판정 불가"), "{reason:?}: {text}");
                        assert!(
                            text.contains("비어 있다는 뜻이 아닙니다"),
                            "{reason:?} ko 설명이 '락 없음 아님'을 말하지 않는다: {text}"
                        );
                    }
                }
            }
        }
    }

    /// 렌더된 화면에서도 손상 행이 `error` 레벨로 뜨고, "판정 불가" 문구를 담는다(패닉
    /// 없이 끝까지 마크업이 만들어진다는 것 자체가 이 테스트의 요점이다).
    #[test]
    fn corrupted_row_renders_error_level_with_explanation() {
        let rows = vec![LockRow {
            profile: "broken".to_string(),
            status: LockStatus::Indeterminate(IndeterminateReason::Malformed),
        }];
        let out = render(Lang::Ko, &Scan::Rows(rows)).into_string();
        assert!(out.contains(r#"data-level="error""#));
        assert!(out.contains("판정 불가"));
        assert!(
            !out.contains("잡힌 락 없음"),
            "판정 불가가 '락 없음' 문구와 섞였다"
        );
    }

    /// 디렉터리 자체를 못 읽는 경우도 "락 없음"과 다른 화면을 낸다.
    #[test]
    fn dir_unavailable_is_not_rendered_as_no_locks() {
        let out = render(
            Lang::En,
            &Scan::DirUnavailable("permission denied".to_string()),
        )
        .into_string();
        assert!(out.contains("does not mean no locks"));
        assert!(!out.contains("No locks held"));
    }

    // ---- 개별 파일 읽기 실패(권한 등) ------------------------------------------------

    /// 파일 자체를 읽을 수 없는 경우(디렉터리를 파일 대신 준 경우로 흉내)도 패닉 없이
    /// 판정 불가로 접힌다.
    #[test]
    fn unreadable_path_is_indeterminate_without_panic() {
        let tmp = tempfile::tempdir().unwrap();
        // 파일이 아니라 디렉터리를 준다 — std::fs::read는 이걸 IsADirectory 오류로
        // 돌려준다(플랫폼에 따라 다른 오류일 수 있지만 항상 Err다).
        let dir_as_lock = tmp.path().join("weird.lock");
        std::fs::create_dir(&dir_as_lock).unwrap();
        let status = classify_file(&dir_as_lock, "any-host");
        assert!(
            matches!(
                status,
                LockStatus::Indeterminate(IndeterminateReason::Unreadable(_))
            ),
            "{status:?}"
        );
    }

    // ---- 렌더 안전성 ----------------------------------------------------------------

    /// 마크업에 인라인 스타일·색 리터럴이 없다(`components.rs`의
    /// `markup_carries_no_presentation`과 같은 계약).
    #[test]
    fn markup_carries_no_presentation() {
        let rows = vec![
            LockRow {
                profile: "a".to_string(),
                status: LockStatus::Held(LockHolder {
                    pid: 1,
                    age_secs: 5,
                }),
            },
            LockRow {
                profile: "b".to_string(),
                status: LockStatus::Indeterminate(IndeterminateReason::Malformed),
            },
        ];
        for out in [
            render(Lang::En, &Scan::Rows(Vec::new())).into_string(),
            render(Lang::En, &Scan::Rows(rows)).into_string(),
            render(Lang::En, &Scan::DirUnavailable("x".to_string())).into_string(),
        ] {
            assert!(!out.contains("style="), "인라인 스타일이 들어갔다: {out}");
            assert!(!out.contains('#'), "색 리터럴로 보이는 값이 있다: {out}");
        }
    }

    /// 적대적 프로파일 이름(파일명에서 온 문자열)이 이스케이프된다 — 파일명은 원칙적으로
    /// `lock_path()`가 무해화하지만, 이 파일은 그 보장에 기대지 않고 자체적으로도
    /// maud 자동 이스케이프에 맡긴다는 것을 고정한다.
    #[test]
    fn hostile_profile_name_is_escaped() {
        let rows = vec![LockRow {
            profile: "<script>alert(1)</script>".to_string(),
            status: LockStatus::Held(LockHolder {
                pid: 1,
                age_secs: 0,
            }),
        }];
        let out = render(Lang::En, &Scan::Rows(rows)).into_string();
        assert!(!out.contains("<script>"), "이스케이프되지 않았다: {out}");
        assert!(out.contains("&lt;script&gt;"));
    }

    /// 락 파일의 절대 경로가 화면 어디에도 없다 — 모듈 헤더 "절대 경로는 싣지 않는다".
    #[test]
    fn render_never_leaks_the_lock_file_path() {
        let tmp = tempfile::tempdir().unwrap();
        let host = file_lock::hostname();
        write_lock(tmp.path(), "p", &sample_data(std::process::id(), &host));
        let scan = collect_in(tmp.path());
        let out = render(Lang::En, &scan).into_string();
        assert!(
            !out.contains(&tmp.path().to_string_lossy().to_string()),
            "임시 디렉터리 경로가 화면에 샜다: {out}"
        );
        assert!(
            !out.contains(".lock"),
            "락 파일 확장자 흔적이 화면에 샜다: {out}"
        );
    }

    // ---- 경로 상수 ---------------------------------------------------------------

    /// 경로 상수가 `/`로 시작하는 절대 경로다(라우터에 그대로 등록할 수 있어야 한다).
    #[test]
    fn lock_path_is_an_absolute_route() {
        assert!(LOCK_PATH.starts_with('/'));
        assert_eq!(LOCK_TITLE, "Lock");
    }

    /// 나이 포맷팅이 경계에서 자연스럽게 넘어간다(59s → 1m, 59m → 1h0m 등 확인).
    #[test]
    fn format_age_rolls_over_at_boundaries() {
        assert_eq!(format_age(0), "0s");
        assert_eq!(format_age(59), "59s");
        assert_eq!(format_age(60), "1m");
        assert_eq!(format_age(3599), "59m");
        assert_eq!(format_age(3600), "1h0m");
        assert_eq!(format_age(86399), "23h59m");
        assert_eq!(format_age(86400), "1d0h");
        assert_eq!(format_age(90000), "1d1h");
    }
}
