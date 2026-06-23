//! `backup` 서브커맨드 핸들러 — config 로드 → 파이프라인 실행 → 요약 출력.
//!
//! 풀/증분 백업(Mongo oplog · PostgreSQL logical decoding), 로컬/S3 destination + 멀티
//! destination 복제, 압축·암호화, 자동 사전 점검(status 선행)을 지원한다. DB 종류는 source
//! URI 스킴으로 분기한다([`crate::engine::DbKind`] — Mongo는 아래 본 경로, PG는
//! [`handle_pg_backup`]).

use std::path::PathBuf;

use crate::cli::args::{BackupArgs, BackupType};
use crate::cli::output::{field_line, field_line_toned, style, OutputFlags, OutputMode, Tone};
use crate::cli::progress::{new_counter, ProgressKind, ProgressReporter};
use crate::compress::{ZstdCompressStage, ALGORITHM_ZSTD};
use crate::config::env::collect_overrides_from_process;
use crate::config::file::DestinationConfig;
use crate::config::merged::MergeInput;
use crate::config::secret::Secret;
use crate::config::ResolvedConfig;
use crate::crypto::build_encrypt_stage;
use crate::engine::mongo::status::{human_bytes, CheckStatus, StatusChecker};
use crate::error::{Result, XBackupError};
use crate::i18n::Lang;
use crate::manifest::schema::{CompressionMeta, EncryptionMeta, OplogRange, Topology};
use crate::pipeline::backup::{run_full_backup_with_meta, BackupMeta, BackupRequest, Engine};
use crate::pipeline::incremental::{
    run_incremental_backup, IncrementalOutcome, IncrementalRequest,
};
use crate::pipeline::stage::{StageStack, ENV_AES_KEY_HEX};
use crate::storage::{from_config, replicate_artifact, Storage};

