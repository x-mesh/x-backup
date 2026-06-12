//! 통합 테스트 — 로컬 풀 백업 → 풀 복구 round-trip 정합(SC1).
//!
//! 환경에 `mongodump` + `mongorestore` + replica set(`tests/fixtures/replica-set.sh`)이
//! 필요하므로 `integration-tests` feature로 격리한다. 기본 CI 단위 테스트에는 포함되지
//! 않는다.
//!
//! 실행:
//! ```bash
//! tests/fixtures/replica-set.sh up
//! # 두 번째(타깃) replica set — 분리 복구 검증용(spike §3.2 패턴).
//! XB_RS_CONTAINER=x-backup-rs-restore XB_RS_PORT=27018 XB_RS_NAME=rs1 \
//!   tests/fixtures/replica-set.sh up
//!
//! XB_TEST_MONGO_URI="$(tests/fixtures/replica-set.sh uri)" \
//! XB_TEST_TARGET_URI="mongodb://localhost:27018/?replicaSet=rs1&directConnection=true" \
//!   cargo test --features integration-tests --test full_restore -- --ignored --nocapture
//!
//! tests/fixtures/replica-set.sh down
//! XB_RS_CONTAINER=x-backup-rs-restore tests/fixtures/replica-set.sh down
//! ```
//!
//! 도구 경로는 `mongodump`/`mongorestore`(PATH) 기본이며, `XB_MONGODUMP`/`XB_MONGORESTORE`
//! env로 spike 보고서 위치(직접 내려받은 tarball 등)를 지정할 수 있다(spike §2). 도구가
//! 없으면 테스트는 `#[ignore]`로 건너뛴다(아래 사유 명시).

#![cfg(feature = "integration-tests")]

use mongodb::bson::{doc, Document};
use mongodb::Client;

use x_backup::config::secret::Secret;
use x_backup::manifest::schema::{
    BackupManifest, BackupStatus, BackupType, Topology, FORMAT_VERSION,
};
use x_backup::manifest::store::{data_path, ManifestStore};
use x_backup::pipeline::backup::{run_full_backup, BackupRequest};
use x_backup::pipeline::restore::{run_restore, RestoreRequest};
use x_backup::pipeline::stage::StageStack;
use x_backup::storage::{BoxAsyncRead, LocalFs, Storage};

/// 백업 원본 URI(replica set). spike §3.0의 directConnection=true 필수.
fn source_uri() -> String {
    std::env::var("XB_TEST_MONGO_URI").unwrap_or_else(|_| {
        "mongodb://localhost:27017/?replicaSet=rs0&directConnection=true".to_string()
    })
}

/// 복구 타깃 URI. 미지정 시 source와 동일(드롭 후 동일 서버 복구). 분리 복구 검증은
/// 별도 타깃 URI를 주입한다(spike §3.2 rs1:27018 패턴).
fn target_uri() -> String {
    std::env::var("XB_TEST_TARGET_URI").unwrap_or_else(|_| source_uri())
}

fn mongodump_program() -> String {
    std::env::var("XB_MONGODUMP").unwrap_or_else(|_| "mongodump".to_string())
}

fn mongorestore_program() -> String {
    std::env::var("XB_MONGORESTORE").unwrap_or_else(|_| "mongorestore".to_string())
}

/// 시드 데이터셋: 결정적 문서들을 삽입하고, 복구 후 비교할 콘텐츠 해시를 계산한다.
const TEST_DB: &str = "xb_restore_it";
const TEST_COLL: &str = "items";
const DOC_COUNT: usize = 500;

/// 정렬된 문서들의 콘텐츠 해시(SC1: 콘텐츠 해시 일치). `_id` 오름차순으로 정렬한 뒤
/// 각 문서의 정준(canonical) BSON 바이트를 누적 sha256한다.
async fn collection_content_hash(client: &Client) -> (u64, String) {
    use futures::TryStreamExt;
    use sha2::{Digest, Sha256};

    let coll = client
        .database(TEST_DB)
        .collection::<Document>(TEST_COLL);
    let find_opts = mongodb::options::FindOptions::builder()
        .sort(doc! { "_id": 1 })
        .build();
    let docs: Vec<Document> = coll
        .find(doc! {})
        .with_options(find_opts)
        .await
        .expect("find")
        .try_collect()
        .await
        .expect("collect");

    let mut hasher = Sha256::new();
    for d in &docs {
        let bytes = mongodb::bson::to_vec(d).expect("bson encode");
        hasher.update(&bytes);
    }
    (docs.len() as u64, hex::encode(hasher.finalize()))
}

/// 시드: 기존 컬렉션을 drop하고 결정적 문서를 채운다.
async fn seed(client: &Client) {
    let coll = client
        .database(TEST_DB)
        .collection::<Document>(TEST_COLL);
    coll.drop().await.ok();
    let docs: Vec<Document> = (0..DOC_COUNT)
        .map(|i| doc! { "_id": i as i64, "n": (i * 7 % 101) as i64, "tag": format!("row-{i}") })
        .collect();
    coll.insert_many(docs).await.expect("insert_many");
}

