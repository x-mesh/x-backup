//! 5필드 표준 cron 표현식 — 파싱과 "다음 실행 시각" 계산.
//!
//! ## 왜 크레이트를 쓰지 않고 직접 파싱하는가
//! `cron`·`croner` 같은 크레이트가 있지만 `Cargo.toml`에 의존성을 추가하는 것은 리더
//! 승인 사항이고, 우리가 실제로 필요한 문법은 아래 "지원 문법"의 좁은 집합이다. 그
//! 집합에 대해서는 파서가 100줄 남짓이고, 대신 **거부 이유를 한국어로 구체적으로 말할
//! 수 있다** — 운영자가 폼에 표현식을 잘못 적었을 때 "invalid cron"이 아니라 "분 필드의
//! 60은 범위를 넘습니다(0~59)"를 읽는다. 범용 크레이트에서 그 메시지를 얻으려면 결국
//! 우리가 다시 검증해야 한다.
//!
//! ## 지원 문법 — 그리고 무엇이 범위 밖인가
//! ```text
//! <분> <시> <일> <월> <요일>
//!  0-59 0-23 1-31 1-12 0-7   (요일 7 = 0 = 일요일)
//! ```
//! 각 필드에 쓸 수 있는 것: `*` · `5` · `5-9` · `*/15` · `5-20/3` · 그리고 이들을 쉼표로
//! 이은 목록(`0,15,30,45`). 별칭은 [`ALIAS_HOURLY`]·[`ALIAS_DAILY`] **둘만** 받는다.
//!
//! 범위 밖(PRD Q3 확정): 초 단위 6필드 · Quartz 확장(`L`·`W`·`#`·`?`) · `@weekly`·
//! `@monthly`·`@yearly`·`@reboot`. `@reboot`는 특히 의도적으로 뺐다 — "다음 실행 시각"이
//! 존재하지 않는 표현식이라 이 모듈의 계약(모든 유효 표현식은 다음 시각을 계산할 수
//! 있다)을 깨뜨린다.
//!
//! 월·요일의 영문 이름(`JAN`·`MON`)도 받지 않는다. 표준 cron에는 있지만 파싱 표면을
//! 두 배로 늘리고, 이름을 쓴 표현식은 **거부 메시지가 숫자를 알려주므로** 운영자가 한 번
//! 고치면 끝난다([`parse`]의 이름 감지 분기). 조용히 다르게 해석하는 것보다 낫다.
//!
//! ## 계산 기준은 UTC다 — DST를 다루는 방식
//! 이 모듈의 모든 시각은 [`DateTime<Utc>`]다. 로컬 시각으로 계산하면 DST 전환에서 두
//! 가지 사고가 구조적으로 발생한다:
//!
//! - **봄 전환(spring forward):** 02:00이 곧바로 03:00이 되어 `30 2 * * *`(매일 02:30)이
//!   가리키는 시각이 **그날 존재하지 않는다.** 그날의 백업이 조용히 누락된다.
//! - **가을 전환(fall back):** 01:00~02:00이 두 번 흐르므로 `30 1 * * *`이 **같은 날
//!   두 번** 맞는다. 같은 프로파일 백업이 두 번 떠서 뒤에 온 쪽이 파일 락에 걸려 exit 5로
//!   끊긴다 — 이력에는 "실패"처럼 쌓인다.
//!
//! UTC는 DST가 없으므로 두 사고가 **표현 자체로 불가능하다.** 윤초도 마찬가지다 — chrono의
//! [`DateTime<Utc>`]는 윤초를 별도 초로 세지 않고(TAI가 아니라 UTC-SLS 계열 표현), 이
//! 모듈은 애초에 분 단위로만 계산해 초를 0으로 고정하므로 윤초가 끼어들 자리가 없다.
//!
//! 대가는 정직하게 밝힌다: **운영자가 "매일 03:00(로컬)"을 기대하면 그 기대와 어긋난다.**
//! 그래서 이 어긋남을 코드가 아니라 **화면이** 해결한다 — [`crate::web::view::schedule`]은
//! 표현식 라벨을 `Cron (UTC)`로 찍고, 다음 실행 시각을 `UTC 표기 + 서버 로컬 표기` 두 줄로
//! 함께 보여준다. 어느 쪽이 기준인지 운영자가 화면에서 즉시 읽을 수 있어야 한다는 것이
//! 이 판단의 전제다.
//!
//! 로컬 시각 기준을 제대로 하려면 tz 데이터베이스(`chrono-tz`)가 필요하다. [`chrono::Local`]
//! 하나로는 프로세스가 읽은 tz가 런타임에 바뀔 수 있고(TZ 환경변수·tzdata 갱신), 그러면
//! **모든 스케줄의 실제 발화 시각이 조용히 한 시간 밀린다.** 새 의존성 없이 얻을 수 있는
//! 가장 안전한 답이 UTC다.
//!
//! ## 다음 실행 시각을 어떻게 찾는가 — 분 단위 순회를 하지 않는 이유
//! 순진하게 1분씩 전진하면 `0 0 29 2 *`(2월 29일) 같은 표현식에서 최악 8년치 ≈ 420만 번을
//! 돈다. 그래서 **날짜와 시각을 두 단계로 나눈다**: 먼저 월/일/요일이 맞는 날짜를 하루씩
//! 찾고(최대 [`MAX_DAYS_AHEAD`]일), 그 날 안에서만 시/분을 찾는다. 최악이 3300번 + 1440번이라
//! 마이크로초 규모다.

use std::fmt;

use chrono::{
    DateTime, Datelike, NaiveDate, NaiveDateTime, NaiveTime, TimeDelta, TimeZone, Timelike, Utc,
};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{Result, XBackupError};
use crate::i18n::Lang;

/// 표준 cron 필드 수(분·시·일·월·요일). 초 필드는 범위 밖(모듈 헤더).
pub const CRON_FIELDS: usize = 5;

/// 매시 정각 별칭 — `0 * * * *`로 확장된다.
pub const ALIAS_HOURLY: &str = "@hourly";

/// 매일 00:00(UTC) 별칭 — `0 0 * * *`로 확장된다.
pub const ALIAS_DAILY: &str = "@daily";

