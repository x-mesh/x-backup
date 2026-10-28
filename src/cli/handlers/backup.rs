//! `backup` 서브커맨드 핸들러 — config 로드 → 파이프라인 실행 → 요약 출력.
//!
//! 풀/증분 백업(Mongo oplog · PostgreSQL logical decoding), 로컬/S3 destination + 멀티
//! destination 복제, 압축·암호화, 자동 사전 점검(status 선행)을 지원한다. DB 종류는 source
//! URI 스킴으로 분기한다([`crate::engine::DbKind`] — Mongo는 아래 본 경로, PG는
//! [`handle_pg_backup`]).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::cli::args::{BackupArgs, BackupType};
use crate::cli::output::{field_line, field_line_toned, style, OutputFlags, OutputMode, Tone};
use crate::cli::progress::{new_counter, DetailFn, ProgressDetail, ProgressKind, ProgressReporter};
use crate::compress::{compress_stage, ALGORITHM_NONE};
use crate::config::env::collect_overrides_from_process;
use crate::config::file::DestinationConfig;
use crate::config::merged::MergeInput;
use crate::config::secret::Secret;
use crate::config::ResolvedConfig;
use crate::crypto::build_encrypt_stage;
use crate::engine::mongo::status::{human_bytes, CheckStatus, StatusChecker};
use crate::error::{Result, XBackupError};
use crate::hooks::{HookContext, HookEvent, HookSet};
use crate::i18n::Lang;
use crate::manifest::schema::{CompressionMeta, EncryptionMeta, OplogRange, Topology};
use crate::pipeline::backup::{
    run_full_backup_with_rows, BackupMeta, BackupOutcome, BackupRequest, Engine,
};
use crate::pipeline::incremental::{
    run_incremental_backup, IncrementalOutcome, IncrementalRequest,
};
use crate::pipeline::rows_delta::{compute_delta, find_previous, DroppedTable, RowsDelta};
use crate::pipeline::stage::{StageStack, ENV_AES_KEY_HEX};
use crate::storage::{from_config, replicate_artifact, Storage};

/// `backup --json` 요약 스키마 버전.
///
/// 필드 의미가 바뀌면 올린다 — 소비자(웹 콘솔)가 버전으로 파서를 고르고, 모르는 버전을
/// 만나면 조용히 오파싱하는 대신 명확히 실패할 수 있게 하기 위함이다.
pub const BACKUP_JSON_SCHEMA: u32 = 1;

/// `backup --json`이 내보내는 요약.
///
/// ## 왜 타입인가
/// 이 요약은 원래 엔진·백업 유형별로 흩어진 8개의 `serde_json::json!` 리터럴이었다.
/// 리터럴은 타입이 아니라서 필드 이름을 잘못 적거나 빠뜨려도 컴파일이 통과하고, 그 출력을
/// 파싱하는 쪽만 조용히 깨진다. 하나의 타입으로 모으면 계약이 컴파일러의 관할로 들어온다.
///
/// ## 왜 Option이 많은가
/// 변형마다 의미 있는 필드가 다르다(풀에는 체크섬, 증분에는 base·변경 건수). `None`은
/// `skip_serializing_if`로 **키 자체가 생략**되므로, 리터럴 시절과 키 집합이 같다 —
/// 없는 정보를 `null`로 채워 소비자에게 거짓 필드를 만들지 않는다.
#[derive(Debug, serde::Serialize)]
pub struct BackupJsonSummary<'a> {
    /// 스키마 버전(항상 존재).
    pub schema: u32,
    /// "full" | "incremental". 풀 백업 경로 일부는 유형을 표기하지 않는다(기존 동작 유지).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup_type: Option<&'a str>,
    pub backup_id: &'a str,
    /// 증분이 딛고 선 풀 백업 ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_id: Option<&'a str>,
    /// MongoDB 증분이 캡처한 oplog 항목 수.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oplog_count: Option<u64>,
    /// PostgreSQL·MySQL 증분이 캡처한 변경 건수.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_count: Option<u64>,
    pub stored_size_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checksum_sha256: Option<&'a str>,
    /// oplog/binlog gap 때문에 증분이 풀로 승격됐는지(exit 4 경로).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub promoted_from_gap: Option<bool>,
    /// MongoDB 토폴로지 표기(replica set / standalone).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topology: Option<String>,
    /// "postgresql" | "mysql". MongoDB 경로는 기존대로 표기하지 않는다.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database: Option<&'a str>,
    /// 기록한 destination 개수(멀티 destination).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destinations: Option<usize>,
    /// 승격 사유 등 사람이 읽는 부연.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'a str>,
    /// MySQL 풀 백업의 전체 행 수.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows_total: Option<u64>,
    /// MySQL 풀 백업의 테이블별 행 수.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub table_rows: Option<&'a BTreeMap<String, u64>>,
    /// 백업 실행에 걸린 시간(초).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_secs: Option<f64>,
    /// 직전 풀 백업 대비 행 수 변화와 경고(비교할 수 없으면 키 생략).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows_vs_previous: Option<serde_json::Value>,
}

impl Default for BackupJsonSummary<'_> {
    fn default() -> Self {
        Self {
            schema: BACKUP_JSON_SCHEMA,
            backup_type: None,
            backup_id: "",
            base_id: None,
            oplog_count: None,
            change_count: None,
            stored_size_bytes: 0,
            checksum_sha256: None,
            promoted_from_gap: None,
            topology: None,
            database: None,
            destinations: None,
            reason: None,
            rows_total: None,
            table_rows: None,
            elapsed_secs: None,
            rows_vs_previous: None,
        }
    }
}

impl BackupJsonSummary<'_> {
    /// 요약을 stdout에 한 줄 JSON으로 내보낸다.
    ///
    /// 직렬화는 실패할 수 없는 형태(모든 필드가 원시 타입)라 실패 시 패닉이 정당하다 —
    /// 조용히 빈 출력을 내보내면 소비자가 성공으로 오인한다.
    fn emit(&self) {
        let line = serde_json::to_string(self).expect("요약 직렬화는 실패할 수 없다");
        println!("{line}");
    }
}

