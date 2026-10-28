//! `verify` 서브커맨드 핸들러 — 구조/심층/체인 무결성 검증(PRD §FR-7, §8.5).
//!
//! destination은 local/s3 모두 동작한다(동일 [`Storage`] trait). verify는 저장 바이트만
//! 보므로 DB 종류(Mongo/PG)와 무관하다(체크섬·디코드 검증).
//!
//! ## 키 격리(§8.5)
//! 기본 verify(구조)는 **키 없이** 동작한다 — 체크섬은 저장 바이트 기준이라 복호화가
//! 필요 없다. `--deep`만 개인키(env)를 요구하며, 키 부재 시 명확히 거부하고 개인키 보유
//! 호스트에서 실행하라고 안내한다.
//!
//! ## 종료 코드(PRD §9)
//! - 0: 모든 검증 통과(무경고).
//! - 1: 변조·체크섬 불일치·deep 디코드 실패·**키 부재**(§8.5: 키 없이는 deep 불가 → 실패).
//! - 4: incomplete 등 경고 동반 성공, broken chain 감지.

use std::path::PathBuf;

use crate::cli::args::VerifyArgs;
use std::collections::BTreeMap;

use crate::cli::output::{field_line, field_line_toned, status_token, style, Tone};
use crate::config::env::collect_overrides_from_process;
use crate::config::merged::MergeInput;
use crate::config::secret::Secret;
use crate::config::ResolvedConfig;
use crate::engine::mysql::archive;
use crate::engine::mysql::compare::{
    compare_counts, count_archive_rows, count_live_rows, live_table_names, CompareReport,
    TableStatus,
};
use crate::engine::mysql::conn::MysqlClient;
use crate::engine::DbKind;
use crate::error::{Result, XBackupError};
use crate::i18n::Lang;
use crate::manifest::chain::ChainReport;
use crate::manifest::schema::BackupType;
use crate::manifest::store::{data_path, ManifestStore};
use crate::pipeline::stage::reverse_stack_for;
use crate::pipeline::verify::{verify_backup, verify_chain_for, VerifyReport};
use crate::storage::{LocalFs, Storage};

/// `verify --json` 출력 스키마 버전.
///
/// 필드 의미가 바뀌면 올린다 — 소비자(웹 콘솔)가 버전으로 파서를 고르고, 모르는 버전을
/// 만나면 조용히 오파싱하는 대신 명확히 실패할 수 있게 하기 위함이다.
const VERIFY_JSON_SCHEMA: u32 = 1;

/// `verify` 핸들러 진입점.
///
/// `--profile`이 없을 때 destination을 알아내기 위해 config가 필요하다. verify는 프로파일
/// 선택 인자가 없으므로(현 args 표면) default_profile 또는 단일 프로파일을 사용한다.
pub async fn handle(
    config_path: Option<PathBuf>,
    lang_flag: Option<crate::i18n::Lang>,
    args: VerifyArgs,
) -> Result<()> {
    // config 부재/읽기 실패로 activate 전에 끝날 수 있는 경로들도 --lang을 따르도록
    // 미리 잡아 둔다(§tr_lang! 주석 참고). config가 읽히면 아래에서 다시 정한다.
    crate::i18n::set_active(lang_flag.unwrap_or_default());
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
    let against = match args.against.as_deref() {
        Some(spec) => Some(resolve_against(spec, config_toml.as_deref())?),
        None => None,
    };
    let storage = open_storage(&config_path, args.from.as_deref()).await?;
    run(storage.as_ref(), &args, lang, against.as_ref()).await
}

