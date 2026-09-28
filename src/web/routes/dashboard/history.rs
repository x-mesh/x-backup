//! 대시보드 백업 이력 — 프로파일 × UTC 날짜 격자로 접는 **순수 집계**.
//!
//! ## 칸은 두 원천을 겹친다
//! - destination의 manifest(`list --json`)는 외부 cron이 돌린 백업까지 **모든 백업**을 담지만,
//!   남는 것은 성공과 부분 성공뿐이다. 실패한 백업은 `pipeline::backup::cleanup`이 부분
//!   산출물을 지워 흔적이 없다.
//! - 실패는 이 콘솔의 잡 이력(`state_dir/jobs`)에만 남는다. 콘솔 밖에서 실행된 백업의
//!   실패는 어디에도 기록되지 않는다.
//!
//! 그래서 둘 다 비어 있는 날은 "실패 없음"이 아니라 **기록 없음**이다. 화면은 그 칸을
//! 실패와 다른 모양으로 그리고, 이 한계를 패널에 고정 문구로 밝힌다.
//!
//! ## 날짜 경계는 UTC다
//! 내장 스케줄러가 cron 표현식을 UTC로 해석하므로(`crate::web::schedule` 헤더), 서버 로컬
//! 시각으로 자르면 "매일 00:30 UTC" 백업이 시간대에 따라 이틀에 걸쳐 보인다.

use chrono::{DateTime, Days, NaiveDate, Utc};

use crate::i18n::Lang;
use crate::web::cache::{ProbeOutcome, ProbeOutput};
use crate::web::job::exit as job_exit;
use crate::web::job::JobCommand;
use crate::web::routes::catalog::{self, CatalogEntry};
use crate::web::state::jobs::JobSummary;
use crate::web::view::components::Level;

use super::{excerpt, severity};

/// 격자가 보여 주는 날 수(오늘 포함). 주 단위로 반복되는 실패가 네 번 보이는 길이다.
pub const WINDOW_DAYS: u64 = 30;

/// 날짜가 정해진 결과 하나.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Event {
    /// 백업 생성 시각(manifest) 또는 잡 종료 시각(잡 이력).
    pub at: DateTime<Utc>,
    /// 그 결과의 레벨.
    pub level: Level,
}

/// 격자 한 칸 — 프로파일 하나의 하루.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DayCell {
    /// UTC 날짜.
    pub date: NaiveDate,
    /// 그날 가장 나쁜 결과. `None`이면 두 원천 모두 기록이 없다.
    pub level: Option<Level>,
    /// 그날 생성된 백업 수(manifest 기준).
    pub backups: usize,
    /// 그날 성공하지 못한 콘솔 잡 수.
    pub failed_runs: usize,
}

/// 격자 한 행.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryRow {
    /// 목록을 읽었다.
    Days {
        /// 프로파일 이름.
        profile: String,
        /// 가장 오래된 날부터 오늘까지 [`WINDOW_DAYS`]칸.
        days: Vec<DayCell>,
        /// 날짜를 읽지 못해 격자에 넣지 못한 목록 항목 수. 숨기지 않고 센다.
        undated: usize,
    },
    /// 목록을 읽지 못했다 — 칸을 그리면 "기록 없음"으로 오해되므로 칸 대신 이유를 보인다.
    Unavailable {
        /// 프로파일 이름.
        profile: String,
        /// 무슨 일이 있었는지 한 줄.
        reason: String,
    },
}

/// 화면이 그릴 이력 전체.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct History {
    /// 격자의 마지막 날(UTC 오늘).
    pub today: NaiveDate,
    /// 프로파일별 행(대시보드 표와 같은 순서).
    pub rows: Vec<HistoryRow>,
}

impl History {
    /// 격자의 첫날.
    pub fn first_day(&self) -> NaiveDate {
        first_day(self.today)
    }
}

fn first_day(today: NaiveDate) -> NaiveDate {
    today
        .checked_sub_days(Days::new(WINDOW_DAYS - 1))
        .unwrap_or(today)
}

