//! `list` 서브커맨드 핸들러 — 가용 백업·증분 체인 카탈로그(PRD §FR-7, R22).
//!
//! destination의 모든 manifest를 모아 카탈로그를 출력한다. 각 항목은 id·유형(full/incr)·
//! 생성 시각·저장 크기·체인 상태(ok/broken/incomplete)를 보여준다. manifest 없이 data만
//! 있는 디렉터리는 **orphan**(유령 산출물, pitfall 7-1)으로 표시한다. `--json`을 지원한다.
//!
//! ## 체인 상태 판정
//! - 풀백업: 자신을 base로 한 체인이 연속이면 `ok`, 끊겼으면 `broken`(증분이 없으면 ok).
//! - 증분: 자신이 속한 체인 연속성으로 판정.
//! - incomplete manifest: 항상 `incomplete`(체인 판정과 무관하게 사용 위험 표시).
//!
//! 스코프(t10): destination type=local. S3는 t7이 동일 Storage trait로 동작.

use std::path::PathBuf;

use crate::cli::args::ListArgs;
use crate::config::env::collect_overrides_from_process;
use crate::config::merged::MergeInput;
use crate::config::ResolvedConfig;
use crate::error::{Result, XBackupError};
use crate::manifest::chain::verify_chain;
use crate::manifest::schema::{BackupStatus, BackupType};
use crate::manifest::store::{ManifestStore, DATA_FILE, MANIFEST_FILE};
use crate::manifest::ChainNode;
use crate::pipeline::verify::collect_manifest_ids;
use crate::storage::{LocalFs, Storage};

/// 카탈로그 한 행(백업 또는 orphan).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogRow {
    /// 백업 ID(또는 orphan 디렉터리명).
    pub id: String,
    /// 백업 유형 문자열(`full`/`incr`/`orphan`).
    pub kind: String,
    /// 생성 시각(RFC3339; orphan은 None).
    pub created_at: Option<String>,
    /// 저장 크기(data.bin 바이트). orphan은 실제 data 크기.
    pub stored_size_bytes: u64,
    /// 체인 상태(`ok`/`broken`/`incomplete`/`orphan`).
    pub chain_status: String,
    /// base 풀백업 ID(증분만; full/orphan은 None).
    pub base_id: Option<String>,
}

/// `list` 핸들러 진입점.
pub async fn handle(config_path: Option<PathBuf>, args: ListArgs) -> Result<()> {
    let storage = open_storage(&config_path, &args).await?;
    let rows = build_catalog(storage.as_ref()).await?;

    if args.json {
        print_json(&rows);
    } else {
        print_human(&rows);
    }

    // broken/incomplete가 하나라도 있으면 경고 동반 성공(exit 4) — 운영자가 알아채도록.
    let has_warning = rows.iter().any(|r| {
        r.chain_status == "broken" || r.chain_status == "incomplete" || r.chain_status == "orphan"
    });
    if has_warning {
        return Err(XBackupError::VerifyWarning(
            "broken/incomplete/orphan 항목이 있습니다 — 위 카탈로그를 확인하세요".into(),
        ));
    }
    Ok(())
}

/// destination의 manifest·orphan을 모아 카탈로그 행을 만든다(Storage 주입 — 테스트 가능).
pub async fn build_catalog(storage: &dyn Storage) -> Result<Vec<CatalogRow>> {
    let store = ManifestStore::new(storage);

    // 1) manifest를 가진 백업 ID 수집 + 노드 구성(체인 판정용).
    let ids = collect_manifest_ids(storage).await?;
    let mut manifests = Vec::with_capacity(ids.len());
    for id in &ids {
        match store.read(id).await {
            Ok(m) => manifests.push(m),
            Err(e) => {
                tracing::debug!(id = %id, "manifest 읽기 실패(카탈로그에서 제외): {e}");
            }
        }
    }
    let nodes: Vec<ChainNode> = manifests.iter().map(ChainNode::from_manifest).collect();

    // 2) 각 백업의 행을 만든다.
    let mut rows = Vec::new();
    for m in &manifests {
        let kind = match m.backup_type {
            BackupType::Full => "full",
            BackupType::Incremental => "incr",
        };
        let chain_status = if matches!(m.status, BackupStatus::Incomplete) {
            "incomplete".to_string()
        } else {
            // 체인 연속성으로 ok/broken 판정.
            let report = verify_chain(&nodes, &m.id);
            if report.is_continuous() {
                "ok".to_string()
            } else {
                "broken".to_string()
            }
        };
        rows.push(CatalogRow {
            id: m.id.clone(),
            kind: kind.to_string(),
            created_at: Some(m.created_at.clone()),
            stored_size_bytes: m.stored_size_bytes,
            chain_status,
            base_id: m.base_id.clone(),
        });
    }

    // 3) orphan 감지 — manifest 없이 data.bin만 있는 디렉터리(pitfall 7-1).
    let orphans = detect_orphans(storage, &ids).await?;
    rows.extend(orphans);

    // 4) id(=UUID v7, 시간 정렬 가능)로 정렬해 안정적 출력.
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(rows)
}

