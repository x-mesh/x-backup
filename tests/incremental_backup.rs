//! 통합 테스트 — 증분(oplog) 백업 E2E(R3/R4, t8).
//!
//! 환경에 `mongodump` + replica set(`tests/fixtures/replica-set.sh`)이 필요하므로
//! `integration-tests` feature로 격리한다. 기본 CI 단위 테스트에는 포함되지 않는다.
//!
//! 실행:
//! ```bash
//! tests/fixtures/replica-set.sh up
//! XB_TEST_MONGO_URI="$(tests/fixtures/replica-set.sh uri)" \
//!   cargo test --features integration-tests --test incremental_backup -- --ignored --nocapture
//! tests/fixtures/replica-set.sh down
//! ```
//!
//! 검증 항목(DoD):
//! - 증분 manifest 체인(base_id·oplog_range·oplog_count 기록).
//! - gap 경로(증분 거부 + 풀 승격 + exit 4) — **base 기준점을 oplog 최소 ts보다 작은
//!   가짜 값으로 조작**해 동일 코드 경로(gap → 승격)를 강제한다(오플로그 강제 롤오버가
//!   어려워 채택한 방식 — 보고서에 명시).
//! - applyOps 실측 — 멀티도큐먼트 트랜잭션 1건이 증분 슬라이스에 온전히 담기는지.

#![cfg(feature = "integration-tests")]

use bson::{doc, Timestamp};
use mongodb::options::ClientOptions;
use mongodb::Client;

use x_backup::config::secret::Secret;
use x_backup::manifest::schema::{
    BackupManifest, BackupStatus, BackupType, OplogRange, OplogTimestamp, Topology, ToolVersions,
    FORMAT_VERSION,
};
use x_backup::manifest::store::ManifestStore;
use x_backup::pipeline::backup::{run_full_backup, BackupMeta, BackupRequest};
use x_backup::pipeline::incremental::{
    run_incremental_backup, IncrementalOutcome, IncrementalRequest,
};
use x_backup::pipeline::stage::StageStack;
use x_backup::storage::LocalFs;

fn test_uri() -> String {
    std::env::var("XB_TEST_MONGO_URI").unwrap_or_else(|_| {
        "mongodb://localhost:27017/?replicaSet=rs0&directConnection=true".to_string()
    })
}

/// 평문 StageStack 팩토리(압축·암호화 없음 — 통합 테스트에서 산출물 바이트를 직접 검증).
fn identity_factory() -> impl Fn() -> x_backup::Result<(StageStack, BackupMeta)> {
    || Ok((StageStack::new(), BackupMeta::none()))
}

/// 드라이버 클라이언트로 testdb.items에 문서를 넣는다(증분 캡처 대상 쓰기 생성).
async fn insert_docs(uri: &str, n: usize, tag: &str) {
    let opts = ClientOptions::parse(uri).await.unwrap();
    let client = Client::with_options(opts).unwrap();
    let coll = client.database("testdb").collection::<bson::Document>("items");
    let docs: Vec<bson::Document> = (0..n)
        .map(|i| doc! { "tag": tag, "n": i as i64 })
        .collect();
    coll.insert_many(docs).await.unwrap();
}

/// 멀티도큐먼트 트랜잭션 1건을 커밋한다(applyOps 엔트리 생성 — 경계 실측).
async fn run_multidoc_transaction(uri: &str, tag: &str) {
    use mongodb::options::{Acknowledgment, ReadConcern, TransactionOptions, WriteConcern};

    let opts = ClientOptions::parse(uri).await.unwrap();
    let client = Client::with_options(opts).unwrap();
    let mut session = client.start_session().await.unwrap();

    let tx_opts = TransactionOptions::builder()
        .read_concern(ReadConcern::snapshot())
        .write_concern(WriteConcern::builder().w(Acknowledgment::Majority).build())
        .build();
    session.start_transaction().with_options(tx_opts).await.unwrap();

    let coll = client.database("testdb").collection::<bson::Document>("txn");
    // 트랜잭션 안에서 여러 문서를 삽입 → 커밋 시 applyOps oplog 엔트리로 묶인다.
    for i in 0..5 {
        coll.insert_one(doc! { "tag": tag, "k": i as i64 })
            .session(&mut session)
            .await
            .unwrap();
    }
    session.commit_transaction().await.unwrap();
}

