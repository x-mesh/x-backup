//! `migrate` 핸들러 — source(프로파일) → target으로 파일 없이 직접 마이그레이션.
//!
//! config 로드 → source URI 해석 → [`run_migrate`] 직접 스트림. 백업과 달리 destination
//! (파일 저장소)·암호화·manifest를 쓰지 않는다. 잠금(FR-12)은 backup/restore/prune과
//! 같은 프로파일에서 직렬화한다(같은 source를 동시에 dump하지 않도록).

use std::io::IsTerminal;
use std::path::PathBuf;

use crate::cli::args::MigrateArgs;
use crate::cli::output::{OutputFlags, OutputMode};
use crate::config::env::collect_overrides_from_process;
use crate::config::merged::MergeInput;
use crate::config::secret::Secret;
use crate::config::ResolvedConfig;
use crate::error::{Result, XBackupError};
use crate::pipeline::migrate::{run_migrate, MigratePlan, MigrateRequest};

/// `migrate` 핸들러 진입점.
pub async fn handle(config_path: Option<PathBuf>, args: MigrateArgs) -> Result<()> {
    // source를 동시에 dump하지 않도록 같은 프로파일 작업과 직렬화한다(FR-12).
    let _lock = crate::lock::acquire(&args.profile)?;

    // 1) config 로드 + 병합.
    let config_toml = match &config_path {
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

    // 2) source URI(프로파일 source) 확보.
    let source_uri = resolved.resolved_uri.clone().ok_or_else(|| {
        XBackupError::Config(format!(
            "프로파일 '{}'에 source.uri/uri_env가 없습니다(마이그레이션 원본)",
            resolved.profile_name
        ))
    })?;

    let mode = OutputMode::resolve_from_env(
        OutputFlags {
            json: args.json,
            quiet: args.quiet,
            progress: args.progress,
        },
        Some(resolved.profile.mode.output.as_str()),
    );

    let request = MigrateRequest {
        source_uri,
        target_uri: Secret::new(args.target.clone()),
        mongodump_program: "mongodump".to_string(),
        mongorestore_program: "mongorestore".to_string(),
        db: args.db.clone(),
        collection: args.collection.clone(),
        drop: args.drop,
        dry_run: args.dry_run,
    };

    let is_tty = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let (plan, outcome) = run_migrate(&request, args.force, is_tty, prompt_confirm).await?;

    // 3) 출력. dry-run은 계획을, 실제 실행은 완료 요약을 낸다(진행=stderr, 결과=stdout).
    if request.dry_run {
        print_plan(&plan, mode.emits_json());
    } else if let Some(out) = outcome {
        if mode.emits_json() {
            let summary = serde_json::json!({
                "migrated": true,
                "source_topology": out.source_topology,
                "target_had_data": out.target_had_data,
                "ns": plan.ns,
            });
            println!("{summary}");
        } else if mode.shows_human_summary() {
            println!("마이그레이션 완료(파일 없음, 직접 전송)");
            println!(
                "  source:   {} ({})",
                plan.source_server_version, out.source_topology
            );
            println!("  target:   {}", plan.target_server_version);
            match &plan.ns {
                Some(ns) => println!("  대상 ns:  {ns}"),
                None => println!("  대상 ns:  전체"),
            }
            if let Some(w) = &plan.version_warning {
                println!("  경고:     {w}");
            }
        }
    }

    Ok(())
}

/// dry-run 계획 출력(연결·버전·충돌 — 무변경). 시크릿은 출력하지 않는다.
fn print_plan(plan: &MigratePlan, json: bool) {
    if json {
        let summary = serde_json::json!({
            "dry_run": true,
            "source_server_version": plan.source_server_version,
            "source_topology": plan.source_topology,
            "target_server_version": plan.target_server_version,
            "ns": plan.ns,
            "conflicting_namespaces": plan.conflicting_namespaces,
            "version_warning": plan.version_warning,
        });
        println!("{summary}");
        return;
    }
    println!("마이그레이션 계획(dry-run) — 실제 전송 없음");
    println!(
        "  source: {} ({})",
        plan.source_server_version, plan.source_topology
    );
    println!("  target: {}", plan.target_server_version);
    match &plan.ns {
        Some(ns) => println!("  대상 ns: {ns}(선택적)"),
        None => println!("  대상 ns: 전체"),
    }
    if plan.conflicting_namespaces.is_empty() {
        println!("  target 충돌: 없음(빈 대상)");
    } else {
        println!(
            "  target 충돌: {}개 — {} (실제 실행은 --force/--drop 또는 대화형 확인 필요)",
            plan.conflicting_namespaces.len(),
            plan.conflicting_namespaces.join(", ")
        );
    }
    if let Some(w) = &plan.version_warning {
        println!("  경고: {w}");
    }
}

/// target에 기존 데이터가 있을 때 대화형 확인(TTY). 덮어쓰기 전 명시적 동의를 받는다.
fn prompt_confirm(plan: &MigratePlan) -> bool {
    use std::io::Write;
    eprintln!(
        "경고: target에 기존 데이터가 있습니다({}개 네임스페이스): {}",
        plan.conflicting_namespaces.len(),
        plan.conflicting_namespaces.join(", ")
    );
    eprint!("이 데이터를 덮어쓰고 마이그레이션하시겠습니까? [y/N] ");
    let _ = std::io::stderr().flush();
    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        return false;
    }
    matches!(input.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}
