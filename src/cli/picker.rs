//! 인터랙티브 백업 선택 — `restore --id` 미지정 + TTY일 때 fuzzy 피커로 복구할 백업을 고른다.
//!
//! 후보는 **풀 + 완료(Full + Complete)** 백업만 보여준다 — 증분·미완료는 단독 복구 베이스로
//! 부적격이기 때문이다. 최신순으로 정렬해 첫 항목(가장 최근)이 피커 기본 선택이 된다.
//!
//! ## 분리 설계(테스트 용이성)
//! 후보 구성([`full_backup_choices`], Storage 주입 → 단위 테스트 가능)과 표시([`pick_backup`],
//! dialoguer FuzzySelect — TTY 필요)를 분리한다. 표시는 stderr 기준 터미널에 그려지고
//! 선택 결과(백업 ID)만 반환하므로 stdout(결과·--json)을 오염시키지 않는다.

use dialoguer::{theme::ColorfulTheme, FuzzySelect};

use crate::engine::mongo::status::human_bytes;
use crate::error::{Result, XBackupError};
use crate::manifest::schema::{BackupManifest, BackupStatus, BackupType};
use crate::manifest::store::ManifestStore;
use crate::pipeline::verify::collect_manifest_ids;
use crate::storage::Storage;

/// 피커에 보여줄 복구 후보 한 건.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupChoice {
    /// 백업 ID(선택 결과로 반환되는 값).
    pub id: String,
    /// 한 줄 표시 라벨(id·유형·DB·생성시각·크기) — fuzzy 매칭 대상이기도 하다.
    pub label: String,
}

/// 복구 베이스로 유효한 후보(Full + Complete)를 **최신순**으로 모은다.
///
/// 최신순 = `created_at` 내림차순(동률이면 id 내림차순 — UUID v7라 사실상 동일 순서).
/// 첫 항목이 가장 최근이라 피커 기본 선택이 된다. manifest 읽기에 실패한 ID는 건너뛴다
/// (list 카탈로그와 동일 정책 — 깨진 한 건이 전체 선택을 막지 않게).
pub async fn full_backup_choices(storage: &dyn Storage) -> Result<Vec<BackupChoice>> {
    let ids = collect_manifest_ids(storage).await?;
    let store = ManifestStore::new(storage);

    let mut manifests = Vec::with_capacity(ids.len());
    for id in ids {
        match store.read(&id).await {
            // 풀 + 완료만 — 증분/미완료는 단독 복구 베이스로 부적격.
            Ok(m) if m.backup_type == BackupType::Full && m.status == BackupStatus::Complete => {
                manifests.push(m);
            }
            Ok(_) => {}
            Err(e) => tracing::debug!(id = %id, "manifest 읽기 실패(피커에서 제외): {e}"),
        }
    }

    // 최신순: created_at desc, 동률이면 id desc.
    manifests.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.id.cmp(&a.id))
    });

    Ok(manifests
        .into_iter()
        .map(|m| BackupChoice {
            label: format_choice(&m),
            id: m.id,
        })
        .collect())
}

/// 후보 한 줄 라벨: `<id>  full  <db> v<server>  <created>  <size>`.
///
/// `server_version`을 넣어 "어느 서버에서 뜬 백업인지"를 행에서 바로 구분할 수 있게 한다
/// (피커는 한 프로파일의 저장소를 보지만, 시점마다 서버 버전이 다를 수 있다).
fn format_choice(m: &BackupManifest) -> String {
    let engine = match m.tool_versions.archive_format.as_deref() {
        Some(f) if f.starts_with("xb-pg") => "postgresql",
        _ => "mongodb",
    };
    // gap 승격 full은 일반 full과 구분 표시(증분 요청이 풀로 폴백된 백업).
    let kind = if m.promoted_from_gap {
        "full(gap)"
    } else {
        "full"
    };
    format!(
        "{}  {}  {} v{}  {}  {}",
        m.id,
        kind,
        engine,
        m.server_version,
        short_created(&m.created_at),
        human_bytes(m.stored_size_bytes as i64),
    )
}

/// 생성 시각을 초 단위까지 축약(`2026-06-14 14:56:11`) — RFC3339의 마이크로초·TZ는 생략.
fn short_created(ts: &str) -> String {
    ts.replacen('T', " ", 1).chars().take(19).collect()
}