/// manifest가 없는 data.bin 디렉터리를 orphan 행으로 만든다.
///
/// `known_ids`는 manifest를 가진 백업 ID 집합이다. data.bin은 있으나 manifest가 없는
/// 디렉터리를 유령 산출물로 본다(실패한 백업의 잔재 또는 수동 삭제로 깨진 체인).
async fn detect_orphans(storage: &dyn Storage, known_ids: &[String]) -> Result<Vec<CatalogRow>> {
    let entries = storage.list("").await?;
    let data_suffix = format!("/{DATA_FILE}");
    let manifest_suffix = format!("/{MANIFEST_FILE}");

    let mut rows = Vec::new();
    for e in &entries {
        // data.bin 경로에서 디렉터리(id)를 추출한다.
        let Some(id) = e
            .path
            .strip_suffix(&data_suffix)
            .filter(|id| !id.is_empty() && !id.contains('/'))
        else {
            continue;
        };
        // manifest가 있으면 orphan이 아니다.
        if known_ids.iter().any(|k| k == id) {
            continue;
        }
        // 같은 list에 manifest.json이 있는데 read만 실패한 경우도 orphan 대신 깨진 manifest로
        // 취급할 수 있으나, manifest 파일이 아예 없는 경우만 orphan으로 좁힌다.
        let has_manifest_file = entries
            .iter()
            .any(|x| x.path == format!("{id}{manifest_suffix}"));
        if has_manifest_file {
            continue;
        }
        rows.push(CatalogRow {
            id: id.to_string(),
            kind: "orphan".to_string(),
            created_at: e.last_modified.clone(),
            stored_size_bytes: e.size,
            chain_status: "orphan".to_string(),
            base_id: None,
        });
    }
    Ok(rows)
}