/// 표현식 문자열의 상한 — 폼에서 들어온 값이 파서에 도달하기 전 길이로 먼저 걸린다.
///
/// 128의 근거: 5필드 각각이 쉼표 목록을 최대한 늘려도(`0,1,2,...,59`는 그 자체로 170자지만
/// 그런 표현식은 `*/1`로 쓰는 것이 정상이다) 실전 표현식은 30자를 넘지 않는다. 상한이
/// 없으면 거부 메시지에 남의 긴 문자열을 그대로 담게 되고(화면에 반사된다), 파서가 쓸데없이
/// 큰 입력을 훑는다.
pub const MAX_EXPR_LEN: usize = 128;

/// 날짜 후보를 앞으로 몇 일까지 훑는지의 상한.
///
/// 3300일 ≈ 9.04년. 이 값이 왜 8년보다 커야 하는가: `0 0 29 2 *`(2월 29일)의 최대 간격은
/// **8년**이다(2096 → 2104 — 2100년은 400의 배수가 아니라 윤년이 아니다). 8년치 = 2922~2923일
//  이므로 여유를 얹어 3300으로 둔다. 이 상한에 걸리면 [`CronExpr::next_after`]가 `None`을
/// 돌려주고, 그 `None`이 곧 "이 표현식은 앞으로 실행되지 않는다"는 판정이다(`0 0 30 2 *`
/// 처럼 존재할 수 없는 날짜 조합이 여기서 걸린다 — 파서가 조합의 실현 가능성까지 검사하지
/// 않고 이 계산 한 곳에서 균일하게 잡는다).
const MAX_DAYS_AHEAD: u32 = 3_300;

/// 하루의 분 수 — 시:분 탐색 상한.
const MINUTES_PER_DAY: u32 = 24 * 60;

/// 한 시간의 분 수.
const MINUTES_PER_HOUR: u32 = 60;

/// 각 필드의 허용 범위. `(최소, 최대, 라벨)` — 라벨은 거부 메시지에 그대로 들어간다.
/// 두 언어 표기를 함께 드는 이름 — 필드 이름과 항목 부분 이름에 쓴다.
///
/// `job::args::Field`와 같은 이유로 짝을 타입에 묶는다: 한쪽만 고치는 실수를 컴파일 단계에서
/// 불가능하게 만든다(그 실수는 한 언어 화면에서만 드러나 조용히 살아남는다).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Name {
    en: &'static str,
    ko: &'static str,
}

impl Name {
    const fn new(en: &'static str, ko: &'static str) -> Self {
        Self { en, ko }
    }
    fn of(self, lang: Lang) -> &'static str {
        lang.sel(self.en, self.ko)
    }
}

// 항목 안의 부분 이름 — 어디가 잘못됐는지 지목하는 데 쓴다.
const PART_STEP: Name = Name::new("step", "간격(step)");
const PART_RANGE_START: Name = Name::new("range start", "범위 시작");
const PART_RANGE_END: Name = Name::new("range end", "범위 끝");
const PART_VALUE: Name = Name::new("value", "값");

const RANGE_MINUTE: (u32, u32, Name) = (0, 59, Name::new("minute", "분(minute)"));
const RANGE_HOUR: (u32, u32, Name) = (0, 23, Name::new("hour", "시(hour)"));
const RANGE_DOM: (u32, u32, Name) = (1, 31, Name::new("day-of-month", "일(day-of-month)"));
const RANGE_MONTH: (u32, u32, Name) = (1, 12, Name::new("month", "월(month)"));
/// 요일은 입력 7까지 받고 파싱 단계에서 0으로 접는다(일요일) — 표준 cron 관례.
const RANGE_DOW: (u32, u32, Name) = (0, 7, Name::new("day-of-week", "요일(day-of-week)"));

/// 요일 비트셋이 실제로 쓰는 상한(7을 0으로 접은 뒤).
const DOW_MAX_NORMALIZED: u32 = 6;

// ---------------------------------------------------------------------------
// 비트셋
// ---------------------------------------------------------------------------

/// 한 필드가 허용하는 값의 집합 — 0~59가 최대 범위이므로 [`u64`] 하나로 충분하다.
///
/// [`Vec<bool>`]이나 [`std::collections::BTreeSet`] 대신 비트셋을 쓰는 이유는 성능이
/// 아니라 **동등성**이다: "이 필드가 전체 범위인가"([`FieldSet::is_full`])가 정수 비교
/// 하나로 끝나고, 그 판정이 일/요일 OR 규칙의 입력이다(모듈 아래 [`CronExpr::matches_date`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FieldSet(u64);

impl FieldSet {
    /// 빈 집합.
    const fn empty() -> Self {
        Self(0)
    }

    /// 값 하나를 넣는다. 호출부가 범위를 이미 검사했다고 전제한다(`value < 64`).
    fn insert(&mut self, value: u32) {
        self.0 |= 1u64 << value;
    }

    /// 값이 들어 있는지.
    fn contains(self, value: u32) -> bool {
        self.0 & (1u64 << value) != 0
    }

    /// `min..=max` 전체를 담은 집합.
    fn full(min: u32, max: u32) -> Self {
        let mut set = Self::empty();
        for value in min..=max {
            set.insert(value);
        }
        set
    }

    /// 이 집합이 `min..=max` 전체와 같은지 — 일/요일 OR 규칙의 판정 입력.
    fn is_full(self, min: u32, max: u32) -> bool {
        self == Self::full(min, max)
    }

    /// `from` 이상인 가장 작은 원소(`max` 이하). 없으면 `None`.
    fn first_at_or_after(self, from: u32, max: u32) -> Option<u32> {
        (from..=max).find(|&value| self.contains(value))
    }
}

// ---------------------------------------------------------------------------
// 표현식
// ---------------------------------------------------------------------------

