//! `daemon` 핸들러 — 내장 스케줄러(P3-1). 외부 cron 없이 profile별 `schedule`(5필드
//! cron)에 따라 backup을 상주 실행한다.
//!
//! - **재사용 원칙**: 발화 = 기존 [`backup` 핸들러](super::backup) 호출. 프로파일 잠금
//!   (FR-12)·exit code 계약(0~5)·auto-quiet가 그대로 적용된다. 잠금 충돌(exit 5)은
//!   "이전 실행이 아직 도는 중"이라는 뜻 — 이번 발화를 건너뛰고 다음을 기다린다.
//! - **실패 격리**: 한 발화의 실패는 루프를 죽이지 않는다 — 로그(+webhook 알림, P3-2)
//!   후 계속. daemon 자체가 죽는 건 시작 시 설정 오류뿐이다(빠른 실패).
//! - **catch-up 없음(v1)**: 시작 시점 이후의 발화만 스케줄한다 — daemon이 꺼져 있던
//!   동안의 발화는 소급하지 않는다(문서화; catch-up 옵션은 로드맵).
//! - `--print-systemd`는 유닛 파일을 **출력만** 한다 — 시스템 변경을 임의로 하지 않는
//!   기존 원칙(설치는 사용자 몫).

use std::path::PathBuf;

use chrono::Local;

use crate::cli::args::DaemonArgs;
use crate::config::file::Config;
use crate::error::{Result, XBackupError};
use crate::i18n::Lang;
use crate::schedule::CronExpr;

/// 스케줄된 잡 — 프로파일 이름 + cron + 다음 발화 시각.
struct Job {
    profile: String,
    cron: CronExpr,
    cron_text: String,
    next: chrono::DateTime<Local>,
    webhook_env: Option<String>,
}

/// `daemon` 핸들러 진입점.
pub async fn handle(
    config_path: Option<PathBuf>,
    lang_flag: Option<Lang>,
    args: DaemonArgs,
) -> Result<()> {
    if args.print_systemd {
        print_systemd_unit(config_path.as_deref());
        return Ok(());
    }

    let config_path = config_path.ok_or_else(|| {
        XBackupError::Usage("daemon에는 config 파일이 필요합니다(--config/XB_CONFIG)".into())
    })?;
    let config_toml = std::fs::read_to_string(&config_path).map_err(|e| {
        XBackupError::Config(format!(
            "config 파일 읽기 실패({}): {e}",
            config_path.display()
        ))
    })?;
    let lang = crate::i18n::resolve_from_toml(lang_flag, Some(&config_toml));
    let config = Config::from_toml_str(&config_toml)?;

    // 스케줄 잡 수집 — cron 파싱 실패·발화 불가 조합은 시작 시점에 즉시 거부(빠른 실패).
    let now = Local::now();
    let mut jobs = Vec::new();
    let mut names: Vec<&String> = config.profiles.keys().collect();
    names.sort();
    for name in names {
        let prof = &config.profiles[name.as_str()];
        let Some(expr) = prof.schedule.as_deref() else {
            continue;
        };
        let cron = CronExpr::parse(expr)
            .map_err(|e| XBackupError::Config(format!("[profiles.{name}] schedule: {e}")))?;
        let next = cron.next_after(now).ok_or_else(|| {
            XBackupError::Config(format!(
                "[profiles.{name}] schedule '{expr}'는 발화 시각이 없습니다(불가능 조합)"
            ))
        })?;
        jobs.push(Job {
            profile: name.clone(),
            cron,
            cron_text: expr.to_string(),
            next,
            webhook_env: prof.notify.webhook_url_env.clone(),
        });
    }
    if jobs.is_empty() {
        return Err(XBackupError::Usage(
            "schedule이 설정된 프로파일이 없습니다 — config에 \
             `schedule = \"0 3 * * *\"`(5필드 cron)을 추가하세요"
                .into(),
        ));
    }

    // --dry-run: 해석 결과만 출력하고 종료(무변경).
    if args.dry_run {
        if args.json {
            let items: Vec<serde_json::Value> = jobs
                .iter()
                .map(|j| {
                    serde_json::json!({
                        "profile": j.profile,
                        "schedule": j.cron_text,
                        "next_fire": j.next.to_rfc3339(),
                    })
                })
                .collect();
            println!("{}", serde_json::json!({ "jobs": items }));
        } else {
            println!(
                "{}",
                lang.sel("daemon schedule (dry-run):", "daemon 스케줄(dry-run):")
            );
            for j in &jobs {
                println!(
                    "  {}  \"{}\"  → {}",
                    j.profile,
                    j.cron_text,
                    j.next.format("%Y-%m-%d %H:%M %Z")
                );
            }
        }
        return Ok(());
    }

    // 상주 루프 — 가장 이른 발화까지 잠들고, 도래한 잡을 순차 실행한다.
    for j in &jobs {
        tracing::info!(profile = %j.profile, cron = %j.cron_text, next = %j.next, "daemon 스케줄 등록");
    }
    loop {
        let earliest = jobs
            .iter()
            .map(|j| j.next)
            .min()
            .expect("jobs 비어있지 않음(위에서 검증)");
        let now = Local::now();
        if earliest > now {
            let wait = (earliest - now)
                .to_std()
                .unwrap_or(std::time::Duration::from_secs(0));
            tokio::time::sleep(wait).await;
        }

        // 도래한 잡을 순차 실행(같은 storage를 쓰는 프로파일 간 경합 회피 — 단순·안전).
        let now = Local::now();
        for job in jobs.iter_mut() {
            if job.next > now {
                continue;
            }
            run_job(&config_path, lang_flag, job).await;
            // 실행 소요와 무관하게 "지금 이후"의 다음 발화로 전진(밀린 발화 소급 없음).
            let after = Local::now();
            match job.cron.next_after(after) {
                Some(next) => {
                    tracing::info!(profile = %job.profile, next = %next, "다음 발화 예약");
                    job.next = next;
                }
                None => {
                    // 이론상 도달 불가(시작 시 검증) — 방어적으로 잡만 비활성화.
                    tracing::error!(profile = %job.profile, "다음 발화 계산 실패 — 잡 비활성화");
                    job.next = after + chrono::Duration::days(3650);
                }
            }
        }
    }
}

