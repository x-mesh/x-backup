//! 통합 테스트 — 로컬 풀 백업 E2E(산출물 3종 존재 확인).
//!
//! 환경에 `mongodump` + replica set(`tests/fixtures/replica-set.sh`)이 필요하므로
//! `integration-tests` feature로 격리한다. 기본 CI 단위 테스트에는 포함되지 않는다.
//!
//! 실행:
//! ```bash
//! tests/fixtures/replica-set.sh up
//! XB_TEST_MONGO_URI="$(tests/fixtures/replica-set.sh uri)" \
//!   cargo test --features integration-tests --test full_backup -- --ignored --nocapture
//! tests/fixtures/replica-set.sh down
//! ```
//!
//! `XB_TEST_MONGO_URI`가 없으면 기본 fixture URI를 사용한다.

#![cfg(feature = "integration-tests")]

use x_backup::config::secret::Secret;
use x_backup::manifest::store::{data_path, manifest_path, manifest_sha_path, parse_sidecar};
use x_backup::manifest::ManifestStore;
use x_backup::pipeline::backup::{run_full_backup, BackupRequest, Engine};
use x_backup::pipeline::stage::StageStack;
use x_backup::storage::LocalFs;

fn test_uri() -> String {
    std::env::var("XB_TEST_MONGO_URI").unwrap_or_else(|_| {
        "mongodb://localhost:27017/?replicaSet=rs0&directConnection=true".to_string()
    })
}

/// 로컬 풀 백업 실행 후 data.bin + manifest.json + 사이드카 3종이 존재하고,
/// 사이드카가 manifest 바이트와 정합하며, manifest에 format_version·checksum이
/// 기록되는지 확인한다.
#[tokio::test]
#[ignore = "requires mongodump + replica set; run with --features integration-tests"]
async fn full_backup_produces_three_artifacts() {
    let dir = tempfile::tempdir().expect("temp dir");
    let storage = LocalFs::new(dir.path()).expect("local storage");

    let request = BackupRequest {
        uri: Secret::new(test_uri()),
        mongodump_program: "mongodump".to_string(),
        timeout_secs: None,
        db: None,
        collection: None,
        engine: Engine::Mongodump,
        progress_counter: None,
    };

    let outcome = run_full_backup(&request, &storage, StageStack::new())
        .await
        .expect("풀 백업 성공");

    let id = &outcome.backup_id;

    // 1) 산출물 3종이 디스크에 존재해야 한다.
    let base = dir.path().join(id);
    assert!(base.join("data.bin").exists(), "data.bin 없음");
    assert!(base.join("manifest.json").exists(), "manifest.json 없음");
    assert!(base.join("manifest.json.sha256").exists(), "사이드카 없음");

    // 2) manifest 내용 검증: format_version·checksum 기록.
    let store = ManifestStore::new(&storage);
    let manifest = store.read(id).await.expect("manifest 읽기");
    assert_eq!(manifest.format_version, 1);
    assert_eq!(manifest.checksum_sha256, outcome.checksum_sha256);
    assert_eq!(manifest.checksum_sha256.len(), 64);
    // replica set이면 oplog_range가 기록되어야 한다.
    assert!(
        manifest.oplog_range.is_some(),
        "replica set인데 oplog_range 미기록"
    );

    // 3) 사이드카가 manifest.json 바이트의 sha256과 정합.
    let sidecar = std::fs::read_to_string(base.join("manifest.json.sha256")).unwrap();
    let manifest_bytes = std::fs::read(base.join("manifest.json")).unwrap();
    let expected = {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(&manifest_bytes))
    };
    assert_eq!(
        parse_sidecar(&sidecar).unwrap(),
        expected,
        "사이드카 불일치"
    );

    // 4) data.bin 체크섬이 manifest와 일치(저장 바이트 = 체크섬 기준점).
    let data_bytes = std::fs::read(base.join("data.bin")).unwrap();
    let data_hash = {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(&data_bytes))
    };
    assert_eq!(
        data_hash, manifest.checksum_sha256,
        "data.bin 체크섬 불일치"
    );

    // mongodump 엔진은 archive를 해석하지 않으므로 문서 수를 기록하지 않는다.
    assert!(
        manifest.table_rows.is_none(),
        "mongodump 백업에 table_rows 기록됨"
    );

    // 경로 헬퍼 일관성(상대 경로).
    assert_eq!(data_path(id), format!("{id}/data.bin"));
    assert_eq!(manifest_path(id), format!("{id}/manifest.json"));
    assert_eq!(manifest_sha_path(id), format!("{id}/manifest.json.sha256"));
}

/// native 엔진 풀 백업은 아카이브에 쓴 컬렉션별 문서 수를 manifest `table_rows`
/// (`db.컬렉션` → 문서 수)에 남긴다. 빈 컬렉션도 0으로 기록한다.
#[tokio::test]
#[ignore = "requires a replica set; run with --features integration-tests"]
async fn native_full_backup_records_collection_rows() {
    const DB: &str = "xb_native_rows";
    let client = mongodb::Client::with_uri_str(&test_uri())
        .await
        .expect("connect");
    let db = client.database(DB);
    db.drop().await.ok();
    db.collection::<mongodb::bson::Document>("items")
        .insert_many((0..3).map(|i| mongodb::bson::doc! { "i": i }))
        .await
        .expect("insert_many");
    db.create_collection("empty").await.expect("create empty");

    let dir = tempfile::tempdir().expect("temp dir");
    let storage = LocalFs::new(dir.path()).expect("local storage");
    let request = BackupRequest {
        uri: Secret::new(test_uri()),
        mongodump_program: "mongodump".to_string(),
        timeout_secs: None,
        db: Some(DB.to_string()),
        collection: None,
        engine: Engine::Native,
        progress_counter: None,
    };
    let outcome = run_full_backup(&request, &storage, StageStack::new())
        .await
        .expect("native 풀 백업 성공");
    let manifest = ManifestStore::new(&storage)
        .read(&outcome.backup_id)
        .await
        .expect("manifest 읽기");
    db.drop().await.ok();

    assert_eq!(
        manifest.table_rows,
        Some(std::collections::BTreeMap::from([
            (format!("{DB}.empty"), 0u64),
            (format!("{DB}.items"), 3u64),
        ]))
    );
}