/// 검증을 통과한 cron 표현식.
///
/// 원문(`raw`)을 함께 들고 있는 이유: 화면과 저장 파일에는 **운영자가 적은 그대로**가
/// 보여야 한다. `@daily`를 저장했는데 화면에 `0 0 * * *`이 뜨면 "내가 적은 것이 아닌데"가
/// 되고, 그 순간 운영자는 콘솔이 표현식을 임의로 고쳤다고 의심한다. 정규화는 공백 정리
/// 까지만 한다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronExpr {
    /// 정규화된 원문(앞뒤 공백 제거, 필드 사이 공백 1칸). 직렬화·표시에 쓴다.
    raw: String,
    minute: FieldSet,
    hour: FieldSet,
    dom: FieldSet,
    month: FieldSet,
    /// 7을 0으로 접은 요일 집합(0=일요일).
    dow: FieldSet,
    /// 일 필드가 전체 범위가 **아닌지**. 요일과 함께 참이면 OR 규칙이 적용된다.
    dom_restricted: bool,
    /// 요일 필드가 전체 범위가 **아닌지**.
    dow_restricted: bool,
}

impl CronExpr {
    /// 문자열을 표현식으로 접는다. 실패는 모두 [`XBackupError::Usage`](exit 2 계열)다 —
    /// 운영자가 고칠 수 있는 입력 문제이고, 화면은 이 메시지를 400과 함께 그대로 보여준다.
    pub fn parse(input: &str, lang: Lang) -> Result<Self> {
        parse(input, lang)
    }

    /// 정규화된 원문.
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// `after` **이후** 첫 실행 시각(UTC). `after`와 같은 분은 제외한다.
    ///
    /// 같은 분을 제외하는 것이 이 함수의 핵심 계약이다: 스케줄러 루프가 발화 직후 다시
    /// 이 함수를 부르는데, 같은 분을 포함하면 **같은 시각이 다시 후보로 잡혀 1분 동안
    /// 반복 발화한다.** 그래서 시작점을 "분으로 자른 뒤 +1분"으로 고정한다.
    ///
    /// [`MAX_DAYS_AHEAD`] 안에 후보가 없으면 `None` — 그 `None`은 "이 표현식은 앞으로
    /// 실행되지 않는다"는 뜻이고, 생성 화면이 그 표현식을 거부하는 근거가 된다.
    pub fn next_after(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let floor = after
            .with_second(0)?
            .with_nanosecond(0)?
            .checked_add_signed(TimeDelta::minutes(1))?;
        let mut date = floor.date_naive();
        // 첫날만 "지금 이후"라는 하한이 걸린다. 다음 날부터는 00:00부터 전부 후보다.
        let mut floor_minutes = floor.hour() * MINUTES_PER_HOUR + floor.minute();

        for _ in 0..MAX_DAYS_AHEAD {
            if self.matches_date(date) {
                if let Some(minutes) = self.first_minute_at_or_after(floor_minutes) {
                    let time = NaiveTime::from_hms_opt(
                        minutes / MINUTES_PER_HOUR,
                        minutes % MINUTES_PER_HOUR,
                        0,
                    )?;
                    return Some(Utc.from_utc_datetime(&NaiveDateTime::new(date, time)));
                }
            }
            date = date.succ_opt()?;
            floor_minutes = 0;
        }
        None
    }

    /// 이 날짜가 월/일/요일 조건에 맞는지.
    ///
    /// ## 일·요일의 OR 규칙 — cron의 가장 유명한 함정
    /// 두 필드가 **모두** 제한되어 있으면 표준 cron은 `AND`가 아니라 `OR`로 판정한다.
    /// 예: `0 0 13 * 5`는 "13일**이거나** 금요일"이다(13일인 금요일이 아니다). 여기서
    /// AND를 쓰면 대부분의 표현식이 거의 발화하지 않게 되고, 그 침묵은 "백업이 조용히
    /// 멈춘다"는 이 도구에서 가장 나쁜 실패로 직결된다.
    ///
    /// 한쪽이라도 `*`(전체 범위)이면 그 필드는 판정에 영향을 주지 않으므로 AND로 접어도
    /// 결과가 같다 — 그래서 분기 조건이 "둘 다 제한됨"이다.
    fn matches_date(&self, date: NaiveDate) -> bool {
        if !self.month.contains(date.month()) {
            return false;
        }
        let dom_hit = self.dom.contains(date.day());
        // chrono의 `num_days_from_sunday()`가 곧 cron의 요일 번호(0=일요일)다.
        let dow_hit = self.dow.contains(date.weekday().num_days_from_sunday());
        if self.dom_restricted && self.dow_restricted {
            dom_hit || dow_hit
        } else {
            dom_hit && dow_hit
        }
    }

    /// 하루 안에서 `from`(자정부터의 분) 이상인 첫 시:분을 분 단위로 돌려준다.
    fn first_minute_at_or_after(&self, from: u32) -> Option<u32> {
        let start_hour = from / MINUTES_PER_HOUR;
        for hour in self.hour.first_at_or_after(start_hour, RANGE_HOUR.1)?..=RANGE_HOUR.1 {
            if !self.hour.contains(hour) {
                continue;
            }
            let minute_floor = from.saturating_sub(hour * MINUTES_PER_HOUR);
            if let Some(minute) = self.minute.first_at_or_after(minute_floor, RANGE_MINUTE.1) {
                let total = hour * MINUTES_PER_HOUR + minute;
                if total < MINUTES_PER_DAY {
                    return Some(total);
                }
            }
        }
        None
    }
}

impl fmt::Display for CronExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

/// 저장 파일에는 **문자열 하나**로 나간다 — 파싱된 비트셋을 직렬화하면 저장 형식이
/// 파서 내부 구현에 묶이고, 사람이 `schedules.json`을 열어 읽을 수도 없게 된다.
impl Serialize for CronExpr {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.raw)
    }
}