/// config를 읽어 destination=local Storage를 연다.
async fn open_storage(config_path: &Option<PathBuf>, args: &ListArgs) -> Result<Box<dyn Storage>> {
    let config_toml = match config_path {
        Some(path) => Some(std::fs::read_to_string(path).map_err(|e| {
            XBackupError::Config(format!("config 파일 읽기 실패({}): {e}", path.display()))
        })?),
        None => None,
    };
    let overrides = collect_overrides_from_process();
    let profile_name = resolve_profile_name(args.profile.as_deref(), config_toml.as_deref());
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

/// `--profile` 우선, 없으면 config.default_profile, 그것도 없으면 "default".
fn resolve_profile_name(cli_profile: Option<&str>, config_toml: Option<&str>) -> String {
    if let Some(p) = cli_profile {
        return p.to_string();
    }
    config_toml
        .and_then(|raw| toml::from_str::<crate::config::file::Config>(raw).ok())
        .and_then(|c| c.default_profile)
        .unwrap_or_else(|| "default".to_string())
}

/// 사람이 읽는 카탈로그 출력(stdout).
fn print_human(rows: &[CatalogRow]) {
    if rows.is_empty() {
        println!("백업이 없습니다.");
        return;
    }
    println!(
        "{:<38} {:<7} {:<25} {:>12} {:<10} BASE",
        "ID", "TYPE", "CREATED", "SIZE", "CHAIN"
    );
    for r in rows {
        println!(
            "{:<38} {:<7} {:<25} {:>12} {:<10} {}",
            r.id,
            r.kind,
            r.created_at.as_deref().unwrap_or("-"),
            r.stored_size_bytes,
            chain_label(&r.chain_status),
            r.base_id.as_deref().unwrap_or("-"),
        );
    }
    // broken/orphan 요약 경고.
    let broken: Vec<&str> = rows
        .iter()
        .filter(|r| r.chain_status == "broken")
        .map(|r| r.id.as_str())
        .collect();
    if !broken.is_empty() {
        println!();
        println!("경고: 끊어진 체인 — {}", broken.join(", "));
        println!("      수동 삭제로 체인이 깨졌을 수 있습니다. verify --chain --id <id>로 상세 확인하세요.");
    }
    let orphans: Vec<&str> = rows
        .iter()
        .filter(|r| r.chain_status == "orphan")
        .map(|r| r.id.as_str())
        .collect();
    if !orphans.is_empty() {
        println!();
        println!("경고: orphan(유령 산출물) — {}", orphans.join(", "));
        println!("      manifest 없는 data 디렉터리입니다(실패한 백업 잔재 가능).");
    }
}

/// 체인 상태에 표시 라벨을 붙인다(가독성). 경고 상태는 대문자로 눈에 띄게 한다.
fn chain_label(status: &str) -> &str {
    match status {
        "broken" => "BROKEN",
        "incomplete" => "INCOMPLETE",
        "orphan" => "ORPHAN",
        other => other,
    }
}

/// 기계 판독 JSON 출력(stdout).
fn print_json(rows: &[CatalogRow]) {
    let items: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "id": r.id,
                "type": r.kind,
                "created_at": r.created_at,
                "stored_size_bytes": r.stored_size_bytes,
                "chain_status": r.chain_status,
                "base_id": r.base_id,
            })
        })
        .collect();
    let value = serde_json::json!({ "backups": items });
    println!("{value}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::schema::{
        BackupManifest, BackupStatus, BackupType, OplogRange, OplogTimestamp, Topology,
        FORMAT_VERSION,
    };
    use crate::manifest::store::data_path;
    use crate::storage::{BoxAsyncRead, LocalFs};

    fn manifest(id: &str, kind: BackupType, base: Option<&str>) -> BackupManifest {
        BackupManifest {
            format_version: FORMAT_VERSION,
            id: id.to_string(),
            created_at: "2026-06-12T00:00:00Z".into(),
            backup_type: kind,
            base_id: base.map(str::to_string),
            topology: Topology::ReplicaSet,
            server_version: "7.0.35".into(),
            tool_versions: Default::default(),
            selective: false,
            original_size_bytes: 10,
            stored_size_bytes: 10,
            compression: None,
            encryption: None,
            checksum_sha256: "x".into(),
            oplog_range: Some(OplogRange {
                start_ts: OplogTimestamp::new(100, 1),
                end_ts: OplogTimestamp::new(100, 1),
            }),
            oplog_count: None,
            promoted_from_gap: false,
            status: BackupStatus::Complete,
        }
    }

    async fn write_manifest(fs: &LocalFs, m: &BackupManifest) {
        ManifestStore::new(fs).write(m).await.unwrap();
    }

    /// 연속 체인은 모든 행이 ok.
    #[tokio::test]
    async fn catalog_marks_continuous_chain_ok() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        let base = manifest("base", BackupType::Full, None);
        let mut i1 = manifest("i1", BackupType::Incremental, Some("base"));
        i1.oplog_range = Some(OplogRange {
            start_ts: OplogTimestamp::new(100, 1),
            end_ts: OplogTimestamp::new(150, 2),
        });
        write_manifest(&fs, &base).await;
        write_manifest(&fs, &i1).await;

        let rows = build_catalog(&fs).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter().all(|r| r.chain_status == "ok"),
            "rows: {rows:?}"
        );
    }

    /// 끊어진 체인(gap)은 broken으로 표시한다.
    #[tokio::test]
    async fn catalog_marks_broken_chain() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        let base = manifest("base", BackupType::Full, None);
        let mut i1 = manifest("i1", BackupType::Incremental, Some("base"));
        // gap: base end(100,1)이 아니라 105,0에서 시작.
        i1.oplog_range = Some(OplogRange {
            start_ts: OplogTimestamp::new(105, 0),
            end_ts: OplogTimestamp::new(150, 2),
        });
        write_manifest(&fs, &base).await;
        write_manifest(&fs, &i1).await;

        let rows = build_catalog(&fs).await.unwrap();
        let i1_row = rows.iter().find(|r| r.id == "i1").unwrap();
        assert_eq!(i1_row.chain_status, "broken", "rows: {rows:?}");
    }

    /// incomplete manifest는 incomplete로 표시한다.
    #[tokio::test]
    async fn catalog_marks_incomplete() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        let mut m = manifest("bk", BackupType::Full, None);
        m.status = BackupStatus::Incomplete;
        write_manifest(&fs, &m).await;

        let rows = build_catalog(&fs).await.unwrap();
        assert_eq!(rows[0].chain_status, "incomplete");
    }

    /// manifest 없는 data 디렉터리는 orphan으로 표시한다(pitfall 7-1).
    #[tokio::test]
    async fn catalog_marks_orphan() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        // 정상 백업 하나.
        write_manifest(&fs, &manifest("good", BackupType::Full, None)).await;
        // manifest 없는 data만 있는 유령 디렉터리.
        let ghost: BoxAsyncRead = Box::pin(std::io::Cursor::new(b"orphan data".to_vec()));
        fs.put_stream(&data_path("ghost"), ghost, None)
            .await
            .unwrap();

        let rows = build_catalog(&fs).await.unwrap();
        let orphan = rows.iter().find(|r| r.id == "ghost").unwrap();
        assert_eq!(orphan.kind, "orphan");
        assert_eq!(orphan.chain_status, "orphan");
        assert_eq!(orphan.stored_size_bytes, "orphan data".len() as u64);
    }

    /// 빈 카탈로그도 에러 없이 빈 벡터를 만든다.
    #[tokio::test]
    async fn catalog_empty_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        let rows = build_catalog(&fs).await.unwrap();
        assert!(rows.is_empty());
    }

    /// resolve_profile_name: CLI > config.default > "default".
    #[test]
    fn profile_name_resolution() {
        assert_eq!(
            resolve_profile_name(Some("cli"), Some("default_profile=\"cfg\"")),
            "cli"
        );
        assert_eq!(
            resolve_profile_name(None, Some("default_profile = \"cfg\"")),
            "cfg"
        );
        assert_eq!(resolve_profile_name(None, None), "default");
    }
}
