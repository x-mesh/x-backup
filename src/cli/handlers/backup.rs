//! `backup` 서브커맨드 핸들러 — config 로드 → 파이프라인 실행 → 요약 출력.
//!
//! 스코프(t4): destination type=local 풀 백업만. S3는 t7, 증분은 t8,
//! 사전 점검(status 선행)은 t11이 채운다(아래 TODO 주석).

use std::path::PathBuf;

use crate::cli::args::BackupArgs;
use crate::config::env::collect_overrides_from_process;
use crate::config::merged::MergeInput;
use crate::config::ResolvedConfig;
use crate::error::{Result, XBackupError};
use crate::pipeline::backup::{run_full_backup, BackupRequest};
use crate::pipeline::stage::StageStack;
use crate::storage::LocalFs;

/// `backup` 핸들러 진입점.
pub async fn handle(config_path: Option<PathBuf>, args: BackupArgs) -> Result<()> {
    // 1) config 로드 + 레이어 병합(file + ENV; CLI는 아래에서 직접 반영).
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

    // 2) URI 시크릿(uri_env로 해석된 값) 확보.
    let uri = resolved.resolved_uri.ok_or_else(|| {
        XBackupError::Config(format!(
            "프로파일 '{}'에 source.uri_env가 없거나 해석되지 않았습니다",
            resolved.profile_name
        ))
    })?;

    // 3) destination 검증 — t4는 local만 동작(S3는 t7).
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
    // object_store LocalFileSystem은 루트가 미리 존재해야 한다 — 없으면 생성.
    std::fs::create_dir_all(root).map_err(|e| {
        XBackupError::Config(format!("destination 디렉터리 준비 실패({root}): {e}"))
    })?;
    let storage = LocalFs::new(root)?;

    // TODO(t11): backup 실행 전 status 핵심 점검(연결·권한·토폴로지·oplog 윈도우)을
    //   자동 선행하고, --skip-precheck로 우회한다(PRD §9). 현재는 미구현.
    if !args.skip_precheck {
        tracing::debug!("사전 점검(status 선행)은 t11에서 구현 예정 — 현재 건너뜀");
    }

    // 증분은 t8 소유. t4는 풀 백업만.
    if matches!(args.backup_type, Some(crate::cli::args::BackupType::Incr)) {
        return Err(XBackupError::Usage(
            "증분 백업(--type incr)은 아직 미지원입니다(t8) — 풀 백업만 동작".into(),
        ));
    }

    // 4) 파이프라인 실행. t4는 평문(identity StageStack). t6이 compress/encrypt 단계를
    //    여기서 StageStack에 push한다(--no-encrypt/--compress-level 반영 포함).
    let request = BackupRequest {
        uri,
        mongodump_program: "mongodump".to_string(),
        db: args.db.clone(),
        collection: args.collection.clone(),
    };
    let stages = StageStack::new();
    let outcome = run_full_backup(&request, &storage, stages).await?;

    // 5) 요약 출력(stdout — 결과 전용). --json은 t15/t16이 정식화; 여기서는 최소 JSON.
    if args.json {
        let summary = serde_json::json!({
            "backup_id": outcome.backup_id,
            "stored_size_bytes": outcome.stored_size_bytes,
            "checksum_sha256": outcome.checksum_sha256,
            "topology": format!("{:?}", outcome.topology),
        });
        println!("{summary}");
    } else if !args.quiet {
        println!("백업 완료");
        println!("  id:       {}", outcome.backup_id);
        println!("  크기:     {} bytes", outcome.stored_size_bytes);
        println!("  체크섬:   sha256:{}", outcome.checksum_sha256);
        if let Some(range) = &outcome.oplog_range {
            println!(
                "  oplog:    {{t:{},i:{}}} → {{t:{},i:{}}}",
                range.start_ts.t, range.start_ts.i, range.end_ts.t, range.end_ts.i
            );
        }
    }

    Ok(())
}
