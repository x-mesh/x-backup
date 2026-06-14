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
use crate::config::env::collect_overrides_from_process;
use crate::config::merged::MergeInput;
use crate::config::ResolvedConfig;
use crate::error::{Result, XBackupError};
use crate::manifest::chain::ChainReport;
use crate::pipeline::verify::{verify_backup, verify_chain_for, VerifyReport};
use crate::storage::{LocalFs, Storage};

/// `verify` 핸들러 진입점.
///
/// `--profile`이 없을 때 destination을 알아내기 위해 config가 필요하다. verify는 프로파일
/// 선택 인자가 없으므로(현 args 표면) default_profile 또는 단일 프로파일을 사용한다.
pub async fn handle(config_path: Option<PathBuf>, args: VerifyArgs) -> Result<()> {
    let storage = open_storage(&config_path).await?;
    run(storage.as_ref(), &args).await
}

/// 검증을 수행하고 종료 코드를 결정한다(Storage 주입 — 테스트 가능).
async fn run(storage: &dyn Storage, args: &VerifyArgs) -> Result<()> {
    // 1) 구조(+심층) 검증. 키 부재(Config)는 §8.5에 따라 검증 실패(exit 1)로 보정한다.
    let report = match verify_backup(storage, &args.id, args.deep).await {
        Ok(r) => r,
        Err(e) if args.deep && e.exit_code() == 2 => {
            // deep인데 키가 없어 Config(exit 2)가 났다 — 검증 실패(exit 1)로 보정하되
            // 키 격리 안내 메시지를 보존한다(§8.5).
            return Err(XBackupError::Failure(format!(
                "{e} — verify --deep은 개인키 보유 호스트에서 실행하세요(§8.5 키 격리)"
            )));
        }
        Err(e) => return Err(e),
    };

    // 2) 체인 검증(옵션).
    let chain = if args.chain {
        Some(verify_chain_for(storage, &args.id).await?)
    } else {
        None
    };

    // 3) 출력.
    if args.json {
        print_json(&report, chain.as_ref());
    } else {
        print_human(&report, chain.as_ref());
    }

    // 4) 종료 코드 판정.
    //    - broken chain: 경고 동반(exit 4) — verify 자체는 통과했으나 PITR 부적격.
    //    - 구조 경고(incomplete 등): exit 4.
    //    - 모두 정상: exit 0.
    let chain_broken = chain.as_ref().is_some_and(|c| !c.is_continuous());
    if chain_broken {
        return Err(XBackupError::VerifyWarning(format!(
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
async fn open_storage(config_path: &Option<PathBuf>) -> Result<Box<dyn Storage>> {
    let config_toml = match config_path {
        Some(path) => Some(std::fs::read_to_string(path).map_err(|e| {
            XBackupError::Config(format!("config 파일 읽기 실패({}): {e}", path.display()))
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

/// config.toml에서 default_profile을 읽는다(없으면 "default").
fn default_profile_name(config_toml: Option<&str>) -> String {
    config_toml
        .and_then(|raw| toml::from_str::<crate::config::file::Config>(raw).ok())
        .and_then(|c| c.default_profile)
        .unwrap_or_else(|| "default".to_string())
}

/// 사람이 읽는 검증 결과 출력(stdout).
fn print_human(report: &VerifyReport, chain: Option<&ChainReport>) {
    println!("검증 결과 — {}", report.backup_id);
    println!("  manifest 정합: {}", ok_mark(report.manifest_sidecar_ok));
    println!("  data 체크섬:   {}", ok_mark(report.data_checksum_ok));
    if report.empty_slice {
        println!("  (빈 증분 슬라이스 — data.bin 없음, 정상)");
    }
    match report.deep_decode_ok {
        Some(true) => println!("  심층 디코드:   OK"),
        Some(false) => println!("  심층 디코드:   실패"),
        None => {}
    }
    for w in &report.warnings {
        println!("  경고:          {w}");
    }

    if let Some(c) = chain {
        println!("체인 — base: {}", c.base_id);
        println!(
            "  증분 {}개: {}",
            c.incremental_ids.len(),
            c.incremental_ids.join(", ")
        );
        if c.is_continuous() {
            println!("  체인 상태:     연속(PITR 가능)");
        } else {
            println!("  체인 상태:     끊김({}건) — PITR 불가", c.breaks.len());
            for b in &c.breaks {
                println!("    - {b}");
            }
        }
        for w in &c.warnings {
            println!("  체인 경고:     {w}");
        }
    }
}

/// 기계 판독 JSON 출력(stdout).
fn print_json(report: &VerifyReport, chain: Option<&ChainReport>) {
    let chain_json = chain.map(|c| {
        serde_json::json!({
            "base_id": c.base_id,
            "incremental_ids": c.incremental_ids,
            "continuous": c.is_continuous(),
            "breaks": c.breaks.iter().map(|b| b.to_string()).collect::<Vec<_>>(),
            "warnings": c.warnings.iter().map(|w| w.to_string()).collect::<Vec<_>>(),
        })
    });
    let value = serde_json::json!({
        "backup_id": report.backup_id,
        "manifest_sidecar_ok": report.manifest_sidecar_ok,
        "data_checksum_ok": report.data_checksum_ok,
        "deep_decode_ok": report.deep_decode_ok,
        "empty_slice": report.empty_slice,
        "warnings": report.warnings,
        "ok": report.is_ok(),
        "chain": chain_json,
    });
    println!("{value}");
}

/// 불리언을 OK/실패 마크로.
fn ok_mark(ok: bool) -> &'static str {
    if ok {
        "OK"
    } else {
        "실패"
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
            json,
        }
    }

    /// 기본 verify는 키 없이 성공한다(exit 0).
    #[tokio::test]
    async fn run_structural_passes_without_key() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        let payload = b"archive";
        seed(&fs, &full("bk", &sha256_hex(payload)), payload).await;

        run(&fs, &args("bk", false, false, false)).await.unwrap();
    }

    /// 변조 산출물은 exit 1로 실패한다.
    #[tokio::test]
    async fn run_tampered_fails() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        let m = full("bk", &sha256_hex(b"original"));
        seed(&fs, &m, b"TAMPERED").await;

        let err = run(&fs, &args("bk", false, false, false))
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
        let err = run(&fs, &args("bk", true, false, false)).await.unwrap_err();
        // §8.5: 키 부재 deep은 검증 실패(exit 1)로 보정.
        assert_eq!(err.exit_code(), 1, "키 부재 deep은 exit 1: {err}");
        assert!(err.to_string().contains("개인키"), "격리 안내 누락: {err}");
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

        let err = run(&fs, &args("bk", false, false, false))
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

        let err = run(&fs, &args("i1", false, true, false)).await.unwrap_err();
        assert_eq!(err.exit_code(), 4, "broken chain은 exit 4: {err}");
    }

    /// default_profile_name은 config의 default_profile을 읽는다.
    #[test]
    fn default_profile_name_reads_config() {
        let toml = "default_profile = \"prod\"\n";
        assert_eq!(default_profile_name(Some(toml)), "prod");
        assert_eq!(default_profile_name(None), "default");
        assert_eq!(default_profile_name(Some("")), "default");
    }
}