/// fuzzy 피커를 띄워 백업 하나를 고른다(기본=최신=index 0). 사용자가 취소(Esc)하면 `None`.
///
/// 후보가 비어 있으면 호출하지 않는다(호출 측에서 폴백 처리). dialoguer는 stderr 기준
/// 터미널에 그리므로 stdout(결과·--json)을 오염시키지 않는다.
pub fn pick_backup(choices: &[BackupChoice], lang: crate::i18n::Lang) -> Result<Option<String>> {
    let labels: Vec<&str> = choices.iter().map(|c| c.label.as_str()).collect();
    let selection = FuzzySelect::with_theme(&ColorfulTheme::default())
        .with_prompt(lang.sel(
            "Select backup to restore (type=filter, ↑↓=move, Enter=select, Esc=cancel)",
            "복구할 백업 선택 (타이핑=필터, ↑↓=이동, Enter=선택, Esc=취소)",
        ))
        .items(&labels)
        .default(0)
        .interact_opt()
        .map_err(|e| XBackupError::Usage(format!("백업 선택 입력 실패: {e}")))?;
    Ok(selection.map(|i| choices[i].id.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::schema::{ToolVersions, Topology, FORMAT_VERSION};
    use crate::storage::LocalFs;

    fn manifest(
        id: &str,
        created_at: &str,
        ty: BackupType,
        status: BackupStatus,
    ) -> BackupManifest {
        BackupManifest {
            format_version: FORMAT_VERSION,
            id: id.to_string(),
            created_at: created_at.to_string(),
            backup_type: ty,
            base_id: None,
            topology: Topology::ReplicaSet,
            server_version: "7.0.35".to_string(),
            tool_versions: ToolVersions::default(),
            selective: false,
            original_size_bytes: 100,
            stored_size_bytes: 2_000_000,
            compression: None,
            encryption: None,
            checksum_sha256: "abc".to_string(),
            oplog_range: None,
            oplog_count: None,
            promoted_from_gap: false,
            status,
        }
    }

    async fn write(fs: &LocalFs, m: &BackupManifest) {
        ManifestStore::new(fs).write(m).await.unwrap();
    }

    /// 후보는 풀+완료만, 최신순으로 모은다(증분·미완료는 제외).
    #[tokio::test]
    async fn collects_full_complete_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        // 오래된 풀, 최신 풀, 증분(제외), 미완료 풀(제외).
        write(
            &fs,
            &manifest(
                "a-old",
                "2026-06-10T00:00:00Z",
                BackupType::Full,
                BackupStatus::Complete,
            ),
        )
        .await;
        write(
            &fs,
            &manifest(
                "b-new",
                "2026-06-14T00:00:00Z",
                BackupType::Full,
                BackupStatus::Complete,
            ),
        )
        .await;
        write(
            &fs,
            &manifest(
                "c-incr",
                "2026-06-15T00:00:00Z",
                BackupType::Incremental,
                BackupStatus::Complete,
            ),
        )
        .await;
        write(
            &fs,
            &manifest(
                "d-partial",
                "2026-06-16T00:00:00Z",
                BackupType::Full,
                BackupStatus::Incomplete,
            ),
        )
        .await;

        let choices = full_backup_choices(&fs).await.unwrap();

        // 풀+완료 2건만, 최신순(b-new가 먼저).
        let ids: Vec<&str> = choices.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["b-new", "a-old"], "풀+완료만 최신순");
    }

    /// 후보가 없으면 빈 목록(호출 측이 폴백 판단).
    #[tokio::test]
    async fn empty_when_no_full_complete() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        write(
            &fs,
            &manifest(
                "c-incr",
                "2026-06-15T00:00:00Z",
                BackupType::Incremental,
                BackupStatus::Complete,
            ),
        )
        .await;
        let choices = full_backup_choices(&fs).await.unwrap();
        assert!(choices.is_empty());
    }

    /// 라벨 포맷: id·full·db·축약시각·사람단위 크기를 담는다.
    #[test]
    fn label_format_is_human_readable() {
        let m = manifest(
            "bk-1",
            "2026-06-14T14:56:11.354264+00:00",
            BackupType::Full,
            BackupStatus::Complete,
        );
        let label = format_choice(&m);
        assert!(label.contains("bk-1"), "id 포함: {label}");
        assert!(label.contains("full"), "유형 포함: {label}");
        assert!(label.contains("mongodb"), "DB 엔진 포함: {label}");
        assert!(label.contains("v7.0.35"), "서버 버전 포함: {label}");
        assert!(label.contains("2026-06-14 14:56:11"), "축약 시각: {label}");
        assert!(!label.contains('T'), "RFC3339 T는 공백으로 치환: {label}");
    }

    /// gap 승격 full은 라벨에서 `full(gap)`으로 구분 표시된다.
    #[test]
    fn label_marks_gap_promoted_full() {
        let mut m = manifest(
            "bk-2",
            "2026-06-14T14:56:11+00:00",
            BackupType::Full,
            BackupStatus::Complete,
        );
        m.promoted_from_gap = true;
        let label = format_choice(&m);
        assert!(label.contains("full(gap)"), "gap 승격 표식: {label}");
    }
}