/// 읽을 때 다시 검증한다 — 사람이 파일을 손으로 고쳐 잘못된 표현식을 넣었으면 그 항목만
/// 실패하고([`crate::web::state::schedules`]가 항목별로 역직렬화한다) 나머지는 살아남는다.
impl<'de> Deserialize<'de> for CronExpr {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        // 여기서 나온 문장은 화면에 닿지 않는다 — 저장소가 항목별 실패를 `tracing::error!`로만
        // 남기고(`state::schedules`), 뷰는 실패 **개수**만 본다. 그래서 사용자 언어를
        // serde 경계까지 끌고 내려가지 않고 이 파일의 다른 로그 문구와 같은 언어를 쓴다.
        parse(&raw, Lang::Ko).map_err(D::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// 파싱
// ---------------------------------------------------------------------------

/// 표현식 하나를 파싱한다. 별칭은 여기서 5필드로 펼쳐지되 `raw`에는 원문이 남는다.
fn parse(input: &str, lang: Lang) -> Result<CronExpr> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(usage_sel(
            lang,
            "the cron expression is empty — write five fields \
             (`<min> <hour> <day-of-month> <month> <day-of-week>`) or an alias \
             (`@hourly`, `@daily`)."
                .to_string(),
            "cron 표현식이 비어 있습니다 — `<분> <시> <일> <월> <요일>` 5필드로 적거나 \
             별칭(`@hourly`·`@daily`)을 쓰세요."
                .to_string(),
        ));
    }
    if trimmed.len() > MAX_EXPR_LEN {
        let len = trimmed.len();
        return Err(usage_sel(
            lang,
            format!(
                "the cron expression is too long ({len} chars) — keep it at most {MAX_EXPR_LEN}."
            ),
            format!("cron 표현식이 너무 깁니다({len}자) — {MAX_EXPR_LEN}자 이하로 적으세요."),
        ));
    }

    // 별칭. 확장 결과를 그대로 재귀 파싱하지 않고 필드 문자열로 넘겨, `raw`가 원문을
    // 유지하게 한다.
    let (fields_source, raw) = if trimmed.starts_with('@') {
        let expanded = expand_alias(trimmed, lang)?;
        (expanded.to_string(), trimmed.to_string())
    } else {
        let fields: Vec<&str> = trimmed.split_whitespace().collect();
        if fields.len() != CRON_FIELDS {
            let n = fields.len();
            return Err(usage_sel(
                lang,
                format!(
                    "the cron expression has {n} fields — it must have exactly {CRON_FIELDS} \
                     (`<min> <hour> <day-of-month> <month> <day-of-week>`). Six-field \
                     (seconds) and Quartz extensions are not supported."
                ),
                format!(
                    "cron 표현식의 필드가 {n}개입니다 — 정확히 {CRON_FIELDS}개여야 합니다\
                     (`<분> <시> <일> <월> <요일>`). 초 단위 6필드와 Quartz 확장은 지원하지 \
                     않습니다."
                ),
            ));
        }
        (fields.join(" "), fields.join(" "))
    };

    let fields: Vec<&str> = fields_source.split_whitespace().collect();
    let minute = parse_field(fields[0], RANGE_MINUTE, lang)?;
    let hour = parse_field(fields[1], RANGE_HOUR, lang)?;
    let dom = parse_field(fields[2], RANGE_DOM, lang)?;
    let month = parse_field(fields[3], RANGE_MONTH, lang)?;
    let dow_raw = parse_field(fields[4], RANGE_DOW, lang)?;

    // 요일 7을 0으로 접는다(둘 다 일요일 — 표준 cron 관례). 접은 뒤에 "전체 범위인가"를
    // 판정해야 `0-7`·`*`이 모두 제한 없음으로 나온다.
    let mut dow = FieldSet::empty();
    for value in RANGE_DOW.0..=RANGE_DOW.1 {
        if dow_raw.contains(value) {
            dow.insert(if value == RANGE_DOW.1 { 0 } else { value });
        }
    }

    Ok(CronExpr {
        raw,
        minute,
        hour,
        dom,
        month,
        dow,
        dom_restricted: !dom.is_full(RANGE_DOM.0, RANGE_DOM.1),
        dow_restricted: !dow.is_full(RANGE_DOW.0, DOW_MAX_NORMALIZED),
    })
}

/// 별칭을 5필드 문자열로 펼친다. 지원하지 않는 별칭은 **어떤 것이 되는지 알려주며** 거부한다.
fn expand_alias(input: &str, lang: Lang) -> Result<&'static str> {
    match input {
        ALIAS_HOURLY => Ok("0 * * * *"),
        ALIAS_DAILY => Ok("0 0 * * *"),
        other => Err(usage_sel(
            lang,
            format!(
                "alias '{other}' is not supported — only `{ALIAS_HOURLY}` (top of every hour) \
                 and `{ALIAS_DAILY}` (00:00 UTC daily) are accepted. Write `@weekly`, \
                 `@monthly`, `@yearly` as five fields instead (e.g. Sunday 03:00 = \
                 `0 3 * * 0`). `@reboot` has no 'next run time', so it is not supported."
            ),
            format!(
                "별칭 '{other}'는 지원하지 않습니다 — `{ALIAS_HOURLY}`(매시 정각)와 \
                 `{ALIAS_DAILY}`(매일 00:00 UTC) 둘만 받습니다. `@weekly`·`@monthly`·`@yearly`는 \
                 5필드로 직접 적으세요(예: 매주 일요일 03:00 = `0 3 * * 0`). `@reboot`는 \
                 '다음 실행 시각'이 없어 지원하지 않습니다."
            ),
        )),
    }
}

/// 한 필드를 비트셋으로 접는다. `range`는 `(최소, 최대, 라벨)`.
fn parse_field(field: &str, range: (u32, u32, Name), lang: Lang) -> Result<FieldSet> {
    let (min, max, label) = range;
    let mut set = FieldSet::empty();
    // 빈 필드는 `split_whitespace`가 이미 걸러 냈지만, 쉼표 목록 안의 빈 항목(`1,,2`)은
    // 여기서 걸러야 한다.
    for term in field.split(',') {
        parse_term(term, field, min, max, label, &mut set, lang)?;
    }
    Ok(set)
}

