//! `prune` 서브커맨드 핸들러 — 보존 기준에 따른 체인 안전 삭제(PRD §FR-11, R19).
//!
//! 흐름: 잠금 획득(FR-12) → config 로드(destination=local) → manifest·orphan 수집 →
//! [`plan_prune`]로 체인 단위 보존/삭제 판정 → `--dry-run`이면 목록만, 아니면
//! `--force`/대화형 확인 후 실삭제.
//!
//! ## 동시 실행 잠금(FR-12)
//! prune은 backup/restore와 같은 프로파일에서 동시에 돌면 체인 정합성을 깰 수 있어
//! 핸들러 시작부에서 프로파일 단위 lock을 잡는다. 가드(`_lock`)는 함수 끝까지 살아
//! 있어 작업 동안 lock을 유지한다(Drop 시 자동 해제).
//!
//! ## 삭제 게이팅
//! - `--dry-run`: 삭제 대상(체인 단위·사유)만 출력하고 아무것도 바꾸지 않는다.
//! - 실삭제: `--force`(비대화형 자동 승인) 또는 TTY 대화형 확인. 비-TTY인데 `--force`도
//!   없으면 거부(exit 1) — cron에서 의도치 않은 삭제를 막는다.
//! - orphan/incomplete 잔재는 정상 체인과 분리해, **`--force`일 때만** 삭제한다(보수적).

use std::io::IsTerminal;
use std::path::PathBuf;

use crate::cli::args::PruneArgs;
use crate::cli::output::{field_line, field_line_toned, style, style_stderr, Tone};
use crate::config::env::collect_overrides_from_process;
use crate::config::merged::MergeInput;
use crate::config::ResolvedConfig;
use crate::error::{Result, XBackupError};
use crate::i18n::Lang;
use crate::lock::LockGuard;
use crate::pipeline::prune::{
    execute_prune, load_backups, plan_prune, PruneKind, PrunePlan, RetentionPolicy,
};
use crate::storage::{LocalFs, Storage};

/// `prune` 핸들러 진입점.
pub async fn handle(
    config_path: Option<PathBuf>,
    lang_flag: Option<crate::i18n::Lang>,
    args: PruneArgs,
) -> Result<()> {
    // 동시 실행 잠금(FR-12) — 같은 프로파일의 backup/restore/prune과 직렬화한다.
    // 가드를 함수 스코프 끝까지 유지(_lock)해 작업 동안 lock을 잡는다.
    let _lock: LockGuard = crate::lock::acquire(&args.profile)?;

    // 출력 설명 언어 결정(라벨은 항상 영문, 설명만 ko/en 토글).
    let config_toml = config_path
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok());
    let lang = crate::i18n::resolve_from_toml(lang_flag, config_toml.as_deref());

    let storage = open_storage(&config_path, &args).await?;

    // 컨텍스트(프로파일) 표시 — prune은 store 대상이라 DB는 생략(행별 의미 없음).
    crate::cli::output::print_run_context(
        &args.profile,
        None,
        crate::cli::output::context_mode(false),
    );

    // 보존 정책 — CLI 플래그가 우선, 없으면 config의 [profiles.<name>.retention].
    let cfg_ret = resolve_retention(&config_path, &args.profile);
    let policy = RetentionPolicy {
        keep_full: args.keep_full.or(cfg_ret.keep_full),
        keep_days: args.keep_days.or(cfg_ret.keep_days),
        keep_last: args.keep_last.or(cfg_ret.keep_last),
    };

    // manifest·orphan 수집 → 순수 판정.
    let (manifests, orphan_ids) = load_backups(storage.as_ref()).await?;
    let now_secs = chrono::Utc::now().timestamp();
    let plan = plan_prune(&manifests, &orphan_ids, policy, now_secs);

    // 보존 기준 미지정 가드: 명시적 기준 없는 prune은 거부(실수로 전부 보존만 하고 끝).
    if policy.is_unspecified() {
        print_plan(&plan, args.dry_run, lang);
        return Err(XBackupError::Usage(
            "보존 기준이 필요합니다 — --keep-full N / --keep-days D / --keep-last N 중 하나를 \
             주거나 config의 [profiles.<name>.retention]에 설정하세요(기준 없는 prune은 \
             아무것도 삭제하지 않습니다)"
                .into(),
        ));
    }

    // dry-run: 목록만 출력하고 종료(무변경).
    if args.dry_run {
        print_plan(&plan, true, lang);
        return Ok(());
    }

    // 삭제할 게 없으면 조기 종료.
    if plan.is_empty() {
        println!(
            "{}",
            style(
                lang.sel(
                    "No backups to delete (all chains within retention).",
                    "삭제할 백업이 없습니다(모든 체인이 보존 기준 이내)."
                ),
                Tone::Success,
            )
        );
        return Ok(());
    }

    // 계획을 먼저 보여준다(삭제 전 운영자가 확인할 수 있도록).
    print_plan(&plan, false, lang);

    // 실삭제 승인 — orphan/incomplete까지 지울지(include_force_only) 결정한다.
    let is_tty = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let approval = decide_approval(&plan, args.force, is_tty, |p| prompt_confirm(p, lang))?;

    if !approval.proceed {
        println!(
            "{}",
            style(
                lang.sel(
                    "Cancelled — nothing was deleted.",
                    "취소되었습니다 — 삭제하지 않았습니다."
                ),
                Tone::Muted,
            )
        );
        return Ok(());
    }

    let outcome = execute_prune(storage.as_ref(), &plan, approval.include_force_only).await?;

    println!(
        "{}",
        style(
            &lang.sel(
                &format!(
                    "prune done — deleted {} chains, {} backups",
                    outcome.deleted_chains, outcome.deleted_backups
                ),
                &format!(
                    "prune 완료 — 체인 {}개, 백업 {}개 삭제",
                    outcome.deleted_chains, outcome.deleted_backups
                )
            ),
            Tone::Success,
        )
    );
    Ok(())
}

