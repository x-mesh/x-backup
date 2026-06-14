//! `restore` 서브커맨드 핸들러 — config 로드 → 복구 파이프라인 실행 → 요약 출력.
//!
//! 스코프(t5): destination type=local 풀 복구. `--target`(분리 복구)·`--id`/최신 자동
//! 선택·`--only`(선택적)·`--force`/대화형 가드·`--dry-run`·`--skip-precheck`를 지원한다.
//! PITR(`--at`)은 t9, S3 destination은 t7 소유다(아래 명시적 거부).

use std::io::IsTerminal;
use std::path::PathBuf;

use crate::cli::args::RestoreArgs;
use crate::cli::output::{OutputFlags, OutputMode};
use crate::cli::progress::{new_counter, ProgressKind, ProgressReporter};
use crate::config::env::collect_overrides_from_process;
use crate::config::file::Profile;
use crate::config::merged::MergeInput;
use crate::config::secret::Secret;
use crate::config::ResolvedConfig;
use crate::error::{Result, XBackupError};
use crate::pipeline::pitr::{run_pitr, PitrPlan, PitrRequest};
use crate::pipeline::restore::{run_restore, RestorePlan, RestoreRequest};
use crate::storage::{from_config, Storage};

/// `restore` 핸들러 진입점.
pub async fn handle(config_path: Option<PathBuf>, args: RestoreArgs) -> Result<()> {
    // 동시 실행 잠금(FR-12) — 같은 프로파일의 backup/restore/prune과 직렬화한다.
    // PITR·풀 복구 양쪽을 덮도록 --at 분기보다 먼저 잡고, 가드(_lock)를 끝까지 유지한다.
    let _lock = crate::lock::acquire(&args.profile)?;

    // PITR(--at) 분기(t9) — base 풀 복원 + 증분 oplog 슬라이스 재생으로 시점 복구.
    if let Some(at) = args.at.clone() {
        return handle_pitr(config_path, args, at).await;
    }

    // 1) config 로드 + 레이어 병합(file + ENV).
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

    // 2) 복구 대상 URI: --target 우선, 없으면 프로파일 source(uri_env).
    let target_uri = match &args.target {
        Some(uri) => Secret::new(uri.clone()),
        None => resolved.resolved_uri.ok_or_else(|| {
            XBackupError::Config(format!(
                "프로파일 '{}'에 source.uri_env가 없고 --target도 지정되지 않았습니다",
                resolved.profile_name
            ))
        })?,
    };

    // 3) destination 구성 — local/s3 모두 from_config로. 멀티 destination이면 --from으로
    //    특정 복제본을 고를 수 있고(미지정 시 primary), 모든 복제본은 동일 바이트라 어디서
    //    읽어도 같다.
    let storage = select_restore_storage(&resolved.profile, args.from.as_deref())?;

    // 출력 모드 결정(R15) — CLI > config(mode.output) > stderr TTY 자동.
    let mode = OutputMode::resolve_from_env(
        OutputFlags {
            json: args.json,
            quiet: args.quiet,
            progress: args.progress,
        },
        Some(resolved.profile.mode.output.as_str()),
    );

    // 진행 표시(R16): mongorestore stdin으로 흘리는 바이트를 폴링한다. 복원 입력 총량은
    //   압축 백업이면 복호화·압축해제 후 크기라 사전에 정확히 모르므로 부정형(spinner)으로
    //   처리 바이트·속도를 stderr에 표시한다(dry-run이면 표시 불필요 — 무변경 계획만).
    let progress_counter = new_counter();

    // 4) 복구 요청 조립(진행 카운터 주입 — dry-run이 아니면 폴링한다).
    let request = RestoreRequest {
        target_uri,
        mongorestore_program: "mongorestore".to_string(),
        backup_id: args.id.clone(),
        only: args.only.clone(),
        force: args.force,
        dry_run: args.dry_run,
        skip_precheck: args.skip_precheck,
        timeout_secs: resolved.profile.source.connect_timeout_secs,
        progress_counter: if args.dry_run {
            None
        } else {
            Some(std::sync::Arc::clone(&progress_counter))
        },
    };

    // TTY 여부 — 대화형 확인 가능 여부. stdout 대신 stdin TTY로 본다(확인 입력을 받음).
    let is_tty = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();

    let reporter = if request.dry_run {
        ProgressReporter::disabled(progress_counter)
    } else {
        ProgressReporter::start(
            mode,
            ProgressKind::Indeterminate {
                label: "복구".into(),
            },
            progress_counter,
        )
    };
    let result = run_restore(&request, storage.as_ref(), is_tty, prompt_confirm).await;
    reporter.finish().await;
    let outcome = result?;

    // 5) 출력. dry-run은 계획을, 실제 복구는 완료 요약을 낸다. 진행=stderr, 결과=stdout.
    if request.dry_run {
        print_plan(&outcome.plan, mode.emits_json());
    } else if mode.emits_json() {
        let summary = serde_json::json!({
            "backup_id": outcome.backup_id,
            "stored_size_bytes": outcome.stored_size_bytes,
            "restored": true,
        });
        println!("{summary}");
    } else if mode.shows_human_summary() {
        println!("복구 완료");
        println!("  id:       {}", outcome.backup_id);
        println!("  입력크기: {} bytes", outcome.stored_size_bytes);
        if let Some(only) = &outcome.plan.ns_include {
            println!("  대상ns:   {only}");
        }
    }

    Ok(())
}