/// `--against` 값을 DB URI로 푼다. `://`가 있으면 URI, 아니면 같은 config의 프로파일 이름이다.
fn resolve_against(spec: &str, config_toml: Option<&str>) -> Result<Secret> {
    let uri = if spec.contains("://") {
        Secret::new(spec.to_string())
    } else {
        let overrides = collect_overrides_from_process();
        let cfg = ResolvedConfig::build(MergeInput {
            config_toml,
            profile_name: spec,
            overrides: &overrides,
        })?;
        cfg.resolved_uri.ok_or_else(|| {
            XBackupError::Config(crate::tr!(
                "profile '{spec}' has no source.uri/uri_env",
                "프로파일 '{spec}'에 source.uri/uri_env가 없습니다"
            ))
        })?
    };
    if DbKind::from_uri(uri.expose()) != DbKind::Mysql {
        return Err(XBackupError::Usage(crate::tr!(
            "--against supports only MariaDB/MySQL targets",
            "--against는 MariaDB/MySQL 대상만 지원합니다"
        )));
    }
    Ok(uri)
}

/// 백업 아카이브와 대상 DB의 테이블별 행 수를 비교한다. 대상 DB는 테이블마다 순차로 센다.
async fn compare_backup(
    storage: &dyn Storage,
    backup_id: &str,
    target: &Secret,
    only: &[String],
) -> Result<CompareReport> {
    let manifest = ManifestStore::new(storage)
        .read(backup_id)
        .await
        .map_err(|e| {
            XBackupError::Failure(crate::tr!(
                "failed to load the manifest ({backup_id}): {e}",
                "manifest 로드 실패({backup_id}): {e}"
            ))
        })?;
    let is_mysql_full = matches!(manifest.backup_type, BackupType::Full)
        && manifest.tool_versions.archive_format.as_deref() == Some(archive::FORMAT_ID);
    if !is_mysql_full {
        return Err(XBackupError::Usage(crate::tr!(
            "--against supports only MariaDB/MySQL full backups ({backup_id})",
            "--against는 MariaDB/MySQL 풀 백업만 지원합니다({backup_id})"
        )));
    }

    let stages = reverse_stack_for(&manifest)?;
    let raw = storage
        .get_stream(&data_path(backup_id))
        .await
        .map_err(|e| {
            XBackupError::Failure(crate::tr!(
                "failed to read data.bin ({backup_id}): {e}",
                "data.bin 읽기 실패({backup_id}): {e}"
            ))
        })?;
    let mut decoded = stages.apply(raw);
    let mut backup_counts = count_archive_rows(&mut decoded).await?;

    let mut client = MysqlClient::connect(target, None).await?;
    let live = live_table_names(client.conn_mut()).await?;

    if !only.is_empty() {
        let unknown: Vec<&str> = only
            .iter()
            .filter(|t| !backup_counts.contains_key(*t) && !live.contains(*t))
            .map(String::as_str)
            .collect();
        if !unknown.is_empty() {
            return Err(XBackupError::Usage(crate::tr!(
                "--tables names tables that exist neither in the backup nor in the target: {}",
                "--tables에 백업과 대상 어디에도 없는 테이블이 있습니다: {}",
                unknown.join(", ")
            )));
        }
        backup_counts.retain(|table, _| only.contains(table));
    }

    let mut target_counts = BTreeMap::new();
    for table in backup_counts.keys().filter(|t| live.contains(*t)) {
        let rows = count_live_rows(client.conn_mut(), table).await?;
        target_counts.insert(table.clone(), rows);
    }
    let extra = live
        .iter()
        .filter(|t| !backup_counts.contains_key(*t) && (only.is_empty() || only.contains(t)))
        .cloned()
        .collect();
    Ok(compare_counts(&backup_counts, &target_counts, &extra))
}