/// `backup` 핸들러 진입점.
pub async fn handle(
    config_path: Option<PathBuf>,
    lang_flag: Option<crate::i18n::Lang>,
    args: BackupArgs,
) -> Result<()> {
    // config를 읽기도 전에 lock 충돌·config 부재/읽기 실패로 끝날 수 있다. 그 경로들의
    // 에러 라벨·본문이 --lang을 따르도록 config 병합보다 먼저 CLI flag만으로 언어를
    // 잡아 둔다. config가 성공적으로 읽히면 아래에서 activate_from_toml이 그 안의
    // [output].language까지 반영해 다시 정한다(Mutex라 재설정 가능).
    crate::i18n::set_active(lang_flag.unwrap_or_default());

    // 명시 profile은 config·secret 해석 전에 잠근다. 다른 실행이 이미 작업 중이면 URI 설정
    // 오류보다 lock 충돌을 먼저 보고한다. profile 생략 시에는 default_profile 해석 뒤 잠근다.
    let explicit_lock = if args.profile.is_empty() {
        None
    } else {
        Some(crate::lock::acquire(&args.profile)?)
    };

    // 1) config 로드 + 레이어 병합(file + ENV; CLI는 아래에서 직접 반영).
    let config_toml = match &config_path {
        Some(path) => Some(std::fs::read_to_string(path).map_err(|e| {
            XBackupError::Config(crate::tr_lang!(
                lang_flag.unwrap_or_default(),
                "failed to read the config file ({}): {e}",
                "config 파일 읽기 실패({}): {e}",
                path.display()
            ))
        })?),
        None => None,
    };
    let lang = crate::i18n::activate_from_toml(lang_flag, config_toml.as_deref());
    let overrides = collect_overrides_from_process();
    let resolved = ResolvedConfig::build(MergeInput {
        config_toml: config_toml.as_deref(),
        profile_name: &args.profile,
        overrides: &overrides,
    })?;

    // 동시 실행 잠금(FR-12) — default_profile까지 해석한 실효 이름으로 같은 프로파일의
    // backup/restore/prune을 직렬화한다. 가드는 함수 끝까지 유지한다(충돌 시 exit 5).
    let _lock = match explicit_lock {
        Some(lock) => lock,
        None => crate::lock::acquire(&resolved.profile_name)?,
    };

    // 2) 백업 읽기 소스 URI 확보(PRD-05). 우선순위: CLI --read-source > config read_uri(복제본)
    //    > 주 소스(uri). 복제본에서 읽으면 primary 부하를 분리한다. build_stages가 &resolved를
    //    쓰므로 clone으로 꺼내 부분 이동을 피한다(Secret은 Clone).
    let uri = select_read_uri(args.read_source.as_deref(), &resolved)?;
    // 접속 타임아웃(초) — config source.connect_timeout_secs(미설정이면 None → 기본 5초).
    let timeout_secs = resolved.profile.source.connect_timeout_secs;

    // 3) destination 구성 — local/s3 모두 from_config로 일반화. 멀티 destination이면
    //    첫 항목이 primary(필수), 나머지는 보조(순차 fan-out으로 복제 — 아래).
    //    primary 구성 실패는 백업을 막는 하드 에러, 보조 구성/복제 실패는 경고(exit 4).
    let dests: Vec<DestinationConfig> = resolved
        .profile
        .effective_destinations()
        .into_iter()
        .cloned()
        .collect();
    let primary = from_config(&dests[0])?;

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
    let backup_type = effective_backup_type(&resolved, &args)?;

    // 실행 컨텍스트(프로파일·DB) 표시 — 다중 DB 툴이라 무엇을 백업하는지 항상 보인다.
    let db = crate::engine::DbKind::from_uri(uri.expose());
    crate::cli::output::print_run_context(&resolved.profile_name, Some(db), mode);

    // PRD-04 생명주기 훅 — pre_backup 게이트(비-0이면 백업 미시작) → 백업 실행 →
    //   결과에 따라 post_backup(성공·경고 동반 성공) 또는 on_error(실패) 관측 훅.
    let hooks = build_hookset(&resolved, args.no_hooks);
    let mut hook_ctx = HookContext {
        profile: resolved.profile_name.clone(),
        engine: Some(db_hook_label(db).to_string()),
        backup_type: Some(backup_type_label(backup_type).to_string()),
        started_at: Some(chrono::Utc::now().to_rfc3339()),
        ..Default::default()
    };
    hooks.run_gate(HookEvent::PreBackup, &hook_ctx).await?;

    // 실제 백업을 async 블록으로 감싸 한 Result로 모은다(pg/mysql/mongo·full/incr의
    //   여러 early-return을 블록의 반환으로 수렴). uri 등은 여기서 소비된다.
    let result: Result<()> = async move {
        let secondaries = &dests[1..];

        // DB 종류 분기 — source URI 스킴이 postgres면 PostgreSQL 경로(드라이버 COPY, oplog/토폴로지
        //   개념 없음)로 빠진다. 그 외(mongodb)는 아래 Mongo 경로.
        if db == crate::engine::DbKind::Postgres {
            return handle_pg_backup(
                &resolved,
                &args,
                uri,
                timeout_secs,
                primary.as_ref(),
                &dests[0],
                secondaries,
                dests.len(),
                mode,
                backup_type,
                lang,
            )
            .await;
        }
        if db == crate::engine::DbKind::Mysql {
            return handle_mysql_backup(
                &resolved,
                &args,
                uri,
                timeout_secs,
                primary.as_ref(),
                &dests[0],
                secondaries,
                dests.len(),
                mode,
                backup_type,
                lang,
            )
            .await;
        }
        if db == crate::engine::DbKind::File {
            return handle_file_backup(
                &resolved,
                &args,
                uri,
                primary.as_ref(),
                &dests[0],
                secondaries,
                dests.len(),
                mode,
                backup_type,
                lang,
            )
            .await;
        }

        // backup 실행 전 status 핵심 점검(연결·권한·토폴로지·도구 존재)을 자동 선행한다(FR-8,
        //   PRD §9). 전체 status보다 가벼운 서브셋([`StatusChecker::precheck_subset`])으로, 백업을
        //   *막는* 결함(Fail)만 본다. 하나라도 Fail이면 PrecheckFailed(exit 3)로 백업을 미시작한다.
        //   --skip-precheck면 우회한다(읽기 전용·무부작용).
        // 엔진 결정(native | mongodump) — 사전 점검(도구 존재 여부)과 덤프 경로 양쪽이 쓴다.
        let engine = Engine::parse(&resolved.profile.mode.engine)?;
        let should_precheck = resolved.profile.mode.precheck && !args.skip_precheck;
        if should_precheck {
            run_precheck(&uri, &resolved.profile_name, timeout_secs, engine).await?;
        } else if args.skip_precheck {
            tracing::warn!(
                "{}",
                crate::tr!(
                    "--skip-precheck given, so the pre-backup checks are skipped (FR-8 bypass)",
                    "--skip-precheck가 있어 백업 전 점검을 건너뜁니다(FR-8 우회)",
                )
            );
        } else {
            tracing::warn!(
                "{}",
                crate::tr!(
                    "config sets mode.precheck=false, so the pre-backup checks are skipped",
                    "config의 mode.precheck=false라 백업 전 점검을 건너뜁니다",
                )
            );
        }

        // 증분(--type incr)은 드라이버 oplog 캡처 경로로 분기한다(t8). 선택적 백업
        //   (--db/--collection)과는 병용 불가(증분은 항상 전체 oplog 슬라이스).
        if matches!(backup_type, BackupType::Incr) {
            if args.db.is_some() || args.collection.is_some() {
                return Err(XBackupError::Usage(crate::tr!("an incremental backup (--type incr) cannot be combined with a selective backup (--db/--collection) — an incremental captures the entire oplog slice (FR-1/FR-2).", "증분 백업(--type incr)은 선택적 백업(--db/--collection)과 병용할 수 없습니다 \
                 — 증분은 전체 oplog 슬라이스를 캡처합니다(FR-1/FR-2).")));
            }
            return handle_incremental(
                &resolved,
                &args,
                &uri,
                primary.as_ref(),
                &dests[0],
                secondaries,
                mode,
                lang,
            )
            .await;
        }

        // 4) 파이프라인 단계 구성(t6): compress → encrypt 고정 순서(PRD §8.4). 단계와
        //    manifest 메타를 함께 만든다(아래 build_stages 참조). --no-encrypt면 암호화 생략.
        //    진행 표시(R16): 공유 카운터를 만들어 BackupRequest에 주입하고, 백업은 dump 총량을
        //    사전에 모르므로 부정형(spinner)로 처리 바이트·속도를 stderr에 표시한다(PRD §FR-9).
        let progress_counter = new_counter();
        // 행·컬렉션 진행은 native 엔진만 알려 준다(mongodump는 아카이브를 해석하지 않는다).
        let row_progress = matches!(engine, Engine::Native)
            .then(crate::engine::progress::RowProgress::new);
        let request = BackupRequest {
            uri,
            mongodump_program: "mongodump".to_string(),
            db: args.db.clone(),
            collection: args.collection.clone(),
            timeout_secs,
            engine,
            progress_counter: Some(std::sync::Arc::clone(&progress_counter)),
        };
        let (stages, meta) = build_stages(&resolved, &args)?;

        let reporter = ProgressReporter::start_with_detail(
            mode,
            ProgressKind::Indeterminate {
                label: lang.sel("backup", "백업").into(),
            },
            progress_counter,
            row_progress.as_ref().map(|progress| {
                row_progress_detail(Arc::clone(progress), RowUnit::Collections, lang)
            }),
        );
        let started = Instant::now();
        let result = run_full_backup_with_rows(
            &request,
            primary.as_ref(),
            stages,
            meta,
            row_progress,
        )
        .await;
        let elapsed = started.elapsed();
        reporter.finish().await;
        let outcome = result?;

        // 4.5) 보조 destination으로 순차 복제(멀티 destination). primary는 성공했으므로,
        //      복제 실패는 경고(exit 4)로만 보고한다("primary 필수 + 나머지 경고" 정책).
        let replicate_warning = replicate_and_warn(
            primary.as_ref(),
            secondaries,
            &outcome.backup_id,
            outcome.stored_size_bytes > 0,
        )
        .await;

        // 4.6) 직전 풀 백업 대비 행 수 변화 — 사라진 컬렉션이나 행 급감을 경고(exit 4)로 알린다.
        let delta =
            rows_delta_for(primary.as_ref(), &outcome, engine.archive_format(), &args).await;
        let delta_warning = delta
            .as_ref()
            .and_then(|d| rows_delta_warning(d, RowUnit::Collections, lang));
        let warning = merge_warnings(replicate_warning, delta_warning);

        // 5) 요약 출력(stdout — 결과 전용). 진행은 stderr, 결과/--json은 stdout으로 분리.
        if mode.emits_json() {
            BackupJsonSummary {
                backup_id: &outcome.backup_id,
                stored_size_bytes: outcome.stored_size_bytes,
                checksum_sha256: Some(&outcome.checksum_sha256),
                topology: Some(format!("{:?}", outcome.topology)),
                destinations: Some(dests.len()),
                rows_total: outcome.table_rows.as_ref().map(|rows| rows.values().sum()),
                table_rows: outcome.table_rows.as_ref(),
                elapsed_secs: Some(elapsed.as_secs_f64()),
                rows_vs_previous: delta.as_ref().map(RowsDelta::to_json),
                ..Default::default()
            }
            .emit();
        } else if mode.shows_human_summary() {
            let topo = match outcome.topology {
                Topology::ReplicaSet => "replica_set",
                Topology::Standalone => "standalone",
            };
            print_backup_summary(
                &BackupSummary {
                    kind: "full",
                    detail: topo,
                    backup_id: &outcome.backup_id,
                    primary_dest: &dests[0],
                    stored_size: outcome.stored_size_bytes,
                    original_size: Some(outcome.original_size_bytes),
                    compression: outcome.compression.as_ref(),
                    encryption: outcome.encryption.as_ref(),
                    checksum: Some(&outcome.checksum_sha256),
                    dest_count: dests.len(),
                    base_id: None,
                    change: None,
                    oplog_range: outcome.oplog_range.as_ref(),
                    note: None,
                },
                lang,
            );
            if let Some(rows) = outcome.table_rows.as_ref() {
                print_rows_summary(
                    rows,
                    RowUnit::Collections,
                    elapsed,
                    &outcome.backup_id,
                    delta.as_ref().map(|d| (d, args.warn_drop_percent)),
                    lang,
                );
            }
        }

        // 보조 복제 실패나 직전 백업 대비 이상 징후가 있으면 경고로 마감(exit 4) — primary 백업은
        // 이미 성공했다.
        if let Some(w) = warning {
            return Err(XBackupError::Warning(w));
        }
        Ok(())
    }
    .await;

    // 결과에 따라 관측 훅(post_backup / on_error)을 실행한다(작업 판정은 바꾸지 않음).
    // 성공(exit 0)·경고 동반 성공(exit 4, gap 승격 등)은 백업이 이뤄진 것 → post_backup.
    let event = post_hook_event(&result);
    hook_ctx.ended_at = Some(chrono::Utc::now().to_rfc3339());
    hook_ctx.status = Some(
        if event == HookEvent::PostBackup {
            "ok"
        } else {
            "failed"
        }
        .to_string(),
    );
    if let Err(e) = &result {
        hook_ctx.exit_code = Some(i32::from(e.exit_code()));
        hook_ctx.error = Some(e.to_string());
    } else {
        hook_ctx.exit_code = Some(0);
    }
    hooks.run_observe(event, &hook_ctx).await;
    result
}

/// 백업 결과를 관측 훅 이벤트로 매핑한다. 성공(exit 0)과 경고 동반 성공(exit 4 —
/// gap 승격·보조 복제 실패 등 산출물은 존재)은 `post_backup`, 실제 실패는 `on_error`.
fn post_hook_event(result: &Result<()>) -> HookEvent {
    match result {
        Ok(_) => HookEvent::PostBackup,
        Err(e) if e.is_warning() => HookEvent::PostBackup,
        Err(_) => HookEvent::OnError,
    }
}

/// 백업 읽기 소스 URI를 우선순위로 고른다(PRD-05): `--read-source` > config `read_uri`(복제본)
/// > 주 소스(`uri`). 복제본에서 읽으면 primary 부하를 분리한다. 아무 소스도 없으면 Config 에러.
fn select_read_uri(read_source: Option<&str>, resolved: &ResolvedConfig) -> Result<Secret> {
    if let Some(rs) = read_source.filter(|s| !s.is_empty()) {
        tracing::info!(
            "{}",
            crate::tr!(
                "--read-source given, so the backup reads from a replica and leaves the primary alone",
                "--read-source가 있어 복제본에서 읽습니다. primary에는 부하를 주지 않습니다",
            )
        );
        return Ok(Secret::new(rs.to_string()));
    }
    let from_replica = resolved.resolved_read_uri.is_some();
    let uri = resolved.effective_read_uri().cloned().ok_or_else(|| {
        XBackupError::Config(crate::tr!(
            "profile '{}' has no source.uri_env/uri, or it did not resolve",
            "프로파일 '{}'에 source.uri_env/uri가 없거나 해석되지 않았습니다",
            resolved.profile_name
        ))
    })?;
    if from_replica {
        tracing::info!(
            "{}",
            crate::tr!(
                "config sets source.read_uri, so the backup reads from a replica and leaves the primary alone",
                "config의 source.read_uri를 따라 복제본에서 읽습니다. primary에는 부하를 주지 않습니다",
            )
        );
    }
    Ok(uri)
}

/// 프로파일 훅 설정 + 시크릿 env 목록으로 [`HookSet`]을 만든다(`--no-hooks`면 비활성).
fn build_hookset(resolved: &ResolvedConfig, no_hooks: bool) -> HookSet {
    let secret_env = collect_secret_env_names(&resolved.profile);
    HookSet::new(resolved.profile.hooks.clone(), !no_hooks, secret_env)
}

/// 훅 env에서 제거할 시크릿 env 변수 이름을 모은다(NFR-2).
///
/// source `uri_env`(주 소스) + `read_uri_env`(복제본 — 대개 같은 자격증명) + 각 destination
/// S3 `credentials_env`. AES 키(env)는 [`HookSet`]이 상수로 함께 제거하므로 여기 포함하지 않는다.
fn collect_secret_env_names(profile: &crate::config::file::Profile) -> Vec<String> {
    let mut secret_env = Vec::new();
    if let Some(name) = &profile.source.uri_env {
        secret_env.push(name.clone());
    }
    // read_uri_env(복제본 자격증명)도 반드시 제거한다 — 누락되면 복제본 비밀번호가 훅으로 샌다.
    if let Some(name) = &profile.source.read_uri_env {
        secret_env.push(name.clone());
    }
    for dest in profile.effective_destinations() {
        if let Some(s3) = &dest.s3 {
            if let Some(creds) = &s3.credentials_env {
                secret_env.push(creds.clone());
            }
        }
    }
    secret_env
}

/// DB 종류를 훅 컨텍스트용 엔진 라벨(`XB_ENGINE`)로 — mongodb/postgresql/mysql.
fn db_hook_label(db: crate::engine::DbKind) -> &'static str {
    match db {
        crate::engine::DbKind::Mongo => "mongodb",
        crate::engine::DbKind::Postgres => "postgresql",
        crate::engine::DbKind::Mysql => "mysql",
        crate::engine::DbKind::File => "file",
    }
}

/// 백업 유형을 훅 컨텍스트용 라벨(`XB_BACKUP_TYPE`)로 — full/incremental.
fn backup_type_label(bt: BackupType) -> &'static str {
    match bt {
        BackupType::Full => "full",
        BackupType::Incr => "incremental",
    }
}

/// 백업이 저장된 위치를 사람이 읽는 한 줄로 만든다(완료 요약 표시용).
///
/// 로컬 destination은 `경로/<id>/`(실제 디렉터리), 그 외(S3 등 path 없는 백엔드)는
/// `라벨:<id>`로 식별만 보여준다. 절대 경로/식별만 노출하며 시크릿은 담지 않는다.
fn backup_location(dest: &DestinationConfig, backup_id: &str) -> String {
    match dest.path.as_deref() {
        Some(p) => format!("{}/{}/", p.trim_end_matches('/'), backup_id),
        None => format!("{}:{}", dest.label(0), backup_id),
    }
}

/// 세 자리마다 쉼표를 넣는다(`133277` → `133,277`).
fn group_digits(n: u64) -> String {
    let mut reversed = Vec::new();
    let mut run = 0;
    for c in n.to_string().chars().rev() {
        if run == 3 {
            reversed.push(',');
            run = 0;
        }
        reversed.push(c);
        run += 1;
    }
    reversed.into_iter().rev().collect()
}