/// dry-run 계획을 출력한다(체인·대상·예상 크기·충돌 — PRD §FR-3). 시크릿은 출력하지 않는다.
fn print_plan(plan: &RestorePlan, json: bool) {
    if json {
        let summary = serde_json::json!({
            "dry_run": true,
            "backup_id": plan.backup_id,
            "backup_type": format!("{:?}", plan.backup_type),
            "source_server_version": plan.source_server_version,
            "target_server_version": plan.target_server_version,
            "stored_size_bytes": plan.stored_size_bytes,
            "ns_include": plan.ns_include,
            "conflicting_namespaces": plan.conflicting_namespaces,
            "version_warning": plan.version_warning,
        });
        println!("{summary}");
        return;
    }

    println!("복구 계획(dry-run) — 실제 변경 없음");
    println!("  backup id:     {}", plan.backup_id);
    println!("  유형:          {:?}", plan.backup_type);
    println!("  원본 서버:     {}", plan.source_server_version);
    if let Some(v) = &plan.target_server_version {
        println!("  대상 서버:     {v}");
    }
    println!("  예상 입력크기: {} bytes", plan.stored_size_bytes);
    match &plan.ns_include {
        Some(ns) => println!("  대상 ns:       {ns}(선택적 복구)"),
        None => println!("  대상 ns:       전체"),
    }
    if plan.conflicting_namespaces.is_empty() {
        println!("  충돌 ns:       없음(빈 대상)");
    } else {
        println!(
            "  충돌 ns:       {}개 — {}",
            plan.conflicting_namespaces.len(),
            plan.conflicting_namespaces.join(", ")
        );
        println!(
            "  주의:          기존 데이터가 있습니다 — 실제 복구는 --force 또는 대화형 확인 필요"
        );
    }
    if let Some(w) = &plan.version_warning {
        println!("  경고:          {w}");
    }
}

