//! 통합 테스트 — prune 체인 안전 삭제(FR-11) E2E.
//!
//! DB·외부 도구 없이 **손으로 만든 백업 체인**(LocalFs tempdir)으로 검증한다. 가짜
//! manifest로 base+증분 체인을 구성하고, prune의 순수 판정([`plan_prune`])과 실삭제
//! ([`execute_prune`])가 다음을 지키는지 확인한다:
//! - `--dry-run` 상응: plan만 만들고 산출물은 그대로(execute 미호출).
//! - base+증분 체인 단위 삭제(부분 삭제로 체인이 깨지지 않음).
//! - 살아있는 증분이 참조하는 base 보호.
//!
//! 기본 단위 테스트로 실행된다(feature gate 없음).

use tokio::io::AsyncReadExt;

use x_backup::manifest::schema::{
    BackupManifest, BackupStatus, BackupType, OplogRange, OplogTimestamp, Topology, FORMAT_VERSION,
};
use x_backup::manifest::store::{data_path, manifest_path, ManifestStore};
use x_backup::pipeline::prune::{execute_prune, load_backups, plan_prune, RetentionPolicy};
use x_backup::storage::{BoxAsyncRead, LocalFs, Storage};

const DAY: i64 = 86_400;

/// epoch 초 → RFC3339.
fn at(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0)
        .unwrap()
        .to_rfc3339()
}

/// 풀백업 manifest를 만든다.
fn full(id: &str, created_secs: i64) -> BackupManifest {
    BackupManifest {
        format_version: FORMAT_VERSION,
        id: id.to_string(),
        created_at: at(created_secs),
        backup_type: BackupType::Full,
        base_id: None,
        topology: Topology::ReplicaSet,
        server_version: "7.0.35".into(),
        tool_versions: Default::default(),
        selective: false,
        original_size_bytes: 4,
        stored_size_bytes: 4,
        compression: None,
        encryption: None,
        checksum_sha256: "x".into(),
        oplog_range: Some(OplogRange {
            start_ts: OplogTimestamp::new(created_secs as u32, 1),
            end_ts: OplogTimestamp::new(created_secs as u32, 1),
        }),
        oplog_count: None,
        promoted_from_gap: false,
        mysql_binlog: None,
        status: BackupStatus::Complete,
    }
}

/// 증분 manifest(연속 체인용 — start=prev_end, end=created).
fn incr(id: &str, base: &str, prev_end: u32, created_secs: i64) -> BackupManifest {
    BackupManifest {
        base_id: Some(base.to_string()),
        backup_type: BackupType::Incremental,
        oplog_range: Some(OplogRange {
            start_ts: OplogTimestamp::new(prev_end, 1),
            end_ts: OplogTimestamp::new(created_secs as u32, 1),
        }),
        ..full(id, created_secs)
    }
}

/// data.bin + manifest를 기록한다.
async fn seed(fs: &LocalFs, m: &BackupManifest) {
    let reader: BoxAsyncRead = Box::pin(std::io::Cursor::new(b"data".to_vec()));
    fs.put_stream(&data_path(&m.id), reader, Some(4))
        .await
        .unwrap();
    ManifestStore::new(fs).write(m).await.unwrap();
}

/// 경로 존재 여부.
async fn exists(fs: &LocalFs, path: &str) -> bool {
    fs.get_stream(path).await.is_ok()
}

/// dry-run 상응: plan만 만들고 execute하지 않으면 산출물이 그대로 남는다.
#[tokio::test]
async fn dry_run_does_not_delete_anything() {
    let dir = tempfile::tempdir().unwrap();
    let fs = LocalFs::new(dir.path()).unwrap();
    seed(&fs, &full("base-old", DAY)).await;
    seed(&fs, &full("base-new", 100 * DAY)).await;

    let (manifests, orphans) = load_backups(&fs).await.unwrap();
    let policy = RetentionPolicy {
        keep_full: Some(1),
        keep_days: None,
        keep_last: None,
    };
    let plan = plan_prune(&manifests, &orphans, policy, 200 * DAY);

    // 계획상 base-old는 삭제 대상이지만, execute_prune을 부르지 않으면(=dry-run) 그대로.
    assert!(plan.targets.iter().any(|t| t.base_id == "base-old"));
    assert!(exists(&fs, &manifest_path("base-old")).await);
    assert!(exists(&fs, &data_path("base-old")).await);
}