/// SC2 핵심 + 체인: 풀 백업 → 쓰기 → 증분 → manifest 체인(base_id·oplog_range·count) 검증.
/// 추가로 멀티도큐먼트 트랜잭션 1건(applyOps)을 증분 구간에 포함시켜 캡처를 실측한다.
#[tokio::test]
#[ignore = "requires mongodump + replica set; run with --features integration-tests"]
async fn full_then_incremental_records_chain_and_captures_txn() {
    let uri = test_uri();
    let dir = tempfile::tempdir().expect("temp dir");
    let storage = LocalFs::new(dir.path()).expect("local storage");

    // 0) 초기 시드(풀 백업이 담을 데이터).
    insert_docs(&uri, 20, "seed").await;

    // 1) 풀 백업 — 이후 증분의 base가 된다.
    let full_req = BackupRequest {
        uri: Secret::new(uri.clone()),
        mongodump_program: "mongodump".to_string(),
        db: None,
        collection: None,
    };
    let full = run_full_backup(&full_req, &storage, StageStack::new())
        .await
        .expect("풀 백업 성공");
    let base_manifest = ManifestStore::new(&storage)
        .read(&full.backup_id)
        .await
        .expect("base manifest");
    assert!(
        base_manifest.oplog_range.is_some(),
        "풀 백업 base에 oplog_range가 있어야 증분 기준점이 된다"
    );

    // 2) 풀 백업 이후 쓰기 — 증분이 캡처할 변경(일반 insert + 멀티도큐먼트 트랜잭션).
    insert_docs(&uri, 30, "after-full").await;
    run_multidoc_transaction(&uri, "txn-1").await; // applyOps 엔트리 발생.
    insert_docs(&uri, 5, "after-txn").await;

    // 3) 증분 백업.
    let incr_req = IncrementalRequest {
        uri: Secret::new(uri.clone()),
        mongodump_program: "mongodump".to_string(),
    };
    let outcome = run_incremental_backup(&incr_req, &storage, identity_factory())
        .await
        .expect("증분 백업 성공");

    let (incr_id, base_id, count) = match outcome {
        IncrementalOutcome::Captured {
            backup_id,
            base_id,
            oplog_count,
            ..
        } => (backup_id, base_id, oplog_count),
        IncrementalOutcome::PromotedToFull { reason, .. } => {
            panic!("정상 윈도우에서 증분이 gap 승격됨(예상치 못함): {reason}")
        }
    };

    // 4) 체인 검증 — base 연결·증분 엔트리 캡처.
    assert_eq!(base_id, full.backup_id, "증분 base_id가 풀 백업 ID와 일치해야 함");
    assert!(count > 0, "풀 이후 쓰기가 있었으므로 oplog 엔트리가 캡처되어야 함");

    let incr_manifest = ManifestStore::new(&storage)
        .read(&incr_id)
        .await
        .expect("증분 manifest");
    assert_eq!(incr_manifest.backup_type, BackupType::Incremental);
    assert_eq!(incr_manifest.base_id.as_deref(), Some(full.backup_id.as_str()));
    assert_eq!(incr_manifest.oplog_count, Some(count));
    let range = incr_manifest.oplog_range.expect("증분 oplog_range");
    // 증분 시작 기준점 = base의 oplog_range.end_ts.
    assert_eq!(
        range.start_ts,
        base_manifest.oplog_range.unwrap().end_ts,
        "증분 start_ts가 base end_ts를 이어받아야 함"
    );
    assert!(range.end_ts >= range.start_ts, "캡처 끝 ts >= 시작 ts");

    // 5) data.bin이 존재하고 캡처 바이트가 비어 있지 않아야 한다(엔트리 > 0).
    let data = dir.path().join(&incr_id).join("data.bin");
    assert!(data.exists(), "증분 data.bin 없음");
    let data_bytes = std::fs::read(&data).unwrap();
    assert!(!data_bytes.is_empty(), "캡처 엔트리가 있는데 data.bin이 비었음");
    // 체크섬 정합(저장 바이트 = manifest checksum).
    let hash = {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(&data_bytes))
    };
    assert_eq!(hash, incr_manifest.checksum_sha256, "data.bin 체크섬 불일치");

    // 6) applyOps 실측 — 캡처 바이트에 트랜잭션 마커가 담겼는지 확인한다.
    //    raw oplog BSON 스트림이므로 applyOps 네임스페이스/필드 문자열이 들어 있어야 한다.
    let has_apply_ops = contains_subslice(&data_bytes, b"applyOps");
    let has_txn_tag = contains_subslice(&data_bytes, b"txn-1");
    assert!(
        has_apply_ops,
        "멀티도큐먼트 트랜잭션의 applyOps 엔트리가 캡처에 담겨야 함(applyOps 마커 부재)"
    );
    assert!(
        has_txn_tag,
        "트랜잭션 문서 태그(txn-1)가 캡처에 담겨야 함"
    );
}