/// 삭제 승인 결과.
#[derive(Debug)]
struct Approval {
    /// 삭제를 진행할지.
    proceed: bool,
    /// orphan/incomplete 잔재까지 삭제할지(--force일 때만 true).
    include_force_only: bool,
}

/// `--force`/TTY 여부로 삭제 승인을 결정한다(순수에 가깝게 — confirm은 주입).
///
/// - `--force`: 비대화형 자동 승인 + orphan/incomplete 잔재까지 삭제.
/// - TTY(대화형) + force 없음: 사용자에게 확인을 받는다(정상 체인만 — 잔재는 force 필요).
/// - 비-TTY + force 없음: 거부([`XBackupError::Failure`], exit 1) — cron 안전장치.
fn decide_approval(
    plan: &PrunePlan,
    force: bool,
    is_tty: bool,
    confirm: impl Fn(&PrunePlan) -> bool,
) -> Result<Approval> {
    if force {
        return Ok(Approval {
            proceed: true,
            include_force_only: true, // force는 잔재까지 정리.
        });
    }
    if !is_tty {
        return Err(XBackupError::Failure(
            "비대화형 환경에서는 --force 없이 삭제할 수 없습니다(--dry-run으로 먼저 확인하세요)"
                .into(),
        ));
    }
    // 대화형 확인 — 정상 체인만 삭제(orphan/incomplete 잔재는 --force가 있어야 정리).
    let proceed = confirm(plan);
    Ok(Approval {
        proceed,
        include_force_only: false,
    })
}

/// 대화형 확인 프롬프트(TTY). 삭제 전 명시적 동의를 받는다(stderr — stdout은 결과 전용).
fn prompt_confirm(plan: &PrunePlan, lang: Lang) -> bool {
    use std::io::Write;
    let chain_count = plan
        .targets
        .iter()
        .filter(|t| t.kind == PruneKind::Chain)
        .count();
    eprint!(
        "{}",
        style_stderr(
            &lang.sel(
                &format!("Delete the {chain_count} chains above? [y/N] "),
                &format!("위 {chain_count}개 체인을 삭제하시겠습니까? [y/N] ")
            ),
            Tone::Warning,
        )
    );
    let _ = std::io::stderr().flush();

    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        return false;
    }
    matches!(input.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// 삭제 계획을 출력한다(체인 단위·사유). dry-run이면 헤더를 그에 맞게 표기한다.
fn print_plan(plan: &PrunePlan, dry_run: bool, lang: Lang) {
    if dry_run {
        println!(
            "{}",
            style(
                lang.sel(
                    "prune plan (dry-run) — no actual deletion",
                    "prune 계획(dry-run) — 실제 삭제 없음"
                ),
                Tone::Plan,
            )
        );
    } else {
        println!(
            "{}",
            style(
                lang.sel("prune deletion targets", "prune 삭제 대상"),
                Tone::Warning
            )
        );
    }

    if plan.targets.is_empty() {
        println!(
            "  {}",
            style(
                lang.sel(
                    "no deletion targets (all chains within retention).",
                    "삭제 대상 없음(모든 체인이 보존 기준 이내)."
                ),
                Tone::Success,
            )
        );
    }

    for t in &plan.targets {
        let label = match t.kind {
            PruneKind::Chain => "chain",
            PruneKind::Orphan => "orphan",
            PruneKind::Incomplete => "incomplete",
        };
        let tone = match t.kind {
            PruneKind::Chain => Tone::Warning,
            PruneKind::Orphan | PruneKind::Incomplete => Tone::Danger,
        };
        println!(
            "  {} {}",
            style(&format!("[{label}]"), tone),
            field_line(
                "base",
                format!(
                    "{} ({} {}): {}",
                    style(&t.base_id, Tone::Value),
                    style(&t.member_ids.len().to_string(), tone),
                    lang.sel("members", "구성원"),
                    t.reason
                ),
                6,
            )
            .trim_start()
        );
        // 체인은 구성원 ID를 들여쓰기로 나열(투명성).
        if t.kind == PruneKind::Chain {
            for id in &t.member_ids {
                let role = if id == &t.base_id { "base" } else { "incr" };
                println!("        - {} ({})", style(id, Tone::Muted), role);
            }
        }
    }

    if !plan.kept_base_ids.is_empty() {
        println!(
            "{}",
            field_line_toned("retained", plan.kept_base_ids.join(", "), 11, Tone::Muted)
        );
    }

    if plan.has_force_only_targets() {
        println!();
        println!(
            "  {}",
            style(
                lang.sel(
                    "note: orphan/incomplete residue is deleted only with --force (interactive confirm covers normal chains only).",
                    "주의: orphan/incomplete 잔재는 --force일 때만 삭제됩니다(대화형 확인은 정상 체인만)."
                ),
                Tone::Warning,
            )
        );
    }
}

/// config의 [profiles.<name>.retention]을 읽는다(없거나 해석 실패면 빈 정책). prune 기본값.
fn resolve_retention(
    config_path: &Option<PathBuf>,
    profile: &str,
) -> crate::config::file::RetentionConfig {
    let config_toml = config_path
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok());
    ResolvedConfig::build(MergeInput {
        config_toml: config_toml.as_deref(),
        profile_name: profile,
        overrides: &collect_overrides_from_process(),
    })
    .map(|r| r.profile.retention)
    .unwrap_or_default()
}