/// base+증분 체인을 단위로 삭제한다(부분 삭제로 체인이 깨지지 않음).
#[tokio::test]
async fn deletes_full_chain_as_unit() {
    let dir = tempfile::tempdir().unwrap();
    let fs = LocalFs::new(dir.path()).unwrap();
    // 오래된 체인: base + 증분 2개.
    seed(&fs, &full("old-base", DAY)).await;
    seed(&fs, &incr("old-i1", "old-base", DAY as u32, 2 * DAY)).await;
    seed(&fs, &incr("old-i2", "old-base", 2 * DAY as u32, 3 * DAY)).await;
    // 최신 체인(보존 대상).
    seed(&fs, &full("new-base", 100 * DAY)).await;

    let (manifests, orphans) = load_backups(&fs).await.unwrap();
    let policy = RetentionPolicy {
        keep_full: Some(1), // 최신 1개 체인만 보존 → old 체인 삭제.
        keep_days: None,
        keep_last: None,
    };
    let plan = plan_prune(&manifests, &orphans, policy, 200 * DAY);
    let outcome = execute_prune(&fs, &plan, false).await.unwrap();

    // old 체인(base+증분2) 전부 삭제, new-base 보존.
    assert_eq!(outcome.deleted_chains, 1);
    assert_eq!(outcome.deleted_backups, 3);
    for id in ["old-base", "old-i1", "old-i2"] {
        assert!(!exists(&fs, &manifest_path(id)).await, "{id} manifest 남음");
        assert!(!exists(&fs, &data_path(id)).await, "{id} data 남음");
    }
    assert!(
        exists(&fs, &manifest_path("new-base")).await,
        "new-base 보존 실패"
    );
}

/// 살아있는 증분이 참조하는 base는 보호된다(체인 단위 보존).
#[tokio::test]
async fn live_incremental_protects_base() {
    let dir = tempfile::tempdir().unwrap();
    let fs = LocalFs::new(dir.path()).unwrap();
    // 오래된 base지만 최근 증분이 붙어 있다 → keep-days로 체인 전체 보존.
    seed(&fs, &full("base", DAY)).await;
    seed(&fs, &incr("recent-i", "base", DAY as u32, 95 * DAY)).await;

    let (manifests, orphans) = load_backups(&fs).await.unwrap();
    let policy = RetentionPolicy {
        keep_full: None,
        keep_days: Some(30), // now=100일 → cutoff=70일 → 최근 증분(95일) 보존 → base 보호.
        keep_last: None,
    };
    let plan = plan_prune(&manifests, &orphans, policy, 100 * DAY);
    let outcome = execute_prune(&fs, &plan, true).await.unwrap();

    // 아무것도 삭제되지 않아야 한다(base가 최근 증분에 의해 보호됨).
    assert_eq!(outcome.deleted_backups, 0);
    assert!(
        exists(&fs, &manifest_path("base")).await,
        "base가 삭제됨(보호 실패)"
    );
    assert!(exists(&fs, &manifest_path("recent-i")).await);
}