/// 검증을 수행하고 종료 코드를 결정한다(Storage 주입 — 테스트 가능).
async fn run(
    storage: &dyn Storage,
    args: &VerifyArgs,
    lang: Lang,
    against: Option<&Secret>,
) -> Result<()> {
    // 1) 구조(+심층) 검증. 키 부재(Config)는 §8.5에 따라 검증 실패(exit 1)로 보정한다.
    let report = match verify_backup(storage, &args.id, args.deep).await {
        Ok(r) => r,
        Err(e) if args.deep && e.exit_code() == 2 => {
            // deep인데 키가 없어 Config(exit 2)가 났다 — 검증 실패(exit 1)로 보정하되
            // 키 격리 안내 메시지를 보존한다(§8.5).
            return Err(XBackupError::Failure(crate::tr!("{e} — run verify --deep on a host that holds the private key (key isolation, §8.5)", "{e} — verify --deep은 개인키 보유 호스트에서 실행하세요(§8.5 키 격리)")));
        }
        Err(e) => return Err(e),
    };

    // 2) 체인 검증(옵션).
    let chain = if args.chain {
        Some(verify_chain_for(storage, &args.id).await?)
    } else {
        None
    };

    // 3) 대상 DB와 행 수 비교(옵션).
    let compare = match against {
        Some(target) => Some(compare_backup(storage, &args.id, target, &args.tables).await?),
        None => None,
    };

    // 4) 출력.
    if args.json {
        print_json(&report, chain.as_ref(), compare.as_ref());
    } else {
        print_human(&report, chain.as_ref(), compare.as_ref(), lang);
    }

    // 5) 종료 코드 판정.
    //    - 행 수 불일치: 실패(exit 1).
    //    - broken chain: 경고 동반(exit 4) — verify 자체는 통과했으나 PITR 부적격.
    //    - 구조 경고(incomplete 등): exit 4.
    //    - 모두 정상: exit 0.
    if let Some(c) = compare.as_ref().filter(|c| !c.is_match()) {
        return Err(XBackupError::Failure(crate::tr!(
            "row counts differ between the backup and the target in {} table(s)",
            "백업과 대상의 행 수가 다른 테이블이 {}개 있습니다",
            c.mismatched().count()
        )));
    }
    let chain_broken = chain.as_ref().is_some_and(|c| !c.is_continuous());
    if chain_broken {
        return Err(XBackupError::VerifyWarning(crate::tr!(
            "chain '{}' has broken points and cannot be used for PITR ({} break(s))",
            "체인 '{}'에 끊어진 지점이 있어 PITR에 사용할 수 없습니다({}건)",
            args.id,
            chain.as_ref().map(|c| c.breaks.len()).unwrap_or(0)
        )));
    }
    if !report.warnings.is_empty() {
        return Err(XBackupError::VerifyWarning(report.warnings.join("; ")));
    }
    Ok(())
}

/// config를 읽어 destination=local Storage를 연다(backup/restore와 동일 규칙).
async fn open_storage(
    config_path: &Option<PathBuf>,
    from: Option<&str>,
) -> Result<Box<dyn Storage>> {
    let config_toml = match config_path {
        Some(path) => Some(std::fs::read_to_string(path).map_err(|e| {
            XBackupError::Config(crate::tr!(
                "failed to read the config file ({}): {e}",
                "config 파일 읽기 실패({}): {e}",
                path.display()
            ))
        })?),
        None => None,
    };
    let overrides = collect_overrides_from_process();
    // verify는 프로파일 인자가 없으므로 default 프로파일 이름을 사용한다(merged가 default를
    // 채운다). config에 default_profile이 있으면 그 이름을 우선한다.
    let profile_name = default_profile_name(config_toml.as_deref());
    let resolved = ResolvedConfig::build(MergeInput {
        config_toml: config_toml.as_deref(),
        profile_name: &profile_name,
        overrides: &overrides,
    })?;

    let dest = resolved.profile.select_destination(from)?;
    match dest.r#type.as_deref() {
        Some("local") => {}
        Some("s3") => {
            return Err(XBackupError::Usage(crate::tr!(
                "destination type=s3 is not supported yet (t7) — only type=local works",
                "destination type=s3는 아직 미지원입니다(t7) — type=local만 동작"
            )))
        }
        Some(other) => {
            return Err(XBackupError::Config(crate::tr!(
                "unknown destination type: '{other}' (only local is supported)",
                "알 수 없는 destination type: '{other}'(local만 지원)"
            )))
        }
        None => {
            return Err(XBackupError::Config(crate::tr!(
                "destination.type is not set (local is required)",
                "destination.type이 지정되지 않았습니다(local 필요)"
            )))
        }
    }
    let root = dest.path.as_deref().ok_or_else(|| {
        XBackupError::Config(crate::tr!(
            "destination.path is not set (a local path)",
            "destination.path가 지정되지 않았습니다(local 경로)"
        ))
    })?;
    Ok(Box::new(LocalFs::new(root)?))
}

