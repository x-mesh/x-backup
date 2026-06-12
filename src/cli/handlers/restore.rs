//! `restore` 서브커맨드 핸들러 — config 로드 → 복구 파이프라인 실행 → 요약 출력.
//!
//! 스코프(t5): destination type=local 풀 복구. `--target`(분리 복구)·`--id`/최신 자동
//! 선택·`--only`(선택적)·`--force`/대화형 가드·`--dry-run`·`--skip-precheck`를 지원한다.
//! PITR(`--at`)은 t9, S3 destination은 t7 소유다(아래 명시적 거부).

use std::io::IsTerminal;
use std::path::PathBuf;

use crate::cli::args::RestoreArgs;
use crate::config::env::collect_overrides_from_process;
use crate::config::merged::MergeInput;
use crate::config::secret::Secret;
use crate::config::ResolvedConfig;
use crate::error::{Result, XBackupError};
use crate::pipeline::restore::{run_restore, RestorePlan, RestoreRequest};
use crate::storage::LocalFs;

/// `restore` 핸들러 진입점.
pub async fn handle(config_path: Option<PathBuf>, args: RestoreArgs) -> Result<()> {
    // 동시 실행 잠금(FR-12) — 같은 프로파일의 backup/restore/prune과 직렬화한다.
    // 가드(_lock)를 함수 끝까지 유지해 작업 동안 lock을 잡는다(충돌 시 exit 5).
    let _lock = crate::lock::acquire(&args.profile)?;

    // PITR(--at)은 t9 소유 — 풀 복구(t5)는 oplog replay를 수행하지 않는다.
    if args.at.is_some() {
        return Err(XBackupError::Usage(
            "PITR 복구(--at)는 아직 미지원입니다(t9) — 풀 복구만 동작".into(),
        ));
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

    // 3) destination 검증 — t5는 local만(S3는 t7). 백업이 저장된 위치에서 읽는다.
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
    let storage = LocalFs::new(root)?;

    // 4) 복구 요청 조립.
    let request = RestoreRequest {
        target_uri,
        mongorestore_program: "mongorestore".to_string(),
        backup_id: args.id.clone(),
        only: args.only.clone(),
        force: args.force,
        dry_run: args.dry_run,
        skip_precheck: args.skip_precheck,
        // 진행 카운터 미사용(R16 진행 표시는 t13/t9 restore 경로에서 배선) — 빌드 정합용 None.
        progress_counter: None,
    };

    // TTY 여부 — 대화형 확인 가능 여부. stdout 대신 stdin TTY로 본다(확인 입력을 받음).
    let is_tty = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();

    let outcome = run_restore(&request, &storage, is_tty, prompt_confirm).await?;

    // 5) 출력. dry-run은 계획을, 실제 복구는 완료 요약을 낸다.
    if request.dry_run {
        print_plan(&outcome.plan, args.json);
    } else if args.json {
        let summary = serde_json::json!({
            "backup_id": outcome.backup_id,
            "stored_size_bytes": outcome.stored_size_bytes,
            "restored": true,
        });
        println!("{summary}");
    } else if !args.quiet {
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
        println!("  주의:          기존 데이터가 있습니다 — 실제 복구는 --force 또는 대화형 확인 필요");
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
