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
    execute_prune, load_backups, plan_prune, PruneKind, PruneOutcome, PrunePlan, RetentionPolicy,
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
        crate::cli::output::context_mode(args.json),
    );

    // store 위치 표기는 list와 같은 함수를 쓴다(표기 이원화 방지). json일 때만 필요하다.
    let store_loc = if args.json {
        crate::cli::handlers::list::store_location(config_toml.as_deref(), &args.profile)
    } else {
        String::new()
    };

    // 보존 정책 — CLI 플래그가 우선, 없으면 config의 [profiles.<name>.retention].
    let cfg_ret = resolve_retention(&config_path, &args.profile);
    let policy = RetentionPolicy {
        keep_full: args.keep_full.or(cfg_ret.keep_full),
        keep_days: args.keep_days.or(cfg_ret.keep_days),
        keep_last: args.keep_last.or(cfg_ret.keep_last),
        recovery_window_days: args.recovery_window_days.or(cfg_ret.recovery_window_days),
        min_redundancy: args.min_redundancy.or(cfg_ret.min_redundancy),
    };

    // manifest·orphan 수집 → 순수 판정.
    let (manifests, orphan_ids) = load_backups(storage.as_ref()).await?;
    let now_secs = chrono::Utc::now().timestamp();
    let plan = plan_prune(&manifests, &orphan_ids, policy, now_secs);

    // 보존 기준 미지정 가드: 명시적 기준 없는 prune은 거부(실수로 전부 보존만 하고 끝).
    if policy.is_unspecified() {
        // json이어도 계획은 내보낸다 — 웹이 "왜 아무것도 안 지웠나"를 stdout으로 읽을 수
        // 있어야 한다(에러 메시지는 stderr, exit 2).
        if args.json {
            print_json(
                &args.profile,
                &store_loc,
                args.dry_run,
                &policy,
                &plan,
                JsonOutcome::Planned,
            );
        } else {
            print_plan(&plan, args.dry_run, lang);
        }
        return Err(XBackupError::Usage(
            "보존 기준이 필요합니다 — --keep-full N / --keep-days D / --keep-last N 중 하나를 \
             주거나 config의 [profiles.<name>.retention]에 설정하세요(기준 없는 prune은 \
             아무것도 삭제하지 않습니다)"
                .into(),
        ));
    }

    // dry-run: 목록만 출력하고 종료(무변경).
    if args.dry_run {
        if args.json {
            print_json(
                &args.profile,
                &store_loc,
                true,
                &policy,
                &plan,
                JsonOutcome::Planned,
            );
        } else {
            print_plan(&plan, true, lang);
        }
        return Ok(());
    }

    // 삭제할 게 없으면 조기 종료. json은 "실행했고 0건 지웠다"로 표기한다(dry_run=false와
    // 함께 읽으면 사람이 승인 단계까지 갈 필요가 없었음이 드러난다).
    if plan.is_empty() {
        if args.json {
            print_json(
                &args.profile,
                &store_loc,
                false,
                &policy,
                &plan,
                JsonOutcome::Executed {
                    deleted: PruneOutcome::default(),
                    include_force_only: false,
                },
            );
            return Ok(());
        }
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

    // 계획을 먼저 보여준다(삭제 전 운영자가 확인할 수 있도록). json은 결과와 함께 한 번만
    // 내보내므로 여기서는 출력하지 않는다 — stdout에 JSON이 두 덩어리로 나오면 안 된다.
    if !args.json {
        print_plan(&plan, false, lang);
    }

    // 실삭제 승인 — orphan/incomplete까지 지울지(include_force_only) 결정한다.
    // json 모드의 프롬프트는 stderr로 나가므로 stdout JSON을 오염시키지 않는다.
    let is_tty = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let approval = decide_approval(&plan, args.force, is_tty, |p| prompt_confirm(p, lang))?;

    if !approval.proceed {
        if args.json {
            print_json(
                &args.profile,
                &store_loc,
                false,
                &policy,
                &plan,
                JsonOutcome::Cancelled,
            );
            return Ok(());
        }
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

    if args.json {
        print_json(
            &args.profile,
            &store_loc,
            false,
            &policy,
            &plan,
            JsonOutcome::Executed {
                deleted: outcome,
                include_force_only: approval.include_force_only,
            },
        );
        return Ok(());
    }

    println!(
        "{}",
        style(
            lang.sel(
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
            lang.sel(
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

/// `prune --json` 출력 스키마 버전.
///
/// 필드 의미가 바뀌면 올린다 — 소비자(웹 콘솔)가 버전으로 파서를 고르고, 모르는 버전을
/// 만나면 조용히 오파싱하는 대신 명확히 실패할 수 있게 하기 위함이다.
const PRUNE_JSON_SCHEMA: u32 = 1;

/// JSON `outcome` 필드가 나타내는 세 가지 종착 상태.
///
/// 사람용 출력은 각 경우를 다른 문장으로 알리지만, 기계 소비자는 같은 파서로 셋을 모두
/// 읽어야 하므로 한 필드의 변형으로 표현한다.
enum JsonOutcome {
    /// dry-run 또는 보존 기준 미지정 — 아무것도 바꾸지 않았다.
    Planned,
    /// 대화형 확인에서 거절 — 아무것도 바꾸지 않았다.
    Cancelled,
    /// 실제 삭제를 수행했다(0건 삭제도 포함).
    Executed {
        deleted: PruneOutcome,
        include_force_only: bool,
    },
}

/// 삭제 후보 분류의 기계·사람 공용 라벨.
fn prune_kind_label(kind: PruneKind) -> &'static str {
    match kind {
        PruneKind::Chain => "chain",
        PruneKind::Orphan => "orphan",
        PruneKind::Incomplete => "incomplete",
    }
}

/// 계획과 결과를 기계 판독 JSON 한 덩어리로 stdout에 출력한다.
///
/// 호출은 실행 경로당 **한 번**이다 — stdout에 JSON이 두 덩어리로 나오면 스트림 파서가
/// 깨진다. 값 생성은 [`build_json`]에 있고 여기서는 출력만 한다(순수부를 테스트하기 위함).
fn print_json(
    profile: &str,
    store_loc: &str,
    dry_run: bool,
    policy: &RetentionPolicy,
    plan: &PrunePlan,
    outcome: JsonOutcome,
) {
    let value = build_json(profile, store_loc, dry_run, policy, plan, outcome);
    println!("{value}");
}

/// JSON 출력 값을 만든다(순수 — 출력 부작용 없음).
///
/// `dry_run`은 요청 자체가 dry-run이었는지를, `outcome`은 실제로 무엇이 일어났는지를
/// 각각 알린다(둘은 독립이다 — 보존 기준 미지정이면 dry_run=false여도 outcome은 null).
fn build_json(
    profile: &str,
    store_loc: &str,
    dry_run: bool,
    policy: &RetentionPolicy,
    plan: &PrunePlan,
    outcome: JsonOutcome,
) -> serde_json::Value {
    let targets: Vec<serde_json::Value> = plan
        .targets
        .iter()
        .map(|t| {
            serde_json::json!({
                "kind": prune_kind_label(t.kind),
                "base_id": t.base_id,
                "member_ids": t.member_ids,
                "reason": t.reason,
                // 잔재(orphan/incomplete)는 --force가 있어야 지워진다 — 웹이 확인 UI를
                // 다르게 그려야 하므로 분류에서 파생하지 말고 명시한다.
                "force_only": !matches!(t.kind, PruneKind::Chain),
            })
        })
        .collect();

    let outcome_value = match outcome {
        JsonOutcome::Planned => serde_json::Value::Null,
        JsonOutcome::Cancelled => serde_json::json!({ "cancelled": true }),
        JsonOutcome::Executed {
            deleted,
            include_force_only,
        } => serde_json::json!({
            "cancelled": false,
            "deleted_chains": deleted.deleted_chains,
            "deleted_backups": deleted.deleted_backups,
            "include_force_only": include_force_only,
        }),
    };

    serde_json::json!({
        "schema": PRUNE_JSON_SCHEMA,
        "profile": profile,
        "store": store_loc,
        "dry_run": dry_run,
        "policy": {
            "keep_full": policy.keep_full,
            "keep_days": policy.keep_days,
            "keep_last": policy.keep_last,
            "recovery_window_days": policy.recovery_window_days,
            "min_redundancy": policy.min_redundancy,
        },
        "targets": targets,
        "retained_base_ids": plan.kept_base_ids,
        "outcome": outcome_value,
    })
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
        let label = prune_kind_label(t.kind);
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

    /// 분류 라벨은 사람용 출력과 JSON이 공유한다 — 문자열이 바뀌면 기존 사람용 출력이
    /// 회귀하므로 값을 고정한다(C6: 기존 CLI 출력 불변).
    #[test]
    fn kind_labels_are_stable() {
        assert_eq!(prune_kind_label(PruneKind::Chain), "chain");
        assert_eq!(prune_kind_label(PruneKind::Orphan), "orphan");
        assert_eq!(prune_kind_label(PruneKind::Incomplete), "incomplete");
    }

    fn sample_policy() -> RetentionPolicy {
        RetentionPolicy {
            keep_full: Some(7),
            keep_days: None,
            keep_last: None,
            recovery_window_days: None,
            min_redundancy: Some(2),
        }
    }

    /// dry-run JSON은 계약 필드를 모두 갖고 outcome이 null이다(무변경 신호).
    #[test]
    fn json_dry_run_shape() {
        let mut plan = plan_with(vec![chain_target("b1"), orphan_target("o1")]);
        plan.kept_base_ids = vec!["b9".to_string()];

        let v = build_json(
            "prod",
            "/srv/backups",
            true,
            &sample_policy(),
            &plan,
            JsonOutcome::Planned,
        );

        assert_eq!(v["schema"], PRUNE_JSON_SCHEMA);
        assert_eq!(v["profile"], "prod");
        assert_eq!(v["store"], "/srv/backups");
        assert_eq!(v["dry_run"], true);
        assert_eq!(v["policy"]["keep_full"], 7);
        assert!(v["policy"]["keep_days"].is_null());
        assert_eq!(v["policy"]["min_redundancy"], 2);
        assert_eq!(v["retained_base_ids"][0], "b9");
        assert!(v["outcome"].is_null(), "dry-run은 아무것도 바꾸지 않는다");

        let targets = v["targets"].as_array().unwrap();
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0]["kind"], "chain");
        assert_eq!(targets[0]["base_id"], "b1");
        assert_eq!(targets[0]["member_ids"][0], "b1");
        assert_eq!(
            targets[0]["force_only"], false,
            "정상 체인은 --force 없이 삭제 가능"
        );
        assert_eq!(targets[1]["kind"], "orphan");
        assert_eq!(
            targets[1]["force_only"], true,
            "잔재는 --force가 있어야 삭제된다"
        );
    }

    /// targets 원소의 키 집합을 고정한다.
    ///
    /// `tests/json_schema_snapshot.rs`는 빈 store로 돌기 때문에 배열 **원소**의 구조를
    /// 관측하지 못한다. 그 사각을 여기서 메운다 — 키가 하나라도 늘거나 줄면 실패한다.
    #[test]
    fn json_target_keys_are_stable() {
        let plan = plan_with(vec![chain_target("b1")]);
        let v = build_json(
            "prod",
            "/srv",
            true,
            &sample_policy(),
            &plan,
            JsonOutcome::Planned,
        );

        let mut keys: Vec<&str> = v["targets"][0]
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["base_id", "force_only", "kind", "member_ids", "reason"],
            "targets 원소 키가 바뀌면 웹 콘솔 파서가 깨진다"
        );
    }

    /// 실삭제 JSON은 삭제 건수와 잔재 포함 여부를 알린다.
    #[test]
    fn json_executed_reports_counts() {
        let plan = plan_with(vec![chain_target("b1")]);
        let v = build_json(
            "prod",
            "s3://bucket",
            false,
            &sample_policy(),
            &plan,
            JsonOutcome::Executed {
                deleted: PruneOutcome {
                    deleted_backups: 5,
                    deleted_chains: 2,
                },
                include_force_only: true,
            },
        );

        assert_eq!(v["dry_run"], false);
        assert_eq!(v["outcome"]["cancelled"], false);
        assert_eq!(v["outcome"]["deleted_chains"], 2);
        assert_eq!(v["outcome"]["deleted_backups"], 5);
        assert_eq!(v["outcome"]["include_force_only"], true);
    }

    /// 대화형 거절은 삭제 건수가 아니라 cancelled로 구분된다(0건 삭제와 다른 의미).
    #[test]
    fn json_cancelled_is_distinct_from_zero_deletion() {
        let plan = plan_with(vec![chain_target("b1")]);
        let cancelled = build_json(
            "prod",
            "/srv",
            false,
            &sample_policy(),
            &plan,
            JsonOutcome::Cancelled,
        );
        let zero = build_json(
            "prod",
            "/srv",
            false,
            &sample_policy(),
            &plan,
            JsonOutcome::Executed {
                deleted: PruneOutcome::default(),
                include_force_only: false,
            },
        );

        assert_eq!(cancelled["outcome"]["cancelled"], true);
        assert!(cancelled["outcome"]["deleted_chains"].is_null());
        assert_eq!(zero["outcome"]["cancelled"], false);
        assert_eq!(zero["outcome"]["deleted_chains"], 0);
    }

    /// 출력은 단일 JSON 문서로 직렬화된다(스트림 파서가 한 덩어리로 읽을 수 있어야 한다).
    #[test]
    fn json_serializes_to_single_document() {
        let plan = plan_with(vec![chain_target("b1")]);
        let v = build_json(
            "prod",
            "/srv",
            true,
            &sample_policy(),
            &plan,
            JsonOutcome::Planned,
        );
        let s = v.to_string();
        assert!(!s.contains('\n'), "개행 없이 한 줄이어야 한다");
        let parsed: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed, v);
    }
}