/// config.toml에서 default_profile을 읽는다(없으면 "default").
fn default_profile_name(config_toml: Option<&str>) -> String {
    config_toml
        .and_then(|raw| crate::config::file::Config::from_toml_str(raw).ok())
        .and_then(|c| c.default_profile)
        .unwrap_or_else(|| "default".to_string())
}

/// 사람이 읽는 검증 결과 출력(stdout).
fn print_human(
    report: &VerifyReport,
    chain: Option<&ChainReport>,
    compare: Option<&CompareReport>,
    lang: Lang,
) {
    const W: usize = 21;
    println!(
        "{} — {}",
        style(lang.sel("verify result", "검증 결과"), Tone::Plan),
        style(&report.backup_id, Tone::Value)
    );
    println!(
        "{}",
        field_line("manifest integrity", ok_mark(report.manifest_sidecar_ok), W,)
    );
    println!(
        "{}",
        field_line("data checksum", ok_mark(report.data_checksum_ok), W,)
    );
    if report.empty_slice {
        println!(
            "  {}",
            style(
                lang.sel(
                    "(empty incremental slice — no data.bin, normal)",
                    "(빈 증분 슬라이스 — data.bin 없음, 정상)"
                ),
                Tone::Muted,
            )
        );
    }
    match report.deep_decode_ok {
        Some(true) => println!("{}", field_line("deep decode", ok_mark(true), W)),
        Some(false) => println!("{}", field_line("deep decode", ok_mark(false), W)),
        None => {}
    }
    for w in &report.warnings {
        println!(
            "{}",
            field_line_toned(lang.sel("warning", "경고"), w, W, Tone::Warning)
        );
    }

    if let Some(c) = chain {
        println!(
            "{} — base: {}",
            style(lang.sel("chain", "체인"), Tone::Plan),
            style(&c.base_id, Tone::Value)
        );
        println!(
            "{}",
            field_line(
                lang.sel("incremental", "증분"),
                format!(
                    "{}: {}",
                    style(&c.incremental_ids.len().to_string(), Tone::Value),
                    c.incremental_ids.join(", ")
                ),
                W,
            )
        );
        if c.is_continuous() {
            println!(
                "{}",
                field_line_toned(
                    lang.sel("chain status", "체인 상태"),
                    lang.sel("continuous (PITR available)", "연속(PITR 가능)"),
                    W,
                    Tone::Success,
                )
            );
        } else {
            println!(
                "{}",
                field_line_toned(
                    lang.sel("chain status", "체인 상태"),
                    lang.sel(
                        &format!("broken ({} breaks) — PITR unavailable", c.breaks.len()),
                        &format!("끊김({}건) — PITR 불가", c.breaks.len())
                    ),
                    W,
                    Tone::Danger,
                )
            );
            for b in &c.breaks {
                println!("    - {}", style(&b.to_string(), Tone::Danger));
            }
        }
        for w in &c.warnings {
            println!(
                "{}",
                field_line_toned(
                    lang.sel("chain warning", "체인 경고"),
                    w.to_string(),
                    W,
                    Tone::Warning,
                )
            );
        }
    }

    if let Some(c) = compare {
        let total = c.tables.len();
        let differing = c.mismatched().count();
        println!(
            "{} — {}",
            style(lang.sel("row count comparison", "행 수 비교"), Tone::Plan),
            style(
                lang.sel(
                    &format!("{differing} of {total} tables differ"),
                    &format!("{total}개 중 {differing}개 테이블 불일치")
                ),
                if differing == 0 {
                    Tone::Success
                } else {
                    Tone::Danger
                }
            )
        );
        for t in c.mismatched() {
            let show = |rows: Option<u64>| rows.map_or("-".to_string(), |n| n.to_string());
            println!(
                "    {} {}: backup {} / target {}",
                status_token(status_label(t.status), Tone::Danger),
                t.table,
                show(t.backup_rows),
                show(t.target_rows)
            );
        }
    }
}

