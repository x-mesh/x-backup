//! 내장 스케줄러의 cron 표현식(P3-1) — 외부 cron 의존을 제거하는 마지막 조각.
//!
//! 표준 5필드(`분 시 일 월 요일`)를 자체 구현으로 파싱한다(~200 LOC — 외부 크레이트도
//! pure Rust지만 의존 최소 원칙에 부합, 로드맵 P3-1). 지원 문법:
//! - `*`(전체), 숫자, 범위 `a-b`, 목록 `a,b,c`, 스텝 `*/n`·`a-b/n`
//! - 요일: 0~7(0과 7 모두 일요일 — 표준 cron과 동일). 월/요일 이름은 미지원(숫자만).
//!
//! **일(dom)·요일(dow) 규칙**: 둘 다 제한되면(둘 다 `*`가 아니면) **둘 중 하나**만
//! 맞아도 매치한다 — Vixie cron의 전통 의미론을 따른다.
//!
//! 시각 해석은 **로컬 타임존**이다(cron 관례). DST 전환은 chrono의 로컬 타임라인
//! 산술을 따른다(존재하지 않는 시각은 건너뛰고, 반복 시각은 첫 번째만).

use chrono::{DateTime, Datelike, Duration, Local, Timelike};

use crate::error::{Result, XBackupError};

/// 파싱된 5필드 cron 표현식 — 필드별 허용 값 비트마스크.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronExpr {
    /// 분(0-59).
    minutes: u64,
    /// 시(0-23).
    hours: u32,
    /// 일(1-31).
    dom: u32,
    /// 월(1-12).
    months: u16,
    /// 요일(0-6, 0=일요일; 입력의 7은 0으로 정규화).
    dow: u8,
    /// dom 필드가 `*`였는지(dom/dow OR 규칙 판정용).
    dom_is_wildcard: bool,
    /// dow 필드가 `*`였는지.
    dow_is_wildcard: bool,
}

impl CronExpr {
    /// `"분 시 일 월 요일"` 5필드를 파싱한다. 형식 오류는 설정 오류(exit 2).
    pub fn parse(expr: &str) -> Result<Self> {
        let fields: Vec<&str> = expr.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(XBackupError::Config(format!(
                "cron 표현식은 5필드(분 시 일 월 요일)여야 합니다(현재 {}필드): '{expr}'",
                fields.len()
            )));
        }
        let minutes = parse_field(fields[0], 0, 59, expr)?;
        let hours = parse_field(fields[1], 0, 23, expr)? as u32;
        let dom = parse_field(fields[2], 1, 31, expr)? as u32;
        let months = parse_field(fields[3], 1, 12, expr)? as u16;
        // 요일은 0-7 허용(7=일요일) → 비트 7을 비트 0으로 접는다.
        let dow_raw = parse_field(fields[4], 0, 7, expr)?;
        let dow = ((dow_raw & 0x7f) | (dow_raw >> 7)) as u8;
        Ok(Self {
            minutes,
            hours,
            dom,
            months,
            dow,
            dom_is_wildcard: fields[2] == "*",
            dow_is_wildcard: fields[4] == "*",
        })
    }

    /// 주어진 시각 **이후**의 첫 발화 시각(분 단위 절단). 400일 내에 없으면 None
    /// (5필드 조합상 발생 불가에 가깝지만 2/30 같은 불가능 조합의 무한 루프 방어).
    pub fn next_after(&self, from: DateTime<Local>) -> Option<DateTime<Local>> {
        // 분 경계로 올림(현재 분은 "이후"가 아니다).
        let mut t = (from + Duration::minutes(1))
            .with_second(0)?
            .with_nanosecond(0)?;
        let limit = from + Duration::days(400);
        while t <= limit {
            if !self.matches_date(&t) {
                // 날짜가 안 맞으면 다음 날 00:00으로 점프(분 단위 순회 회피).
                t = (t + Duration::days(1))
                    .with_hour(0)?
                    .with_minute(0)?
                    .with_second(0)?
                    .with_nanosecond(0)?;
                continue;
            }
            if self.hours & (1 << t.hour()) == 0 {
                // 시가 안 맞으면 다음 시 00분으로 점프.
                t = (t + Duration::hours(1)).with_minute(0)?;
                continue;
            }
            if self.minutes & (1u64 << t.minute()) != 0 {
                return Some(t);
            }
            t += Duration::minutes(1);
        }
        None
    }

    /// 날짜 필드(월·일·요일) 매치 — dom/dow는 둘 다 제한이면 OR(모듈 주석).
    fn matches_date(&self, t: &DateTime<Local>) -> bool {
        if self.months & (1 << t.month()) == 0 {
            return false;
        }
        let dom_ok = self.dom & (1 << t.day()) != 0;
        let dow_ok = self.dow & (1 << t.weekday().num_days_from_sunday()) != 0;
        match (self.dom_is_wildcard, self.dow_is_wildcard) {
            (false, false) => dom_ok || dow_ok,
            _ => dom_ok && dow_ok,
        }
    }
}