/// 빈 슬라이스: 풀 백업 직후(변경 없음) 증분 → data 없이 manifest만(oplog_count=0).
#[tokio::test]
#[ignore = "requires mongodump + replica set; run with --features integration-tests"]
async fn incremental_empty_slice_records_manifest_only() {
    let uri = test_uri();
    let dir = tempfile::tempdir().expect("temp dir");
    let storage = LocalFs::new(dir.path()).expect("local storage");

    insert_docs(&uri, 10, "seed").await;

    let full_req = BackupRequest {
        uri: Secret::new(uri.clone()),
        mongodump_program: "mongodump".to_string(),
        db: None,
        collection: None,
    };
    let full = run_full_backup(&full_req, &storage, StageStack::new())
        .await
        .expect("풀 백업 성공");

    // 풀 백업 직후 — 변경이 없도록 곧바로 증분(빈 슬라이스 유도). 단, 풀 백업 dump 중
    // 발생한 내부 oplog 엔트리로 인해 소량 캡처될 수 있어 0 또는 소수 모두 허용하되,
    // 0건 경로(빈 슬라이스)일 때 계약(data 부재)을 검증한다.
    let incr_req = IncrementalRequest {
        uri: Secret::new(uri.clone()),
        mongodump_program: "mongodump".to_string(),
    };
    let outcome = run_incremental_backup(&incr_req, &storage, identity_factory())
        .await
        .expect("증분 백업 성공");

    if let IncrementalOutcome::Captured {
        backup_id,
        oplog_count,
        stored_size_bytes,
        ..
    } = outcome
    {
        let _ = full;
        if oplog_count == 0 {
            // 빈 슬라이스 계약: manifest 있음, data.bin 없음, stored=0.
            let base = dir.path().join(&backup_id);
            assert!(base.join("manifest.json").exists(), "빈 슬라이스 manifest 없음");
            assert!(
                !base.join("data.bin").exists(),
                "빈 슬라이스인데 data.bin이 존재함"
            );
            assert_eq!(stored_size_bytes, 0);
            let m = ManifestStore::new(&storage).read(&backup_id).await.unwrap();
            assert_eq!(m.oplog_count, Some(0));
            assert_eq!(m.stored_size_bytes, 0);
            assert_eq!(m.oplog_range.unwrap().start_ts, m.oplog_range.unwrap().end_ts);
        } else {
            // 0건이 아니면(풀 dump 중 내부 쓰기) 일반 증분 계약을 검증한다.
            assert!(stored_size_bytes > 0, "엔트리가 있는데 stored가 0");
        }
    } else {
        panic!("빈 슬라이스 경로에서 풀 승격은 예상치 못함");
    }
}