/// 한 발화 실행 — backup 핸들러 호출 + 결과 로그/알림. 실패해도 반환한다(루프 계속).
async fn run_job(config_path: &std::path::Path, lang_flag: Option<Lang>, job: &Job) {
    tracing::info!(profile = %job.profile, "스케줄 발화 — backup 실행");
    let started = std::time::Instant::now();
    let result = super::backup::handle(
        Some(config_path.to_path_buf()),
        lang_flag,
        crate::cli::args::BackupArgs {
            profile: job.profile.clone(),
            backup_type: None, // config 기본(mode.backup_type)을 따른다.
            db: None,
            collection: None,
            no_encrypt: false,
            compress_level: None,
            quiet: false, // 비-TTY auto-quiet가 처리.
            progress: false,
            json: false,
            skip_precheck: false,
        },
    )
    .await;
    let duration_ms = started.elapsed().as_millis() as u64;

    let (ok, exit_code, error) = match &result {
        Ok(()) => (true, 0u8, None),
        Err(e) => {
            let code = e.exit_code();
            // 경고 동반 성공(exit 4)은 성공으로 분류하되 사유를 남긴다.
            (code == 4, code, Some(e.to_string()))
        }
    };
    match (ok, exit_code) {
        (true, 0) => tracing::info!(profile = %job.profile, duration_ms, "발화 성공"),
        (true, _) => {
            tracing::warn!(profile = %job.profile, duration_ms, "발화 성공(경고 동반, exit 4)")
        }
        _ => tracing::error!(
            profile = %job.profile,
            duration_ms,
            exit_code,
            error = error.as_deref().unwrap_or("-"),
            "발화 실패 — 루프는 계속됩니다"
        ),
    }

    // webhook 알림(P3-2) — best-effort: 실패는 경고 로그만(백업 결과 불변).
    if let Some(env_name) = &job.webhook_env {
        let event = crate::notify::BackupEvent {
            event: "backup",
            profile: &job.profile,
            ok,
            exit_code,
            error,
            duration_ms,
            at: chrono::Utc::now().to_rfc3339(),
        };
        let send = async {
            let url = crate::notify::resolve_webhook_url(env_name)?;
            crate::notify::send_webhook(&url, &event).await
        };
        if let Err(e) = send.await {
            tracing::warn!(profile = %job.profile, "webhook 알림 실패(무시): {e}");
        }
    }
}

/// systemd 서비스 유닛을 stdout에 출력한다(설치는 사용자 몫 — 시스템 무변경 원칙).
fn print_systemd_unit(config_path: Option<&std::path::Path>) {
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "/usr/local/bin/x-backup".to_string());
    let config = config_path
        .map(|p| format!(" --config {}", p.display()))
        .unwrap_or_default();
    println!(
        "\
[Unit]
Description=x-backup scheduler daemon (built-in cron)
After=network-online.target
Wants=network-online.target

[Service]
ExecStart={exe} daemon{config}
Restart=on-failure
RestartSec=10
# 시크릿(uri_env 등)은 Environment=/EnvironmentFile=로 주입한다.
# EnvironmentFile=/etc/x-backup/env

[Install]
WantedBy=multi-user.target"
    );
}