/// `backup` 핸들러 진입점.
pub async fn handle(
    config_path: Option<PathBuf>,
    lang_flag: Option<crate::i18n::Lang>,
    args: BackupArgs,
) -> Result<()> {
    // 동시 실행 잠금(FR-12) — 같은 프로파일의 backup/restore/prune과 직렬화한다.
    // 가드(_lock)를 함수 끝까지 유지해 작업 동안 lock을 잡는다(충돌 시 exit 5).
    let _lock = crate::lock::acquire(&args.profile)?;

    // 1) config 로드 + 레이어 병합(file + ENV; CLI는 아래에서 직접 반영).
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

    // 2) URI 시크릿(uri_env로 해석된 값) 확보. 이후 build_stages가 &resolved를 쓰므로
    //    clone으로 꺼내 부분 이동을 피한다(Secret은 Clone).
    let uri = resolved.resolved_uri.clone().ok_or_else(|| {
        XBackupError::Config(format!(
            "프로파일 '{}'에 source.uri_env가 없거나 해석되지 않았습니다",
            resolved.profile_name
        ))
    })?;
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
    let secondaries = &dests[1..];

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
        tracing::warn!("--skip-precheck 지정 — 백업 사전 점검을 건너뜁니다(FR-8 우회)");
    } else {
        tracing::warn!("config mode.precheck=false — 백업 사전 점검을 건너뜁니다");
    }

    // 증분(--type incr)은 드라이버 oplog 캡처 경로로 분기한다(t8). 선택적 백업
    //   (--db/--collection)과는 병용 불가(증분은 항상 전체 oplog 슬라이스).
    if matches!(backup_type, BackupType::Incr) {
        if args.db.is_some() || args.collection.is_some() {
            return Err(XBackupError::Usage(
                "증분 백업(--type incr)은 선택적 백업(--db/--collection)과 병용할 수 없습니다 \
                 — 증분은 전체 oplog 슬라이스를 캡처합니다(FR-1/FR-2)."
                    .into(),
            ));
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

    let reporter = ProgressReporter::start(
        mode,
        ProgressKind::Indeterminate {
            label: lang.sel("backup", "백업").into(),
        },
        progress_counter,
    );
    let result = run_full_backup_with_meta(&request, primary.as_ref(), stages, meta).await;
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

    // 5) 요약 출력(stdout — 결과 전용). 진행은 stderr, 결과/--json은 stdout으로 분리.
    if mode.emits_json() {
        let summary = serde_json::json!({
            "backup_id": outcome.backup_id,
            "stored_size_bytes": outcome.stored_size_bytes,
            "checksum_sha256": outcome.checksum_sha256,
            "topology": format!("{:?}", outcome.topology),
            "destinations": dests.len(),
        });
        println!("{summary}");
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
    }

    // 보조 복제 실패가 있으면 경고로 마감(exit 4) — primary 백업은 이미 성공.
    if let Some(w) = replicate_warning {
        return Err(XBackupError::Warning(w));
    }
    Ok(())
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
        tracing::warn!("--skip-precheck 지정 — PG 백업 사전 점검을 건너뜁니다(FR-8 우회)");
    } else {
        tracing::warn!("config mode.precheck=false — PG 백업 사전 점검을 건너뜁니다");
    }

    // 증분(--type incr)은 logical decoding 캡처 경로로 분기한다. pg_logical 미활성이면
    //   slot이 없어 캡처 불가하므로 명확히 안내하고 거부한다(선택적 백업과도 병용 불가).
    if matches!(backup_type, BackupType::Incr) {
        if !enable_incremental {
            return Err(XBackupError::Usage(
                "PG 증분(--type incr)은 features.incremental.pg_logical=true가 필요합니다 \
                 (풀 백업이 replication slot을 만든 상태여야 캡처 가능). 설정을 켜고 풀 백업을 \
                 한 번 수행한 뒤 증분을 사용하세요."
                    .into(),
            ));
        }
        if args.db.is_some() || args.collection.is_some() {
            return Err(XBackupError::Usage(
                "PG 증분(--type incr)은 선택적 백업(--db/--collection)과 병용할 수 없습니다 \
                 — 증분은 전체 변경 슬라이스를 캡처합니다."
                    .into(),
            ));
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
    let reporter = ProgressReporter::start(
        mode,
        ProgressKind::Indeterminate {
            label: lang.sel("PG backup", "PG 백업").into(),
        },
        std::sync::Arc::clone(&progress_counter),
    );
    let result = crate::pipeline::backup::run_pg_full_backup(
        &uri,
        timeout_secs,
        args.db.clone(),
        args.collection.clone(),
        primary,
        stages,
        meta,
        Some(progress_counter),
        &resolved.profile_name,
        enable_incremental,
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
            "database": "postgresql",
            "destinations": dest_count,
        });
        println!("{summary}");
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
    }

    if let Some(w) = replicate_warning {
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
        tracing::warn!("--skip-precheck 지정 — MySQL 백업 사전 점검을 건너뜁니다(FR-8 우회)");
    }

    // 증분(--type incr)은 binlog 캡처 경로로 분기한다. mysql_binlog 미활성이면 명확히 거부.
    if matches!(backup_type, BackupType::Incr) {
        if !resolved.profile.features.incremental.mysql_binlog {
            return Err(XBackupError::Usage(
                "MySQL 증분(--type incr)은 features.incremental.mysql_binlog=true가 필요합니다 \
                 (서버 log_bin=ON·binlog_format=ROW·binlog_row_image=FULL 전제). 설정을 켜고 \
                 풀 백업을 한 번 수행한 뒤 증분을 사용하세요."
                    .into(),
            ));
        }
        if args.collection.is_some() {
            return Err(XBackupError::Usage(
                "MySQL 증분(--type incr)은 선택적 백업(--collection)과 병용할 수 없습니다 \
                 — 증분은 전체 변경 슬라이스를 캡처합니다."
                    .into(),
            ));
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
    let reporter = ProgressReporter::start(
        mode,
        ProgressKind::Indeterminate {
            label: lang.sel("MySQL backup", "MySQL 백업").into(),
        },
        std::sync::Arc::clone(&progress_counter),
    );
    let result = crate::pipeline::backup::run_mysql_full_backup(
        &uri,
        timeout_secs,
        args.collection.clone(),
        primary,
        stages,
        meta,
        Some(progress_counter),
        /* promoted_from_gap */ false,
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
            "database": "mysql",
            "destinations": dest_count,
        });
        println!("{summary}");
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
    }

    if let Some(w) = replicate_warning {
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
        let summary = serde_json::json!({
            "backup_type": if outcome.promoted { "full" } else { "incremental" },
            "backup_id": outcome.backup_id,
            "base_id": outcome.base_id,
            "change_count": outcome.change_count,
            "stored_size_bytes": outcome.stored_size_bytes,
            "promoted_from_gap": outcome.promoted,
            "database": "mysql",
            "destinations": dest_count,
        });
        println!("{summary}");
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
        let summary = serde_json::json!({
            "backup_type": "incremental",
            "backup_id": outcome.backup_id,
            "base_id": outcome.base_id,
            "change_count": outcome.change_count,
            "stored_size_bytes": outcome.stored_size_bytes,
            "database": "postgresql",
            "destinations": dest_count,
        });
        println!("{summary}");
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
    tracing::warn!("PG 증분 gap 감지({reason}) — 풀 백업으로 승격합니다");

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
        let summary = serde_json::json!({
            "backup_type": "full",
            "promoted_from_gap": true,
            "backup_id": outcome.backup_id,
            "stored_size_bytes": outcome.stored_size_bytes,
            "checksum_sha256": outcome.checksum_sha256,
            "database": "postgresql",
            "destinations": dest_count,
            "reason": reason,
        });
        println!("{summary}");
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
    let mut msg = format!(
        "PG 증분이 gap으로 풀 백업({})으로 승격되었습니다: {reason}",
        outcome.backup_id
    );
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
        Some(format!(
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
                let summary = serde_json::json!({
                    "backup_type": "incremental",
                    "backup_id": backup_id,
                    "base_id": base_id,
                    "oplog_count": oplog_count,
                    "stored_size_bytes": stored_size_bytes,
                });
                println!("{summary}");
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
            let mut msg = format!(
                "증분이 gap으로 풀 백업({})으로 승격되었습니다: {reason}",
                outcome.backup_id
            );
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
        other => Err(XBackupError::Config(format!(
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
            XBackupError::PrecheckFailed(format!("사전 점검 연결 준비 실패: {e}"))
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
fn build_stages(resolved: &ResolvedConfig, args: &BackupArgs) -> Result<(StageStack, BackupMeta)> {
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