/// manifest 항목 하나의 레벨.
///
/// 체인 끊김(`broken`)은 성공으로 본다 — 백업 자체는 끝났고, 끊긴 체인은 Catalog가 다루는
/// 복구 경로 문제다. `incomplete`·`orphan`·`corrupt`는 백업이 온전하다고 말할 수 없으므로
/// 경고다.
pub fn manifest_level(kind: &str, chain_status: &str) -> Level {
    match (kind, chain_status) {
        ("full" | "incr", "ok" | "broken") => Level::Ok,
        _ => Level::Warn,
    }
}

/// 목록 항목들을 날짜 있는 결과로 바꾼다. 날짜를 읽지 못한 항목 수를 함께 돌려준다.
pub fn manifest_events(entries: &[CatalogEntry]) -> (Vec<Event>, usize) {
    let mut events = Vec::with_capacity(entries.len());
    let mut undated = 0;
    for entry in entries {
        let parsed = match entry {
            CatalogEntry::Row(row) => row
                .created_at
                .as_deref()
                .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
                .map(|at| Event {
                    at: at.with_timezone(&Utc),
                    level: manifest_level(&row.kind, &row.chain_status),
                }),
            CatalogEntry::Unparseable { .. } => None,
        };
        match parsed {
            Some(event) => events.push(event),
            None => undated += 1,
        }
    }
    (events, undated)
}

/// 잡 이력에서 이 프로파일의 **성공하지 못한** backup 잡만 고른다.
///
/// 성공한 잡은 넣지 않는다 — 그 백업은 manifest로 이미 격자에 있고, 넣으면 같은 백업이 두 번
/// 세어진다. 아직 끝나지 않은 잡도 넣지 않는다(결과가 없다).
pub fn failed_run_events(jobs: &[JobSummary], profile: &str) -> Vec<Event> {
    let backup = JobCommand::Backup.verb();
    jobs.iter()
        .filter(|job| job.command == backup && job.profile.as_deref() == Some(profile))
        .filter_map(|job| {
            let at = job.finished_at?;
            let level = job_exit::level_for_label(job.outcome.as_deref()?);
            (level != Level::Ok).then_some(Event { at, level })
        })
        .collect()
}

/// 결과들을 `today`로 끝나는 [`WINDOW_DAYS`]칸으로 접는다. 창 밖의 결과는 버린다.
pub fn build_days(today: NaiveDate, backups: &[Event], failed_runs: &[Event]) -> Vec<DayCell> {
    let first = first_day(today);
    first
        .iter_days()
        .take_while(|date| *date <= today)
        .map(|date| {
            let on_day = |e: &&Event| e.at.date_naive() == date;
            let day_backups: Vec<&Event> = backups.iter().filter(on_day).collect();
            let day_failures: Vec<&Event> = failed_runs.iter().filter(on_day).collect();
            let level = day_backups
                .iter()
                .chain(day_failures.iter())
                .map(|e| e.level)
                .max_by_key(|l| severity(*l));
            DayCell {
                date,
                level,
                backups: day_backups.len(),
                failed_runs: day_failures.len(),
            }
        })
        .collect()
}

/// 목록 프로브 결과와 잡 이력으로 행 하나를 만든다.
pub fn row_from_list(
    profile: &str,
    probe: &ProbeOutput,
    jobs: &[JobSummary],
    today: NaiveDate,
    lang: Lang,
) -> HistoryRow {
    let unavailable = |reason: String| HistoryRow::Unavailable {
        profile: profile.to_string(),
        reason,
    };
    let (outcome, stdout) = match &probe.outcome {
        ProbeOutcome::Completed {
            outcome, stdout, ..
        } => (outcome, stdout),
        ProbeOutcome::SpawnFailed(detail) | ProbeOutcome::WaitFailed(detail) => {
            return unavailable(format!(
                "{} {detail}",
                lang.sel(
                    "The backup list could not be read:",
                    "백업 목록을 읽지 못했습니다:"
                )
            ))
        }
        ProbeOutcome::TimedOut(limit) => {
            return unavailable(format!(
                "{} ({}s)",
                lang.sel(
                    "Listing backups did not finish within the time limit.",
                    "백업 목록 조회가 제한 시간 안에 끝나지 않았습니다.",
                ),
                limit.as_secs()
            ))
        }
    };
    match catalog::parse_document(stdout) {
        Ok(doc) => {
            let entries: Vec<CatalogEntry> = doc.backups.iter().map(catalog::parse_row).collect();
            let (backups, undated) = manifest_events(&entries);
            let failures = failed_run_events(jobs, profile);
            HistoryRow::Days {
                profile: profile.to_string(),
                days: build_days(today, &backups, &failures),
                undated,
            }
        }
        // endpoint-only 프로파일처럼 백업 저장소가 없으면 `list`는 JSON 없이 exit 2로 끝난다.
        Err(_) => {
            let presented = job_exit::present(outcome, lang);
            unavailable(excerpt(&format!(
                "{} {}",
                lang.sel("No backup list:", "백업 목록 없음:"),
                presented.headline
            )))
        }
    }
}