/// 경과 시간을 `MM:SS`(1시간 이상이면 `H:MM:SS`)로 쓴다.
fn format_elapsed(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, secs % 3600 / 60, secs % 60)
    } else {
        format!("{:02}:{:02}", secs / 60, secs % 60)
    }
}

/// 테이블별 행 수 요약.
#[derive(Debug, PartialEq, Eq)]
struct RowsSummary {
    total: u64,
    tables: usize,
    empty: usize,
    /// 행이 많은 순서의 상위 3개(행이 0인 테이블은 제외).
    largest: Vec<(String, u64)>,
}

fn summarize_rows(rows: &BTreeMap<String, u64>) -> RowsSummary {
    let mut by_size: Vec<(&String, &u64)> = rows.iter().filter(|(_, &n)| n > 0).collect();
    by_size.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    RowsSummary {
        total: rows.values().sum(),
        tables: rows.len(),
        empty: rows.values().filter(|&&n| n == 0).count(),
        largest: by_size
            .into_iter()
            .take(3)
            .map(|(name, &n)| (name.clone(), n))
            .collect(),
    }
}

/// 행을 세는 단위 — 표시 문구만 다르다(MySQL·PostgreSQL은 테이블, MongoDB는 컬렉션).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowUnit {
    Tables,
    Collections,
}

impl RowUnit {
    fn en(self) -> &'static str {
        match self {
            Self::Tables => "tables",
            Self::Collections => "collections",
        }
    }

    fn ko(self) -> &'static str {
        match self {
            Self::Tables => "테이블",
            Self::Collections => "컬렉션",
        }
    }
}

/// 백업 진행 줄에 붙일 상세(지금 덤프 중인 단위·단위 n/N·누적 행 수).
fn row_progress_detail(
    progress: Arc<crate::engine::progress::RowProgress>,
    unit: RowUnit,
    lang: Lang,
) -> DetailFn {
    Arc::new(move || {
        let snap = progress.snapshot();
        let fields = vec![
            ("rows", snap.rows),
            ("tables_done", snap.tables_done),
            ("tables_total", snap.tables_total),
        ];
        // 테이블 목록을 읽기 전에는 보여 줄 것이 없다.
        if snap.tables_total == 0 {
            return ProgressDetail {
                text: String::new(),
                fields,
            };
        }
        let rows = group_digits(snap.rows);
        let text = lang
            .sel(
                &format!(
                    "{} · {}/{} {} · {rows} rows",
                    printable(&snap.current),
                    snap.tables_done,
                    snap.tables_total,
                    unit.en()
                ),
                &format!(
                    "{} · {}/{} {} · {rows}행",
                    printable(&snap.current),
                    snap.tables_done,
                    snap.tables_total,
                    unit.ko()
                ),
            )
            .to_string();
        ProgressDetail { text, fields }
    })
}

/// 테이블 이름은 DB가 정한 임의 문자열이라, 제어 문자가 터미널 이스케이프로 해석되지 않게 치환한다.
fn printable(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// 직전 풀 백업 대비 행 수 변화를 계산한다.
///
/// 선택적 백업(`--db`/`--collection`)은 일부만 담으므로, 행 수가 없거나 직전 백업이 없으면 비교할
/// 수 없다. 정보 제공이 목적이라 직전 백업을 읽지 못해도 백업을 실패시키지 않고 경고만 남긴다.
async fn rows_delta_for(
    storage: &dyn Storage,
    outcome: &BackupOutcome,
    archive_format: &str,
    args: &BackupArgs,
) -> Option<RowsDelta> {
    if args.db.is_some() || args.collection.is_some() {
        return None;
    }
    let rows = outcome.table_rows.as_ref()?;
    match find_previous(storage, &outcome.backup_id, archive_format).await {
        Ok(Some(previous)) => Some(compute_delta(
            &previous,
            rows,
            outcome.stored_size_bytes,
            args.warn_drop_percent,
        )),
        Ok(None) => None,
        Err(e) => {
            tracing::warn!("직전 백업과 행 수를 비교하지 못했습니다: {e}");
            None
        }
    }
}

/// 이름 목록을 최대 5개까지 보여 주고 나머지는 개수로 줄인다.
fn limited_names<'a>(names: impl Iterator<Item = &'a str>, lang: Lang) -> String {
    const SHOWN: usize = 5;
    let names: Vec<String> = names.map(printable).collect();
    let mut text = names[..names.len().min(SHOWN)].join(", ");
    if names.len() > SHOWN {
        text.push_str(lang.sel(
            &format!(" (+{} more)", names.len() - SHOWN),
            &format!(" (외 {}개)", names.len() - SHOWN),
        ));
    }
    text
}

/// 크기 변화율을 `+0.9%` 형태로(소수 첫째 자리).
fn signed_percent(percent: f64) -> String {
    format!("{percent:+.1}%")
}