/// 작은 부분 슬라이스도 정합한지 — 부분 문자열 검색 헬퍼.
fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// 가짜 base manifest를 storage에 직접 기록한다(gap 시나리오용 픽스처).
async fn write_fake_base(storage: &LocalFs, id: &str, end_ts: Timestamp) {
    let m = BackupManifest {
        format_version: FORMAT_VERSION,
        id: id.into(),
        created_at: "2026-06-12T00:00:00Z".into(),
        backup_type: BackupType::Full,
        base_id: None,
        topology: Topology::ReplicaSet,
        server_version: "7.0.35".into(),
        tool_versions: ToolVersions::default(),
        selective: false,
        original_size_bytes: 1,
        stored_size_bytes: 1,
        compression: None,
        encryption: None,
        checksum_sha256: "fake".into(),
        oplog_range: Some(OplogRange {
            start_ts: OplogTimestamp::new(1, 1),
            end_ts: end_ts.into(),
        }),
        oplog_count: None,
        promoted_from_gap: false,
        status: BackupStatus::Complete,
    };
    // data.bin도 같이 둔다(승격 풀 백업과 ID가 겹치지 않게 별도 디렉터리).
    ManifestStore::new(storage).write(&m).await.unwrap();
}

/// SC2 — gap 경로: base 기준점을 oplog 최소 ts보다 **작은 가짜 값**으로 조작해
/// gap → 증분 거부 → 풀 승격(exit 4)을 강제한다.
///
/// 방식 명시: oplog 강제 롤오버가 통합 환경에서 어려우므로, base manifest의
/// `oplog_range.end_ts`를 `{t:1,i:1}`(epoch 직후)로 둬 현재 oplog 최소 ts보다 과거가 되게
/// 한다 — [`detect_gap`]이 동일 gap 코드 경로를 타고 풀 승격한다.
#[tokio::test]
#[ignore = "requires mongodump + replica set; run with --features integration-tests"]
async fn gap_promotes_to_full_with_exit_4() {
    let uri = test_uri();
    let dir = tempfile::tempdir().expect("temp dir");
    let storage = LocalFs::new(dir.path()).expect("local storage");

    insert_docs(&uri, 10, "seed").await;

    // 가짜 base — 기준점을 oplog 최소 ts보다 한참 과거({t:1,i:1})로 둬 gap을 유발.
    write_fake_base(&storage, "00000000-fake-base", Timestamp { time: 1, increment: 1 }).await;

    let incr_req = IncrementalRequest {
        uri: Secret::new(uri.clone()),
        mongodump_program: "mongodump".to_string(),
    };
    // 평문 팩토리(승격 풀 백업도 평문으로 산출).
    let outcome = run_incremental_backup(&incr_req, &storage, identity_factory())
        .await
        .expect("증분 호출은 성공(승격은 outcome으로 표현)");

    match outcome {
        IncrementalOutcome::PromotedToFull { outcome, reason } => {
            // 승격된 풀 백업 manifest 확인.
            let m = ManifestStore::new(&storage)
                .read(&outcome.backup_id)
                .await
                .expect("승격 풀 manifest");
            assert_eq!(m.backup_type, BackupType::Full, "승격 결과는 풀 백업");
            assert!(m.promoted_from_gap, "promoted_from_gap=true 표식이 있어야 함");
            assert!(reason.contains("gap") || reason.contains("롤오버"), "사유: {reason}");

            // 핸들러가 이 경로를 exit 4(Warning)로 보고하는지 — 매핑 계약 확인.
            let warn = x_backup::XBackupError::Warning(reason);
            assert_eq!(warn.exit_code(), 4, "gap 승격은 exit 4여야 함(SC2)");
        }
        IncrementalOutcome::Captured { .. } => {
            panic!("기준점이 oplog 최소보다 과거인데 gap 승격이 일어나지 않음")
        }
    }
}
