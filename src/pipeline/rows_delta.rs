//! 직전 풀 백업 대비 행 수 변화 — 백업 직후 테이블(컬렉션)이 사라졌거나 행이 급감했는지 알린다.
//!
//! 직전 백업은 같은 destination에서 이번 백업보다 앞선, 같은 엔진의 완전한(complete) 비선택 풀
//! 백업 중 `table_rows`가 있는 가장 최근 것이다. 행 수는 manifest에 기록된 값이라 아카이브를
//! 읽지 않는다. 정보 제공이 목적이므로 비교하지 못해도 백업을 실패시키지 않는다.

use std::collections::BTreeMap;

use crate::error::Result;
use crate::manifest::schema::{BackupStatus, BackupType};
use crate::manifest::store::{ManifestStore, MANIFEST_FILE};
use crate::storage::Storage;

/// 행 감소율 경고의 기본 임계값(%). `--warn-drop-percent`로 바꾼다.
pub const DEFAULT_DROP_WARN_PERCENT: u32 = 50;

/// 이보다 행이 적은 테이블은 감소율 경고에서 뺀다 — 몇 행짜리 조회용 테이블의 변동이 매번
/// 경고가 되지 않게 한다. 사라짐(`vanished`) 경고에는 적용하지 않는다.
const MIN_PREVIOUS_ROWS: u64 = 10;

/// 비교 기준이 되는 직전 풀 백업.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviousBackup {
    pub id: String,
    pub table_rows: BTreeMap<String, u64>,
    pub stored_size_bytes: u64,
}

/// 행이 임계값 이상 줄어든 테이블.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedTable {
    pub table: String,
    pub previous: u64,
    pub now: u64,
}

/// 직전 백업 대비 변화.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowsDelta {
    pub previous_id: String,
    pub previous_total: u64,
    pub total: u64,
    pub previous_stored_size: u64,
    pub stored_size: u64,
    /// 직전에는 있었으나 이번에는 없는 테이블.
    pub vanished: Vec<String>,
    /// 행이 임계값 이상 줄어든 테이블.
    pub dropped: Vec<DroppedTable>,
}

impl RowsDelta {
    pub fn has_warnings(&self) -> bool {
        !self.vanished.is_empty() || !self.dropped.is_empty()
    }

    /// 총 행 수 변화율(%). 직전 총 행 수가 0이면 정의할 수 없어 None.
    pub fn rows_change_percent(&self) -> Option<f64> {
        percent_change(self.previous_total, self.total)
    }

    /// 저장 크기 변화율(%). 직전 크기가 0이면 None.
    pub fn size_change_percent(&self) -> Option<f64> {
        percent_change(self.previous_stored_size, self.stored_size)
    }

    /// `--json` 요약에 넣는 값.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "previous_id": self.previous_id,
            "previous_total": self.previous_total,
            "total": self.total,
            "vanished": self.vanished,
            "dropped": self.dropped.iter().map(|d| serde_json::json!({
                "table": d.table,
                "previous": d.previous,
                "now": d.now,
            })).collect::<Vec<_>>(),
        })
    }
}

fn percent_change(previous: u64, now: u64) -> Option<f64> {
    (previous > 0).then(|| (now as f64 - previous as f64) / previous as f64 * 100.0)
}

/// 이번 백업의 행 수를 직전 백업과 비교한다(순수). `drop_percent`가 0이면 경고를 만들지 않는다.
pub fn compute_delta(
    previous: &PreviousBackup,
    now: &BTreeMap<String, u64>,
    stored_size: u64,
    drop_percent: u32,
) -> RowsDelta {
    let mut vanished = Vec::new();
    let mut dropped = Vec::new();
    if drop_percent > 0 {
        for (table, &prev_rows) in &previous.table_rows {
            match now.get(table) {
                None => vanished.push(table.clone()),
                Some(&now_rows)
                    if prev_rows >= MIN_PREVIOUS_ROWS
                        && now_rows < prev_rows
                        && u128::from(prev_rows - now_rows) * 100
                            >= u128::from(prev_rows) * u128::from(drop_percent) =>
                {
                    dropped.push(DroppedTable {
                        table: table.clone(),
                        previous: prev_rows,
                        now: now_rows,
                    });
                }
                Some(_) => {}
            }
        }
    }
    RowsDelta {
        previous_id: previous.id.clone(),
        previous_total: previous.table_rows.values().sum(),
        total: now.values().sum(),
        previous_stored_size: previous.stored_size_bytes,
        stored_size,
        vanished,
        dropped,
    }
}