/// 비교 상태를 출력용 토큰(항상 영문)으로.
fn status_label(status: TableStatus) -> &'static str {
    match status {
        TableStatus::Match => "MATCH",
        TableStatus::Mismatch => "MISMATCH",
        TableStatus::MissingInTarget => "MISSING",
        TableStatus::ExtraInTarget => "EXTRA",
    }
}

/// 기계 판독 JSON 출력(stdout).
fn print_json(report: &VerifyReport, chain: Option<&ChainReport>, compare: Option<&CompareReport>) {
    let value = build_json(report, chain, compare);
    println!("{value}");
}

/// JSON 출력 값을 만든다(순수 — 출력 부작용 없음).
fn build_json(
    report: &VerifyReport,
    chain: Option<&ChainReport>,
    compare: Option<&CompareReport>,
) -> serde_json::Value {
    let chain_json = chain.map(|c| {
        serde_json::json!({
            "base_id": c.base_id,
            "incremental_ids": c.incremental_ids,
            "continuous": c.is_continuous(),
            "breaks": c.breaks.iter().map(|b| b.to_string()).collect::<Vec<_>>(),
            "warnings": c.warnings.iter().map(|w| w.to_string()).collect::<Vec<_>>(),
        })
    });
    let compare_json = compare.map(|c| {
        serde_json::json!({
            "match": c.is_match(),
            "tables": c.tables.iter().map(|t| serde_json::json!({
                "table": t.table,
                "backup_rows": t.backup_rows,
                "target_rows": t.target_rows,
                "status": status_label(t.status).to_ascii_lowercase(),
            })).collect::<Vec<_>>(),
        })
    });
    serde_json::json!({
        "schema": VERIFY_JSON_SCHEMA,
        "backup_id": report.backup_id,
        "manifest_sidecar_ok": report.manifest_sidecar_ok,
        "data_checksum_ok": report.data_checksum_ok,
        "deep_decode_ok": report.deep_decode_ok,
        "empty_slice": report.empty_slice,
        "warnings": report.warnings,
        "ok": report.is_ok() && compare.is_none_or(|c| c.is_match()),
        "chain": chain_json,
        "compare": compare_json,
    })
}