/// 직전 백업 대비 변화 줄과 경고 줄을 출력한다.
fn print_rows_delta(delta: &RowsDelta, unit: RowUnit, drop_percent: u32, lang: Lang) {
    const W: usize = 11;
    let rows_diff = i128::from(delta.total) - i128::from(delta.previous_total);
    let mut change = format!(
        "{}… {} {}{}",
        delta.previous_id.chars().take(8).collect::<String>(),
        lang.sel("vs", "대비"),
        if rows_diff >= 0 { "+" } else { "-" },
        group_digits(rows_diff.unsigned_abs() as u64),
    );
    change.push_str(lang.sel(" rows", "행"));
    if let Some(percent) = delta.rows_change_percent() {
        change.push_str(&format!(" ({})", signed_percent(percent)));
    }
    if let Some(percent) = delta.size_change_percent() {
        change.push_str(&format!(
            " · {} {}",
            lang.sel("size", "크기"),
            signed_percent(percent)
        ));
    }
    println!("{}", field_line_toned("vs prev", change, W, Tone::Value));

    if !delta.vanished.is_empty() {
        let text = lang
            .sel(
                &format!(
                    "{} {} missing since the previous backup: {}",
                    delta.vanished.len(),
                    unit.en(),
                    limited_names(delta.vanished.iter().map(String::as_str), lang)
                ),
                &format!(
                    "직전 백업에 있던 {} {}개가 없습니다: {}",
                    unit.ko(),
                    delta.vanished.len(),
                    limited_names(delta.vanished.iter().map(String::as_str), lang)
                ),
            )
            .to_string();
        println!("{}", field_line_toned("warning", text, W, Tone::Warning));
    }
    if !delta.dropped.is_empty() {
        let shown = delta
            .dropped
            .iter()
            .take(5)
            .map(|d: &DroppedTable| {
                format!(
                    "{} {} → {}",
                    printable(&d.table),
                    group_digits(d.previous),
                    group_digits(d.now)
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let more = delta.dropped.len().saturating_sub(5);
        let more = if more > 0 {
            lang.sel(&format!(" (+{more} more)"), &format!(" (외 {more}개)"))
                .to_string()
        } else {
            String::new()
        };
        let text = lang
            .sel(
                &format!("rows dropped by {drop_percent}% or more: {shown}{more}"),
                &format!("행이 {drop_percent}% 이상 줄었습니다: {shown}{more}"),
            )
            .to_string();
        println!("{}", field_line_toned("warning", text, W, Tone::Warning));
    }
}

/// 직전 백업 대비 경고를 종료 코드(exit 4)용 한 줄로 요약한다. 경고가 없으면 None.
fn rows_delta_warning(delta: &RowsDelta, unit: RowUnit, lang: Lang) -> Option<String> {
    delta.has_warnings().then(|| {
        lang.sel(
            &format!(
                "compared with the previous backup: {} {} missing, {} {} lost many rows",
                delta.vanished.len(),
                unit.en(),
                delta.dropped.len(),
                unit.en()
            ),
            &format!(
                "직전 백업과 비교하면 사라진 {unit_ko} {}개, 행이 급감한 {unit_ko} {}개가 있습니다",
                delta.vanished.len(),
                delta.dropped.len(),
                unit_ko = unit.ko()
            ),
        )
        .to_string()
    })
}

/// 경고가 둘 다 있으면 `; `로 이어 하나의 종료 경고로 만든다.
fn merge_warnings(first: Option<String>, second: Option<String>) -> Option<String> {
    match (first, second) {
        (Some(a), Some(b)) => Some(format!("{a}; {b}")),
        (a, b) => a.or(b),
    }
}

/// 완료 요약 뒤에 행 수·소요 시간·다음 단계를 이어 출력한다(풀 백업).
fn print_rows_summary(
    rows: &BTreeMap<String, u64>,
    unit: RowUnit,
    elapsed: Duration,
    backup_id: &str,
    delta: Option<(&RowsDelta, u32)>,
    lang: Lang,
) {
    const W: usize = 11;
    let sum = summarize_rows(rows);
    let total = group_digits(sum.total);
    let empty = if sum.empty > 0 {
        lang.sel(
            &format!(" ({} empty)", sum.empty),
            &format!(" (빈 {} {})", unit.ko(), sum.empty),
        )
        .to_string()
    } else {
        String::new()
    };
    let rows_text = lang
        .sel(
            &format!("{total} rows · {} {}{empty}", sum.tables, unit.en()),
            &format!("{total}행 · {}개 {}{empty}", sum.tables, unit.ko()),
        )
        .to_string();
    println!("{}", field_line_toned("rows", rows_text, W, Tone::Value));
    if !sum.largest.is_empty() {
        let largest = sum
            .largest
            .iter()
            .map(|(name, n)| format!("{} {}", printable(name), group_digits(*n)))
            .collect::<Vec<_>>()
            .join(" · ");
        println!("{}", field_line_toned("largest", largest, W, Tone::Value));
    }
    let mut elapsed_text = format_elapsed(elapsed);
    if elapsed.as_secs() >= 1 && sum.total > 0 {
        let rate = (sum.total as f64 / elapsed.as_secs_f64()).round() as u64;
        elapsed_text.push_str(&format!(
            " ({}/s)",
            lang.sel(
                &format!("{} rows", group_digits(rate)),
                &format!("{}행", group_digits(rate)),
            )
        ));
    }
    println!(
        "{}",
        field_line_toned("elapsed", elapsed_text, W, Tone::Value)
    );
    if let Some((delta, drop_percent)) = delta {
        print_rows_delta(delta, unit, drop_percent, lang);
    }
    println!(
        "{}",
        field_line(
            "next",
            format!(
                "x-backup verify --id {} --deep  ·  x-backup list",
                style(backup_id, Tone::Value)
            ),
            W,
        )
    );
}

/// 백업 완료 요약(사람용)의 통일된 뷰 — 4개 경로(mongo·pg × full·incr)가 공유한다.
///
/// 각 경로가 자기 outcome/메타에서 이 뷰를 채워 [`print_backup_summary`]에 넘긴다. 경로
/// 특유의 필드(base/변경/oplog/note)는 `Option`이라 해당 없으면 줄이 생략된다.
struct BackupSummary<'a> {
    /// 분류 — "full" | "incr" | "full(gap 승격)".
    kind: &'a str,
    /// 부가 분류 — Mongo는 토폴로지("replica_set"/"standalone"), PG는 "PostgreSQL".
    detail: &'a str,
    backup_id: &'a str,
    /// primary destination(저장 위치 표시용).
    primary_dest: &'a DestinationConfig,
    /// 저장 바이트(data.bin). 빈 증분 슬라이스면 0.
    stored_size: u64,
    /// 압축 전 원본 바이트 — 풀 백업만 Some(절감률 표시). 증분은 미보유라 None.
    original_size: Option<u64>,
    compression: Option<&'a CompressionMeta>,
    encryption: Option<&'a EncryptionMeta>,
    /// data.bin sha256 — 풀 백업만 Some(증분 outcome은 미노출).
    checksum: Option<&'a str>,
    /// 전체 destination 수(primary + 보조). 2 이상이면 fan-out 줄 표시.
    dest_count: usize,
    /// 증분: 연결된 base 백업 ID.
    base_id: Option<&'a str>,
    /// 증분: (라벨, 건수) — Mongo "entries"(oplog), PG "changes"(레코드). 라벨은 항상 영문.
    change: Option<(&'a str, u64)>,
    /// Mongo: oplog 구간.
    oplog_range: Option<&'a OplogRange>,
    /// 꼬리 주석 — "변경 없음 …", gap 승격 사유 등.
    note: Option<&'a str>,
}

/// 통일된 백업 완료 요약을 stdout(결과 전용)에 출력한다.
fn print_backup_summary(s: &BackupSummary, lang: Lang) {
    const W: usize = 11;
    println!(
        "{}",
        style(
            &format!(
                "{} ({} · {})",
                lang.sel("Backup complete", "백업 완료"),
                s.kind,
                s.detail
            ),
            Tone::Success
        )
    );
    println!("{}", field_line_toned("id", s.backup_id, W, Tone::Value));
    if let Some(b) = s.base_id {
        println!("{}", field_line_toned("base", b, W, Tone::Value));
    }
    if let Some((label, n)) = s.change {
        // 값 컬럼을 다른 라벨 줄과 맞춘다 — 라벨 표시폭(ASCII 1폭) 기준 패딩.
        println!("{}", field_line_toned(label, n.to_string(), W, Tone::Value));
    }
    let location = backup_location(s.primary_dest, s.backup_id);
    let dest_type = s.primary_dest.r#type.as_deref().unwrap_or("local");
    println!(
        "{}",
        field_line(
            "location",
            format!(
                "{}  ({})",
                style(&location, Tone::Value),
                style(dest_type, Tone::Muted)
            ),
            W,
        )
    );
    // 크기: 사람이 읽는 단위 + 정확한 bytes. 풀 백업은 원본 대비 절감률도 한 줄 덧붙인다.
    print!(
        "{}",
        field_line(
            "size",
            format!(
                "{} ({} bytes)",
                style(&human_bytes(s.stored_size as i64), Tone::Value),
                s.stored_size
            ),
            W,
        )
    );
    if let (Some(orig), Some(c)) = (s.original_size, s.compression) {
        if orig > s.stored_size {
            let saved = ((1.0 - s.stored_size as f64 / orig as f64) * 100.0).round() as i64;
            print!(
                "\n             {} {} · {} {saved}% {}",
                style(lang.sel("from", "← 원본"), Tone::Muted),
                style(&human_bytes(orig as i64), Tone::Value),
                style(&c.algorithm, Tone::Value),
                style(lang.sel("saved", "절감"), Tone::Success),
            );
        }
    }
    println!();
    if let Some(c) = s.compression {
        println!(
            "{}",
            field_line(
                "compression",
                format!("{} level {}", style(&c.algorithm, Tone::Value), c.level),
                W,
            )
        );
    }
    if let Some(e) = s.encryption {
        println!(
            "{}",
            field_line_toned("encryption", &e.algorithm, W, Tone::Value)
        );
    }
    if let Some(cs) = s.checksum {
        println!(
            "{}",
            field_line("checksum", format!("sha256:{}", style(cs, Tone::Value)), W,)
        );
    }
    if s.dest_count > 1 {
        println!(
            "{}",
            field_line(
                "destination",
                format!(
                    "{} ({})",
                    style(&s.dest_count.to_string(), Tone::Value),
                    lang.sel(
                        &format!("primary + {} secondary", s.dest_count - 1),
                        &format!("primary + 보조 {}", s.dest_count - 1),
                    )
                ),
                W,
            ),
        );
    }
    if let Some(r) = s.oplog_range {
        println!(
            "{}",
            field_line_toned(
                "oplog",
                format!(
                    "{{t:{},i:{}}} → {{t:{},i:{}}}",
                    r.start_ts.t, r.start_ts.i, r.end_ts.t, r.end_ts.i,
                ),
                W,
                Tone::Value,
            )
        );
    }
    if let Some(n) = s.note {
        println!("  {}", style(n, Tone::Warning));
    }
}

/// 파일/디렉터리 풀 백업 경로(P2-1) — 로컬 경로의 tar 스트림을 압축·암호화·저장한다.
///
/// PG/MySQL 경로의 storage/stage/summary 골격을 공유한다. 증분(스냅샷 인덱스)은 P2-2
/// 로드맵 — `--type incr`은 명확히 거부한다. `--db/--collection`은 DB 개념이라 거부.
/// 사전 점검은 소스 경로 접근 확인([`FileDumper::open`])이 겸한다.
///
/// [`FileDumper::open`]: crate::engine::file::backup::FileDumper::open
#[allow(clippy::too_many_arguments)]
async fn handle_file_backup(
    resolved: &ResolvedConfig,
    args: &BackupArgs,
    uri: Secret,
    primary: &dyn Storage,
    primary_dest: &DestinationConfig,
    secondaries: &[DestinationConfig],
    dest_count: usize,
    mode: OutputMode,
    backup_type: BackupType,
    lang: Lang,
) -> Result<()> {
    if args.db.is_some() || args.collection.is_some() {
        return Err(XBackupError::Usage(
            "파일 백업은 --db/--collection(DB 선택 백업)과 병용할 수 없습니다 — 백업 범위는 \
             source URI 경로로 지정합니다."
                .into(),
        ));
    }

    let source_path = crate::engine::file::path_from_uri(uri.expose())?;
    let (stages, meta) = build_stages(resolved, args)?;
    let progress_counter = new_counter();

    // 증분(--type incr) — 스냅샷 인덱스 diff 기반 슬라이스(P2-2). gap이면 풀로 승격(exit 4).
    if matches!(backup_type, BackupType::Incr) {
        let reporter = ProgressReporter::start(
            mode,
            ProgressKind::Indeterminate {
                label: lang.sel("file incremental", "파일 증분").into(),
            },
            std::sync::Arc::clone(&progress_counter),
        );
        let result = crate::pipeline::backup::run_file_incremental_backup(
            source_path,
            primary,
            stages,
            meta,
            Some(progress_counter),
        )
        .await;
        reporter.finish().await;
        let outcome = result?;

        let replicate_warning = replicate_and_warn(
            primary,
            secondaries,
            &outcome.backup_id,
            outcome.stored_size_bytes > 0,
        )
        .await;

        if mode.emits_json() {
            let summary = serde_json::json!({
                "backup_id": outcome.backup_id,
                "backup_type": if outcome.promoted { "full" } else { "incremental" },
                "base_id": outcome.base_id,
                "change_count": outcome.change_count,
                "stored_size_bytes": outcome.stored_size_bytes,
                "promoted_from_gap": outcome.promoted,
                "database": "file",
                "destinations": dest_count,
            });
            println!("{summary}");
        } else if mode.shows_human_summary() {
            let note = if outcome.promoted {
                Some(lang.sel(
                    "(gap — promoted to full backup)",
                    "(gap — 풀 백업으로 승격)",
                ))
            } else if outcome.change_count == 0 {
                Some(lang.sel(
                    "(no changes — empty slice, manifest only)",
                    "(변경 없음 — 빈 슬라이스, manifest만)",
                ))
            } else {
                None
            };
            print_backup_summary(
                &BackupSummary {
                    kind: if outcome.promoted { "full" } else { "incr" },
                    detail: "file",
                    backup_id: &outcome.backup_id,
                    primary_dest,
                    stored_size: outcome.stored_size_bytes,
                    original_size: None,
                    compression: None,
                    encryption: None,
                    checksum: None,
                    dest_count,
                    base_id: outcome.base_id.as_deref(),
                    change: Some(("changes", outcome.change_count)),
                    oplog_range: None,
                    note,
                },
                lang,
            );
        }
        if let Some(w) = replicate_warning {
            return Err(XBackupError::Warning(w));
        }
        // gap 승격은 경고 동반 성공(exit 4) — DB 엔진들과 동일 계약.
        if outcome.promoted {
            return Err(XBackupError::Warning(
                lang.sel(
                    "incremental promoted to full backup (gap — chain-head index missing)",
                    "증분이 풀 백업으로 승격됨(gap — 체인 헤드 인덱스 소실)",
                )
                .to_string(),
            ));
        }
        return Ok(());
    }

    let reporter = ProgressReporter::start(
        mode,
        ProgressKind::Indeterminate {
            label: lang.sel("file backup", "파일 백업").into(),
        },
        std::sync::Arc::clone(&progress_counter),
    );
    let result = crate::pipeline::backup::run_file_full_backup(
        source_path,
        primary,
        stages,
        meta,
        Some(progress_counter),
    )
    .await;
    reporter.finish().await;
    let outcome = result?;

    let replicate_warning = replicate_and_warn(
        primary,
        secondaries,
        &outcome.backup_id,
        outcome.stored_size_bytes > 0,
    )
    .await;

    if mode.emits_json() {
        let summary = serde_json::json!({
            "backup_id": outcome.backup_id,
            "stored_size_bytes": outcome.stored_size_bytes,
            "checksum_sha256": outcome.checksum_sha256,
            "database": "file",
            "destinations": dest_count,
        });
        println!("{summary}");
    } else if mode.shows_human_summary() {
        print_backup_summary(
            &BackupSummary {
                kind: "full",
                detail: "file",
                backup_id: &outcome.backup_id,
                primary_dest,
                stored_size: outcome.stored_size_bytes,
                original_size: Some(outcome.original_size_bytes),
                compression: outcome.compression.as_ref(),
                encryption: outcome.encryption.as_ref(),
                checksum: Some(&outcome.checksum_sha256),
                dest_count,
                base_id: None,
                change: None,
                oplog_range: None,
                note: None,
            },
            lang,
        );
    }

    if let Some(w) = replicate_warning {
        return Err(XBackupError::Warning(w));
    }
    Ok(())
}

/// PostgreSQL 풀 백업 경로 — 드라이버 COPY 아카이브를 압축·암호화·저장하고 보조 복제까지.
///
/// Mongo 경로의 storage/mode/replicate/summary 골격을 공유하되, 덤프는
/// [`run_pg_full_backup`](crate::pipeline::backup::run_pg_full_backup)가 담당한다(oplog/토폴로지
/// 없음). 증분(`--type incr`)은 `features.incremental.pg_logical` 활성 시
/// [`handle_pg_incremental`]로 분기한다(미활성이면 안내와 함께 거부).
#[allow(clippy::too_many_arguments)]
async fn handle_pg_backup(
    resolved: &ResolvedConfig,
    args: &BackupArgs,
    uri: Secret,
    timeout_secs: Option<u64>,
    primary: &dyn Storage,
    primary_dest: &DestinationConfig,
    secondaries: &[DestinationConfig],
    dest_count: usize,
    mode: OutputMode,
    backup_type: BackupType,
    lang: Lang,
) -> Result<()> {
    let enable_incremental = resolved.profile.features.incremental.pg_logical;

    // FR-8 자동 사전 점검(엔진 무관, PRD §323) — mode.precheck && !--skip-precheck면 연결+읽기
    //   권한을 점검해 차단 결함이면 exit 3로 미시작한다(스트림 도중 실패 대신 빠른 실패). 풀·증분
    //   양 경로보다 먼저 한 번 수행한다. Mongo 경로의 run_precheck 게이팅과 동일.
    let should_precheck = resolved.profile.mode.precheck && !args.skip_precheck;
    if should_precheck {
        crate::engine::postgres::status::precheck(&uri, timeout_secs).await?;
    } else if args.skip_precheck {
        tracing::warn!(
            "{}",
            crate::tr!(
                "--skip-precheck given, so the pre-backup checks are skipped (FR-8 bypass)",
                "--skip-precheck가 있어 PostgreSQL 백업 전 점검을 건너뜁니다(FR-8 우회)",
            )
        );
    } else {
        tracing::warn!(
            "{}",
            crate::tr!(
                "config sets mode.precheck=false, so the pre-backup checks are skipped",
                "config의 mode.precheck=false라 PostgreSQL 백업 전 점검을 건너뜁니다",
            )
        );
    }

    // 증분(--type incr)은 logical decoding 캡처 경로로 분기한다. pg_logical 미활성이면
    //   slot이 없어 캡처 불가하므로 명확히 안내하고 거부한다(선택적 백업과도 병용 불가).
    if matches!(backup_type, BackupType::Incr) {
        if !enable_incremental {
            return Err(XBackupError::Usage(crate::tr!("PostgreSQL incremental (--type incr) requires features.incremental.pg_logical=true (a full backup must have created the replication slot first). Turn it on and run a full backup once before using incremental.", "PG 증분(--type incr)은 features.incremental.pg_logical=true가 필요합니다 \
                 (풀 백업이 replication slot을 만든 상태여야 캡처 가능). 설정을 켜고 풀 백업을 \
                 한 번 수행한 뒤 증분을 사용하세요.")));
        }
        if args.db.is_some() || args.collection.is_some() {
            return Err(XBackupError::Usage(crate::tr!("PostgreSQL incremental (--type incr) cannot be combined with a selective backup (--db/--collection) — an incremental captures the entire change slice.", "PG 증분(--type incr)은 선택적 백업(--db/--collection)과 병용할 수 없습니다 \
                 — 증분은 전체 변경 슬라이스를 캡처합니다.")));
        }
        return handle_pg_incremental(
            resolved,
            args,
            uri,
            timeout_secs,
            primary,
            primary_dest,
            secondaries,
            dest_count,
            mode,
            lang,
        )
        .await;
    }

    let (stages, meta) = build_stages(resolved, args)?;
    let progress_counter = new_counter();
    let row_progress = crate::engine::progress::RowProgress::new();
    let reporter = ProgressReporter::start_with_detail(
        mode,
        ProgressKind::Indeterminate {
            label: lang.sel("PG backup", "PG 백업").into(),
        },
        std::sync::Arc::clone(&progress_counter),
        Some(row_progress_detail(
            Arc::clone(&row_progress),
            RowUnit::Tables,
            lang,
        )),
    );
    let started = Instant::now();
    let result = crate::pipeline::backup::run_pg_full_backup_with_rows(
        &uri,
        timeout_secs,
        args.db.clone(),
        args.collection.clone(),
        primary,
        stages,
        meta,
        Some(progress_counter),
        Some(row_progress),
        &resolved.profile_name,
        enable_incremental,
    )
    .await;
    let elapsed = started.elapsed();
    reporter.finish().await;
    let outcome = result?;

    let replicate_warning = replicate_and_warn(
        primary,
        secondaries,
        &outcome.backup_id,
        outcome.stored_size_bytes > 0,
    )
    .await;

    let delta = rows_delta_for(
        primary,
        &outcome,
        crate::engine::postgres::archive::FORMAT_ID,
        args,
    )
    .await;
    let delta_warning = delta
        .as_ref()
        .and_then(|d| rows_delta_warning(d, RowUnit::Tables, lang));
    let warning = merge_warnings(replicate_warning, delta_warning);

    if mode.emits_json() {
        BackupJsonSummary {
            backup_id: &outcome.backup_id,
            stored_size_bytes: outcome.stored_size_bytes,
            checksum_sha256: Some(&outcome.checksum_sha256),
            database: Some("postgresql"),
            destinations: Some(dest_count),
            rows_total: outcome.table_rows.as_ref().map(|rows| rows.values().sum()),
            table_rows: outcome.table_rows.as_ref(),
            elapsed_secs: Some(elapsed.as_secs_f64()),
            rows_vs_previous: delta.as_ref().map(RowsDelta::to_json),
            ..Default::default()
        }
        .emit();
    } else if mode.shows_human_summary() {
        print_backup_summary(
            &BackupSummary {
                kind: "full",
                detail: "PostgreSQL",
                backup_id: &outcome.backup_id,
                primary_dest,
                stored_size: outcome.stored_size_bytes,
                original_size: Some(outcome.original_size_bytes),
                compression: outcome.compression.as_ref(),
                encryption: outcome.encryption.as_ref(),
                checksum: Some(&outcome.checksum_sha256),
                dest_count,
                base_id: None,
                change: None,
                oplog_range: None,
                note: None,
            },
            lang,
        );
        if let Some(rows) = outcome.table_rows.as_ref() {
            print_rows_summary(
                rows,
                RowUnit::Tables,
                elapsed,
                &outcome.backup_id,
                delta.as_ref().map(|d| (d, args.warn_drop_percent)),
                lang,
            );
        }
    }

    if let Some(w) = warning {
        return Err(XBackupError::Warning(w));
    }
    Ok(())
}

/// MySQL 풀 백업 경로 — 드라이버(SHOW CREATE + SELECT) 아카이브를 압축·암호화·저장한다.
///
/// PG 경로의 storage/stage/summary 골격을 공유한다. 덤프가 스냅샷 시점 binlog 좌표를 manifest에
/// 기록한다(증분 base). 증분(`--type incr`)은 t7에서 binlog 캡처 파이프라인으로 배선한다.
#[allow(clippy::too_many_arguments)]
async fn handle_mysql_backup(
    resolved: &ResolvedConfig,
    args: &BackupArgs,
    uri: Secret,
    timeout_secs: Option<u64>,
    primary: &dyn Storage,
    primary_dest: &DestinationConfig,
    secondaries: &[DestinationConfig],
    dest_count: usize,
    mode: OutputMode,
    backup_type: BackupType,
    lang: Lang,
) -> Result<()> {
    // FR-8 자동 사전 점검 — 연결·대상 DB 점검(차단 결함이면 exit 3로 미시작).
    let should_precheck = resolved.profile.mode.precheck && !args.skip_precheck;
    if should_precheck {
        crate::engine::mysql::status::precheck(&uri, timeout_secs).await?;
    } else if args.skip_precheck {
        tracing::warn!(
            "{}",
            crate::tr!(
                "--skip-precheck given, so the pre-backup checks are skipped (FR-8 bypass)",
                "--skip-precheck가 있어 MySQL 백업 전 점검을 건너뜁니다(FR-8 우회)",
            )
        );
    }

    // 증분(--type incr)은 binlog 캡처 경로로 분기한다. mysql_binlog 미활성이면 명확히 거부.
    if matches!(backup_type, BackupType::Incr) {
        if !resolved.profile.features.incremental.mysql_binlog {
            return Err(XBackupError::Usage(crate::tr!("MySQL incremental (--type incr) requires features.incremental.mysql_binlog=true (the server needs log_bin=ON, binlog_format=ROW, binlog_row_image=FULL). Turn it on and run a full backup once before using incremental.", "MySQL 증분(--type incr)은 features.incremental.mysql_binlog=true가 필요합니다 \
                 (서버 log_bin=ON·binlog_format=ROW·binlog_row_image=FULL 전제). 설정을 켜고 \
                 풀 백업을 한 번 수행한 뒤 증분을 사용하세요.")));
        }
        if args.collection.is_some() {
            return Err(XBackupError::Usage(crate::tr!("MySQL incremental (--type incr) cannot be combined with a selective backup (--collection) — an incremental captures the entire change slice.", "MySQL 증분(--type incr)은 선택적 백업(--collection)과 병용할 수 없습니다 \
                 — 증분은 전체 변경 슬라이스를 캡처합니다.")));
        }
        return handle_mysql_incremental(
            resolved,
            args,
            uri,
            timeout_secs,
            primary,
            primary_dest,
            secondaries,
            dest_count,
            mode,
            lang,
        )
        .await;
    }

    let (stages, meta) = build_stages(resolved, args)?;
    let progress_counter = new_counter();
    let row_progress = crate::engine::progress::RowProgress::new();
    let reporter = ProgressReporter::start_with_detail(
        mode,
        ProgressKind::Indeterminate {
            label: lang.sel("MySQL backup", "MySQL 백업").into(),
        },
        std::sync::Arc::clone(&progress_counter),
        Some(row_progress_detail(
            Arc::clone(&row_progress),
            RowUnit::Tables,
            lang,
        )),
    );
    let started = Instant::now();
    let result = crate::pipeline::backup::run_mysql_full_backup_with_rows(
        &uri,
        timeout_secs,
        args.collection.clone(),
        primary,
        stages,
        meta,
        Some(progress_counter),
        Some(row_progress),
        /* promoted_from_gap */ false,
    )
    .await;
    let elapsed = started.elapsed();
    reporter.finish().await;
    let outcome = result?;

    let replicate_warning = replicate_and_warn(
        primary,
        secondaries,
        &outcome.backup_id,
        outcome.stored_size_bytes > 0,
    )
    .await;

    let delta = rows_delta_for(
        primary,
        &outcome,
        crate::engine::mysql::archive::FORMAT_ID,
        args,
    )
    .await;
    let delta_warning = delta
        .as_ref()
        .and_then(|d| rows_delta_warning(d, RowUnit::Tables, lang));
    let warning = merge_warnings(replicate_warning, delta_warning);

    if mode.emits_json() {
        BackupJsonSummary {
            backup_id: &outcome.backup_id,
            stored_size_bytes: outcome.stored_size_bytes,
            checksum_sha256: Some(&outcome.checksum_sha256),
            database: Some("mysql"),
            destinations: Some(dest_count),
            rows_total: outcome.table_rows.as_ref().map(|rows| rows.values().sum()),
            table_rows: outcome.table_rows.as_ref(),
            elapsed_secs: Some(elapsed.as_secs_f64()),
            rows_vs_previous: delta.as_ref().map(RowsDelta::to_json),
            ..Default::default()
        }
        .emit();
    } else if mode.shows_human_summary() {
        print_backup_summary(
            &BackupSummary {
                kind: "full",
                detail: "MySQL",
                backup_id: &outcome.backup_id,
                primary_dest,
                stored_size: outcome.stored_size_bytes,
                original_size: Some(outcome.original_size_bytes),
                compression: outcome.compression.as_ref(),
                encryption: outcome.encryption.as_ref(),
                checksum: Some(&outcome.checksum_sha256),
                dest_count,
                base_id: None,
                change: None,
                oplog_range: None,
                note: None,
            },
            lang,
        );
        if let Some(rows) = outcome.table_rows.as_ref() {
            print_rows_summary(
                rows,
                RowUnit::Tables,
                elapsed,
                &outcome.backup_id,
                delta.as_ref().map(|d| (d, args.warn_drop_percent)),
                lang,
            );
        }
    }

    if let Some(w) = warning {
        return Err(XBackupError::Warning(w));
    }
    Ok(())
}

/// MySQL 증분 백업 경로 — binlog ROW 변경을 캡처해 저장한다(gap이면 풀 승격).
#[allow(clippy::too_many_arguments)]
async fn handle_mysql_incremental(
    resolved: &ResolvedConfig,
    args: &BackupArgs,
    uri: Secret,
    timeout_secs: Option<u64>,
    primary: &dyn Storage,
    primary_dest: &DestinationConfig,
    secondaries: &[DestinationConfig],
    dest_count: usize,
    mode: OutputMode,
    lang: Lang,
) -> Result<()> {
    let should_precheck = resolved.profile.mode.precheck && !args.skip_precheck;
    if should_precheck {
        crate::engine::mysql::status::precheck(&uri, timeout_secs).await?;
    }

    let (stages, meta) = build_stages(resolved, args)?;
    let summary_meta = meta.clone();
    let progress_counter = new_counter();
    let reporter = ProgressReporter::start(
        mode,
        ProgressKind::Indeterminate {
            label: lang.sel("MySQL incremental", "MySQL 증분").into(),
        },
        std::sync::Arc::clone(&progress_counter),
    );
    let result = crate::pipeline::backup::run_mysql_incremental_backup(
        &uri,
        timeout_secs,
        &resolved.profile_name,
        primary,
        stages,
        meta,
        Some(progress_counter),
    )
    .await;
    reporter.finish().await;
    let outcome = result?;

    let replicate_warning = replicate_and_warn(
        primary,
        secondaries,
        &outcome.backup_id,
        outcome.stored_size_bytes > 0,
    )
    .await;

    if mode.emits_json() {
        BackupJsonSummary {
            backup_type: Some(if outcome.promoted {
                "full"
            } else {
                "incremental"
            }),
            backup_id: &outcome.backup_id,
            base_id: Some(&outcome.base_id),
            change_count: Some(outcome.change_count as u64),
            stored_size_bytes: outcome.stored_size_bytes,
            promoted_from_gap: Some(outcome.promoted),
            database: Some("mysql"),
            destinations: Some(dest_count),
            ..Default::default()
        }
        .emit();
    } else if mode.shows_human_summary() {
        let note = if outcome.promoted {
            Some(lang.sel(
                "(gap — promoted to full backup)",
                "(gap — 풀 백업으로 승격됨)",
            ))
        } else if outcome.change_count == 0 {
            Some(lang.sel(
                "(no changes — empty slice: manifest only)",
                "(변경 없음 — 빈 슬라이스: manifest만 기록)",
            ))
        } else {
            None
        };
        print_backup_summary(
            &BackupSummary {
                kind: if outcome.promoted { "full" } else { "incr" },
                detail: "MySQL",
                backup_id: &outcome.backup_id,
                primary_dest,
                stored_size: outcome.stored_size_bytes,
                original_size: None,
                compression: summary_meta.compression.as_ref(),
                encryption: summary_meta.encryption.as_ref(),
                checksum: None,
                dest_count,
                base_id: Some(&outcome.base_id),
                change: Some(("changes", outcome.change_count)),
                oplog_range: None,
                note,
            },
            lang,
        );
    }

    if let Some(w) = replicate_warning {
        return Err(XBackupError::Warning(w));
    }
    // gap으로 풀 승격됐으면 경고 동반 성공(exit 4) — 증분을 요청했으나 base가 끊겨 풀로 대체됨을
    // 운영/모니터링에 신호한다(PG promote_pg_incremental_to_full과 동일 의미).
    if outcome.promoted {
        return Err(XBackupError::Warning(
            lang.sel(
                "incremental promoted to full backup (gap — base binlog purged)",
                "증분이 풀 백업으로 승격됨(gap — base binlog가 purge됨)",
            )
            .to_string(),
        ));
    }
    Ok(())
}

/// PostgreSQL 증분 백업 경로 — logical decoding(pgoutput) slot에서 변경을 캡처해 저장한다.
///
/// 파이프라인 단계(compress→encrypt)는 풀 백업과 동일하게 [`build_stages`]로 만든다. 캡처
/// 결과(변경 수·저장 바이트)를 요약하고 보조 destination으로 복제한다(빈 슬라이스면
/// data.bin 없음 → has_data=false).
#[allow(clippy::too_many_arguments)]
async fn handle_pg_incremental(
    resolved: &ResolvedConfig,
    args: &BackupArgs,
    uri: Secret,
    timeout_secs: Option<u64>,
    primary: &dyn Storage,
    primary_dest: &DestinationConfig,
    secondaries: &[DestinationConfig],
    dest_count: usize,
    mode: OutputMode,
    lang: Lang,
) -> Result<()> {
    use crate::engine::postgres::{conn::PgClient, incremental};

    // gap 감지(FR-2): 캡처 전 replication slot 건강도를 본다. 슬롯이 없거나(Missing)
    //   invalidated(Lost)면 증분 체인이 끊긴 것이므로 **조용히 진행하지 않고** 풀 백업으로
    //   승격한다(exit 4). Mongo의 oplog gap → 풀 승격과 동형이다.
    let slot = incremental::slot_name(&resolved.profile_name);
    let health = {
        let admin = PgClient::connect(&uri, timeout_secs).await?;
        incremental::slot_health(admin.client(), &slot).await?
    };
    if health != incremental::SlotHealth::Active {
        return promote_pg_incremental_to_full(
            resolved,
            args,
            &uri,
            timeout_secs,
            primary,
            primary_dest,
            secondaries,
            dest_count,
            mode,
            health,
            lang,
        )
        .await;
    }

    let (stages, meta) = build_stages(resolved, args)?;
    // meta는 run으로 move되므로, 완료 요약(압축·암호화 표시)용 사본을 미리 둔다.
    let summary_meta = meta.clone();
    let progress_counter = new_counter();
    let reporter = ProgressReporter::start(
        mode,
        ProgressKind::Indeterminate {
            label: lang.sel("PG incremental", "PG 증분").into(),
        },
        std::sync::Arc::clone(&progress_counter),
    );
    let result = crate::pipeline::backup::run_pg_incremental_backup(
        &uri,
        timeout_secs,
        &resolved.profile_name,
        primary,
        stages,
        meta,
    )
    .await;
    reporter.finish().await;
    let outcome = result?;

    let replicate_warning = replicate_and_warn(
        primary,
        secondaries,
        &outcome.backup_id,
        outcome.stored_size_bytes > 0,
    )
    .await;

    if mode.emits_json() {
        BackupJsonSummary {
            backup_type: Some("incremental"),
            backup_id: &outcome.backup_id,
            base_id: Some(&outcome.base_id),
            change_count: Some(outcome.change_count as u64),
            stored_size_bytes: outcome.stored_size_bytes,
            database: Some("postgresql"),
            destinations: Some(dest_count),
            ..Default::default()
        }
        .emit();
    } else if mode.shows_human_summary() {
        print_backup_summary(
            &BackupSummary {
                kind: "incr",
                detail: "PostgreSQL",
                backup_id: &outcome.backup_id,
                primary_dest,
                stored_size: outcome.stored_size_bytes,
                original_size: None,
                compression: summary_meta.compression.as_ref(),
                encryption: summary_meta.encryption.as_ref(),
                checksum: None,
                dest_count,
                base_id: Some(&outcome.base_id),
                change: Some(("changes", outcome.change_count)),
                oplog_range: None,
                note: (outcome.change_count == 0).then_some(lang.sel(
                    "(no changes — empty slice: manifest only)",
                    "(변경 없음 — 빈 슬라이스: manifest만 기록)",
                )),
            },
            lang,
        );
    }

    if let Some(w) = replicate_warning {
        return Err(XBackupError::Warning(w));
    }
    Ok(())
}

/// PG 증분 슬롯이 끊겼을 때(gap) 풀 백업으로 **승격**한다(FR-2, exit 4 — 경고 동반 성공).
///
/// 풀 백업이 슬롯을 재생성하므로(스타 모델 base 재정렬), 이후 증분은 이 새 풀백업에 체인된다.
/// Mongo의 oplog gap → 풀 승격과 동일한 의미·exit code다(SC2).
#[allow(clippy::too_many_arguments)]
async fn promote_pg_incremental_to_full(
    resolved: &ResolvedConfig,
    args: &BackupArgs,
    uri: &Secret,
    timeout_secs: Option<u64>,
    primary: &dyn Storage,
    primary_dest: &DestinationConfig,
    secondaries: &[DestinationConfig],
    dest_count: usize,
    mode: OutputMode,
    health: crate::engine::postgres::incremental::SlotHealth,
    lang: Lang,
) -> Result<()> {
    use crate::engine::postgres::incremental::SlotHealth;
    // reason은 JSON/에러/로그에 그대로 쓰이므로(기계 판독·범위 밖) 영문화하지 않는다.
    let reason = match health {
        SlotHealth::Missing => "증분 슬롯이 없음(체인 끊김 — 첫 증분이거나 슬롯 유실)",
        SlotHealth::Lost => "증분 슬롯이 invalidated됨(WAL 제거로 체인 끊김)",
        SlotHealth::Active => "정상", // 도달 불가(호출자가 Active면 승격하지 않음).
    };
    // 사람용 요약(note)에 보일 설명 — 언어 토글.
    let reason_human = match health {
        SlotHealth::Missing => lang.sel(
            "incremental slot missing (chain broken — first incr or slot lost)",
            "증분 슬롯이 없음(체인 끊김 — 첫 증분이거나 슬롯 유실)",
        ),
        SlotHealth::Lost => lang.sel(
            "incremental slot invalidated (chain broken by WAL removal)",
            "증분 슬롯이 invalidated됨(WAL 제거로 체인 끊김)",
        ),
        SlotHealth::Active => lang.sel("ok", "정상"),
    };
    tracing::warn!(
        "{}",
        crate::tr!(
            "PostgreSQL incremental gap detected ({reason}) — promoting this run to a full backup",
            "PostgreSQL 증분에서 gap을 발견했습니다({reason}). 이번 실행을 풀 백업으로 올립니다",
        )
    );

    let (stages, meta) = build_stages(resolved, args)?;
    let progress_counter = new_counter();
    let reporter = ProgressReporter::start(
        mode,
        ProgressKind::Indeterminate {
            label: lang
                .sel("PG full backup (gap promotion)", "PG 풀 백업(gap 승격)")
                .into(),
        },
        std::sync::Arc::clone(&progress_counter),
    );
    // 승격 풀 백업은 슬롯을 재생성해 base를 재정렬한다(enable_incremental=true).
    let result = crate::pipeline::backup::run_pg_full_backup(
        uri,
        timeout_secs,
        None,
        None,
        primary,
        stages,
        meta,
        Some(progress_counter),
        &resolved.profile_name,
        /* enable_incremental */ true,
    )
    .await;
    reporter.finish().await;
    let outcome = result?;

    let replicate_warning =
        replicate_and_warn(primary, secondaries, &outcome.backup_id, true).await;

    if mode.emits_json() {
        BackupJsonSummary {
            backup_type: Some("full"),
            promoted_from_gap: Some(true),
            backup_id: &outcome.backup_id,
            stored_size_bytes: outcome.stored_size_bytes,
            checksum_sha256: Some(&outcome.checksum_sha256),
            database: Some("postgresql"),
            destinations: Some(dest_count),
            reason: Some(reason),
            ..Default::default()
        }
        .emit();
    } else if mode.shows_human_summary() {
        print_backup_summary(
            &BackupSummary {
                kind: lang.sel("full (gap promotion)", "full(gap 승격)"),
                detail: "PostgreSQL",
                backup_id: &outcome.backup_id,
                primary_dest,
                stored_size: outcome.stored_size_bytes,
                original_size: Some(outcome.original_size_bytes),
                compression: outcome.compression.as_ref(),
                encryption: outcome.encryption.as_ref(),
                checksum: Some(&outcome.checksum_sha256),
                dest_count,
                base_id: None,
                change: None,
                oplog_range: None,
                note: Some(reason_human),
            },
            lang,
        );
    }

    // exit 4(경고 동반 성공) — main이 Warning을 exit 4로 매핑한다.
    let mut msg = match lang {
        Lang::En => format!(
            "PostgreSQL incremental promoted to full backup ({}) due to a gap: {reason}",
            outcome.backup_id
        ),
        Lang::Ko => format!(
            "PG 증분이 gap으로 풀 백업({})으로 승격되었습니다: {reason}",
            outcome.backup_id
        ),
    };
    if let Some(w) = replicate_warning {
        msg.push_str(" / ");
        msg.push_str(&w);
    }
    Err(XBackupError::Warning(msg))
}

/// 보조 destination들로 백업 산출물을 순차 복제하고, 실패가 있으면 경고 메시지를 만든다.
///
/// "primary 필수 + 나머지 경고" 정책: primary는 이미 성공한 상태에서 호출된다. 각 보조
/// destination을 [`from_config`]로 구성해 [`replicate_artifact`]로 복제하되, 구성·복제
/// 실패는 치명적이지 않게 모아서 한 줄 경고로 반환한다(호출자가 exit 4로 보고). 모두
/// 성공하면 `None`.
async fn replicate_and_warn(
    primary: &dyn Storage,
    secondaries: &[DestinationConfig],
    backup_id: &str,
    has_data: bool,
) -> Option<String> {
    let mut failures = Vec::new();
    for (i, dest) in secondaries.iter().enumerate() {
        let label = dest.label(i + 1); // primary가 #0이므로 보조는 #1부터.
        match from_config(dest) {
            Ok(st) => match replicate_artifact(primary, st.as_ref(), backup_id, has_data).await {
                Ok(()) => tracing::info!(dest = %label, "보조 destination 복제 완료"),
                Err(e) => {
                    tracing::error!(dest = %label, "보조 destination 복제 실패: {e}");
                    failures.push(format!("{label}: {e}"));
                }
            },
            Err(e) => {
                tracing::error!(dest = %label, "보조 destination 구성 실패: {e}");
                failures.push(format!("{label}: {e}"));
            }
        }
    }
    if failures.is_empty() {
        None
    } else {
        Some(crate::tr!(
            "the primary backup succeeded, but replication to {} secondary destination(s) \
             failed: {}",
            "primary 백업은 성공했으나 보조 destination {}곳 복제 실패: {}",
            failures.len(),
            failures.join(" / ")
        ))
    }
}

/// 증분 백업 분기(`--type incr`) — 드라이버 oplog 캡처 → 저장 → manifest, gap 시 풀 승격.
///
/// 파이프라인 단계(compress→encrypt)는 풀 백업과 **동일하게** [`build_stages`]로 만든다.
/// 단, 증분은 캡처 스택 소비 후에도 (late gap 등) 풀 승격을 위해 스택을 새로 만들 수
/// 있어야 하므로 [`build_stages`]를 팩토리 클로저로 넘긴다([`run_incremental_backup`]).
///
/// gap·late gap·oplog-empty로 풀 백업으로 승격되면 **exit 4**(경고 동반 성공,
/// [`XBackupError::Warning`])로 보고한다(SC2). 정상 증분(빈 슬라이스 포함)은 exit 0.
#[allow(clippy::too_many_arguments)]
async fn handle_incremental(
    resolved: &ResolvedConfig,
    args: &BackupArgs,
    uri: &Secret,
    primary: &dyn Storage,
    primary_dest: &DestinationConfig,
    secondaries: &[DestinationConfig],
    mode: OutputMode,
    lang: Lang,
) -> Result<()> {
    let request = IncrementalRequest {
        uri: uri.clone(),
        mongodump_program: "mongodump".to_string(),
        timeout_secs: resolved.profile.source.connect_timeout_secs,
        engine: Engine::parse(&resolved.profile.mode.engine)?,
    };
    // 캡처/승격 양쪽에서 동일 구성의 새 StageStack을 만들 수 있도록 팩토리로 넘긴다.
    let stage_factory = || build_stages(resolved, args);
    // 완료 요약(압축·암호화 표시)용 메타 — 동일 구성을 한 번 더 만들어 메타만 취한다.
    let summary_meta = build_stages(resolved, args)?.1;

    let outcome = run_incremental_backup(&request, primary, stage_factory).await?;

    match outcome {
        IncrementalOutcome::Captured {
            backup_id,
            base_id,
            oplog_count,
            oplog_range,
            stored_size_bytes,
        } => {
            // 보조 destination 복제(빈 슬라이스면 data.bin 없음 → has_data=false).
            let replicate_warning =
                replicate_and_warn(primary, secondaries, &backup_id, stored_size_bytes > 0).await;
            if mode.emits_json() {
                BackupJsonSummary {
                    backup_type: Some("incremental"),
                    backup_id: &backup_id,
                    base_id: Some(&base_id),
                    oplog_count: Some(oplog_count),
                    stored_size_bytes,
                    ..Default::default()
                }
                .emit();
            } else if mode.shows_human_summary() {
                print_backup_summary(
                    &BackupSummary {
                        kind: "incr",
                        detail: "replica_set",
                        backup_id: &backup_id,
                        primary_dest,
                        stored_size: stored_size_bytes,
                        original_size: None,
                        compression: summary_meta.compression.as_ref(),
                        encryption: summary_meta.encryption.as_ref(),
                        checksum: None,
                        dest_count: secondaries.len() + 1,
                        base_id: Some(&base_id),
                        change: Some(("entries", oplog_count)),
                        oplog_range: Some(&oplog_range),
                        note: (oplog_count == 0).then_some(lang.sel(
                            "(no changes — empty slice: manifest only)",
                            "(변경 없음 — 빈 슬라이스: manifest만 기록)",
                        )),
                    },
                    lang,
                );
            }
            // 보조 복제 실패는 경고(exit 4) — 증분 캡처 자체는 성공.
            if let Some(w) = replicate_warning {
                return Err(XBackupError::Warning(w));
            }
            Ok(())
        }
        IncrementalOutcome::PromotedToFull { outcome, reason } => {
            // 승격으로 만들어진 풀 백업도 보조 destination으로 복제한다(풀이라 has_data=true).
            let replicate_warning =
                replicate_and_warn(primary, secondaries, &outcome.backup_id, true).await;
            // 승격은 데이터상 성공이지만 "증분이 아니라 풀이 됨"을 경고로 알린다(exit 4, SC2).
            if mode.emits_json() {
                BackupJsonSummary {
                    backup_type: Some("full"),
                    promoted_from_gap: Some(true),
                    backup_id: &outcome.backup_id,
                    stored_size_bytes: outcome.stored_size_bytes,
                    checksum_sha256: Some(&outcome.checksum_sha256),
                    reason: Some(&reason),
                    ..Default::default()
                }
                .emit();
            } else if mode.shows_human_summary() {
                let topo = match outcome.topology {
                    Topology::ReplicaSet => "replica_set",
                    Topology::Standalone => "standalone",
                };
                print_backup_summary(
                    &BackupSummary {
                        kind: lang.sel("full (gap promotion)", "full(gap 승격)"),
                        detail: topo,
                        backup_id: &outcome.backup_id,
                        primary_dest,
                        stored_size: outcome.stored_size_bytes,
                        original_size: Some(outcome.original_size_bytes),
                        compression: outcome.compression.as_ref(),
                        encryption: outcome.encryption.as_ref(),
                        checksum: Some(&outcome.checksum_sha256),
                        dest_count: secondaries.len() + 1,
                        base_id: None,
                        change: None,
                        oplog_range: outcome.oplog_range.as_ref(),
                        note: Some(&reason),
                    },
                    lang,
                );
            }
            // exit 4(경고 동반 성공) — main이 Warning을 exit 4로 매핑한다.
            // 승격 경고에 보조 복제 실패가 있으면 함께 알린다(둘 다 exit 4).
            let mut msg = match lang {
                Lang::En => format!(
                    "incremental promoted to full backup ({}) due to a gap: {reason}",
                    outcome.backup_id
                ),
                Lang::Ko => format!(
                    "증분이 gap으로 풀 백업({})으로 승격되었습니다: {reason}",
                    outcome.backup_id
                ),
            };
            if let Some(w) = replicate_warning {
                msg.push_str(" / ");
                msg.push_str(&w);
            }
            Err(XBackupError::Warning(msg))
        }
    }
}

/// CLI `--type`이 있으면 그것을, 없으면 config `mode.backup_type`을 적용한다.
fn effective_backup_type(resolved: &ResolvedConfig, args: &BackupArgs) -> Result<BackupType> {
    if let Some(kind) = args.backup_type {
        return Ok(kind);
    }
    match resolved.profile.mode.backup_type.as_str() {
        "full" => Ok(BackupType::Full),
        "incr" => Ok(BackupType::Incr),
        other => Err(XBackupError::Config(crate::tr!(
            "unknown mode.backup_type: '{other}' (only full | incr are supported)",
            "알 수 없는 mode.backup_type: '{other}'(full | incr만 지원)"
        ))),
    }
}

/// backup 자동 사전 점검 — status 핵심 서브셋(연결·권한·토폴로지·도구 존재)을 실행한다.
///
/// 전체 `status`보다 가벼운 [`StatusChecker::precheck_subset`]로 백업을 *막는* 결함만 본다.
/// 보고서 신호등이 `Fail`이면 [`XBackupError::PrecheckFailed`](exit 3)로 백업을 미시작한다.
/// `Warn`은 백업을 막지 않는다(로그만; 전체 status가 경고를 상세히 다룬다). 읽기 전용이다.
async fn run_precheck(
    uri: &Secret,
    profile: &str,
    timeout_secs: Option<u64>,
    engine: Engine,
) -> Result<()> {
    let checker = StatusChecker::connect(uri, timeout_secs)
        .await
        .map_err(|e| {
            // connect 준비 실패(URI 파싱 등)는 사전 점검 실패로 본다(백업 미시작).
            XBackupError::PrecheckFailed(crate::tr!(
                "failed to prepare the precheck connection: {e}",
                "사전 점검 연결 준비 실패: {e}"
            ))
        })?;
    // 네이티브 엔진은 외부 도구 불필요 — mongodump 존재 점검을 생략한다.
    let tool = match engine {
        Engine::Mongodump => Some("mongodump"),
        Engine::Native => None,
    };
    let report = checker.precheck_subset(profile, tool).await;

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
        return Err(XBackupError::PrecheckFailed(crate::tr!(
            "backup precheck failed (bypass with --skip-precheck) — {}",
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
/// - **압축**: config `features.compression.algorithm`(zstd/gzip)에 맞는 압축 단계를 push한다. `none`이나 `--no-compress`면 생략.
///   레벨은 CLI `--compress-level` > config `level` 우선. manifest.compression 기록.
/// - **암호화**: 기본 ON(PRD §FR-5 — 평문은 명시적 `--no-encrypt`만). `--no-encrypt`면
///   경고 로그 후 암호화 단계를 생략한다. config `features.encryption.algorithm`에 따라
///   age(recipient_file) 또는 aes-256-gcm(env 키)으로 단계를 만든다. manifest.encryption 기록.
fn build_stages(resolved: &ResolvedConfig, args: &BackupArgs) -> Result<(StageStack, BackupMeta)> {
    let features = &resolved.profile.features;
    let mut stack = StageStack::new();
    let mut meta = BackupMeta::none();

    // ── 압축(compress 먼저) ──
    // 레벨은 CLI 우선. `none`이면 manifest에 압축 메타를 남기지 않아 복구가 해제를 건너뛴다.
    let comp = &features.compression;
    let algorithm = if args.no_compress {
        ALGORITHM_NONE
    } else {
        comp.algorithm.as_str()
    };
    let level = args.compress_level.unwrap_or(comp.level);
    if let Some((stage, applied_level)) = compress_stage(algorithm, level)? {
        // manifest에는 실제 적용된(클램프 후) 레벨을 기록한다.
        meta.compression = Some(CompressionMeta {
            algorithm: algorithm.to_string(),
            level: applied_level,
        });
        stack.push(stage);
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

    #[test]
    fn printable_replaces_control_characters_in_table_names() {
        assert_eq!(printable("orders"), "orders");
        assert_eq!(printable("a\x1b[2Jb\n"), "a?[2Jb?");
    }
    use crate::cli::args::BackupArgs;
    use crate::config::file::Profile;
    use crate::config::merged::ResolvedConfig;

    #[test]
    fn progress_detail_uses_the_unit_label_and_counts_for_each_engine() {
        let progress = crate::engine::progress::RowProgress::new();
        // 사전 조회 전(총량 0)에는 보여 줄 텍스트가 없고 JSON 필드만 0으로 나간다.
        let detail = row_progress_detail(Arc::clone(&progress), RowUnit::Collections, Lang::Ko);
        assert_eq!(detail().text, "");
        assert_eq!(
            detail().fields,
            vec![("rows", 0), ("tables_done", 0), ("tables_total", 0)]
        );

        progress.set_total(3);
        progress.start_table("shop.orders");
        progress.add_rows(1_500);
        assert_eq!(detail().text, "shop.orders · 0/3 컬렉션 · 1,500행");
        let en = row_progress_detail(Arc::clone(&progress), RowUnit::Tables, Lang::En);
        assert_eq!(en().text, "shop.orders · 0/3 tables · 1,500 rows");
        assert_eq!(
            detail().fields,
            vec![("rows", 1_500), ("tables_done", 0), ("tables_total", 3)]
        );
    }

    fn delta_with(vanished: &[&str], dropped: usize) -> RowsDelta {
        RowsDelta {
            previous_id: "01a17ff8-d9b8".into(),
            previous_total: 1_000,
            total: 1_204,
            previous_stored_size: 1_000,
            stored_size: 980,
            vanished: vanished.iter().map(|s| s.to_string()).collect(),
            dropped: (0..dropped)
                .map(|i| DroppedTable {
                    table: format!("t{i}"),
                    previous: 100,
                    now: 10,
                })
                .collect(),
        }
    }

    #[test]
    fn limited_names_shows_five_and_counts_the_rest() {
        let names = ["a", "b", "c", "d", "e", "f", "g"];
        assert_eq!(
            limited_names(names.iter().copied(), Lang::Ko),
            "a, b, c, d, e (외 2개)"
        );
        assert_eq!(
            limited_names(names.iter().copied(), Lang::En),
            "a, b, c, d, e (+2 more)"
        );
        assert_eq!(limited_names(["x", "y"].into_iter(), Lang::Ko), "x, y");
    }

    #[test]
    fn rows_delta_warning_exists_only_when_something_vanished_or_dropped() {
        assert_eq!(
            rows_delta_warning(&delta_with(&[], 0), RowUnit::Tables, Lang::Ko),
            None
        );
        let text = rows_delta_warning(&delta_with(&["gone"], 2), RowUnit::Collections, Lang::Ko)
            .expect("경고");
        assert!(
            text.contains("컬렉션 1개") && text.contains("컬렉션 2개"),
            "{text}"
        );
    }

    #[test]
    fn merge_warnings_joins_both_and_passes_a_single_one_through() {
        assert_eq!(merge_warnings(None, None), None);
        assert_eq!(merge_warnings(Some("a".into()), None), Some("a".into()));
        assert_eq!(merge_warnings(None, Some("b".into())), Some("b".into()));
        assert_eq!(
            merge_warnings(Some("a".into()), Some("b".into())),
            Some("a; b".into())
        );
    }

    #[test]
    fn signed_percent_always_shows_the_sign() {
        assert_eq!(signed_percent(0.94), "+0.9%");
        assert_eq!(signed_percent(-61.0), "-61.0%");
        assert_eq!(signed_percent(0.0), "+0.0%");
    }

    #[test]
    fn rows_vs_previous_is_omitted_from_json_when_absent() {
        let plain = serde_json::to_string(&BackupJsonSummary::default()).unwrap();
        assert!(!plain.contains("rows_vs_previous"));
        let with = serde_json::to_string(&BackupJsonSummary {
            rows_vs_previous: Some(delta_with(&["gone"], 0).to_json()),
            ..Default::default()
        })
        .unwrap();
        assert!(
            with.contains(r#""rows_vs_previous":{"#) && with.contains(r#""vanished":["gone"]"#)
        );
    }

    #[test]
    fn group_digits_inserts_commas_every_three_digits() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(1_000), "1,000");
        assert_eq!(group_digits(133_277), "133,277");
        assert_eq!(group_digits(1_234_567_890), "1,234,567,890");
    }

    #[test]
    fn format_elapsed_switches_to_hours_at_one_hour() {
        assert_eq!(format_elapsed(Duration::from_secs(38)), "00:38");
        assert_eq!(format_elapsed(Duration::from_secs(61)), "01:01");
        assert_eq!(format_elapsed(Duration::from_secs(3600)), "1:00:00");
        assert_eq!(format_elapsed(Duration::from_secs(3725)), "1:02:05");
    }

    #[test]
    fn summarize_rows_counts_empty_tables_and_ranks_the_largest() {
        let rows = BTreeMap::from([
            ("a_small".to_string(), 5u64),
            ("b_big".to_string(), 900),
            ("c_empty".to_string(), 0),
            ("d_mid".to_string(), 40),
            ("e_tie".to_string(), 40),
            ("f_empty".to_string(), 0),
        ]);
        let sum = summarize_rows(&rows);
        assert_eq!(sum.total, 985);
        assert_eq!(sum.tables, 6);
        assert_eq!(sum.empty, 2);
        assert_eq!(
            sum.largest,
            vec![
                ("b_big".to_string(), 900),
                ("d_mid".to_string(), 40),
                ("e_tie".to_string(), 40),
            ],
            "행이 같으면 이름순, 빈 테이블은 제외"
        );
    }

    #[test]
    fn summarize_rows_of_only_empty_tables_has_no_largest() {
        let rows = BTreeMap::from([("t".to_string(), 0u64)]);
        let sum = summarize_rows(&rows);
        assert_eq!((sum.total, sum.empty), (0, 1));
        assert!(sum.largest.is_empty());
    }

    #[test]
    fn rows_fields_are_omitted_from_json_when_absent_and_present_when_set() {
        let plain = serde_json::to_string(&BackupJsonSummary::default()).unwrap();
        assert!(!plain.contains("rows_total") && !plain.contains("table_rows"));
        assert!(!plain.contains("elapsed_secs"));

        let rows = BTreeMap::from([("users".to_string(), 5u64)]);
        let with_rows = serde_json::to_string(&BackupJsonSummary {
            rows_total: Some(5),
            table_rows: Some(&rows),
            elapsed_secs: Some(1.5),
            ..Default::default()
        })
        .unwrap();
        assert!(with_rows.contains(r#""rows_total":5"#));
        assert!(with_rows.contains(r#""table_rows":{"users":5}"#));
        assert!(with_rows.contains(r#""elapsed_secs":1.5"#));
    }

    /// 요약을 직렬화해 키 목록을 정렬해 돌려준다(값이 아니라 계약만 본다).
    fn keys_of(s: &BackupJsonSummary<'_>) -> Vec<String> {
        let v: serde_json::Value = serde_json::to_value(s).unwrap();
        let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    }

    /// None 필드는 키 자체가 생략된다 — 리터럴 시절과 키 집합을 같게 유지하는 근거다.
    /// 없는 정보를 `null`로 채우면 소비자가 "값이 null인 필드가 있다"로 오독한다.
    #[test]
    fn json_omits_absent_fields_instead_of_nulling() {
        let s = BackupJsonSummary {
            backup_id: "b1",
            stored_size_bytes: 10,
            ..Default::default()
        };
        assert_eq!(
            keys_of(&s),
            vec!["backup_id", "schema", "stored_size_bytes"],
            "설정하지 않은 필드는 출력에 나타나지 않아야 한다"
        );
    }

    /// MongoDB 풀 백업 변형의 키 집합(리터럴 시절 필드 + schema).
    #[test]
    fn json_mongo_full_keys() {
        let s = BackupJsonSummary {
            backup_id: "b1",
            stored_size_bytes: 10,
            checksum_sha256: Some("abc"),
            topology: Some("ReplicaSet".to_string()),
            destinations: Some(2),
            ..Default::default()
        };
        assert_eq!(
            keys_of(&s),
            vec![
                "backup_id",
                "checksum_sha256",
                "destinations",
                "schema",
                "stored_size_bytes",
                "topology",
            ]
        );
    }

    /// PostgreSQL 증분 변형 — base_id·change_count가 붙고 checksum·topology는 빠진다.
    #[test]
    fn json_pg_incremental_keys() {
        let s = BackupJsonSummary {
            backup_type: Some("incremental"),
            backup_id: "b2",
            base_id: Some("b1"),
            change_count: Some(7),
            stored_size_bytes: 20,
            database: Some("postgresql"),
            destinations: Some(1),
            ..Default::default()
        };
        assert_eq!(
            keys_of(&s),
            vec![
                "backup_id",
                "backup_type",
                "base_id",
                "change_count",
                "database",
                "destinations",
                "schema",
                "stored_size_bytes",
            ]
        );
    }

    /// MongoDB 증분은 change_count가 아니라 oplog_count를 쓴다 — 둘을 섞으면 소비자가 깨진다.
    #[test]
    fn json_mongo_incremental_uses_oplog_count() {
        let s = BackupJsonSummary {
            backup_type: Some("incremental"),
            backup_id: "b2",
            base_id: Some("b1"),
            oplog_count: Some(42),
            stored_size_bytes: 20,
            ..Default::default()
        };
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["oplog_count"], 42);
        assert!(
            v.get("change_count").is_none(),
            "MongoDB 경로에 change_count가 새어 나오면 안 된다"
        );
        assert!(
            v.get("destinations").is_none(),
            "이 경로는 개수를 내지 않는다"
        );
    }

    /// gap 승격(exit 4) 변형은 promoted_from_gap과 reason을 함께 낸다.
    #[test]
    fn json_gap_promotion_keys() {
        let s = BackupJsonSummary {
            backup_type: Some("full"),
            promoted_from_gap: Some(true),
            backup_id: "b3",
            stored_size_bytes: 30,
            checksum_sha256: Some("def"),
            reason: Some("oplog gap"),
            ..Default::default()
        };
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["promoted_from_gap"], true);
        assert_eq!(v["reason"], "oplog gap");
        assert_eq!(v["backup_type"], "full");
    }

    /// 스키마 버전은 항상 실린다 — 소비자가 파서를 고르는 근거다.
    #[test]
    fn json_always_carries_schema_version() {
        let s = BackupJsonSummary::default();
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["schema"], BACKUP_JSON_SCHEMA);
    }

    /// 테스트용 ResolvedConfig — 주어진 profile로 구성(URI는 비워 둠).
    fn resolved_with(profile: Profile) -> ResolvedConfig {
        ResolvedConfig {
            profile_name: "test".to_string(),
            profile,
            resolved_uri: None,
            resolved_read_uri: None,
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
            no_compress: false,
            quiet: false,
            progress: false,
            json: false,
            skip_precheck: false,
            no_hooks: false,
            read_source: None,
            warn_drop_percent: crate::pipeline::rows_delta::DEFAULT_DROP_WARN_PERCENT,
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
        profile.features.encryption.recipient_file = Some(pub_path.to_str().unwrap().to_string());

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

    #[test]
    fn gzip_algorithm_is_recorded_with_its_clamped_level() {
        let mut profile = Profile::default();
        profile.features.compression.algorithm = "gzip".into();
        profile.features.compression.level = 10;
        let mut args = default_args();
        args.no_encrypt = true;
        let (_stack, meta) = build_stages(&resolved_with(profile), &args).unwrap();
        let comp = meta.compression.unwrap();
        assert_eq!((comp.algorithm.as_str(), comp.level), ("gzip", 9));
    }

    #[test]
    fn none_or_no_compress_leaves_no_compression_meta() {
        let mut profile = Profile::default();
        profile.features.compression.algorithm = "none".into();
        let mut args = default_args();
        args.no_encrypt = true;
        let (_stack, meta) = build_stages(&resolved_with(profile), &args).unwrap();
        assert!(meta.compression.is_none());

        let mut args = default_args();
        args.no_encrypt = true;
        args.no_compress = true;
        let (_stack, meta) = build_stages(&resolved_with(Profile::default()), &args).unwrap();
        assert!(meta.compression.is_none());
    }

    #[test]
    fn unknown_compression_algorithm_is_config_error() {
        let mut profile = Profile::default();
        profile.features.compression.algorithm = "lz4".into();
        let mut args = default_args();
        args.no_encrypt = true;
        assert!(matches!(
            build_stages(&resolved_with(profile), &args),
            Err(XBackupError::Config(_))
        ));
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

    /// F1 회귀: 훅 시크릿 스크럽 목록이 uri_env + read_uri_env + s3 credentials_env를 모두 포함.
    #[test]
    fn collect_secret_env_includes_read_uri_env() {
        let mut profile = Profile::default();
        profile.source.uri_env = Some("MAIN_URI".to_string());
        profile.source.read_uri_env = Some("REPLICA_URI".to_string());
        profile.destination.r#type = Some("s3".to_string());
        profile.destination.s3 = Some(crate::config::file::S3Config {
            credentials_env: Some("S3_CREDS".to_string()),
            ..Default::default()
        });
        let names = collect_secret_env_names(&profile);
        assert!(names.contains(&"MAIN_URI".to_string()));
        assert!(
            names.contains(&"REPLICA_URI".to_string()),
            "read_uri_env(복제본 자격증명)가 스크럽 목록에 있어야 함(NFR-2): {names:?}"
        );
        assert!(names.contains(&"S3_CREDS".to_string()));
    }

    /// select_read_uri: --read-source(CLI)가 config read_uri보다 우선한다.
    #[test]
    fn read_source_cli_beats_config() {
        let profile = Profile::default();
        let mut resolved = resolved_with(profile);
        resolved.resolved_read_uri = Some(Secret::new("mongodb://replica/db".to_string()));
        resolved.resolved_uri = Some(Secret::new("mongodb://primary/db".to_string()));
        let uri = select_read_uri(Some("mongodb://cli/db"), &resolved).unwrap();
        assert_eq!(uri.expose(), "mongodb://cli/db");
    }

    /// select_read_uri: CLI 없으면 config read_uri(복제본)를 쓴다.
    #[test]
    fn read_source_uses_config_read_uri() {
        let mut resolved = resolved_with(Profile::default());
        resolved.resolved_read_uri = Some(Secret::new("mongodb://replica/db".to_string()));
        resolved.resolved_uri = Some(Secret::new("mongodb://primary/db".to_string()));
        assert_eq!(
            select_read_uri(None, &resolved).unwrap().expose(),
            "mongodb://replica/db"
        );
    }

    /// select_read_uri: read 소스가 없으면 주 소스로 폴백한다.
    #[test]
    fn read_source_falls_back_to_primary() {
        let mut resolved = resolved_with(Profile::default());
        resolved.resolved_uri = Some(Secret::new("mongodb://primary/db".to_string()));
        assert_eq!(
            select_read_uri(None, &resolved).unwrap().expose(),
            "mongodb://primary/db"
        );
    }

    /// select_read_uri: 아무 소스도 없으면 Config 에러(exit 2).
    #[test]
    fn read_source_none_is_config_error() {
        let resolved = resolved_with(Profile::default());
        let err = select_read_uri(None, &resolved).unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    /// post_hook_event: 성공(0)·경고 동반 성공(4)은 post_backup, 실패는 on_error.
    #[test]
    fn post_hook_event_maps_result_to_event() {
        assert_eq!(post_hook_event(&Ok(())), HookEvent::PostBackup);
        assert_eq!(
            post_hook_event(&Err(XBackupError::Warning(crate::tr!(
                "gap promotion",
                "gap 승격"
            )))),
            HookEvent::PostBackup
        );
        assert_eq!(
            post_hook_event(&Err(XBackupError::Failure(crate::tr!(
                "upload failed",
                "업로드 실패"
            )))),
            HookEvent::OnError
        );
    }
}