/// 같은 destination에서 `current_id`보다 앞선 직전 풀 백업을 찾는다.
///
/// 같은 엔진(`archive_format`)의 complete·비선택 풀 백업 중 `table_rows`가 있는 것만 후보다.
/// 기준은 `created_at`(동률 시 id)이다. 읽을 수 없는 manifest는 건너뛴다.
pub async fn find_previous(
    storage: &dyn Storage,
    current_id: &str,
    archive_format: &str,
) -> Result<Option<PreviousBackup>> {
    let store = ManifestStore::new(storage);
    let current = store.read(current_id).await?;
    let current_key = (current.created_at.clone(), current.id.clone());

    let suffix = format!("/{MANIFEST_FILE}");
    let mut ids: Vec<String> = storage
        .list("")
        .await?
        .iter()
        .filter_map(|e| {
            e.path
                .strip_suffix(&suffix)
                .filter(|id| !id.is_empty() && !id.contains('/') && *id != current_id)
                .map(str::to_string)
        })
        .collect();
    ids.sort();
    ids.dedup();

    let mut best: Option<(String, String, PreviousBackup)> = None;
    for id in ids {
        let manifest = match store.read(&id).await {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!(id = %id, "manifest 읽기 실패(건너뜀): {e}");
                continue;
            }
        };
        let comparable = matches!(manifest.backup_type, BackupType::Full)
            && matches!(manifest.status, BackupStatus::Complete)
            && !manifest.selective
            && manifest.tool_versions.archive_format.as_deref() == Some(archive_format);
        let Some(rows) = manifest.table_rows.clone().filter(|_| comparable) else {
            continue;
        };
        let key = (manifest.created_at.clone(), manifest.id.clone());
        if key >= current_key {
            continue;
        }
        if best.as_ref().is_none_or(|(created, best_id, _)| {
            (created.as_str(), best_id.as_str()) < (key.0.as_str(), key.1.as_str())
        }) {
            best = Some((
                key.0,
                key.1,
                PreviousBackup {
                    id: manifest.id,
                    table_rows: rows,
                    stored_size_bytes: manifest.stored_size_bytes,
                },
            ));
        }
    }
    Ok(best.map(|(_, _, previous)| previous))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(pairs: &[(&str, u64)]) -> BTreeMap<String, u64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    fn previous(pairs: &[(&str, u64)]) -> PreviousBackup {
        PreviousBackup {
            id: "prev".into(),
            table_rows: rows(pairs),
            stored_size_bytes: 1_000,
        }
    }

    #[test]
    fn no_change_has_no_warnings_and_zero_percent() {
        let prev = previous(&[("a", 100), ("b", 50)]);
        let delta = compute_delta(&prev, &rows(&[("a", 100), ("b", 50)]), 1_000, 50);
        assert!(!delta.has_warnings());
        assert_eq!(delta.rows_change_percent(), Some(0.0));
        assert_eq!(delta.size_change_percent(), Some(0.0));
    }

    #[test]
    fn growth_is_reported_as_a_positive_percent_without_warnings() {
        let prev = previous(&[("a", 100)]);
        let delta = compute_delta(&prev, &rows(&[("a", 150), ("new_table", 5)]), 900, 50);
        assert!(!delta.has_warnings());
        assert_eq!((delta.previous_total, delta.total), (100, 155));
        assert_eq!(delta.rows_change_percent().map(|p| p.round()), Some(55.0));
        assert_eq!(delta.size_change_percent().map(|p| p.round()), Some(-10.0));
    }

    #[test]
    fn a_table_missing_from_the_new_backup_is_flagged_even_when_it_was_empty() {
        let prev = previous(&[("kept", 20), ("gone", 3), ("empty_gone", 0)]);
        let delta = compute_delta(&prev, &rows(&[("kept", 20)]), 1_000, 50);
        assert_eq!(
            delta.vanished,
            vec!["empty_gone".to_string(), "gone".to_string()]
        );
        assert!(delta.dropped.is_empty());
    }

    #[test]
    fn drop_is_flagged_at_or_above_the_threshold_only() {
        let prev = previous(&[("half", 100), ("less_than_half", 100), ("up", 100)]);
        let now = rows(&[("half", 50), ("less_than_half", 51), ("up", 400)]);
        let delta = compute_delta(&prev, &now, 1_000, 50);
        assert_eq!(
            delta.dropped,
            vec![DroppedTable {
                table: "half".into(),
                previous: 100,
                now: 50
            }],
            "정확히 50% 감소는 경고, 49%는 경고가 아니다"
        );
    }

    #[test]
    fn small_tables_are_exempt_from_drop_warnings() {
        let prev = previous(&[
            ("lookup", MIN_PREVIOUS_ROWS - 1),
            ("tiny_edge", MIN_PREVIOUS_ROWS),
        ]);
        let delta = compute_delta(&prev, &rows(&[("lookup", 0), ("tiny_edge", 0)]), 1_000, 50);
        assert_eq!(
            delta
                .dropped
                .iter()
                .map(|d| d.table.as_str())
                .collect::<Vec<_>>(),
            vec!["tiny_edge"]
        );
    }

    #[test]
    fn zero_threshold_disables_all_warnings_but_keeps_the_totals() {
        let prev = previous(&[("gone", 500), ("shrunk", 500)]);
        let delta = compute_delta(&prev, &rows(&[("shrunk", 1)]), 1_000, 0);
        assert!(!delta.has_warnings());
        assert_eq!((delta.previous_total, delta.total), (1_000, 1));
    }

    #[test]
    fn percent_is_undefined_when_the_previous_total_is_zero() {
        let prev = previous(&[("a", 0)]);
        let delta = compute_delta(&prev, &rows(&[("a", 5)]), 1_000, 50);
        assert_eq!(delta.rows_change_percent(), None);
    }

    #[test]
    fn json_lists_only_the_warnings() {
        let prev = previous(&[("gone", 3), ("shrunk", 100)]);
        let delta = compute_delta(&prev, &rows(&[("shrunk", 10)]), 1_000, 50);
        let json = delta.to_json();
        assert_eq!(json["previous_id"], "prev");
        assert_eq!(json["vanished"], serde_json::json!(["gone"]));
        assert_eq!(json["dropped"][0]["table"], "shrunk");
        assert_eq!(json["dropped"][0]["previous"], 100);
        assert_eq!(json["dropped"][0]["now"], 10);
    }

    use crate::manifest::schema::{BackupManifest, Topology, FORMAT_VERSION};
    use crate::storage::LocalFs;

    const FORMAT: &str = "xb-test-v1";

    fn manifest(id: &str, created_at: &str, table_rows: Option<&[(&str, u64)]>) -> BackupManifest {
        BackupManifest {
            format_version: FORMAT_VERSION,
            id: id.to_string(),
            created_at: created_at.to_string(),
            backup_type: BackupType::Full,
            base_id: None,
            topology: Topology::Standalone,
            server_version: "x".into(),
            tool_versions: crate::manifest::schema::ToolVersions {
                mongodump: None,
                archive_format: Some(FORMAT.to_string()),
            },
            selective: false,
            original_size_bytes: 0,
            stored_size_bytes: 1_000,
            compression: None,
            encryption: None,
            checksum_sha256: "x".into(),
            oplog_range: None,
            oplog_count: None,
            promoted_from_gap: false,
            mysql_binlog: None,
            table_rows: table_rows.map(rows),
            table_digests: None,
            status: BackupStatus::Complete,
        }
    }

    async fn store_with(manifests: &[BackupManifest]) -> (tempfile::TempDir, LocalFs) {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        for m in manifests {
            ManifestStore::new(&fs).write(m).await.unwrap();
        }
        (dir, fs)
    }

    #[tokio::test]
    async fn find_previous_picks_the_latest_comparable_backup_before_the_current_one() {
        let rows_a: &[(&str, u64)] = &[("t", 1)];
        let rows_b: &[(&str, u64)] = &[("t", 2)];
        let (_dir, fs) = store_with(&[
            manifest("a", "2026-10-01T00:00:00Z", Some(rows_a)),
            manifest("b", "2026-10-02T00:00:00Z", Some(rows_b)),
            manifest("cur", "2026-10-03T00:00:00Z", Some(rows_b)),
            manifest("later", "2026-10-04T00:00:00Z", Some(rows_a)),
        ])
        .await;
        let previous = find_previous(&fs, "cur", FORMAT).await.unwrap().unwrap();
        assert_eq!(previous.id, "b", "현재보다 늦은 백업은 후보가 아니다");
        assert_eq!(previous.table_rows, rows(rows_b));
    }

    #[tokio::test]
    async fn find_previous_skips_other_engines_selective_incomplete_and_unrecorded() {
        let some: &[(&str, u64)] = &[("t", 1)];
        let mut other_engine = manifest("other", "2026-10-02T00:00:00Z", Some(some));
        other_engine.tool_versions.archive_format = Some("xb-other-v1".into());
        let mut selective = manifest("sel", "2026-10-02T01:00:00Z", Some(some));
        selective.selective = true;
        let mut incomplete = manifest("inc", "2026-10-02T02:00:00Z", Some(some));
        incomplete.status = BackupStatus::Incomplete;
        let mut incremental = manifest("incr", "2026-10-02T03:00:00Z", Some(some));
        incremental.backup_type = BackupType::Incremental;
        let unrecorded = manifest("old", "2026-10-02T04:00:00Z", None);
        let (_dir, fs) = store_with(&[
            other_engine,
            selective,
            incomplete,
            incremental,
            unrecorded,
            manifest("cur", "2026-10-03T00:00:00Z", Some(some)),
        ])
        .await;
        assert_eq!(find_previous(&fs, "cur", FORMAT).await.unwrap(), None);
    }

    #[tokio::test]
    async fn find_previous_is_none_for_the_first_backup() {
        let some: &[(&str, u64)] = &[("t", 1)];
        let (_dir, fs) = store_with(&[manifest("cur", "2026-10-03T00:00:00Z", Some(some))]).await;
        assert_eq!(find_previous(&fs, "cur", FORMAT).await.unwrap(), None);
    }
}