/// 불리언을 색 입힌 OK/FAIL 마크로(상태 토큰 — 항상 영문).
fn ok_mark(ok: bool) -> String {
    if ok {
        status_token("OK", Tone::Success)
    } else {
        status_token("FAIL", Tone::Danger)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::schema::{
        BackupManifest, BackupStatus, BackupType, EncryptionMeta, OplogRange, OplogTimestamp,
        Topology, FORMAT_VERSION,
    };
    use crate::manifest::store::{data_path, ManifestStore};
    use crate::storage::{BoxAsyncRead, LocalFs};

    fn sha256_hex(data: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(data))
    }

    fn full(id: &str, checksum: &str) -> BackupManifest {
        BackupManifest {
            format_version: FORMAT_VERSION,
            id: id.to_string(),
            created_at: "2026-06-12T00:00:00Z".into(),
            backup_type: BackupType::Full,
            base_id: None,
            topology: Topology::ReplicaSet,
            server_version: "7.0.35".into(),
            tool_versions: Default::default(),
            selective: false,
            original_size_bytes: 0,
            stored_size_bytes: 0,
            compression: None,
            encryption: None,
            checksum_sha256: checksum.to_string(),
            oplog_range: Some(OplogRange {
                start_ts: OplogTimestamp::new(100, 1),
                end_ts: OplogTimestamp::new(100, 1),
            }),
            oplog_count: None,
            promoted_from_gap: false,
            mysql_binlog: None,
            status: BackupStatus::Complete,
        }
    }

    async fn seed(fs: &LocalFs, m: &BackupManifest, data: &[u8]) {
        let reader: BoxAsyncRead = Box::pin(std::io::Cursor::new(data.to_vec()));
        fs.put_stream(&data_path(&m.id), reader, Some(data.len() as u64))
            .await
            .unwrap();
        ManifestStore::new(fs).write(m).await.unwrap();
    }

    fn args(id: &str, deep: bool, chain: bool, json: bool) -> VerifyArgs {
        VerifyArgs {
            id: id.to_string(),
            deep,
            chain,
            against: None,
            tables: Vec::new(),
            json,
            from: None,
        }
    }

    /// 기본 verify는 키 없이 성공한다(exit 0).
    #[tokio::test]
    async fn run_structural_passes_without_key() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        let payload = b"archive";
        seed(&fs, &full("bk", &sha256_hex(payload)), payload).await;

        run(
            &fs,
            &args("bk", false, false, false),
            crate::i18n::Lang::En,
            None,
        )
        .await
        .unwrap();
    }

    /// 변조 산출물은 exit 1로 실패한다.
    #[tokio::test]
    async fn run_tampered_fails() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        let m = full("bk", &sha256_hex(b"original"));
        seed(&fs, &m, b"TAMPERED").await;

        let err = run(
            &fs,
            &args("bk", false, false, false),
            crate::i18n::Lang::En,
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(err.exit_code(), 1);
    }

    /// --deep인데 암호화 백업이고 키 env가 없으면 exit 1로 거부하고 격리 안내한다.
    #[tokio::test]
    async fn run_deep_without_key_rejected_exit1() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        let ct = b"ciphertext";
        let mut m = full("bk", &sha256_hex(ct));
        m.encryption = Some(EncryptionMeta {
            algorithm: "age".into(),
            key_id: Some("age1xxx".into()),
        });
        seed(&fs, &m, ct).await;

        // SAFETY: 테스트에서 동기적으로 제거 후 즉시 호출.
        unsafe {
            std::env::remove_var(crate::pipeline::stage::ENV_AGE_IDENTITY_FILE);
        }
        let err = run(
            &fs,
            &args("bk", true, false, false),
            crate::i18n::Lang::En,
            None,
        )
        .await
        .unwrap_err();
        // §8.5: 키 부재 deep은 검증 실패(exit 1)로 보정.
        assert_eq!(err.exit_code(), 1, "키 부재 deep은 exit 1: {err}");
        assert!(
            err.to_string().contains("private key"),
            "missing key-isolation guidance: {err}"
        );
    }

    /// incomplete 백업은 exit 4(경고 동반 성공).
    #[tokio::test]
    async fn run_incomplete_returns_warning_exit4() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        let payload = b"partial";
        let mut m = full("bk", &sha256_hex(payload));
        m.status = BackupStatus::Incomplete;
        seed(&fs, &m, payload).await;

        let err = run(
            &fs,
            &args("bk", false, false, false),
            crate::i18n::Lang::En,
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(err.exit_code(), 4);
    }

    /// --chain broken은 exit 4로 경고하고 끊긴 지점을 보고한다.
    #[tokio::test]
    async fn run_broken_chain_returns_warning_exit4() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        // base + gap 있는 증분.
        let base = full("base", sha256_hex(b"").as_str());
        let mut i1 = full("i1", sha256_hex(b"").as_str());
        i1.backup_type = BackupType::Incremental;
        i1.base_id = Some("base".into());
        // gap: base end(100,1)이 아니라 105,0에서 시작.
        i1.oplog_range = Some(OplogRange {
            start_ts: OplogTimestamp::new(105, 0),
            end_ts: OplogTimestamp::new(150, 2),
        });
        ManifestStore::new(&fs).write(&base).await.unwrap();
        ManifestStore::new(&fs).write(&i1).await.unwrap();
        // base data.bin(빈 입력)으로 구조 검증도 통과시킨다.
        let empty: BoxAsyncRead = Box::pin(std::io::Cursor::new(Vec::new()));
        fs.put_stream(&data_path("i1"), empty, Some(0))
            .await
            .unwrap();

        let err = run(
            &fs,
            &args("i1", false, true, false),
            crate::i18n::Lang::En,
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(err.exit_code(), 4, "broken chain은 exit 4: {err}");
    }

    fn report(id: &str) -> VerifyReport {
        VerifyReport {
            backup_id: id.to_string(),
            manifest_sidecar_ok: true,
            data_checksum_ok: true,
            deep_decode_ok: None,
            warnings: Vec::new(),
            empty_slice: false,
        }
    }

    /// 최상위 JSON 문서는 스키마 버전을 달고 나간다 — 소비자가 파서를 고르는 근거다.
    #[test]
    fn build_json_stamps_schema_at_top_level() {
        let v = build_json(&report("bk"), None, None);
        assert_eq!(v["schema"], VERIFY_JSON_SCHEMA);
        assert_eq!(v["backup_id"], "bk");
        assert!(v["chain"].is_null(), "--chain 없으면 chain은 null");
    }

    /// 행 수가 다르면 종료 코드(exit 1)와 같이 최상위 `ok`도 false다.
    #[test]
    fn build_json_ok_is_false_when_row_counts_differ() {
        let backup = BTreeMap::from([("t".to_string(), 3u64)]);
        let differ = BTreeMap::from([("t".to_string(), 2u64)]);
        let mismatch =
            crate::engine::mysql::compare::compare_counts(&backup, &differ, &Default::default());
        let same = crate::engine::mysql::compare::compare_counts(
            &backup,
            &backup.clone(),
            &Default::default(),
        );
        assert_eq!(
            build_json(&report("bk"), None, Some(&mismatch))["ok"],
            false
        );
        assert_eq!(build_json(&report("bk"), None, Some(&same))["ok"], true);
    }

    /// `--chain`이 붙어도 schema는 최상위에만 있다(중첩 객체는 버전을 갖지 않는다).
    #[test]
    fn build_json_schema_is_not_nested_into_chain() {
        let chain = ChainReport {
            base_id: "base".into(),
            incremental_ids: vec!["i1".into()],
            breaks: Vec::new(),
            warnings: Vec::new(),
        };
        let v = build_json(&report("i1"), Some(&chain), None);
        assert_eq!(v["schema"], VERIFY_JSON_SCHEMA);
        assert!(
            v["chain"].get("schema").is_none(),
            "중첩 객체는 독립 문서가 아니므로 버전을 갖지 않는다"
        );
    }

    /// default_profile_name은 config의 default_profile을 읽는다.
    #[test]
    fn default_profile_name_reads_config() {
        let toml = "default_profile = \"prod\"\n";
        assert_eq!(default_profile_name(Some(toml)), "prod");
        assert_eq!(default_profile_name(None), "default");
        assert_eq!(default_profile_name(Some("")), "default");
    }

    /// 위치를 여럿 둔 프로파일은 단일 `destination` 필드가 비어 있다. 그 필드만 읽던 판본은
    /// "destination.type이 없다"로 실패했다 — primary를 읽고, `--from`으로 다른 위치를 고른다.
    #[tokio::test]
    async fn multi_destination_profile_reads_primary_or_the_chosen_one() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a")).unwrap();
        std::fs::create_dir_all(dir.path().join("b")).unwrap();
        std::fs::write(dir.path().join("b").join("only-offsite"), b"x").unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "default_profile = \"p\"\n\
                 [profile.p]\nuri = \"mongodb://127.0.0.1:1/app\"\n\
                 [[profile.p.dest]]\nname = \"primary\"\ndest = \"local:{a}\"\n\
                 [[profile.p.dest]]\nname = \"offsite\"\ndest = \"local:{b}\"\n",
                a = dir.path().join("a").display(),
                b = dir.path().join("b").display(),
            ),
        )
        .unwrap();
        let config = Some(config_path);

        let primary = open_storage(&config, None).await.unwrap();
        assert!(primary.list("").await.unwrap().is_empty());
        let offsite = open_storage(&config, Some("offsite")).await.unwrap();
        let entries = offsite.list("").await.unwrap();
        assert!(entries.iter().any(|e| e.path == "only-offsite"));

        let err = open_storage(&config, Some("nope"))
            .await
            .err()
            .expect("없는 위치 이름은 거부해야 함");
        assert!(
            err.to_string().contains("primary, offsite"),
            "고를 수 있는 이름을 알려야 함: {err}"
        );
    }
}
