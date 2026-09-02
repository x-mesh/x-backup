//! `restore` 서브커맨드 핸들러 — config 로드 → 복구 파이프라인 실행 → 요약 출력.
//!
//! 풀 복구 + 시점 복구(PITR, `--at`). 복구 대상은 `--target`(URI 직접)·`--target-profile`(다른
//! 프로파일의 source)·미지정 시 프로파일 자신 source(in-place) 순으로 정한다([`resolve_restore_target`]).
//! 그 외 `--id`/최신 자동 선택·`--only`(선택적)·`--force`/대화형 가드·`--dry-run`·`--skip-precheck`를
//! 지원하고 로컬/S3 destination을 모두 다룬다. PITR은 MongoDB(oplog 재생)·PostgreSQL(logical decoding
//! 재생) 양쪽을 지원한다([`handle_pitr`] → Mongo는 [`run_pitr`], PG는 [`handle_pg_pitr`]).

use std::io::IsTerminal;
use std::path::PathBuf;

use crate::cli::args::RestoreArgs;
use crate::cli::output::{
    field_line, field_line_toned, style, style_stderr, OutputFlags, OutputMode, Tone,
};
use crate::cli::progress::{new_counter, ProgressKind, ProgressReporter};
use crate::config::env::collect_overrides_from_process;
use crate::config::file::{DestinationConfig, Profile};
use crate::config::merged::MergeInput;
use crate::config::secret::Secret;
use crate::config::ResolvedConfig;
use crate::error::{Result, XBackupError};
use crate::i18n::Lang;
use crate::pipeline::mysql_pitr::{run_mysql_pitr, MysqlPitrPlan, MysqlPitrRequest};
use crate::pipeline::pg_pitr::{run_pg_pitr, PgPitrPlan, PgPitrRequest};
use crate::pipeline::pitr::{run_pitr, PitrPlan, PitrRequest};
use crate::pipeline::restore::{export_to_dir, run_restore, RestorePlan, RestoreRequest};
use crate::storage::{from_config, Storage};