/// 백업→복구 후 문서 수·콘텐츠 해시가 원본과 일치하는지 검증한다(SC1).
///
/// 절차:
/// 1. source에 결정적 데이터셋 시드 → 원본 (count, hash) 계산.
/// 2. run_full_backup으로 로컬에 풀 백업.
/// 3. (타깃 == source면) 컬렉션을 drop해 빈 상태로 만든 뒤 복구.
/// 4. run_restore(--force; drop 허용)로 복구.
/// 5. 타깃의 (count, hash)가 원본과 일치하는지 확인.
#[tokio::test]
#[ignore = "requires mongodump + mongorestore + replica set; run with --features integration-tests"]
async fn backup_then_restore_matches_counts_and_hash() {
    let src = source_uri();
    let tgt = target_uri();
    let separate_target = tgt != src;

    // 1) 시드 + 원본 해시.
    let src_client = Client::with_uri_str(&src).await.expect("source connect");
    seed(&src_client).await;
    let (orig_count, orig_hash) = collection_content_hash(&src_client).await;
    assert_eq!(orig_count, DOC_COUNT as u64, "시드 문서 수 불일치");

    // 2) 로컬 풀 백업(평문 — t5 단독 검증, reverse 스택 = identity).
    let dir = tempfile::tempdir().expect("temp dir");
    let storage = LocalFs::new(dir.path()).expect("local storage");
    let backup_request = BackupRequest {
        uri: Secret::new(src.clone()),
        mongodump_program: mongodump_program(),
        db: None,
        collection: None,
        progress_counter: None,
    };
    let backup = run_full_backup(&backup_request, &storage, StageStack::new())
        .await
        .expect("풀 백업 성공");

    // 3) 동일 서버 복구라면 먼저 컬렉션을 비워 복구 효과를 명확히 한다.
    let tgt_client = Client::with_uri_str(&tgt).await.expect("target connect");
    if !separate_target {
        tgt_client
            .database(TEST_DB)
            .collection::<Document>(TEST_COLL)
            .drop()
            .await
            .ok();
    } else {
        // 분리 타깃은 깨끗한 상태에서 시작하도록 대상 DB를 drop.
        tgt_client.database(TEST_DB).drop().await.ok();
    }

    // 4) 풀 복구(--force=drop 허용, skip-precheck로 충돌 점검 우회 — 빈 상태이므로 무해).
    let restore_request = RestoreRequest {
        target_uri: Secret::new(tgt.clone()),
        mongorestore_program: mongorestore_program(),
        backup_id: Some(backup.backup_id.clone()),
        only: None,
        force: true,
        dry_run: false,
        skip_precheck: false,
        progress_counter: None,
    };
    // 비-TTY(테스트) — confirm은 호출되지 않아야 한다(force=true).
    let outcome = run_restore(&restore_request, &storage, false, |_| {
        panic!("force=true이면 confirm 미호출")
    })
    .await
    .expect("풀 복구 성공");
    assert_eq!(outcome.backup_id, backup.backup_id);

    // 5) 복구 결과 정합 — 문서 수 + 콘텐츠 해시(SC1).
    let (restored_count, restored_hash) = collection_content_hash(&tgt_client).await;
    assert_eq!(restored_count, orig_count, "복구 후 문서 수 불일치");
    assert_eq!(restored_hash, orig_hash, "복구 후 콘텐츠 해시 불일치(SC1)");
}

/// dry-run은 실제 복구 없이 계획만 출력하고 대상을 변경하지 않는다(무부작용).
#[tokio::test]
#[ignore = "requires mongodump + replica set; run with --features integration-tests"]
async fn dry_run_does_not_modify_target() {
    let src = source_uri();
    let tgt = target_uri();

    let src_client = Client::with_uri_str(&src).await.expect("source connect");
    seed(&src_client).await;

    let dir = tempfile::tempdir().expect("temp dir");
    let storage = LocalFs::new(dir.path()).expect("local storage");
    let backup = run_full_backup(
        &BackupRequest {
            uri: Secret::new(src.clone()),
            mongodump_program: mongodump_program(),
            db: None,
            collection: None,
            progress_counter: None,
        },
        &storage,
        StageStack::new(),
    )
    .await
    .expect("풀 백업");

    // 타깃을 빈 상태로.
    let tgt_client = Client::with_uri_str(&tgt).await.expect("target connect");
    tgt_client.database(TEST_DB).drop().await.ok();
    let (before_count, _) = collection_content_hash(&tgt_client).await;
    assert_eq!(before_count, 0, "dry-run 전 타깃이 비어있어야 함");

    let request = RestoreRequest {
        target_uri: Secret::new(tgt.clone()),
        mongorestore_program: mongorestore_program(),
        backup_id: Some(backup.backup_id.clone()),
        only: None,
        force: false,
        dry_run: true,
        skip_precheck: false,
        progress_counter: None,
    };
    let outcome = run_restore(&request, &storage, false, |_| panic!("dry-run confirm 미호출"))
        .await
        .expect("dry-run 성공");

    // 계획은 채워지되 대상은 그대로 비어 있어야 한다(무부작용).
    assert_eq!(outcome.backup_id, backup.backup_id);
    let (after_count, _) = collection_content_hash(&tgt_client).await;
    assert_eq!(after_count, 0, "dry-run이 대상을 변경함(무부작용 위반)");
}