/// config를 읽어 destination=local Storage를 연다(list/backup 핸들러와 동일 패턴).
async fn open_storage(config_path: &Option<PathBuf>, args: &PruneArgs) -> Result<Box<dyn Storage>> {
    let config_toml = match config_path {
        Some(path) => Some(std::fs::read_to_string(path).map_err(|e| {
            XBackupError::Config(format!("config 파일 읽기 실패({}): {e}", path.display()))
        })?),
        None => None,
    };
    let overrides = collect_overrides_from_process();
    let resolved = ResolvedConfig::build(MergeInput {
        config_toml: config_toml.as_deref(),
        profile_name: &args.profile,
        overrides: &overrides,
    })?;

    let dest = &resolved.profile.destination;
    match dest.r#type.as_deref() {
        Some("local") => {}
        Some("s3") => {
            return Err(XBackupError::Usage(
                "destination type=s3는 아직 미지원입니다(t7) — type=local만 동작".into(),
            ))
        }
        Some(other) => {
            return Err(XBackupError::Config(format!(
                "알 수 없는 destination type: '{other}'(local만 지원)"
            )))
        }
        None => {
            return Err(XBackupError::Config(
                "destination.type이 지정되지 않았습니다(local 필요)".into(),
            ))
        }
    }
    let root = dest.path.as_deref().ok_or_else(|| {
        XBackupError::Config("destination.path가 지정되지 않았습니다(local 경로)".into())
    })?;
    Ok(Box::new(LocalFs::new(root)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::prune::PruneTarget;

    fn chain_target(base: &str) -> PruneTarget {
        PruneTarget {
            kind: PruneKind::Chain,
            base_id: base.to_string(),
            member_ids: vec![base.to_string()],
            reason: "test".to_string(),
        }
    }

    fn orphan_target(id: &str) -> PruneTarget {
        PruneTarget {
            kind: PruneKind::Orphan,
            base_id: id.to_string(),
            member_ids: vec![id.to_string()],
            reason: "test".to_string(),
        }
    }

    fn plan_with(targets: Vec<PruneTarget>) -> PrunePlan {
        PrunePlan {
            targets,
            kept_base_ids: Vec::new(),
        }
    }

    /// --force는 비대화형이라도 승인하고 잔재까지 삭제한다.
    #[test]
    fn force_approves_and_includes_residue() {
        let plan = plan_with(vec![chain_target("c"), orphan_target("o")]);
        let a = decide_approval(&plan, true, false, |_| false).unwrap();
        assert!(a.proceed);
        assert!(a.include_force_only);
    }

    /// 비-TTY + force 없음은 거부(exit 1) — cron 안전장치.
    #[test]
    fn non_tty_without_force_is_rejected() {
        let plan = plan_with(vec![chain_target("c")]);
        let err = decide_approval(&plan, false, false, |_| true).unwrap_err();
        assert_eq!(err.exit_code(), 1);
    }

    /// TTY 대화형에서 yes면 승인하되 잔재는 제외(force 필요).
    #[test]
    fn tty_yes_approves_chains_only() {
        let plan = plan_with(vec![chain_target("c"), orphan_target("o")]);
        let a = decide_approval(&plan, false, true, |_| true).unwrap();
        assert!(a.proceed);
        assert!(!a.include_force_only, "대화형은 잔재를 지우지 않아야 함");
    }

    /// TTY 대화형에서 no면 삭제하지 않는다.
    #[test]
    fn tty_no_declines() {
        let plan = plan_with(vec![chain_target("c")]);
        let a = decide_approval(&plan, false, true, |_| false).unwrap();
        assert!(!a.proceed);
    }
}