/// `restore` 핸들러 진입점.
pub async fn handle(
    config_path: Option<PathBuf>,
    lang_flag: Option<crate::i18n::Lang>,
    args: RestoreArgs,
) -> Result<()> {
    // 동시 실행 잠금(FR-12) — 같은 프로파일의 backup/restore/prune과 직렬화한다.
    // PITR·풀 복구 양쪽을 덮도록 --at 분기보다 먼저 잡고, 가드(_lock)를 끝까지 유지한다.
    let _lock = crate::lock::acquire(&args.profile)?;

    // PITR(--at) 분기 — base 풀 복원 + 증분 슬라이스 재생으로 시점 복구(Mongo oplog / PG
    // logical decoding). 대상 DB 종류는 handle_pitr 안에서 분기한다.
    if let Some(at) = args.at.clone() {
        return handle_pitr(config_path, lang_flag, args, at).await;
    }

    // 1) config 로드 + 레이어 병합(file + ENV).
    let config_toml = match &config_path {
        Some(path) => Some(std::fs::read_to_string(path).map_err(|e| {
            XBackupError::Config(format!("config 파일 읽기 실패({}): {e}", path.display()))
        })?),
        None => None,
    };
    let lang = crate::i18n::resolve_from_toml(lang_flag, config_toml.as_deref());
    let overrides = collect_overrides_from_process();
    let resolved = ResolvedConfig::build(MergeInput {
        config_toml: config_toml.as_deref(),
        profile_name: &args.profile,
        overrides: &overrides,
    })?;

    // --to-dir: DB 서버로 복원하는 대신 백업을 로컬 mongodump 덤프로 추출한다(서버 불필요).
    //   target 해석·연결을 전부 건너뛰는 별도 경로.
    if let Some(to_dir) = args.to_dir.clone() {
        return handle_export(&resolved, &args, to_dir, lang).await;
    }

    // 2) 복구 대상 URI·출처: --target(URI) > --target-profile(다른 프로파일 source) >
    //    프로파일 자신 source(in-place 기본).
    let (target_uri, target_origin) =
        resolve_restore_target(&args, config_toml.as_deref(), &overrides, &resolved)?;

    // 3) destination 구성 — local/s3 모두 from_config로. 멀티 destination이면 --from으로
    //    특정 복제본을 고를 수 있고(미지정 시 primary), 모든 복제본은 동일 바이트라 어디서
    //    읽어도 같다.
    let storage = select_restore_storage(
        &resolved.profile,
        &resolved.profile_name,
        args.from.as_deref(),
    )?;

    // 출력 모드 결정(R15) — CLI > config(mode.output) > stderr TTY 자동.
    let mode = OutputMode::resolve_from_env(
        OutputFlags {
            json: args.json,
            quiet: args.quiet,
            progress: args.progress,
        },
        Some(resolved.profile.mode.output.as_str()),
    );

    // 실행 컨텍스트를 **백업 선택(피커) 전에** 먼저 보여준다 — "어떤 프로파일·어느 store의
    // 백업을, 어디로 복구하는지"가 고를 때부터 보이도록(다중 DB·다중 프로파일 오조작 방지).
    crate::cli::output::print_run_context(
        &args.profile,
        Some(crate::engine::DbKind::from_uri(target_uri.expose())),
        mode,
    );
    // 백업을 읽어올 store 위치(list의 `store:`와 동일 의미) — 선택 대상이 어느 저장소인지 명시.
    let store_label =
        store_location_label(choose_destination(&resolved.profile, args.from.as_deref())?);
    crate::cli::output::print_backup_store(&store_label, mode);
    // 복구가 어느 host로 들어가는지 명시(자격증명 마스킹) — in-place 기본값이라 운영 DB
    // 오버라이트 사고를 막는다. 대상 출처(--target / --target-profile / 프로파일 source)도 함께 보인다.
    crate::cli::output::print_restore_target(&target_uri, &target_origin, mode, lang);

    // TTY 여부 — 대화형 선택/확인 가능 여부. stdin/stderr 모두 터미널일 때만 인터랙션한다
    //   (선택·확인 입력은 stdin, 표시는 stderr; stdout은 결과 전용).
    // Bubble Tea는 대체 화면을 stdout에 그린다. 세 표준 스트림이 모두 TTY일 때만 열어
    // stdout 파이프의 기계 출력 계약을 지킨다.
    let is_tty = std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && std::io::stderr().is_terminal();

    // 복구할 백업 결정: --id 있으면 그대로, 없고 대화형(TTY·비-json·비-quiet)이면 fuzzy
    //   피커로 고르게 한다(최신이 기본). 비대화형/기계출력은 최신 풀백업 자동선택으로 폴백.
    let backup_id = resolve_backup_id(&args, storage.as_ref(), is_tty, lang).await?;

    // 진행 표시(R16): mongorestore stdin으로 흘리는 바이트를 폴링한다. 복원 입력 총량은
    //   압축 백업이면 복호화·압축해제 후 크기라 사전에 정확히 모르므로 부정형(spinner)으로
    //   처리 바이트·속도를 stderr에 표시한다(dry-run이면 표시 불필요 — 무변경 계획만).
    let progress_counter = new_counter();

    // 4) 복구 요청 조립(진행 카운터 주입 — dry-run이 아니면 폴링한다).
    let request = RestoreRequest {
        target_uri,
        mongorestore_program: "mongorestore".to_string(),
        backup_id,
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
    // 덮어쓰기 확인 프롬프트는 스피너가 도는 stderr에 그려지므로, 바를 잠시 비운 채
    // 출력·입력을 받도록 suspend로 감싼다(안 그러면 [y/N] 줄이 스피너 프레임에 덮인다).
    let suspend = reporter.suspend_handle();
    let result = run_restore(&request, storage.as_ref(), is_tty, |plan| {
        suspend.run(|| prompt_confirm(plan, lang))
    })
    .await;
    reporter.finish().await;
    let outcome = result?;

    // 5) 출력. dry-run은 계획을, 실제 복구는 완료 요약을 낸다. 진행=stderr, 결과=stdout.
    //    복구 대상(host)은 자격증명을 가린 형태로 계획/요약/json 모두에 명시한다.
    let destination = crate::cli::output::redact_uri(request.target_uri.expose());
    if request.dry_run {
        print_plan(&outcome.plan, mode.emits_json(), &destination, lang);
    } else if mode.emits_json() {
        let summary = serde_json::json!({
            "backup_id": outcome.backup_id,
            "stored_size_bytes": outcome.stored_size_bytes,
            "destination": destination,
            "restored": true,
        });
        println!("{summary}");
    } else if mode.shows_human_summary() {
        const W: usize = 15;
        println!(
            "{}",
            style(lang.sel("Restore complete", "복구 완료"), Tone::Success)
        );
        println!(
            "{}",
            field_line_toned("id", &outcome.backup_id, W, Tone::Value)
        );
        println!(
            "{}",
            field_line_toned("destination", &destination, W, Tone::Value)
        );
        println!(
            "{}",
            field_line(
                "stored size",
                format!(
                    "{} bytes",
                    style(&outcome.stored_size_bytes.to_string(), Tone::Value)
                ),
                W,
            )
        );
        if let Some(only) = &outcome.plan.ns_include {
            println!("{}", field_line_toned("target ns", only, W, Tone::Value));
        }
    }

    Ok(())
}

/// `--to-dir` 경로 — 백업을 **서버 없이** 로컬 mongodump 덤프 디렉터리로 추출한다.
///
/// target 해석·연결을 전혀 하지 않는다. 백업 선택(`--id`/피커/최신)·`--from`·`--only`·dry-run·
/// `--json`은 일반 복구와 동일하게 동작하고, 실제 쓰기는 [`export_to_dir`]가 한다(native 풀
/// 백업만 — 가드는 거기서). 끝에 `mongorestore <DIR>` 안내를 덧붙인다.
async fn handle_export(
    resolved: &ResolvedConfig,
    args: &RestoreArgs,
    to_dir: PathBuf,
    lang: Lang,
) -> Result<()> {
    let storage = select_restore_storage(
        &resolved.profile,
        &resolved.profile_name,
        args.from.as_deref(),
    )?;
    let mode = OutputMode::resolve_from_env(
        OutputFlags {
            json: args.json,
            quiet: args.quiet,
            progress: args.progress,
        },
        Some(resolved.profile.mode.output.as_str()),
    );
    crate::cli::output::print_run_context(&args.profile, None, mode);

    let is_tty = std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && std::io::stderr().is_terminal();
    let backup_id = resolve_backup_id(args, storage.as_ref(), is_tty, lang).await?;

    // dry-run: 무엇을 어디로 추출할지만 알리고 무변경(피커는 위에서 이미 선택값 반영).
    if args.dry_run {
        let target = backup_id.as_deref().unwrap_or("(latest full)");
        if mode.emits_json() {
            println!(
                "{}",
                serde_json::json!({
                    "dry_run": true,
                    "backup_id": backup_id,
                    "to_dir": to_dir.display().to_string(),
                })
            );
        } else {
            const W: usize = 12;
            println!(
                "{}",
                style(
                    lang.sel(
                        "Export plan (dry-run) — no files written",
                        "추출 계획(dry-run) — 파일 미생성"
                    ),
                    Tone::Plan,
                )
            );
            println!("{}", field_line_toned("backup", target, W, Tone::Value));
            println!(
                "{}",
                field_line_toned("to-dir", to_dir.display().to_string(), W, Tone::Value)
            );
        }
        return Ok(());
    }

    // 출력 디렉터리에 기존 내용이 있으면 경고(같은 이름 파일은 덮어쓴다).
    if !mode.emits_json() && crate::engine::native::export::dir_nonempty(&to_dir) {
        eprintln!(
            "{}",
            style_stderr(
                lang.sel(
                    &format!(
                        "⚠ output dir is not empty — same-named files will be overwritten: {}",
                        to_dir.display()
                    ),
                    &format!(
                        "⚠ 출력 디렉터리가 비어있지 않습니다 — 같은 이름 파일은 덮어씁니다: {}",
                        to_dir.display()
                    ),
                ),
                Tone::Warning,
            )
        );
    }

    let outcome = export_to_dir(
        storage.as_ref(),
        backup_id.as_deref(),
        &to_dir,
        args.only.as_deref(),
    )
    .await?;

    if mode.emits_json() {
        println!(
            "{}",
            serde_json::json!({
                "backup_id": outcome.backup_id,
                "to_dir": outcome.out_dir.display().to_string(),
                "collections": outcome.summary.collections,
                "documents": outcome.summary.documents,
                "bytes": outcome.summary.bytes,
                "exported": true,
            })
        );
    } else if mode.shows_human_summary() {
        const W: usize = 14;
        println!(
            "{}",
            style(lang.sel("Export complete", "추출 완료"), Tone::Success)
        );
        println!(
            "{}",
            field_line_toned("backup id", &outcome.backup_id, W, Tone::Value)
        );
        println!(
            "{}",
            field_line_toned(
                "to-dir",
                outcome.out_dir.display().to_string(),
                W,
                Tone::Value
            )
        );
        println!(
            "{}",
            field_line_toned(
                "collections",
                outcome.summary.collections.to_string(),
                W,
                Tone::Value
            )
        );
        println!(
            "{}",
            field_line_toned(
                "documents",
                outcome.summary.documents.to_string(),
                W,
                Tone::Value
            )
        );
        println!(
            "{}",
            field_line(
                "size",
                crate::engine::mongo::status::human_bytes(outcome.summary.bytes as i64),
                W,
            )
        );
        println!(
            "{}",
            style(
                lang.sel(
                    &format!(
                        "→ restore with:  mongorestore {}",
                        outcome.out_dir.display()
                    ),
                    &format!("→ 복원:  mongorestore {}", outcome.out_dir.display())
                ),
                Tone::Plan,
            )
        );
    }
    Ok(())
}

/// 복구할 백업 ID를 정한다 — `--id` 우선, 없고 대화형이면 fuzzy 피커, 그 외는 `None`(최신 자동).
///
/// - `--id` 지정: 그대로 사용한다.
/// - 비대화형(비-TTY)·기계출력(`--json`)·조용(`--quiet`): 인터랙션 없이 `None`을 돌려준다
///   — `build_plan`이 최신 풀백업을 자동 선택한다(기존 동작 보존, cron/CI 안전).
/// - 대화형: 풀+완료 후보를 fuzzy 피커로 고르게 한다(최신이 기본). 후보가 없으면 `None`으로
///   폴백해 `latest_full_manifest`가 명확한 에러를 내게 하고, 사용자가 취소(Esc)하면 거부(exit 1).
async fn resolve_backup_id(
    args: &RestoreArgs,
    storage: &dyn Storage,
    is_tty: bool,
    lang: Lang,
) -> Result<Option<String>> {
    if let Some(id) = &args.id {
        return Ok(Some(id.clone()));
    }
    if !is_tty || args.json || args.quiet {
        return Ok(None);
    }
    let choices = crate::cli::picker::full_backup_choices(storage).await?;
    if choices.is_empty() {
        return Ok(None);
    }
    match crate::cli::picker::pick_backup(&choices, lang).await? {
        Some(id) => Ok(Some(id)),
        None => Err(XBackupError::Failure("백업 선택을 취소했습니다".into())),
    }
}

/// dry-run 계획을 출력한다(체인·대상·예상 크기·충돌 — PRD §FR-3). 시크릿은 출력하지 않는다.
/// `destination`은 [`redact_uri`](crate::cli::output::redact_uri)로 가린 복구 대상 host다.
fn print_plan(plan: &RestorePlan, json: bool, destination: &str, lang: Lang) {
    if json {
        let summary = serde_json::json!({
            "dry_run": true,
            "backup_id": plan.backup_id,
            "backup_type": format!("{:?}", plan.backup_type),
            "destination": destination,
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

    const W: usize = 22;
    println!(
        "{}",
        style(
            lang.sel(
                "Restore plan (dry-run) — no changes applied",
                "복구 계획(dry-run) — 실제 변경 없음"
            ),
            Tone::Plan,
        )
    );
    println!(
        "{}",
        field_line_toned("destination", destination, W, Tone::Value)
    );
    println!(
        "{}",
        field_line_toned("backup id", &plan.backup_id, W, Tone::Value)
    );
    println!(
        "{}",
        field_line_toned("type", format!("{:?}", plan.backup_type), W, Tone::Value)
    );
    println!(
        "{}",
        field_line_toned("source server", &plan.source_server_version, W, Tone::Value)
    );
    if let Some(v) = &plan.target_server_version {
        println!("{}", field_line_toned("target server", v, W, Tone::Value));
    }
    println!(
        "{}",
        field_line(
            "est. stored size",
            format!(
                "{} bytes",
                style(&plan.stored_size_bytes.to_string(), Tone::Value)
            ),
            W,
        )
    );
    match &plan.ns_include {
        Some(ns) => println!(
            "{}",
            field_line(
                "target ns",
                format!(
                    "{}{}",
                    style(ns, Tone::Value),
                    lang.sel(" (selective restore)", "(선택적 복구)")
                ),
                W,
            )
        ),
        None => println!(
            "{}",
            field_line_toned("target ns", lang.sel("all", "전체"), W, Tone::Value)
        ),
    }
    if plan.conflicting_namespaces.is_empty() {
        println!(
            "{}",
            field_line_toned(
                "conflicting ns",
                lang.sel("none (empty target)", "없음(빈 대상)"),
                W,
                Tone::Success,
            )
        );
    } else {
        println!(
            "{}",
            field_line(
                "conflicting ns",
                format!(
                    "{} — {}",
                    style(
                        &plan.conflicting_namespaces.len().to_string(),
                        Tone::Warning
                    ),
                    plan.conflicting_namespaces.join(", ")
                ),
                W,
            )
        );
        println!(
            "{}",
            field_line_toned(
                "note",
                lang.sel(
                    "target already has data — an actual restore needs --force or interactive confirmation",
                    "기존 데이터가 있습니다 — 실제 복구는 --force 또는 대화형 확인 필요"
                ),
                W,
                Tone::Warning,
            )
        );
    }
    if let Some(w) = &plan.version_warning {
        println!("{}", field_line_toned("warning", w, W, Tone::Warning));
    }
}

/// 대화형 확인 프롬프트(TTY). 기존 데이터 덮어쓰기 전 명시적 동의를 받는다.
fn prompt_confirm(plan: &RestorePlan, lang: Lang) -> bool {
    use std::io::Write;
    eprintln!(
        "{}",
        style_stderr(
            &format!(
                "{}{} namespaces): {}",
                lang.sel(
                    "warning: target already has data (",
                    "경고: 복원 대상에 기존 데이터가 있습니다("
                ),
                plan.conflicting_namespaces.len(),
                plan.conflicting_namespaces.join(", ")
            ),
            Tone::Warning,
        ),
    );
    eprint!(
        "{} ",
        style_stderr(
            &format!(
                "{} [y/N]",
                lang.sel(
                    "Overwrite this data and proceed with the restore?",
                    "이 데이터를 덮어쓰고 복구를 진행하시겠습니까?"
                )
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

// ── PITR(--at) 분기 (t9) ──────────────────────────────────────────────────

/// PITR 복구 핸들러 — `--at` 분기. base 풀 복원 후 증분 슬라이스를 목표 시점까지 재생한다.
/// 대상이 PostgreSQL이면 [`handle_pg_pitr`](logical decoding 재생)로 위임하고, 그 외(Mongo)는
/// oplog 재생([`run_pitr`])을 수행한다.
///
/// 절차: PITR+--only 즉시 거부(exit 2) → config·URI·storage 해석 → (PG면 위임) → [`run_pitr`] →
/// 결정 종료 ts({t,i}+wall-clock)·체인 보고 출력.
async fn handle_pitr(
    config_path: Option<PathBuf>,
    lang_flag: Option<crate::i18n::Lang>,
    args: RestoreArgs,
    at: String,
) -> Result<()> {
    // PITR + --only 병용 즉시 거부(pitfall 1-4: --oplogReplay는 ns 필터와 병용 불가, PRD Edge Case).
    crate::pipeline::pitr::reject_pitr_with_only(args.only.as_deref())?;

    // 출력 언어 해석(설명 텍스트 ko/en) — config_path의 [output].language를 본다.
    let config_toml = match &config_path {
        Some(path) => std::fs::read_to_string(path).ok(),
        None => None,
    };
    let lang = crate::i18n::resolve_from_toml(lang_flag, config_toml.as_deref());

    // config·URI·storage·타임아웃·대상 출처 해석(풀 복구 경로와 동일 규칙).
    let (target_uri, storage, timeout_secs, target_origin) =
        resolve_target_and_storage(&config_path, &args)?;

    // PostgreSQL 대상은 logical decoding 기반 PITR로 분기한다(base 풀 복원 + 증분 DML 재생).
    if crate::engine::DbKind::from_uri(target_uri.expose()) == crate::engine::DbKind::Postgres {
        return handle_pg_pitr(
            target_uri,
            storage,
            timeout_secs,
            target_origin,
            args,
            at,
            lang,
        )
        .await;
    }
    // MySQL 대상은 binlog 기반 PITR로 분기한다(base 풀 복원 + 증분 ROW 재생).
    if crate::engine::DbKind::from_uri(target_uri.expose()) == crate::engine::DbKind::Mysql {
        return handle_mysql_pitr(
            target_uri,
            storage,
            timeout_secs,
            target_origin,
            args,
            at,
            lang,
        )
        .await;
    }

    // 여기까지 왔으면 Mongo PITR. 실행 컨텍스트 + 복구 대상(host) 표시.
    let mode = crate::cli::output::context_mode(args.json);
    crate::cli::output::print_run_context(&args.profile, Some(crate::engine::DbKind::Mongo), mode);
    crate::cli::output::print_restore_target(&target_uri, &target_origin, mode, lang);
    let destination = crate::cli::output::redact_uri(target_uri.expose());

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
    let outcome = run_pitr(&request, storage.as_ref(), is_tty, |plan| {
        prompt_confirm(plan, lang)
    })
    .await?;

    if request.dry_run {
        print_pitr_plan(&outcome.plan, args.json, &destination, lang);
    } else if args.json {
        let summary = serde_json::json!({
            "base_id": outcome.plan.base_id,
            "incremental_ids": outcome.plan.incremental_ids,
            "decided_ts": { "t": outcome.plan.decided_ts.t, "i": outcome.plan.decided_ts.i },
            "decided_wall_clock": outcome.plan.decided_wall_clock,
            "replayed_slices": outcome.replayed_slices,
            "destination": destination,
            "restored": true,
        });
        println!("{summary}");
    } else if !args.quiet {
        const W: usize = 20;
        println!(
            "{}",
            style(
                lang.sel("PITR restore complete", "PITR 복구 완료"),
                Tone::Success
            )
        );
        println!(
            "{}",
            field_line_toned("destination", &destination, W, Tone::Value)
        );
        println!(
            "{}",
            field_line_toned("base id", &outcome.plan.base_id, W, Tone::Value)
        );
        println!(
            "{}",
            field_line_toned(
                "replayed slices",
                outcome.replayed_slices.to_string(),
                W,
                Tone::Value,
            )
        );
        println!(
            "{}",
            field_line_toned(
                "decided end ts",
                format!(
                    "{{t:{}, i:{}}}  ({})",
                    outcome.plan.decided_ts.t,
                    outcome.plan.decided_ts.i,
                    outcome.plan.decided_wall_clock
                ),
                W,
                Tone::Value,
            )
        );
    }

    Ok(())
}

/// PostgreSQL 시점 복구 핸들러 — base 풀 복원 + 증분 슬라이스 재생(`--at <RFC3339>|latest`).
///
/// `--id`로 base 풀백업을 고정할 수 있다(미지정 시 최신 PG 풀백업). `--only`(선택적 복구)는
/// PG PITR에서 미지원이라 거부한다.
async fn handle_pg_pitr(
    target_uri: Secret,
    storage: Box<dyn Storage>,
    timeout_secs: Option<u64>,
    target_origin: crate::cli::output::RestoreTargetOrigin,
    args: RestoreArgs,
    at: String,
    lang: Lang,
) -> Result<()> {
    if args.only.is_some() {
        return Err(XBackupError::Usage(
            "PG 시점 복구(--at)는 --only(선택적 복구)와 함께 쓸 수 없습니다".into(),
        ));
    }

    let mode = crate::cli::output::context_mode(args.json);
    crate::cli::output::print_run_context(
        &args.profile,
        Some(crate::engine::DbKind::Postgres),
        mode,
    );
    crate::cli::output::print_restore_target(&target_uri, &target_origin, mode, lang);
    let destination = crate::cli::output::redact_uri(target_uri.expose());

    let request = PgPitrRequest {
        target_uri,
        at,
        base_id: args.id.clone(),
        force: args.force,
        dry_run: args.dry_run,
        timeout_secs,
    };

    let is_tty = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let outcome = run_pg_pitr(&request, storage.as_ref(), is_tty, |conflicts| {
        prompt_confirm_pg(conflicts, lang)
    })
    .await?;

    if request.dry_run {
        print_pg_pitr_plan(&outcome.plan, args.json, &destination, lang);
    } else if args.json {
        let summary = serde_json::json!({
            "base_id": outcome.plan.base_id,
            "incremental_ids": outcome.plan.incremental_ids,
            "target": outcome.plan.target_label,
            "destination": destination,
            "replayed_slices": outcome.replayed_slices,
            "applied_changes": outcome.applied_changes,
            "database": "postgresql",
            "restored": true,
        });
        println!("{summary}");
    } else if !args.quiet {
        const W: usize = 20;
        println!(
            "{}",
            style(
                lang.sel(
                    "PITR restore complete (PostgreSQL)",
                    "PITR 복구 완료(PostgreSQL)"
                ),
                Tone::Success,
            )
        );
        println!(
            "{}",
            field_line_toned("destination", &destination, W, Tone::Value)
        );
        println!(
            "{}",
            field_line_toned("base id", &outcome.plan.base_id, W, Tone::Value)
        );
        println!(
            "{}",
            field_line_toned(
                "replayed slices",
                outcome.replayed_slices.to_string(),
                W,
                Tone::Value,
            )
        );
        println!(
            "{}",
            field_line_toned(
                "applied changes",
                outcome.applied_changes.to_string(),
                W,
                Tone::Value,
            )
        );
        println!(
            "{}",
            field_line_toned("target", &outcome.plan.target_label, W, Tone::Value)
        );
    }

    Ok(())
}

/// MySQL 시점 복구 핸들러 — base 풀 복원 + 증분 ROW 슬라이스 재생(`--at <RFC3339>|latest`).
async fn handle_mysql_pitr(
    target_uri: Secret,
    storage: Box<dyn Storage>,
    timeout_secs: Option<u64>,
    target_origin: crate::cli::output::RestoreTargetOrigin,
    args: RestoreArgs,
    at: String,
    lang: Lang,
) -> Result<()> {
    if args.only.is_some() {
        return Err(XBackupError::Usage(
            "MySQL 시점 복구(--at)는 --only(선택적 복구)와 함께 쓸 수 없습니다".into(),
        ));
    }

    let mode = crate::cli::output::context_mode(args.json);
    crate::cli::output::print_run_context(&args.profile, Some(crate::engine::DbKind::Mysql), mode);
    crate::cli::output::print_restore_target(&target_uri, &target_origin, mode, lang);
    let destination = crate::cli::output::redact_uri(target_uri.expose());

    let request = MysqlPitrRequest {
        target_uri,
        at,
        base_id: args.id.clone(),
        force: args.force,
        dry_run: args.dry_run,
        timeout_secs,
    };

    let is_tty = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let outcome = run_mysql_pitr(&request, storage.as_ref(), is_tty, |conflicts| {
        prompt_confirm_pg(conflicts, lang)
    })
    .await?;

    if request.dry_run {
        print_mysql_pitr_plan(&outcome.plan, args.json, &destination, lang);
    } else if args.json {
        let summary = serde_json::json!({
            "base_id": outcome.plan.base_id,
            "incremental_ids": outcome.plan.incremental_ids,
            "target": outcome.plan.target_label,
            "destination": destination,
            "replayed_slices": outcome.replayed_slices,
            "applied_changes": outcome.applied_changes,
            "database": "mysql",
            "restored": true,
        });
        println!("{summary}");
    } else if !args.quiet {
        const W: usize = 20;
        println!(
            "{}",
            style(
                lang.sel("PITR restore complete (MySQL)", "PITR 복구 완료(MySQL)"),
                Tone::Success,
            )
        );
        println!(
            "{}",
            field_line_toned("destination", &destination, W, Tone::Value)
        );
        println!(
            "{}",
            field_line_toned("base id", &outcome.plan.base_id, W, Tone::Value)
        );
        println!(
            "{}",
            field_line_toned(
                "replayed slices",
                outcome.replayed_slices.to_string(),
                W,
                Tone::Value,
            )
        );
        println!(
            "{}",
            field_line_toned(
                "applied changes",
                outcome.applied_changes.to_string(),
                W,
                Tone::Value,
            )
        );
        println!(
            "{}",
            field_line_toned("target", &outcome.plan.target_label, W, Tone::Value)
        );
    }

    Ok(())
}

/// MySQL PITR dry-run 계획을 출력한다(PG와 동일 형식).
fn print_mysql_pitr_plan(plan: &MysqlPitrPlan, json: bool, destination: &str, lang: Lang) {
    if json {
        let summary = serde_json::json!({
            "dry_run": true,
            "base_id": plan.base_id,
            "destination": destination,
            "incremental_ids": plan.incremental_ids,
            "target": plan.target_label,
            "conflicting_tables": plan.conflicting_tables,
            "database": "mysql",
        });
        println!("{summary}");
        return;
    }
    const W: usize = 22;
    println!(
        "{}",
        style(
            lang.sel(
                "PITR restore plan (dry-run, MySQL) — no changes applied",
                "PITR 복구 계획(dry-run, MySQL) — 실제 변경 없음"
            ),
            Tone::Plan,
        )
    );
    println!(
        "{}",
        field_line_toned("destination", destination, W, Tone::Value)
    );
    println!(
        "{}",
        field_line_toned("base id", &plan.base_id, W, Tone::Value)
    );
    if plan.incremental_ids.is_empty() {
        println!(
            "{}",
            field_line_toned(
                "incremental chain",
                lang.sel("none (base only)", "없음(base만 복원)"),
                W,
                Tone::Muted,
            )
        );
    } else {
        println!(
            "{}",
            field_line(
                "incremental chain",
                format!(
                    "{} — {}",
                    style(&plan.incremental_ids.len().to_string(), Tone::Value),
                    plan.incremental_ids.join(", ")
                ),
                W,
            )
        );
    }
    println!(
        "{}",
        field_line_toned("target", &plan.target_label, W, Tone::Value)
    );
    if plan.conflicting_tables.is_empty() {
        println!(
            "{}",
            field_line_toned(
                "conflicting tables",
                lang.sel("none (empty target)", "없음(빈 대상)"),
                W,
                Tone::Success,
            )
        );
    } else {
        println!(
            "{}",
            field_line(
                "conflicting tables",
                format!(
                    "{} — {}",
                    style(&plan.conflicting_tables.len().to_string(), Tone::Warning),
                    preview_list(&plan.conflicting_tables)
                ),
                W,
            )
        );
    }
}

/// 목록을 최대 5개까지 미리보기로 잘라 표시한다(나머지는 "+N more").
fn preview_list(list: &[String]) -> String {
    const MAX: usize = 5;
    if list.len() <= MAX {
        list.join(", ")
    } else {
        format!("{}, +{} more", list[..MAX].join(", "), list.len() - MAX)
    }
}

/// PG PITR 대화형 확인 — 충돌 테이블 목록을 보이고 덮어쓰기 동의를 받는다.
fn prompt_confirm_pg(conflicts: &[String], lang: Lang) -> bool {
    use std::io::Write;
    eprintln!(
        "{}",
        style_stderr(
            &format!(
                "{}{} tables): {}",
                lang.sel(
                    "warning: target already has data (",
                    "경고: 복원 대상에 기존 데이터가 있습니다("
                ),
                conflicts.len(),
                preview_list(conflicts)
            ),
            Tone::Warning,
        ),
    );
    eprint!(
        "{} ",
        style_stderr(
            &format!(
                "{} [y/N]",
                lang.sel(
                    "Overwrite this data and proceed with the PITR restore?",
                    "이 데이터를 덮어쓰고 PITR 복구를 진행하시겠습니까?"
                )
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

/// PG PITR dry-run 계획 출력. 시크릿 미출력.
/// `destination`은 [`redact_uri`](crate::cli::output::redact_uri)로 가린 복구 대상 host다.
fn print_pg_pitr_plan(plan: &PgPitrPlan, json: bool, destination: &str, lang: Lang) {
    if json {
        let summary = serde_json::json!({
            "dry_run": true,
            "base_id": plan.base_id,
            "destination": destination,
            "incremental_ids": plan.incremental_ids,
            "target": plan.target_label,
            "conflicting_tables": plan.conflicting_tables,
            "database": "postgresql",
        });
        println!("{summary}");
        return;
    }
    const W: usize = 22;
    println!(
        "{}",
        style(
            lang.sel(
                "PITR restore plan (dry-run, PostgreSQL) — no changes applied",
                "PITR 복구 계획(dry-run, PostgreSQL) — 실제 변경 없음"
            ),
            Tone::Plan,
        )
    );
    println!(
        "{}",
        field_line_toned("destination", destination, W, Tone::Value)
    );
    println!(
        "{}",
        field_line_toned("base id", &plan.base_id, W, Tone::Value)
    );
    if plan.incremental_ids.is_empty() {
        println!(
            "{}",
            field_line_toned(
                "incremental chain",
                lang.sel("none (base only)", "없음(base만 복원)"),
                W,
                Tone::Muted,
            )
        );
    } else {
        println!(
            "{}",
            field_line(
                "incremental chain",
                format!(
                    "{} — {}",
                    style(&plan.incremental_ids.len().to_string(), Tone::Value),
                    plan.incremental_ids.join(", ")
                ),
                W,
            )
        );
    }
    println!(
        "{}",
        field_line_toned("target", &plan.target_label, W, Tone::Value)
    );
    if plan.conflicting_tables.is_empty() {
        println!(
            "{}",
            field_line_toned(
                "conflicting tables",
                lang.sel("none (empty target)", "없음(빈 대상)"),
                W,
                Tone::Success,
            )
        );
    } else {
        println!(
            "{}",
            field_line(
                "conflicting tables",
                format!(
                    "{} — {}",
                    style(&plan.conflicting_tables.len().to_string(), Tone::Warning),
                    preview_list(&plan.conflicting_tables)
                ),
                W,
            )
        );
        println!(
            "{}",
            field_line_toned(
                "note",
                lang.sel(
                    "target already has data — an actual restore needs --force or interactive confirmation",
                    "기존 데이터가 있습니다 — 실제 복구는 --force 또는 대화형 확인 필요"
                ),
                W,
                Tone::Warning,
            )
        );
    }
}

/// 복구 대상 URI와 그 출처를 정한다 — 우선순위: `--target`(URI 직접) > `--target-profile`(같은
/// config의 다른 프로파일 source) > 프로파일 자신 source(in-place 기본). clap이 두 플래그를 택일로
/// 막으므로 (Some, Some)에는 도달하지 않지만, URI 우선으로 안전하게 처리한다. `--target-profile`은
/// migrate와 동일 규칙 — 대상 프로파일을 별도 해석해 그 source 접속을 복구 대상으로 쓴다.
fn resolve_restore_target(
    args: &RestoreArgs,
    config_toml: Option<&str>,
    overrides: &[(String, String)],
    resolved: &ResolvedConfig,
) -> Result<(Secret, crate::cli::output::RestoreTargetOrigin)> {
    use crate::cli::output::RestoreTargetOrigin;
    match (&args.target, &args.target_profile) {
        (Some(uri), _) => Ok((Secret::new(uri.clone()), RestoreTargetOrigin::Flag)),
        (None, Some(tp)) => {
            let tcfg = ResolvedConfig::build(MergeInput {
                config_toml,
                profile_name: tp,
                overrides,
            })?;
            let uri = tcfg.resolved_uri.ok_or_else(|| {
                XBackupError::Config(format!(
                    "target 프로파일 '{tp}'에 source.uri/uri_env가 없습니다"
                ))
            })?;
            Ok((uri, RestoreTargetOrigin::Profile(tp.clone())))
        }
        (None, None) => {
            let uri = resolved.resolved_uri.clone().ok_or_else(|| {
                XBackupError::Config(format!(
                    "프로파일 '{}'에 source.uri/uri_env가 없고 --target/--target-profile도 \
                     지정되지 않았습니다",
                    resolved.profile_name
                ))
            })?;
            Ok((uri, RestoreTargetOrigin::InPlace))
        }
    }
}

/// config 로드 → 복구 대상 URI·출처·local storage를 해석한다(풀/ PITR 공통 setup).
///
/// 풀 복구 경로의 1~3단계와 동일 규칙: [`resolve_restore_target`]로 대상을 정하고
/// destination을 [`select_restore_storage`]로 고른다. 시크릿은 [`Secret`]로 감싼다.
#[allow(clippy::type_complexity)]
fn resolve_target_and_storage(
    config_path: &Option<PathBuf>,
    args: &RestoreArgs,
) -> Result<(
    Secret,
    Box<dyn Storage>,
    Option<u64>,
    crate::cli::output::RestoreTargetOrigin,
)> {
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

    let (target_uri, origin) =
        resolve_restore_target(args, config_toml.as_deref(), &overrides, &resolved)?;

    let storage = select_restore_storage(
        &resolved.profile,
        &resolved.profile_name,
        args.from.as_deref(),
    )?;
    Ok((
        target_uri,
        storage,
        resolved.profile.source.connect_timeout_secs,
        origin,
    ))
}

/// 복구에 쓸 destination 백엔드를 고른다 — 멀티 destination 중 `--from`(이름 또는
/// `type#idx`)으로 특정 복제본을, 미지정 시 primary(첫 destination)를 [`from_config`]로
/// 구성한다. 모든 복제본은 동일 바이트이므로 어디서 읽어도 결과가 같다.
///
/// dest(store)가 없는 **endpoint 전용** 프로파일(복구/이관 *대상*용)을 `--profile`로 넘기면,
/// 범용 "destination.type이 필요합니다" 대신 의도를 짚어주는 에러를 낸다 — 흔한 혼동이
/// "이 서버로 복구"인데, 그건 `--profile <백업프로파일> --target-profile <이 프로파일>`이다.
fn select_restore_storage(
    profile: &Profile,
    profile_name: &str,
    from: Option<&str>,
) -> Result<Box<dyn Storage>> {
    let dest = choose_destination(profile, from)?;
    if dest.r#type.is_none() {
        return Err(XBackupError::Usage(format!(
            "프로파일 '{profile_name}'은(는) 백업 store(dest)가 없어 복구 소스가 될 수 없습니다\
             (endpoint 전용 — 복구/이관 대상용). 이 서버로 복구하려면 백업을 가진 프로파일을 \
             소스로, 이 프로파일을 대상으로 지정하세요:\n  \
             x-backup restore --profile <백업프로파일> --target-profile {profile_name}"
        )));
    }
    from_config(dest)
}

/// `--from`(이름/라벨)으로 복구에 쓸 destination을 고른다 — 미지정이면 첫(primary) destination.
/// 스토리지 생성([`select_restore_storage`])과 표시용 라벨([`store_location_label`])이 **같은**
/// 대상을 가리키도록 선택 로직을 한 곳에 둔다.
fn choose_destination<'a>(
    profile: &'a Profile,
    from: Option<&str>,
) -> Result<&'a DestinationConfig> {
    let dests = profile.effective_destinations();
    match from {
        None => Ok(dests[0]),
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
            }),
    }
}

/// destination의 사람용 위치 라벨(local 경로 / s3 버킷) — `list`의 `store:` 표기와 일관.
fn store_location_label(dest: &DestinationConfig) -> String {
    match dest.r#type.as_deref() {
        Some("local") => dest
            .path
            .clone()
            .unwrap_or_else(|| "local(경로 미설정)".to_string()),
        Some("s3") => dest
            .s3
            .as_ref()
            .and_then(|s| s.bucket.clone())
            .map(|b| format!("s3://{b}"))
            .unwrap_or_else(|| "s3".to_string()),
        _ => "(미설정)".to_string(),
    }
}

/// PITR dry-run 계획을 출력한다(base·증분 체인·결정 종료 ts·예상 크기 — 무변경). 시크릿 미출력.
/// `destination`은 [`redact_uri`](crate::cli::output::redact_uri)로 가린 복구 대상 host다.
fn print_pitr_plan(plan: &PitrPlan, json: bool, destination: &str, lang: Lang) {
    if json {
        let summary = serde_json::json!({
            "dry_run": true,
            "base_id": plan.base_id,
            "destination": destination,
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

    const W: usize = 21;
    println!(
        "{}",
        style(
            lang.sel(
                "PITR restore plan (dry-run) — no changes applied",
                "PITR 복구 계획(dry-run) — 실제 변경 없음"
            ),
            Tone::Plan,
        )
    );
    println!(
        "{}",
        field_line_toned("destination", destination, W, Tone::Value)
    );
    println!(
        "{}",
        field_line_toned("base id", &plan.base_id, W, Tone::Value)
    );
    if plan.incremental_ids.is_empty() {
        println!(
            "{}",
            field_line_toned(
                "incremental chain",
                lang.sel(
                    "none (base reaches the target time)",
                    "없음(base만으로 목표 시점 도달)"
                ),
                W,
                Tone::Muted,
            )
        );
    } else {
        println!(
            "{}",
            field_line(
                "incremental chain",
                format!(
                    "{} — {}",
                    style(&plan.incremental_ids.len().to_string(), Tone::Value),
                    plan.incremental_ids.join(", ")
                ),
                W,
            )
        );
    }
    if let Some(limit) = &plan.limit_slice_id {
        println!(
            "{}",
            field_line(
                "limit slice",
                format!(
                    "{}{}",
                    style(limit, Tone::Value),
                    lang.sel(
                        " (--oplogLimit applied to this slice)",
                        "(이 슬라이스에 --oplogLimit 적용)"
                    )
                ),
                W,
            )
        );
    } else {
        println!(
            "{}",
            field_line_toned(
                "limit slice",
                lang.sel("none (replay the whole chain)", "없음(체인 전체 재생)"),
                W,
                Tone::Muted,
            )
        );
    }
    println!(
        "{}",
        field_line_toned(
            "decided end ts",
            format!(
                "{{t:{}, i:{}}}  ({})",
                plan.decided_ts.t, plan.decided_ts.i, plan.decided_wall_clock
            ),
            W,
            Tone::Value,
        )
    );
    println!(
        "{}",
        field_line(
            "est. stored size",
            format!(
                "{} bytes",
                style(&plan.estimated_bytes.to_string(), Tone::Value)
            ),
            W,
        )
    );
}
