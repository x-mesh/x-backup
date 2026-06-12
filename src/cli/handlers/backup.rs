//! `backup` 서브커맨드 핸들러 — config 로드 → 파이프라인 실행 → 요약 출력.
//!
//! 스코프(t4): destination type=local 풀 백업만. S3는 t7, 증분은 t8,
//! 사전 점검(status 선행)은 t11이 채운다(아래 TODO 주석).

use std::path::PathBuf;

use crate::cli::args::BackupArgs;
use crate::cli::output::{OutputFlags, OutputMode};
use crate::cli::progress::{new_counter, ProgressKind, ProgressReporter};
use crate::compress::{ZstdCompressStage, ALGORITHM_ZSTD};
use crate::config::env::collect_overrides_from_process;
use crate::config::merged::MergeInput;
use crate::config::ResolvedConfig;
use crate::config::secret::Secret;
use crate::crypto::build_encrypt_stage;
use crate::engine::mongo::status::{CheckStatus, StatusChecker};
use crate::error::{Result, XBackupError};
use crate::manifest::schema::CompressionMeta;
use crate::pipeline::backup::{run_full_backup_with_meta, BackupMeta, BackupRequest};
use crate::pipeline::incremental::{
    run_incremental_backup, IncrementalOutcome, IncrementalRequest,
};
use crate::pipeline::stage::{StageStack, ENV_AES_KEY_HEX};
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

    // 2) URI 시크릿(uri_env로 해석된 값) 확보. 이후 build_stages가 &resolved를 쓰므로
    //    clone으로 꺼내 부분 이동을 피한다(Secret은 Clone).
    let uri = resolved.resolved_uri.clone().ok_or_else(|| {
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

    // 출력 모드 결정(R15) — CLI(--json>--quiet>--progress) > config(mode.output) > TTY 자동.
    // 진행 표시·요약 출력 분기에 일관 사용한다.
    let mode = OutputMode::resolve_from_env(
        OutputFlags {
            json: args.json,
            quiet: args.quiet,
            progress: args.progress,
        },
        Some(resolved.profile.mode.output.as_str()),
    );

    // backup 실행 전 status 핵심 점검(연결·권한·토폴로지·도구 존재)을 자동 선행한다(FR-8,
    //   PRD §9). 전체 status보다 가벼운 서브셋([`StatusChecker::precheck_subset`])으로, 백업을
    //   *막는* 결함(Fail)만 본다. 하나라도 Fail이면 PrecheckFailed(exit 3)로 백업을 미시작한다.
    //   --skip-precheck면 우회한다(읽기 전용·무부작용).
    if !args.skip_precheck {
        run_precheck(&uri, &resolved.profile_name).await?;
    } else {
        tracing::warn!("--skip-precheck 지정 — 백업 사전 점검을 건너뜁니다(FR-8 우회)");
    }

    // 증분(--type incr)은 드라이버 oplog 캡처 경로로 분기한다(t8). 선택적 백업
    //   (--db/--collection)과는 병용 불가(증분은 항상 전체 oplog 슬라이스).
    if matches!(args.backup_type, Some(crate::cli::args::BackupType::Incr)) {
        if args.db.is_some() || args.collection.is_some() {
            return Err(XBackupError::Usage(
                "증분 백업(--type incr)은 선택적 백업(--db/--collection)과 병용할 수 없습니다 \
                 — 증분은 전체 oplog 슬라이스를 캡처합니다(FR-1/FR-2)."
                    .into(),
            ));
        }
        return handle_incremental(&resolved, &args, &uri, &storage, mode).await;
    }

    // 4) 파이프라인 단계 구성(t6): compress → encrypt 고정 순서(PRD §8.4). 단계와
    //    manifest 메타를 함께 만든다(아래 build_stages 참조). --no-encrypt면 암호화 생략.
    //    진행 표시(R16): 공유 카운터를 만들어 BackupRequest에 주입하고, 백업은 dump 총량을
    //    사전에 모르므로 부정형(spinner)로 처리 바이트·속도를 stderr에 표시한다(PRD §FR-9).
    let progress_counter = new_counter();
    let request = BackupRequest {
        uri,
        mongodump_program: "mongodump".to_string(),
        db: args.db.clone(),
        collection: args.collection.clone(),
        progress_counter: Some(std::sync::Arc::clone(&progress_counter)),
    };
    let (stages, meta) = build_stages(&resolved, &args)?;

    let reporter = ProgressReporter::start(
        mode,
        ProgressKind::Indeterminate {
            label: "백업".into(),
        },
        progress_counter,
    );
    let result = run_full_backup_with_meta(&request, &storage, stages, meta).await;
    reporter.finish().await;
    let outcome = result?;

    // 5) 요약 출력(stdout — 결과 전용). 진행은 stderr, 결과/--json은 stdout으로 분리.
    if mode.emits_json() {
        let summary = serde_json::json!({
            "backup_id": outcome.backup_id,
            "stored_size_bytes": outcome.stored_size_bytes,
            "checksum_sha256": outcome.checksum_sha256,
            "topology": format!("{:?}", outcome.topology),
        });
        println!("{summary}");
    } else if mode.shows_human_summary() {
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

/// 증분 백업 분기(`--type incr`) — 드라이버 oplog 캡처 → 저장 → manifest, gap 시 풀 승격.
///
/// 파이프라인 단계(compress→encrypt)는 풀 백업과 **동일하게** [`build_stages`]로 만든다.
/// 단, 증분은 캡처 스택 소비 후에도 (late gap 등) 풀 승격을 위해 스택을 새로 만들 수
/// 있어야 하므로 [`build_stages`]를 팩토리 클로저로 넘긴다([`run_incremental_backup`]).
///
/// gap·late gap·oplog-empty로 풀 백업으로 승격되면 **exit 4**(경고 동반 성공,
/// [`XBackupError::Warning`])로 보고한다(SC2). 정상 증분(빈 슬라이스 포함)은 exit 0.
async fn handle_incremental(
    resolved: &ResolvedConfig,
    args: &BackupArgs,
    uri: &Secret,
    storage: &LocalFs,
    mode: OutputMode,
) -> Result<()> {
    let request = IncrementalRequest {
        uri: uri.clone(),
        mongodump_program: "mongodump".to_string(),
    };
    // 캡처/승격 양쪽에서 동일 구성의 새 StageStack을 만들 수 있도록 팩토리로 넘긴다.
    let stage_factory = || build_stages(resolved, args);

    let outcome = run_incremental_backup(&request, storage, stage_factory).await?;

    match outcome {
        IncrementalOutcome::Captured {
            backup_id,
            base_id,
            oplog_count,
            oplog_range,
            stored_size_bytes,
        } => {
            if mode.emits_json() {
                let summary = serde_json::json!({
                    "backup_type": "incremental",
                    "backup_id": backup_id,
                    "base_id": base_id,
                    "oplog_count": oplog_count,
                    "stored_size_bytes": stored_size_bytes,
                });
                println!("{summary}");
            } else if mode.shows_human_summary() {
                println!("증분 백업 완료");
                println!("  id:       {backup_id}");
                println!("  base:     {base_id}");
                println!("  엔트리:   {oplog_count}건");
                println!("  크기:     {stored_size_bytes} bytes");
                println!(
                    "  oplog:    {{t:{},i:{}}} → {{t:{},i:{}}}",
                    oplog_range.start_ts.t,
                    oplog_range.start_ts.i,
                    oplog_range.end_ts.t,
                    oplog_range.end_ts.i
                );
                if oplog_count == 0 {
                    println!("  (변경 없음 — 빈 슬라이스: manifest만 기록)");
                }
            }
            Ok(())
        }
        IncrementalOutcome::PromotedToFull { outcome, reason } => {
            // 승격은 데이터상 성공이지만 "증분이 아니라 풀이 됨"을 경고로 알린다(exit 4, SC2).
            if mode.emits_json() {
                let summary = serde_json::json!({
                    "backup_type": "full",
                    "promoted_from_gap": true,
                    "backup_id": outcome.backup_id,
                    "stored_size_bytes": outcome.stored_size_bytes,
                    "checksum_sha256": outcome.checksum_sha256,
                    "reason": reason,
                });
                println!("{summary}");
            } else if mode.shows_human_summary() {
                println!("증분 → 풀 백업 승격(gap 감지)");
                println!("  id:       {}", outcome.backup_id);
                println!("  크기:     {} bytes", outcome.stored_size_bytes);
                println!("  사유:     {reason}");
            }
            // exit 4(경고 동반 성공) — main이 Warning을 exit 4로 매핑한다.
            Err(XBackupError::Warning(format!(
                "증분이 gap으로 풀 백업({})으로 승격되었습니다: {reason}",
                outcome.backup_id
            )))
        }
    }
}

/// backup 자동 사전 점검 — status 핵심 서브셋(연결·권한·토폴로지·도구 존재)을 실행한다.
///
/// 전체 `status`보다 가벼운 [`StatusChecker::precheck_subset`]로 백업을 *막는* 결함만 본다.
/// 보고서 신호등이 `Fail`이면 [`XBackupError::PrecheckFailed`](exit 3)로 백업을 미시작한다.
/// `Warn`은 백업을 막지 않는다(로그만; 전체 status가 경고를 상세히 다룬다). 읽기 전용이다.
async fn run_precheck(uri: &Secret, profile: &str) -> Result<()> {
    let checker = StatusChecker::connect(uri).await.map_err(|e| {
        // connect 준비 실패(URI 파싱 등)는 사전 점검 실패로 본다(백업 미시작).
        XBackupError::PrecheckFailed(format!("사전 점검 연결 준비 실패: {e}"))
    })?;
    let report = checker.precheck_subset(profile, "mongodump").await;

    // 점검 항목을 로그로 남긴다(진단용 — stdout 결과 오염 금지, tracing은 stderr).
    for item in &report.items {
        match item.status {
            CheckStatus::Ok => tracing::info!(check = item.key, "{}", item.message),
            CheckStatus::Warn => tracing::warn!(check = item.key, "{}", item.message),
            CheckStatus::Fail => tracing::error!(check = item.key, "{}", item.message),
        }
    }

    if report.overall == CheckStatus::Fail {
        // 실패 항목들을 모아 구체 메시지로 보고(어떤 점검이 막았는지).
        let failed: Vec<String> = report
            .items
            .iter()
            .filter(|i| i.status == CheckStatus::Fail)
            .map(|i| format!("{}: {}", i.label, i.message))
            .collect();
        return Err(XBackupError::PrecheckFailed(format!(
            "백업 사전 점검 실패(--skip-precheck로 우회 가능) — {}",
            failed.join(" / ")
        )));
    }
    Ok(())
}

/// config·CLI를 해석해 백업 파이프라인 단계 스택과 manifest 메타를 만든다(t6).
///
/// 순서(PRD §8.4 고정): **compress → encrypt**. 즉 dump 바이트에 먼저 압축, 그 다음
/// 암호화를 적용한다([`StageStack::push`]는 push 순서로 감싼다).
///
/// - **압축**: config `features.compression`가 활성(현재 zstd 단일)이면 압축 단계 push.
///   레벨은 CLI `--compress-level` > config `level` 우선. manifest.compression 기록.
/// - **암호화**: 기본 ON(PRD §FR-5 — 평문은 명시적 `--no-encrypt`만). `--no-encrypt`면
///   경고 로그 후 암호화 단계를 생략한다. config `features.encryption.algorithm`에 따라
///   age(recipient_file) 또는 aes-256-gcm(env 키)으로 단계를 만든다. manifest.encryption 기록.
fn build_stages(
    resolved: &ResolvedConfig,
    args: &BackupArgs,
) -> Result<(StageStack, BackupMeta)> {
    let features = &resolved.profile.features;
    let mut stack = StageStack::new();
    let mut meta = BackupMeta::none();

    // ── 압축(compress 먼저) ──
    // 현재 지원 알고리즘은 zstd 단일이다(config 기본값도 zstd). 레벨은 CLI 우선.
    let comp = &features.compression;
    if comp.algorithm == ALGORITHM_ZSTD {
        let level = args.compress_level.unwrap_or(comp.level);
        let stage = ZstdCompressStage::new(level);
        // manifest에는 실제 적용된(클램프 후) 레벨을 기록한다.
        meta.compression = Some(CompressionMeta {
            algorithm: ALGORITHM_ZSTD.to_string(),
            level: stage.level(),
        });
        stack.push(Box::new(stage));
    } else {
        return Err(XBackupError::Config(format!(
            "알 수 없는 압축 알고리즘: '{}'(zstd만 지원)",
            comp.algorithm
        )));
    }

    // ── 암호화(encrypt 나중) ──
    // 기본 ON. --no-encrypt면 명시적 opt-out(경고 후 생략).
    let enc = &features.encryption;
    if args.no_encrypt {
        tracing::warn!(
            "--no-encrypt 지정 — 평문으로 백업합니다(암호화 생략). 산출물에 민감 데이터가 \
             평문으로 저장됩니다(PRD §FR-5 명시적 opt-out)."
        );
    } else if !enc.enabled {
        // config에서 암호화를 끈 경우도 평문이나, 의도치 않은 평문 저장을 막기 위해 경고.
        tracing::warn!(
            "config features.encryption.enabled=false — 평문으로 백업합니다(암호화 생략)."
        );
    } else {
        // aes-256-gcm은 env에서 키를 읽어 주입한다(age는 recipient 파일에서 로드).
        let aes_key = std::env::var(ENV_AES_KEY_HEX).ok();
        let (stage, enc_meta) = build_encrypt_stage(enc, aes_key.as_deref())?;
        meta.encryption = Some(enc_meta);
        stack.push(stage);
    }

    Ok((stack, meta))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::args::BackupArgs;
    use crate::config::file::Profile;
    use crate::config::merged::ResolvedConfig;

    /// 테스트용 ResolvedConfig — 주어진 profile로 구성(URI는 비워 둠).
    fn resolved_with(profile: Profile) -> ResolvedConfig {
        ResolvedConfig {
            profile_name: "test".to_string(),
            profile,
            resolved_uri: None,
        }
    }

    /// 기본 BackupArgs(플래그 미지정).
    fn default_args() -> BackupArgs {
        BackupArgs {
            profile: "test".to_string(),
            backup_type: None,
            db: None,
            collection: None,
            no_encrypt: false,
            compress_level: None,
            quiet: false,
            progress: false,
            json: false,
            skip_precheck: false,
        }
    }

    /// 기본 경로: 압축(zstd) + 암호화(age, recipient_file 지정) 둘 다 push, compress→encrypt 순서.
    #[test]
    fn default_path_pushes_compress_then_encrypt() {
        // recipient 파일을 임시로 만든다(age 공개키).
        let id = age::x25519::Identity::generate();
        let dir = tempfile::tempdir().unwrap();
        let pub_path = dir.path().join("age.pub");
        std::fs::write(&pub_path, id.to_public().to_string()).unwrap();

        let mut profile = Profile::default();
        profile.features.encryption.recipient_file =
            Some(pub_path.to_str().unwrap().to_string());

        let (stack, meta) = build_stages(&resolved_with(profile), &default_args()).unwrap();
        // 순서: zstd(압축) 먼저, age(암호화) 나중.
        assert_eq!(stack.stage_names(), vec!["zstd", "age"]);
        assert_eq!(meta.compression.as_ref().unwrap().algorithm, "zstd");
        assert_eq!(meta.encryption.as_ref().unwrap().algorithm, "age");
        // age key_id에는 recipient 지문이 들어간다(키 자체 아님).
        assert!(meta
            .encryption
            .as_ref()
            .unwrap()
            .key_id
            .as_ref()
            .unwrap()
            .starts_with("age1"));
    }

    /// --no-encrypt면 암호화 단계를 생략하고 압축만 남는다(평문, encryption 메타 None).
    #[test]
    fn no_encrypt_skips_encryption_stage() {
        let profile = Profile::default();
        let mut args = default_args();
        args.no_encrypt = true;
        let (stack, meta) = build_stages(&resolved_with(profile), &args).unwrap();
        assert_eq!(stack.stage_names(), vec!["zstd"]);
        assert!(meta.encryption.is_none());
        assert!(meta.compression.is_some());
    }

    /// --compress-level이 config 레벨을 덮어쓴다.
    #[test]
    fn cli_compress_level_overrides_config() {
        let mut profile = Profile::default();
        profile.features.compression.level = 3;
        let mut args = default_args();
        args.compress_level = Some(19);
        args.no_encrypt = true; // 암호화는 이 테스트 범위 밖.
        let (_stack, meta) = build_stages(&resolved_with(profile), &args).unwrap();
        assert_eq!(meta.compression.unwrap().level, 19);
    }

    /// config 압축 레벨이 CLI 미지정 시 그대로 쓰인다.
    #[test]
    fn config_compress_level_used_when_no_cli() {
        let mut profile = Profile::default();
        profile.features.compression.level = 7;
        let mut args = default_args();
        args.no_encrypt = true;
        let (_stack, meta) = build_stages(&resolved_with(profile), &args).unwrap();
        assert_eq!(meta.compression.unwrap().level, 7);
    }

    /// age 암호화인데 recipient_file이 없으면 Config 에러로 막는다.
    #[test]
    fn age_without_recipient_file_is_config_error() {
        let profile = Profile::default(); // recipient_file = None, algorithm = age 기본.
        let result = build_stages(&resolved_with(profile), &default_args());
        let code = match result {
            Ok(_) => panic!("recipient_file 없는 age는 실패해야 함"),
            Err(e) => e.exit_code(),
        };
        assert_eq!(code, 2);
    }
}