/// 복구 절반(t5)을 실제 mongorestore로 단독 검증한다(SC1) — 백업 산출물은 실제
/// `mongodump --archive=-` 출력을 LocalFs에 직접 저장해 준비한다.
///
/// 이 테스트는 t4 `run_full_backup`에 의존하지 않는다(백업 파이프라인의 다른 부분에
/// 영향받지 않고 *복구 경로*만 실측). storage.data.bin = 실제 archive 바이트 → 평문
/// reverse 스택(identity) → mongorestore stdin. 복구 후 문서 수·콘텐츠 해시가 원본과
/// 일치하는지 확인한다.
#[tokio::test]
#[ignore = "requires mongodump + mongorestore + replica set; run with --features integration-tests"]
async fn restore_half_matches_with_real_mongorestore() {
    let src = source_uri();
    let tgt = target_uri();

    // 1) 시드 + 원본 해시.
    let src_client = Client::with_uri_str(&src).await.expect("source connect");
    seed(&src_client).await;
    let (orig_count, orig_hash) = collection_content_hash(&src_client).await;
    assert_eq!(orig_count, DOC_COUNT as u64);

    // 2) 실제 mongodump --archive=- 출력을 LocalFs data.bin으로 직접 저장(백업 산출물 준비).
    let dir = tempfile::tempdir().expect("temp dir");
    let storage = LocalFs::new(dir.path()).expect("local storage");
    let backup_id = "it-restore-half";

    let dump_out = std::process::Command::new(mongodump_program())
        .arg(format!("--uri={src}"))
        .arg("--archive=-")
        .output()
        .expect("mongodump 실행");
    assert!(
        dump_out.status.success(),
        "mongodump 실패: {}",
        String::from_utf8_lossy(&dump_out.stderr)
    );
    let archive = dump_out.stdout;
    assert!(!archive.is_empty(), "archive가 비어 있음");

    let reader: BoxAsyncRead = Box::pin(std::io::Cursor::new(archive.clone()));
    storage
        .put_stream(&data_path(backup_id), reader, Some(archive.len() as u64))
        .await
        .expect("data.bin 저장");

    // 3) 평문 manifest 기록(compression/encryption None → reverse 스택 = identity).
    let manifest = BackupManifest {
        format_version: FORMAT_VERSION,
        id: backup_id.to_string(),
        created_at: "2026-06-12T00:00:00Z".to_string(),
        backup_type: BackupType::Full,
        base_id: None,
        topology: Topology::ReplicaSet,
        server_version: "7.0.35".to_string(),
        tool_versions: Default::default(),
        selective: false,
        original_size_bytes: archive.len() as u64,
        stored_size_bytes: archive.len() as u64,
        compression: None,
        encryption: None,
        checksum_sha256: {
            use sha2::{Digest, Sha256};
            hex::encode(Sha256::digest(&archive))
        },
        oplog_range: None,
        oplog_count: None,
        promoted_from_gap: false,
        status: BackupStatus::Complete,
    };
    ManifestStore::new(&storage)
        .write(&manifest)
        .await
        .expect("manifest 기록");

    // 4) 타깃을 비우고 풀 복구(--force=drop 허용).
    let tgt_client = Client::with_uri_str(&tgt).await.expect("target connect");
    tgt_client.database(TEST_DB).drop().await.ok();

    let request = RestoreRequest {
        target_uri: Secret::new(tgt.clone()),
        mongorestore_program: mongorestore_program(),
        backup_id: Some(backup_id.to_string()),
        only: None,
        force: true,
        dry_run: false,
        skip_precheck: false,
        progress_counter: None,
    };
    run_restore(&request, &storage, false, |_| panic!("force면 confirm 미호출"))
        .await
        .expect("풀 복구 성공");

    // 5) SC1: 문서 수 + 콘텐츠 해시 일치.
    let (restored_count, restored_hash) = collection_content_hash(&tgt_client).await;
    assert_eq!(restored_count, orig_count, "복구 후 문서 수 불일치");
    assert_eq!(restored_hash, orig_hash, "복구 후 콘텐츠 해시 불일치(SC1)");
}