/// 한 필드를 비트마스크로 파싱한다(`*`, 숫자, `a-b`, `a,b`, `*/n`, `a-b/n`).
fn parse_field(field: &str, min: u32, max: u32, expr: &str) -> Result<u64> {
    let err = |why: String| {
        XBackupError::Config(format!("cron 필드 '{field}' 파싱 실패({why}): '{expr}'"))
    };
    let mut mask: u64 = 0;
    for part in field.split(',') {
        // 스텝 분리: "<range>/<step>".
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => {
                let step: u32 = s
                    .parse()
                    .map_err(|_| err(format!("스텝 '{s}'가 숫자가 아님")))?;
                if step == 0 {
                    return Err(err("스텝 0 불가".into()));
                }
                (r, step)
            }
            None => (part, 1),
        };
        // 범위 해석: "*", "a-b", "a"(스텝 있으면 a-max).
        let (lo, hi) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            let a: u32 = a.parse().map_err(|_| err(format!("'{a}'가 숫자가 아님")))?;
            let b: u32 = b.parse().map_err(|_| err(format!("'{b}'가 숫자가 아님")))?;
            if a > b {
                return Err(err(format!("역순 범위 {a}-{b}")));
            }
            (a, b)
        } else {
            let v: u32 = range
                .parse()
                .map_err(|_| err(format!("'{range}'가 숫자가 아님")))?;
            // "5/2" 형태는 5부터 max까지 스텝(cron 관례). 스텝 없으면 단일 값.
            if step > 1 {
                (v, max)
            } else {
                (v, v)
            }
        };
        if lo < min || hi > max {
            return Err(err(format!("{lo}-{hi}가 허용 범위 {min}-{max} 밖")));
        }
        let mut v = lo;
        while v <= hi {
            mask |= 1u64 << v;
            v += step;
        }
    }
    if mask == 0 {
        return Err(err("허용 값이 없음".into()));
    }
    Ok(mask)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn local(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    #[test]
    fn parse_rejects_bad_shapes() {
        for bad in [
            "* * * *",     // 4필드.
            "60 * * * *",  // 분 범위 밖.
            "* 24 * * *",  // 시 범위 밖.
            "* * 0 * *",   // 일은 1부터.
            "* * * 13 *",  // 월 범위 밖.
            "* * * * 8",   // 요일은 0-7.
            "*/0 * * * *", // 스텝 0.
            "5-1 * * * *", // 역순 범위.
            "x * * * *",   // 숫자 아님.
        ] {
            let err = CronExpr::parse(bad).unwrap_err();
            assert_eq!(err.exit_code(), 2, "{bad}는 exit 2여야 함: {err}");
        }
    }

    #[test]
    fn next_daily_at_3am() {
        let c = CronExpr::parse("0 3 * * *").unwrap();
        // 02:59 → 오늘 03:00.
        assert_eq!(
            c.next_after(local(2026, 7, 7, 2, 59)),
            Some(local(2026, 7, 7, 3, 0))
        );
        // 03:00 정각 → **다음날** 03:00("이후" 의미 — 같은 분은 제외).
        assert_eq!(
            c.next_after(local(2026, 7, 7, 3, 0)),
            Some(local(2026, 7, 8, 3, 0))
        );
    }

    #[test]
    fn next_every_15_minutes() {
        let c = CronExpr::parse("*/15 * * * *").unwrap();
        assert_eq!(
            c.next_after(local(2026, 7, 7, 10, 0)),
            Some(local(2026, 7, 7, 10, 15))
        );
        assert_eq!(
            c.next_after(local(2026, 7, 7, 10, 50)),
            Some(local(2026, 7, 7, 11, 0))
        );
    }

    #[test]
    fn next_with_lists_and_ranges() {
        // 평일 09-18시 30분.
        let c = CronExpr::parse("30 9-18 * * 1-5").unwrap();
        // 2026-07-11은 토요일 → 다음 매치는 월요일(07-13) 09:30.
        assert_eq!(
            c.next_after(local(2026, 7, 11, 0, 0)),
            Some(local(2026, 7, 13, 9, 30))
        );
    }

    #[test]
    fn dom_dow_or_semantics() {
        // "매월 1일 또는 월요일" 00:00 — 둘 다 제한이면 OR(Vixie cron).
        let c = CronExpr::parse("0 0 1 * 1").unwrap();
        // 2026-07-02(목) 이후 첫 매치: 07-06(월).
        assert_eq!(
            c.next_after(local(2026, 7, 2, 0, 0)),
            Some(local(2026, 7, 6, 0, 0))
        );
        // 2026-07-30(목) 이후: 08-01(토, 1일) — 월요일(08-03)보다 빠르다.
        assert_eq!(
            c.next_after(local(2026, 7, 30, 0, 0)),
            Some(local(2026, 8, 1, 0, 0))
        );
    }

    #[test]
    fn sunday_as_7_normalizes() {
        let a = CronExpr::parse("0 0 * * 0").unwrap();
        let b = CronExpr::parse("0 0 * * 7").unwrap();
        // 2026-07-07(화) 이후 첫 일요일: 07-12.
        assert_eq!(
            a.next_after(local(2026, 7, 7, 0, 0)),
            Some(local(2026, 7, 12, 0, 0))
        );
        assert_eq!(
            a.next_after(local(2026, 7, 7, 0, 0)),
            b.next_after(local(2026, 7, 7, 0, 0))
        );
    }

    #[test]
    fn impossible_combo_returns_none() {
        // 2월 30일 — 발화 불가 → None(무한 루프 방어).
        let c = CronExpr::parse("0 0 30 2 *").unwrap();
        assert_eq!(c.next_after(local(2026, 1, 1, 0, 0)), None);
    }
}