/// 대화형 확인 프롬프트(TTY). 기존 데이터 덮어쓰기 전 명시적 동의를 받는다.
fn prompt_confirm(plan: &RestorePlan) -> bool {
    use std::io::Write;
    eprintln!(
        "경고: 복원 대상에 기존 데이터가 있습니다({}개 네임스페이스): {}",
        plan.conflicting_namespaces.len(),
        plan.conflicting_namespaces.join(", ")
    );
    eprint!("이 데이터를 덮어쓰고 복구를 진행하시겠습니까? [y/N] ");
    let _ = std::io::stderr().flush();

    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        return false;
    }
    matches!(input.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

// ── PITR(--at) 분기 (t9) ──────────────────────────────────────────────────

/// PITR 복구 핸들러 — `--at` 분기. base 풀 복원 후 증분 oplog 슬라이스를 목표 시점까지 재생.
///
/// 절차: PITR+--only 즉시 거부(exit 2) → config·URI·storage 해석 → [`run_pitr`] →
/// 결정 종료 ts({t,i}+wall-clock)·체인 보고 출력.
async fn handle_pitr(config_path: Option<PathBuf>, args: RestoreArgs, at: String) -> Result<()> {
    // PITR + --only 병용 즉시 거부(pitfall 1-4: --oplogReplay는 ns 필터와 병용 불가, PRD Edge Case).
    crate::pipeline::pitr::reject_pitr_with_only(args.only.as_deref())?;

    // config·URI·storage·타임아웃 해석(풀 복구 경로와 동일 규칙).
    let (target_uri, storage, timeout_secs) = resolve_target_and_storage(&config_path, &args)?;

    // PITR은 oplog 기반(Mongo 전용) — PostgreSQL은 WAL 아카이빙이 필요하며 미지원이다.
    //   --at를 PG 대상에 쓰면 조용히 오작동하므로 명확히 거부한다.
    if crate::engine::DbKind::from_uri(target_uri.expose()) == crate::engine::DbKind::Postgres {
        return Err(XBackupError::Usage(
            "PostgreSQL은 시점 복구(--at, PITR)를 지원하지 않습니다 — PITR은 oplog 기반(Mongo 전용)이며 \
             PG는 WAL 아카이빙이 필요합니다(로드맵). 풀 백업 복구는 --at 없이 사용하세요."
                .into(),
        ));
    }

    let request = PitrRequest {
        target_uri,
        mongorestore_program: "mongorestore".to_string(),
        at,
        force: args.force,
        dry_run: args.dry_run,
        skip_precheck: args.skip_precheck,
        timeout_secs,
    };

    let is_tty = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let outcome = run_pitr(&request, storage.as_ref(), is_tty, prompt_confirm).await?;

    if request.dry_run {
        print_pitr_plan(&outcome.plan, args.json);
    } else if args.json {
        let summary = serde_json::json!({
            "base_id": outcome.plan.base_id,
            "incremental_ids": outcome.plan.incremental_ids,
            "decided_ts": { "t": outcome.plan.decided_ts.t, "i": outcome.plan.decided_ts.i },
            "decided_wall_clock": outcome.plan.decided_wall_clock,
            "replayed_slices": outcome.replayed_slices,
            "restored": true,
        });
        println!("{summary}");
    } else if !args.quiet {
        println!("PITR 복구 완료");
        println!("  base id:       {}", outcome.plan.base_id);
        println!("  재생 슬라이스: {}개", outcome.replayed_slices);
        println!(
            "  결정 종료 ts:  {{t:{}, i:{}}}  ({})",
            outcome.plan.decided_ts.t, outcome.plan.decided_ts.i, outcome.plan.decided_wall_clock
        );
    }

    Ok(())
}

/// config 로드 → 복구 대상 URI·local storage를 해석한다(풀/ PITR 공통 setup).
///
/// 풀 복구 경로의 1~3단계와 동일 규칙: `--target` 우선, destination type=local만,
/// path 필수. 시크릿은 [`Secret`]로 감싼다.
#[allow(clippy::type_complexity)]
fn resolve_target_and_storage(
    config_path: &Option<PathBuf>,
    args: &RestoreArgs,
) -> Result<(Secret, Box<dyn Storage>, Option<u64>)> {
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

    let target_uri = match &args.target {
        Some(uri) => Secret::new(uri.clone()),
        None => resolved.resolved_uri.ok_or_else(|| {
            XBackupError::Config(format!(
                "프로파일 '{}'에 source.uri_env가 없고 --target도 지정되지 않았습니다",
                resolved.profile_name
            ))
        })?,
    };

    let storage = select_restore_storage(&resolved.profile, args.from.as_deref())?;
    Ok((
        target_uri,
        storage,
        resolved.profile.source.connect_timeout_secs,
    ))
}

/// 복구에 쓸 destination 백엔드를 고른다 — 멀티 destination 중 `--from`(이름 또는
/// `type#idx`)으로 특정 복제본을, 미지정 시 primary(첫 destination)를 [`from_config`]로
/// 구성한다. 모든 복제본은 동일 바이트이므로 어디서 읽어도 결과가 같다.
fn select_restore_storage(profile: &Profile, from: Option<&str>) -> Result<Box<dyn Storage>> {
    let dests = profile.effective_destinations();
    let chosen = match from {
        None => dests[0],
        Some(name) => dests
            .iter()
            .enumerate()
            .find(|(i, d)| d.name.as_deref() == Some(name) || d.label(*i) == name)
            .map(|(_, d)| *d)
            .ok_or_else(|| {
                let avail: Vec<String> =
                    dests.iter().enumerate().map(|(i, d)| d.label(i)).collect();
                XBackupError::Config(format!(
                    "--from '{name}'에 해당하는 destination이 없습니다(가용: {})",
                    avail.join(", ")
                ))
            })?,
    };
    from_config(chosen)
}

/// PITR dry-run 계획을 출력한다(base·증분 체인·결정 종료 ts·예상 크기 — 무변경). 시크릿 미출력.
fn print_pitr_plan(plan: &PitrPlan, json: bool) {
    if json {
        let summary = serde_json::json!({
            "dry_run": true,
            "base_id": plan.base_id,
            "incremental_ids": plan.incremental_ids,
            "target_unix": plan.target_unix,
            "decided_ts": { "t": plan.decided_ts.t, "i": plan.decided_ts.i },
            "decided_wall_clock": plan.decided_wall_clock,
            "limit_slice_id": plan.limit_slice_id,
            "estimated_bytes": plan.estimated_bytes,
        });
        println!("{summary}");
        return;
    }

    println!("PITR 복구 계획(dry-run) — 실제 변경 없음");
    println!("  base id:       {}", plan.base_id);
    if plan.incremental_ids.is_empty() {
        println!("  증분 체인:     없음(base만으로 목표 시점 도달)");
    } else {
        println!(
            "  증분 체인:     {}개 — {}",
            plan.incremental_ids.len(),
            plan.incremental_ids.join(", ")
        );
    }
    if let Some(limit) = &plan.limit_slice_id {
        println!("  한계 슬라이스: {limit}(이 슬라이스에 --oplogLimit 적용)");
    } else {
        println!("  한계 슬라이스: 없음(체인 전체 재생)");
    }
    println!(
        "  결정 종료 ts:  {{t:{}, i:{}}}  ({})",
        plan.decided_ts.t, plan.decided_ts.i, plan.decided_wall_clock
    );
    println!("  예상 입력크기: {} bytes", plan.estimated_bytes);
}