/// 쉼표로 나뉜 항목 하나(`*`·`5`·`5-9`·`*/15`·`5-20/3`)를 비트셋에 얹는다.
fn parse_term(
    term: &str,
    field: &str,
    min: u32,
    max: u32,
    name: Name,
    set: &mut FieldSet,
    lang: Lang,
) -> Result<()> {
    let label = name.of(lang);
    if term.is_empty() {
        return Err(usage_sel(
            lang,
            format!(
                "the {label} field '{field}' has an empty item — check for a missing value in \
                 the comma list (e.g. `0,15,30`)."
            ),
            format!(
                "{label} 필드 '{field}'에 빈 항목이 있습니다 — 쉼표 목록에 값을 빠뜨렸는지 \
                 확인하세요(예: `0,15,30`)."
            ),
        ));
    }
    // 부호를 먼저 잡는다. `-5`를 그냥 흘려보내면 아래 범위 분해가 `("", "5")`로 갈라
    // "범위 시작이 비어 있습니다"라는 엉뚱한 진단이 나온다 — 사용자가 적은 것은 음수다.
    if term.starts_with('-') || term.starts_with('+') {
        return Err(usage_sel(
            lang,
            format!(
                "'{term}' in the {label} field '{field}' carries a sign — only digits 0-9 are \
                 allowed (there is no negative time)."
            ),
            format!(
                "{label} 필드 '{field}'의 '{term}'에 부호가 붙어 있습니다 — 0~9 숫자만 쓸 수 \
                 있습니다(음수 시각은 존재하지 않습니다)."
            ),
        ));
    }

    // `/` 뒤의 간격을 먼저 떼어 낸다.
    let (base, step) = match term.split_once('/') {
        Some((base, step_raw)) => {
            let step = parse_number(step_raw, field, name, PART_STEP, lang)?;
            if step == 0 {
                return Err(usage_sel(
                    lang,
                    format!(
                        "the step in the {label} field '{field}' is 0 — `*/0` would mean \
                         'every 0 minutes', which defines no run time. Use 1 or more."
                    ),
                    format!(
                        "{label} 필드 '{field}'의 간격이 0입니다 — `*/0`은 '0분마다'라는 뜻이 \
                         되어 실행 시각을 정의할 수 없습니다. 1 이상을 쓰세요."
                    ),
                ));
            }
            (base, step)
        }
        None => (term, 1),
    };

    let (from, to) = if base == "*" {
        (min, max)
    } else if let Some((lo_raw, hi_raw)) = base.split_once('-') {
        let lo = parse_number(lo_raw, field, name, PART_RANGE_START, lang)?;
        let hi = parse_number(hi_raw, field, name, PART_RANGE_END, lang)?;
        check_range(lo, field, min, max, name, lang)?;
        check_range(hi, field, min, max, name, lang)?;
        if lo > hi {
            return Err(usage_sel(
                lang,
                format!(
                    "the range in the {label} field '{field}' runs backwards ({lo}-{hi}) — the \
                     start must be at most the end. To cross midnight, split it into two items \
                     (e.g. `22-23,0-5`)."
                ),
                format!(
                    "{label} 필드 '{field}'의 범위가 역순입니다({lo}-{hi}) — 시작이 끝보다 \
                     작거나 같아야 합니다. 자정을 넘기려면 두 항목으로 나누세요(예: `22-23,0-5`)."
                ),
            ));
        }
        (lo, hi)
    } else {
        let value = parse_number(base, field, name, PART_VALUE, lang)?;
        check_range(value, field, min, max, name, lang)?;
        // 단일 값에 간격을 붙이면(`5/10`) 표준 cron은 "5부터 최대까지 10 간격"으로 읽는다.
        // 그 해석은 구현마다 갈리므로 여기서는 **간격이 붙은 단일 값을 거부**한다 —
        // 갈리는 문법을 조용히 한쪽으로 해석하는 것이 이 도구에서 가장 위험하다.
        if step != 1 {
            return Err(usage_sel(
                lang,
                format!(
                    "the {label} field '{field}' attaches a step to a single value — \
                     implementations disagree on what that means, so it is not supported. \
                     Write the range explicitly (e.g. `{value}-{max}/{step}`)."
                ),
                format!(
                    "{label} 필드 '{field}'는 단일 값에 간격을 붙였습니다 — 구현마다 해석이 \
                     달라 지원하지 않습니다. 범위를 명시하세요(예: `{value}-{max}/{step}`)."
                ),
            ));
        }
        (value, value)
    };

    let mut value = from;
    while value <= to {
        set.insert(value);
        value += step;
    }
    Ok(())
}

/// 숫자 조각 하나를 파싱한다 — 부호·공백·비숫자를 전부 거부한다.
///
/// [`str::parse`]에 그냥 맡기지 않고 문자 검사를 먼저 하는 이유: `parse::<u32>()`는 `+5`를
/// 받아들이고 `-5`는 "invalid digit"이라는 영문 메시지로 거부한다. 둘 다 우리가 원하는
/// 동작이 아니다(전자는 통과, 후자는 메시지가 불친절).
fn parse_number(raw: &str, field: &str, name: Name, part: Name, lang: Lang) -> Result<u32> {
    let (label, what) = (name.of(lang), part.of(lang));
    if raw.is_empty() {
        return Err(usage_sel(
            lang,
            format!("the {what} in the {label} field '{field}' is empty."),
            format!(
                "{label} 필드 '{field}'의 {what}{} 비어 있습니다.",
                crate::i18n::GA.after(part.ko)
            ),
        ));
    }
    if !raw.bytes().all(|b| b.is_ascii_digit()) {
        // 기본 문구는 언제나 같다("숫자만") — 진단이 위치마다 갈리면 화면 문구를 테스트로
        // 고정할 수 없다. 영문 이름(JAN·MON)을 쓴 경우에만 무엇으로 바꿔야 하는지 덧붙인다.
        let alphabetic = raw.bytes().all(|b| b.is_ascii_alphabetic());
        let mut hint_en =
            String::from(" — only digits 0-9 are allowed (no signs, spaces, or punctuation).");
        let mut hint_ko = String::from(" — 0~9 숫자만 쓸 수 있습니다(부호·공백·특수문자 불가).");
        if alphabetic {
            hint_en.push_str(
                " English names for months and weekdays (JAN, MON, …) are not supported \
                 either — write numbers (1 = January, 0 = Sunday).",
            );
            hint_ko.push_str(
                " 월·요일의 영문 이름(JAN·MON 등)도 지원하지 않습니다 — 숫자로 적으세요\
                 (1=1월, 0=일요일).",
            );
        }
        return Err(usage_sel(
            lang,
            format!("the {what} '{raw}' in the {label} field '{field}' is not a number{hint_en}"),
            format!("{label} 필드 '{field}'의 {what} '{raw}'를 숫자로 읽을 수 없습니다{hint_ko}"),
        ));
    }
    raw.parse::<u32>().map_err(|_| {
        usage_sel(
            lang,
            format!("the {what} '{raw}' in the {label} field '{field}' is too large."),
            format!(
                "{label} 필드 '{field}'의 {what} '{raw}'{} 너무 큽니다.",
                crate::i18n::GA.after(raw)
            ),
        )
    })
}