/// 예산이 끝나 목록을 조회하지 않은 행.
pub fn not_checked(profile: &str, lang: Lang) -> HistoryRow {
    HistoryRow::Unavailable {
        profile: profile.to_string(),
        reason: lang
            .sel(
                "Not listed in this request — the page ran out of its probe budget. Reload to list it.",
                "이번 요청에서는 조회하지 않았습니다(화면의 점검 시간 초과). 새로고침하면 조회합니다.",
            )
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::job::JobOutcome;
    use crate::web::state::jobs::JobId;
    use std::time::Duration;

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn event(s: &str, level: Level) -> Event {
        Event { at: at(s), level }
    }

    fn job(
        command: &str,
        profile: &str,
        outcome: Option<&str>,
        finished: Option<&str>,
    ) -> JobSummary {
        JobSummary {
            id: JobId::generate(),
            command: command.to_string(),
            profile: Some(profile.to_string()),
            args_masked: Vec::new(),
            started_at: at("2026-09-20T00:00:00Z"),
            pid: None,
            finished_at: finished.map(at),
            outcome: outcome.map(str::to_string),
            exit_code: None,
        }
    }

    fn completed(outcome: JobOutcome, stdout: &str) -> ProbeOutput {
        ProbeOutput {
            outcome: ProbeOutcome::Completed {
                outcome,
                stdout: stdout.to_string(),
                stderr: String::new(),
            },
            elapsed: Duration::from_millis(5),
        }
    }

    #[test]
    fn window_ends_today_and_spans_the_fixed_length() {
        let days = build_days(day("2026-09-28"), &[], &[]);
        assert_eq!(days.len() as u64, WINDOW_DAYS);
        assert_eq!(days.first().unwrap().date, day("2026-08-30"));
        assert_eq!(days.last().unwrap().date, day("2026-09-28"));
        assert!(
            days.iter().all(|d| d.level.is_none()),
            "기록 없는 날은 None"
        );
    }

    /// 날짜는 UTC로 자른다 — 자정 직전과 직후는 다른 칸이다.
    #[test]
    fn utc_midnight_splits_days() {
        let backups = [
            event("2026-09-27T23:59:59Z", Level::Ok),
            event("2026-09-28T00:00:00Z", Level::Ok),
            event("2026-09-28T09:00:00+09:00", Level::Ok),
        ];
        let days = build_days(day("2026-09-28"), &backups, &[]);
        let last = &days[days.len() - 1];
        let before = &days[days.len() - 2];
        assert_eq!(before.backups, 1, "{days:?}");
        // +09:00의 오전 9시는 UTC 0시다 — 서버 로컬이 아니라 UTC 날짜로 들어간다.
        assert_eq!(last.backups, 2, "{days:?}");
    }

    #[test]
    fn worst_result_of_the_day_wins_and_counts_stay_separate() {
        let backups = [event("2026-09-28T01:00:00Z", Level::Ok)];
        let failures = [
            event("2026-09-28T02:00:00Z", Level::Warn),
            event("2026-09-28T03:00:00Z", Level::Fail),
        ];
        let cell = build_days(day("2026-09-28"), &backups, &failures)
            .pop()
            .unwrap();
        assert_eq!(cell.level, Some(Level::Fail));
        assert_eq!((cell.backups, cell.failed_runs), (1, 2));
    }

    #[test]
    fn events_outside_the_window_are_dropped() {
        let backups = [
            event("2026-08-29T12:00:00Z", Level::Ok),
            event("2026-09-29T00:00:00Z", Level::Ok),
        ];
        let days = build_days(day("2026-09-28"), &backups, &[]);
        assert!(days.iter().all(|d| d.backups == 0), "{days:?}");
    }

    #[test]
    fn broken_chain_is_still_a_completed_backup() {
        assert_eq!(manifest_level("full", "ok"), Level::Ok);
        assert_eq!(manifest_level("incr", "broken"), Level::Ok);
        assert_eq!(manifest_level("full", "incomplete"), Level::Warn);
        assert_eq!(manifest_level("orphan", "orphan"), Level::Warn);
        assert_eq!(manifest_level("corrupt", "ok"), Level::Warn);
    }

    /// 성공한 잡은 manifest와 겹치므로 빼고, 다른 프로파일·다른 명령·진행 중인 잡도 뺀다.
    #[test]
    fn only_unsuccessful_finished_backup_jobs_of_the_profile_count() {
        let jobs = [
            job(
                "backup",
                "prod",
                Some("failed"),
                Some("2026-09-28T01:00:00Z"),
            ),
            job(
                "backup",
                "prod",
                Some("lock-conflict"),
                Some("2026-09-28T02:00:00Z"),
            ),
            job(
                "backup",
                "prod",
                Some("succeeded"),
                Some("2026-09-28T03:00:00Z"),
            ),
            job("backup", "prod", None, None),
            job(
                "backup",
                "staging",
                Some("failed"),
                Some("2026-09-28T04:00:00Z"),
            ),
            job(
                "restore",
                "prod",
                Some("failed"),
                Some("2026-09-28T05:00:00Z"),
            ),
        ];
        let events = failed_run_events(&jobs, "prod");
        let levels: Vec<Level> = events.iter().map(|e| e.level).collect();
        assert_eq!(levels, vec![Level::Fail, Level::Warn]);
    }

    #[test]
    fn undated_and_unparseable_entries_are_counted_not_hidden() {
        let doc = r#"{"schema":1,"store":"/b","backups":[
            {"id":"a","type":"full","promoted_from_gap":false,"engine":"mysql","created_at":"2026-09-28T00:00:00Z","stored_size_bytes":1,"chain_status":"ok","base_id":null},
            {"id":"b","type":"orphan","promoted_from_gap":false,"engine":"mysql","created_at":null,"stored_size_bytes":0,"chain_status":"orphan","base_id":null},
            {"id":"c"}
        ]}"#;
        let probe = completed(JobOutcome::Succeeded, doc);
        match row_from_list("prod", &probe, &[], day("2026-09-28"), Lang::En) {
            HistoryRow::Days { days, undated, .. } => {
                assert_eq!(undated, 2);
                assert_eq!(days.last().unwrap().backups, 1);
            }
            other => panic!("행이 그려져야 한다: {other:?}"),
        }
    }

    /// 백업 저장소가 없는 프로파일은 칸을 그리지 않는다 — 칸이 "기록 없음"으로 오해된다.
    #[test]
    fn missing_list_becomes_a_reason_not_empty_cells() {
        let probe = completed(JobOutcome::Rejected, "");
        match row_from_list("restore-target", &probe, &[], day("2026-09-28"), Lang::En) {
            HistoryRow::Unavailable { reason, .. } => {
                assert!(reason.starts_with("No backup list:"))
            }
            other => panic!("칸 대신 이유가 나와야 한다: {other:?}"),
        }
        let timed_out = ProbeOutput {
            outcome: ProbeOutcome::TimedOut(Duration::from_secs(8)),
            elapsed: Duration::from_secs(8),
        };
        assert!(matches!(
            row_from_list("prod", &timed_out, &[], day("2026-09-28"), Lang::En),
            HistoryRow::Unavailable { .. }
        ));
    }
}