/// orphan(manifest 없는 data)은 --force일 때만 삭제된다.
#[tokio::test]
async fn orphan_deleted_only_with_force() {
    let dir = tempfile::tempdir().unwrap();
    let fs = LocalFs::new(dir.path()).unwrap();
    seed(&fs, &full("keep", 100 * DAY)).await;
    // manifest 없는 유령 data.
    let ghost: BoxAsyncRead = Box::pin(std::io::Cursor::new(b"ghost".to_vec()));
    fs.put_stream(&data_path("ghost"), ghost, None)
        .await
        .unwrap();

    let (manifests, orphans) = load_backups(&fs).await.unwrap();
    assert_eq!(orphans, vec!["ghost".to_string()]);
    let policy = RetentionPolicy {
        keep_full: Some(10), // keep는 보존.
        keep_days: None,
        keep_last: None,
    };
    let plan = plan_prune(&manifests, &orphans, policy, 200 * DAY);

    // force 미포함: orphan은 안 지워진다.
    execute_prune(&fs, &plan, false).await.unwrap();
    assert!(
        exists(&fs, &data_path("ghost")).await,
        "force 없이 orphan 삭제됨"
    );

    // force 포함: orphan 삭제.
    execute_prune(&fs, &plan, true).await.unwrap();
    assert!(
        !exists(&fs, &data_path("ghost")).await,
        "force로도 orphan 안 지워짐"
    );
}

/// 빈 증분 슬라이스(data.bin 부재)도 manifest 삭제로 안전 처리된다(NotFound 무시).
#[tokio::test]
async fn empty_slice_manifest_only_pruned() {
    let dir = tempfile::tempdir().unwrap();
    let fs = LocalFs::new(dir.path()).unwrap();
    seed(&fs, &full("old", DAY)).await;
    // 빈 슬라이스 증분: manifest만 기록(data.bin 없음).
    let mut empty = incr("empty-i", "old", DAY as u32, 2 * DAY);
    empty.oplog_count = Some(0);
    empty.stored_size_bytes = 0;
    empty.original_size_bytes = 0;
    ManifestStore::new(&fs).write(&empty).await.unwrap(); // data.bin 미기록.
    seed(&fs, &full("new", 100 * DAY)).await;

    let (manifests, orphans) = load_backups(&fs).await.unwrap();
    let policy = RetentionPolicy {
        keep_full: Some(1),
        keep_days: None,
        keep_last: None,
    };
    let plan = plan_prune(&manifests, &orphans, policy, 200 * DAY);
    // old 체인(base + 빈 슬라이스)이 삭제 대상. data 없는 빈 슬라이스도 에러 없이 처리.
    execute_prune(&fs, &plan, false).await.unwrap();

    assert!(!exists(&fs, &manifest_path("old")).await);
    assert!(!exists(&fs, &manifest_path("empty-i")).await);
    assert!(exists(&fs, &manifest_path("new")).await);
}

/// 삭제 순서 불변: 단일 백업 안에서 manifest가 마지막에 사라진다(중간 실패 시 추적성).
///
/// 실제 부분 실패를 주입하기 어려우므로, 정상 삭제 후 data/manifest가 모두 사라졌음을
/// 확인하는 것으로 회귀를 막는다(순서 자체는 단위 테스트 delete_order가 검증).
#[tokio::test]
async fn full_artifact_removed_after_prune() {
    let dir = tempfile::tempdir().unwrap();
    let fs = LocalFs::new(dir.path()).unwrap();
    seed(&fs, &full("gone", DAY)).await;
    seed(&fs, &full("keep", 100 * DAY)).await;

    let (manifests, orphans) = load_backups(&fs).await.unwrap();
    let policy = RetentionPolicy {
        keep_full: Some(1),
        keep_days: None,
        keep_last: None,
    };
    let plan = plan_prune(&manifests, &orphans, policy, 200 * DAY);
    execute_prune(&fs, &plan, false).await.unwrap();

    // gone의 data.bin·manifest.json·사이드카가 모두 제거된다.
    assert!(!exists(&fs, &data_path("gone")).await);
    assert!(!exists(&fs, &manifest_path("gone")).await);
    let mut sidecar = String::new();
    let sidecar_res = fs
        .get_stream("gone/manifest.json.sha256")
        .await
        .map(|mut r| async move {
            let _ = r.read_to_string(&mut sidecar).await;
        });
    assert!(sidecar_res.is_err(), "사이드카가 남아 있음");
}