/// 값이 필드 범위 안인지 검사한다.
fn check_range(value: u32, field: &str, min: u32, max: u32, name: Name, lang: Lang) -> Result<()> {
    if value < min || value > max {
        let label = name.of(lang);
        return Err(usage_sel(
            lang,
            format!(
                "{value} in the {label} field '{field}' is out of range — only {min}-{max} \
                 are allowed."
            ),
            format!(
                "{label} 필드 '{field}'의 {value}{} 범위를 벗어났습니다 — {min}~{max}만 \
                 쓸 수 있습니다.",
                crate::i18n::GA.after(&value.to_string())
            ),
        ));
    }
    Ok(())
}

/// 두 언어 문장 중 하나를 골라 사용법 오류로 접는다.
///
/// 이 모듈의 모든 거부는 사용법 오류(exit 2 계열)다 — 운영자가 표현식을 고치면 해결된다.
fn usage_sel(lang: Lang, en: String, ko: String) -> XBackupError {
    XBackupError::Usage(lang.sel_string(en, ko))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC3339 UTC 문자열을 시각으로. 테스트 가독성용.
    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .expect("테스트 시각 문자열이 잘못됐다")
            .with_timezone(&Utc)
    }

    /// `expr`의 `after` 이후 `count`개 실행 시각.
    fn sequence(expr: &str, after: &str, count: usize) -> Vec<DateTime<Utc>> {
        let cron = CronExpr::parse(expr, Lang::En).expect("유효한 표현식이어야 함");
        let mut out = Vec::with_capacity(count);
        let mut cursor = at(after);
        for _ in 0..count {
            let next = cron.next_after(cursor).expect("다음 시각이 있어야 함");
            out.push(next);
            cursor = next;
        }
        out
    }

    /// 잘못된 표현식은 전부 exit 2로 거부되고, 메시지가 원인을 지목한다.
    ///
    /// 지시서가 요구한 8종을 모두 포함하고 그 위에 실전에서 자주 나오는 형태를 더한다.
    /// `expect_hint`는 "운영자가 무엇을 고쳐야 하는지"가 메시지에 실제로 들어 있는지
    /// 보는 것이다 — 범용 문구로 뭉개지는 회귀를 막는다.
    #[test]
    fn invalid_expressions_are_rejected_with_reason() {
        let cases: &[(&str, &str)] = &[
            // 1. `*/0` — 간격 0.
            ("*/0 * * * *", "간격이 0"),
            // 2. 분 범위 초과.
            ("60 * * * *", "0~59"),
            // 3. 시 범위 초과.
            ("0 24 * * *", "0~23"),
            // 4. 범위 역순.
            ("5-1 * * * *", "역순"),
            // 5. 필드 수 부족.
            ("0 0 * *", "필드가 4개"),
            // 6. 필드 수 초과(초 단위 6필드 시도).
            ("0 0 0 * * *", "필드가 6개"),
            // 7. 빈 문자열.
            ("", "비어 있습니다"),
            // 8. 공백만.
            ("   ", "비어 있습니다"),
            // 9. 별칭 오타.
            ("@dail", "지원하지 않습니다"),
            // 10. 범위 밖 별칭.
            ("@reboot", "@reboot"),
            // 11. 음수 — 부호를 먼저 잡아 "범위 시작이 비었다"가 아니라 부호를 지목한다.
            ("-5 * * * *", "부호가 붙어"),
            // 12. 쉼표 목록의 빈 항목.
            ("1,,2 * * * *", "빈 항목"),
            // 13. 영문 요일 이름.
            ("0 0 * * MON", "영문 이름"),
            // 14. 일 필드 0(1부터다).
            ("0 0 0 * *", "1~31"),
            // 15. 월 필드 13.
            ("0 0 * 13 *", "1~12"),
            // 16. 요일 8(7까지다).
            ("0 0 * * 8", "0~7"),
            // 17. Quartz `?`.
            ("0 0 ? * *", "숫자만"),
            // 18. 단일 값 + 간격(해석이 갈리는 문법).
            ("5/10 * * * *", "단일 값에 간격"),
            // 19. 간격이 숫자가 아님.
            ("*/a * * * *", "숫자만"),
            // 20. 표현식 길이 상한.
            (&"0".repeat(MAX_EXPR_LEN + 1), "너무 깁니다"),
        ];
        for (input, expect_hint) in cases {
            // 한국어 문구를 고정한다. 영문 쪽은 아래 `both_languages_reject_the_same_inputs`가
            // "같은 입력을 같은 이유로 거부하는가"로 따로 본다 — 문장을 두 벌 적으면
            // 규칙이 아니라 번역을 테스트하게 된다.
            let err =
                CronExpr::parse(input, Lang::Ko).expect_err(&format!("'{input}'는 거부되어야 함"));
            assert_eq!(
                err.exit_code(),
                crate::error::exit_codes::USAGE,
                "'{input}'는 사용법 오류(exit 2)여야 함: {err}"
            );
            assert!(
                err.to_string().contains(expect_hint),
                "'{input}' 메시지에 '{expect_hint}'가 없다: {err}"
            );
        }
    }

    /// **판정은 언어에 의존하지 않고, 두 언어 모두 문장을 낸다**(R42).
    ///
    /// 문구를 한쪽만 손보다 다른 쪽 분기를 빠뜨리면 여기서 잡힌다 — 영문 화면에 한국어가
    /// 그대로 남거나, 반대로 한국어 화면이 영문으로 새는 상황이다.
    #[test]
    fn both_languages_reject_the_same_inputs() {
        let inputs = [
            "",
            "60 * * * *",
            "*/0 * * * *",
            "@weekly",
            "0 3 * *",
            "a * * * *",
            "0 5-2 * * *",
            "0 3 * * *",
            "@daily",
        ];
        for input in inputs {
            let en = CronExpr::parse(input, Lang::En);
            let ko = CronExpr::parse(input, Lang::Ko);
            assert_eq!(
                en.is_ok(),
                ko.is_ok(),
                "'{input}'의 판정이 언어에 따라 갈렸다 — 언어는 문구만 정해야 한다"
            );
            if let (Err(en), Err(ko)) = (en, ko) {
                let (en, ko) = (en.to_string(), ko.to_string());
                assert_ne!(
                    en, ko,
                    "'{input}': 두 언어 문장이 같다 — 한쪽 분기를 빠뜨렸다"
                );
                assert!(
                    !en.contains('가') && !en.contains('니'),
                    "'{input}': 영문 메시지에 한국어가 섞였다: {en}"
                );
            }
        }
    }

    /// 유효한 문법은 전부 통과하고 원문 표기가 보존된다.
    #[test]
    fn valid_expressions_keep_their_source_text() {
        for expr in [
            "* * * * *",
            "0 3 * * *",
            "*/15 * * * *",
            "0,15,30,45 * * * *",
            "0 9-17 * * 1-5",
            "30 2 1 * *",
            "0 0 29 2 *",
            "0 0 * * 7",
            "0 0-23/6 * * *",
            ALIAS_HOURLY,
            ALIAS_DAILY,
        ] {
            let cron =
                CronExpr::parse(expr, Lang::En).unwrap_or_else(|e| panic!("'{expr}' 거부됨: {e}"));
            assert_eq!(cron.as_str(), expr, "원문 표기가 변형됐다");
            assert!(
                cron.next_after(at("2026-01-01T00:00:00Z")).is_some(),
                "'{expr}'의 다음 시각을 계산하지 못했다"
            );
        }
        // 앞뒤·중간 공백은 정규화되되 필드 내용은 그대로다.
        let cron = CronExpr::parse("  0   3  *  *  *  ", Lang::En).unwrap();
        assert_eq!(cron.as_str(), "0 3 * * *");
    }

    /// 여러 표현식 × 여러 기준 시각에서 다음 실행 시각이 정확하다.
    #[test]
    fn next_run_is_computed_exactly() {
        let cases: &[(&str, &str, &str)] = &[
            // 매분 — 같은 분은 제외되고 다음 분이 나온다.
            ("* * * * *", "2026-07-25T10:30:00Z", "2026-07-25T10:31:00Z"),
            // 초가 남아 있어도 분으로 잘린다.
            ("* * * * *", "2026-07-25T10:30:59Z", "2026-07-25T10:31:00Z"),
            // 매일 03:00 — 그 시각을 지나면 다음 날.
            ("0 3 * * *", "2026-07-25T03:00:00Z", "2026-07-26T03:00:00Z"),
            ("0 3 * * *", "2026-07-25T02:59:00Z", "2026-07-25T03:00:00Z"),
            // 15분 간격.
            (
                "*/15 * * * *",
                "2026-07-25T10:16:00Z",
                "2026-07-25T10:30:00Z",
            ),
            (
                "*/15 * * * *",
                "2026-07-25T10:46:00Z",
                "2026-07-25T11:00:00Z",
            ),
            // 별칭.
            (ALIAS_HOURLY, "2026-07-25T10:30:00Z", "2026-07-25T11:00:00Z"),
            (ALIAS_DAILY, "2026-07-25T10:30:00Z", "2026-07-26T00:00:00Z"),
            // 평일 업무시간 — 금요일 18시 이후는 월요일 09시.
            (
                "0 9-17 * * 1-5",
                "2026-07-24T18:00:00Z",
                "2026-07-27T09:00:00Z",
            ),
            // 매월 1일 02:30 — 월을 넘긴다.
            ("30 2 1 * *", "2026-07-25T00:00:00Z", "2026-08-01T02:30:00Z"),
            // 2월 29일 — 윤년까지 건너뛴다(2027 아님).
            ("0 0 29 2 *", "2026-03-01T00:00:00Z", "2028-02-29T00:00:00Z"),
            // 요일 7 = 일요일.
            ("0 0 * * 7", "2026-07-25T00:00:00Z", "2026-07-26T00:00:00Z"),
            // 연말 경계.
            ("0 0 1 1 *", "2026-12-31T23:59:00Z", "2027-01-01T00:00:00Z"),
        ];
        for (expr, after, expected) in cases {
            let cron = CronExpr::parse(expr, Lang::En).unwrap();
            assert_eq!(
                cron.next_after(at(after)),
                Some(at(expected)),
                "'{expr}' @ {after}"
            );
        }
    }

    /// 일·요일이 **모두** 제한되면 OR로 판정한다(표준 cron 규칙 — `matches_date` doc).
    #[test]
    fn day_of_month_and_week_are_or_ed_when_both_restricted() {
        // 2026-11-13은 금요일이다. `0 0 13 * 5`는 "13일 이거나 금요일".
        let cron = CronExpr::parse("0 0 13 * 5", Lang::En).unwrap();
        // 11월 6일(금)은 13일이 아니지만 금요일이므로 맞는다.
        assert_eq!(
            cron.next_after(at("2026-11-01T00:00:00Z")),
            Some(at("2026-11-06T00:00:00Z")),
            "요일만 맞는 날이 걸러졌다 — AND로 판정하고 있다"
        );
        // 12월 13일(일)은 금요일이 아니지만 13일이므로 맞는다.
        assert_eq!(
            cron.next_after(at("2026-12-12T00:00:00Z")),
            Some(at("2026-12-13T00:00:00Z")),
            "일자만 맞는 날이 걸러졌다"
        );
        // 한쪽이 `*`면 AND로 접어도 결과가 같다 — 13일만 맞아야 한다.
        let dom_only = CronExpr::parse("0 0 13 * *", Lang::En).unwrap();
        assert_eq!(
            dom_only.next_after(at("2026-11-01T00:00:00Z")),
            Some(at("2026-11-13T00:00:00Z"))
        );
    }

    /// 실현 불가능한 날짜 조합은 `None`으로 접힌다 — 파서가 조합을 검사하지 않고 계산이
    /// 균일하게 잡는다([`MAX_DAYS_AHEAD`] doc).
    #[test]
    fn impossible_date_combination_has_no_next_run() {
        // 2월 30일은 존재하지 않는다.
        let cron = CronExpr::parse("0 0 30 2 *", Lang::En).unwrap();
        assert_eq!(cron.next_after(at("2026-01-01T00:00:00Z")), None);
        // 2월 31일도 마찬가지.
        let cron = CronExpr::parse("0 0 31 2 *", Lang::En).unwrap();
        assert_eq!(cron.next_after(at("2026-01-01T00:00:00Z")), None);
    }

    /// **DST 봄 전환** 구간에서 누락이 없다.
    ///
    /// America/New_York의 2026년 봄 전환은 2026-03-08 02:00 EST → 03:00 EDT이고, 그
    /// 순간은 UTC로 2026-03-08T07:00:00Z다. 로컬 기준으로 계산하는 스케줄러라면 "매일
    /// 02:30"이 그날 존재하지 않아 **하루를 건너뛴다.** UTC 기준이면 그 개념 자체가 없다 —
    /// 간격이 정확히 24시간으로 유지되는지 확인한다(모듈 헤더 "계산 기준은 UTC다").
    #[test]
    fn spring_forward_transition_skips_nothing() {
        let runs = sequence("30 2 * * *", "2026-03-06T00:00:00Z", 5);
        assert_eq!(
            runs,
            vec![
                at("2026-03-06T02:30:00Z"),
                at("2026-03-07T02:30:00Z"),
                // 로컬 기준이면 이 항목이 사라진다.
                at("2026-03-08T02:30:00Z"),
                at("2026-03-09T02:30:00Z"),
                at("2026-03-10T02:30:00Z"),
            ],
            "봄 전환 구간에서 실행이 누락됐다"
        );
        for pair in runs.windows(2) {
            assert_eq!(
                pair[1] - pair[0],
                TimeDelta::hours(24),
                "간격이 24시간이 아니다: {pair:?}"
            );
        }

        // 전환 **순간**(07:00Z)을 관통하는 매분 스케줄도 연속된 분을 정확히 낸다.
        let minutes = sequence("* * * * *", "2026-03-08T06:58:00Z", 4);
        assert_eq!(
            minutes,
            vec![
                at("2026-03-08T06:59:00Z"),
                at("2026-03-08T07:00:00Z"),
                at("2026-03-08T07:01:00Z"),
                at("2026-03-08T07:02:00Z"),
            ]
        );
    }

    /// **DST 가을 전환** 구간에서 중복 실행이 없다.
    ///
    /// America/New_York의 2026년 가을 전환은 2026-11-01 02:00 EDT → 01:00 EST이고, UTC로
    /// 2026-11-01T06:00:00Z다. 로컬 기준이면 "매일 01:30"이 그날 **두 번** 온다(EDT의
    /// 01:30과 EST의 01:30) — 같은 프로파일 백업이 두 번 떠서 뒤쪽이 파일 락에 걸린다.
    /// UTC 기준이면 하루에 정확히 한 번이다.
    #[test]
    fn fall_back_transition_fires_once_per_day() {
        let runs = sequence("30 1 * * *", "2026-10-30T00:00:00Z", 5);
        assert_eq!(
            runs,
            vec![
                at("2026-10-30T01:30:00Z"),
                at("2026-10-31T01:30:00Z"),
                at("2026-11-01T01:30:00Z"),
                at("2026-11-02T01:30:00Z"),
                at("2026-11-03T01:30:00Z"),
            ],
            "가을 전환 구간에서 실행이 중복됐다"
        );
        // 모두 서로 다른 날짜다 — 같은 날 두 번이 없다.
        let dates: std::collections::BTreeSet<_> = runs.iter().map(|t| t.date_naive()).collect();
        assert_eq!(
            dates.len(),
            runs.len(),
            "같은 날에 두 번 실행된다: {runs:?}"
        );

        // 전환 순간(06:00Z)을 관통하는 매분 스케줄도 중복 없이 전진한다.
        let minutes = sequence("* * * * *", "2026-11-01T05:58:00Z", 4);
        let unique: std::collections::BTreeSet<_> = minutes.iter().collect();
        assert_eq!(unique.len(), minutes.len(), "중복된 분이 있다: {minutes:?}");
        assert_eq!(minutes[1], at("2026-11-01T06:00:00Z"));
    }

    /// 반복 호출이 항상 **엄격히 증가**한다 — 스케줄러 루프가 같은 시각을 두 번 발화하지
    /// 않는다는 성질의 근본 근거다.
    #[test]
    fn sequence_is_strictly_increasing_for_every_shape() {
        for expr in [
            "* * * * *",
            "*/7 * * * *",
            "0 3 * * *",
            "0 9-17 * * 1-5",
            "30 2 1 * *",
            ALIAS_HOURLY,
        ] {
            let runs = sequence(expr, "2026-02-27T23:57:00Z", 40);
            for pair in runs.windows(2) {
                assert!(
                    pair[0] < pair[1],
                    "'{expr}'가 후퇴하거나 정체했다: {pair:?}"
                );
            }
        }
    }

    /// 직렬화는 문자열 하나이고, 왕복하면 같은 표현식이 된다.
    #[test]
    fn serde_round_trips_through_a_plain_string() {
        let cron = CronExpr::parse("0 3 * * 1-5", Lang::En).unwrap();
        let json = serde_json::to_string(&cron).unwrap();
        assert_eq!(json, r#""0 3 * * 1-5""#, "문자열 하나로 나가야 한다");
        let back: CronExpr = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cron);
        // 잘못된 표현식은 역직렬화에서도 거부된다(사람이 파일을 고친 경우).
        assert!(serde_json::from_str::<CronExpr>(r#""60 * * * *""#).is_err());
    }
}
