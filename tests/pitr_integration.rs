//! 통합 테스트 — PITR(Point-In-Time Recovery) E2E(SC4, t9).
//!
//! 환경에 `mongodump` + `mongorestore` + replica set(`tests/fixtures/replica-set.sh`)이
//! 필요하므로 `integration-tests` feature로 격리한다. 기본 CI 단위 테스트에는 포함되지
//! 않는다.
//!
//! 실행:
//! ```bash
//! tests/fixtures/replica-set.sh up
//! XB_TEST_MONGO_URI="$(tests/fixtures/replica-set.sh uri)" \
//!   cargo test --features integration-tests --test pitr_integration -- --ignored --nocapture
//! tests/fixtures/replica-set.sh down
//! ```
//!
//! 도구 경로는 `mongodump`/`mongorestore`(PATH) 기본이며, `XB_MONGODUMP`/`XB_MONGORESTORE`
//! env로 위치를 지정할 수 있다(spike §2). 도구가 없으면 `#[ignore]`로 건너뛴다.
//!
//! ## 검증 시나리오(SC4 — DoD)
//! 풀 백업 → 쓰기A → 증분1 → (1초 경계) → 쓰기B → 증분2 → `--at`을 **쓰기A와 쓰기B
//! 사이**로 PITR 복구 → 쓰기A 존재·쓰기B 부재 확인.
//!
//! `--at`은 RFC3339 wall-clock이고 매핑은 `ts.t <= target_unix`(이하 내림)이므로, A와 B는
//! oplog **초(t)** 가 달라야 분리된다. 따라서 A 삽입 후 그 oplog ts.t를 읽어 `--at`을
//! A의 초로 잡고, B는 한 초 이상 뒤에 삽입한다(폴링으로 초 경계 통과 확인).

#![cfg(feature = "integration-tests")]

use bson::{doc, Document};
use mongodb::Client;

use x_backup::config::secret::Secret;
use x_backup::pipeline::backup::{run_full_backup, BackupMeta, BackupRequest, Engine};
use x_backup::pipeline::incremental::{
    run_incremental_backup, IncrementalOutcome, IncrementalRequest,
};
use x_backup::pipeline::pitr::{run_pitr, PitrRequest};
use x_backup::pipeline::stage::StageStack;
use x_backup::storage::LocalFs;

const TEST_DB: &str = "xb_pitr_it";
const TEST_COLL: &str = "events";

fn source_uri() -> String {
    std::env::var("XB_TEST_MONGO_URI").unwrap_or_else(|_| {
        "mongodb://localhost:27017/?replicaSet=rs0&directConnection=true".to_string()
    })
}

fn mongodump_program() -> String {
    std::env::var("XB_MONGODUMP").unwrap_or_else(|_| "mongodump".to_string())
}

fn mongorestore_program() -> String {
    std::env::var("XB_MONGORESTORE").unwrap_or_else(|_| "mongorestore".to_string())
}

/// 평문 StageStack 팩토리(압축·암호화 없음 — 산출물을 직접 검증).
fn identity_factory() -> impl Fn() -> x_backup::Result<(StageStack, BackupMeta)> {
    || Ok((StageStack::new(), BackupMeta::none()))
}

/// `_id`로 문서 1건을 삽입한다(PITR 경계 마커).
async fn insert_marker(client: &Client, id: i64, label: &str) {
    client
        .database(TEST_DB)
        .collection::<Document>(TEST_COLL)
        .insert_one(doc! { "_id": id, "label": label })
        .await
        .expect("insert marker");
}

/// 해당 `_id`의 문서 존재 여부.
async fn doc_exists(client: &Client, id: i64) -> bool {
    client
        .database(TEST_DB)
        .collection::<Document>(TEST_COLL)
        .find_one(doc! { "_id": id })
        .await
        .expect("find_one")
        .is_some()
}

/// 현재 oplog의 최신 ts.t(초)를 읽는다(--at 경계 산정용).
async fn latest_oplog_secs(client: &Client) -> u32 {
    let oplog = client.database("local").collection::<Document>("oplog.rs");
    let opts = mongodb::options::FindOneOptions::builder()
        .sort(doc! { "$natural": -1 })
        .build();
    let e = oplog
        .find_one(doc! {})
        .with_options(opts)
        .await
        .expect("oplog find")
        .expect("oplog not empty");
    match e.get("ts") {
        Some(bson::Bson::Timestamp(ts)) => ts.time,
        other => panic!("oplog ts가 Timestamp가 아님: {other:?}"),
    }
}

/// 진행 oplog 초가 `after` 초를 **초과**할 때까지 더미 쓰기로 폴링한다(초 경계 통과 보장).
///
/// `--at`은 초 단위 매핑이라 A와 B를 분리하려면 oplog ts.t가 달라야 한다. 고정 sleep
/// 대신 oplog 초가 실제로 넘어갈 때까지 폴링한다(pitfall 8-1 동일 정신).
async fn advance_past_second(client: &Client, after: u32) {
    let coll = client.database(TEST_DB).collection::<Document>("_tick");
    for _ in 0..200 {
        if latest_oplog_secs(client).await > after {
            return;
        }
        coll.insert_one(doc! { "tick": 1 }).await.expect("tick");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("oplog 초 경계가 진행하지 않음(after={after})");
}

/// SC4: PITR 후 목표 ts 이후 문서(쓰기B) 부재 + 목표 이하 문서(쓰기A) 존재.
#[tokio::test]
#[ignore = "requires mongodump + mongorestore + replica set; run with --features integration-tests"]
async fn pitr_recovers_to_point_between_writes() {
    let uri = source_uri();
    let client = Client::with_uri_str(&uri).await.expect("connect");

    // 0) 깨끗한 시작 + base 시드.
    client.database(TEST_DB).drop().await.ok();
    insert_marker(&client, 1, "seed").await;

    let dir = tempfile::tempdir().expect("temp dir");
    let storage = LocalFs::new(dir.path()).expect("local storage");

    // 1) 풀 백업(증분 base).
    let full_req = BackupRequest {
        uri: Secret::new(uri.clone()),
        mongodump_program: mongodump_program(),
        timeout_secs: None,
        db: None,
        collection: None,
        engine: Engine::Mongodump,
        progress_counter: None,
    };
    let full = run_full_backup(&full_req, &storage, StageStack::new())
        .await
        .expect("풀 백업");

    // 2) 쓰기A → 그 oplog 초를 --at 목표로 삼는다.
    insert_marker(&client, 101, "writeA").await;
    let at_secs = latest_oplog_secs(&client).await;

    // 3) 증분1(쓰기A까지 캡처).
    let incr_req = IncrementalRequest {
        uri: Secret::new(uri.clone()),
        mongodump_program: mongodump_program(),
        timeout_secs: None,
        engine: Engine::Mongodump,
    };
    let incr1 = run_incremental_backup(&incr_req, &storage, identity_factory())
        .await
        .expect("증분1");
    assert!(
        matches!(incr1, IncrementalOutcome::Captured { .. }),
        "증분1은 정상 캡처여야 함: {incr1:?}"
    );

    // 4) 초 경계 통과 후 쓰기B → 증분2(쓰기B 캡처). B의 oplog 초는 at_secs보다 커야 한다.
    advance_past_second(&client, at_secs).await;
    insert_marker(&client, 102, "writeB").await;
    let b_secs = latest_oplog_secs(&client).await;
    assert!(
        b_secs > at_secs,
        "쓰기B 초({b_secs})가 --at 초({at_secs})보다 커야 분리됨"
    );

    let incr2 = run_incremental_backup(&incr_req, &storage, identity_factory())
        .await
        .expect("증분2");
    assert!(
        matches!(incr2, IncrementalOutcome::Captured { .. }),
        "증분2는 정상 캡처여야 함: {incr2:?}"
    );

    // 5) 체인 연속성 사전 확인(verify --chain 동등) — PITR 전제.
    let chain = x_backup::pipeline::verify::verify_chain_for(&storage, &full.backup_id)
        .await
        .expect("chain 수집");
    assert!(
        chain.is_continuous(),
        "체인 불연속 — PITR 전제 미충족: {:?}",
        chain.breaks
    );

    // 6) PITR 복구 — --at = 쓰기A의 초(RFC3339 UTC). 대상을 비우고 base+oplog 재생.
    client.database(TEST_DB).drop().await.ok();
    let at_rfc3339 = chrono::DateTime::<chrono::Utc>::from_timestamp(at_secs as i64, 0)
        .expect("ts→datetime")
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    let pitr_req = PitrRequest {
        target_uri: Secret::new(uri.clone()),
        mongorestore_program: mongorestore_program(),
        timeout_secs: None,
        at: at_rfc3339.clone(),
        force: true, // 빈 대상이지만 명시(가드 통과).
        dry_run: false,
        skip_precheck: true, // DB 연결 점검 없이 복구 경로만 검증.
    };
    let outcome = run_pitr(&pitr_req, &storage, false, |_| {
        panic!("force면 confirm 미호출")
    })
    .await
    .expect("PITR 복구 성공");

    // 결정 종료 ts는 --at 초 이하여야 한다(내림 매핑).
    assert!(
        outcome.plan.decided_ts.t <= at_secs,
        "결정 종료 ts.t({})가 --at 초({at_secs}) 이하가 아님",
        outcome.plan.decided_ts.t
    );
    assert_eq!(outcome.plan.base_id, full.backup_id, "PITR base = 풀 백업");

    // 7) SC4 핵심 검증: 쓰기A(101) 존재, 쓰기B(102) 부재, seed(1) 존재.
    assert!(
        doc_exists(&client, 1).await,
        "seed(base) 문서가 복원돼야 함"
    );
    assert!(
        doc_exists(&client, 101).await,
        "쓰기A(--at 이하)가 복원돼야 함(SC4)"
    );
    assert!(
        !doc_exists(&client, 102).await,
        "쓰기B(--at 이후)는 복원되면 안 됨(SC4 — 목표 시점 이후 문서 부재)"
    );
}

/// dry-run PITR은 대상을 변경하지 않고 계획만 출력한다(무부작용).
#[tokio::test]
#[ignore = "requires mongodump + replica set; run with --features integration-tests"]
async fn pitr_dry_run_does_not_modify_target() {
    let uri = source_uri();
    let client = Client::with_uri_str(&uri).await.expect("connect");

    client.database(TEST_DB).drop().await.ok();
    insert_marker(&client, 1, "seed").await;

    let dir = tempfile::tempdir().expect("temp dir");
    let storage = LocalFs::new(dir.path()).expect("local storage");

    let full = run_full_backup(
        &BackupRequest {
            uri: Secret::new(uri.clone()),
            mongodump_program: mongodump_program(),
            timeout_secs: None,
            db: None,
            collection: None,
            engine: Engine::Mongodump,
            progress_counter: None,
        },
        &storage,
        StageStack::new(),
    )
    .await
    .expect("풀 백업");

    insert_marker(&client, 101, "writeA").await;
    let at_secs = latest_oplog_secs(&client).await;
    run_incremental_backup(
        &IncrementalRequest {
            uri: Secret::new(uri.clone()),
            mongodump_program: mongodump_program(),
            timeout_secs: None,
            engine: Engine::Mongodump,
        },
        &storage,
        identity_factory(),
    )
    .await
    .expect("증분");

    // 대상을 비운 뒤 dry-run — 비어 있는 상태가 유지돼야 한다.
    client.database(TEST_DB).drop().await.ok();
    let at_rfc3339 = chrono::DateTime::<chrono::Utc>::from_timestamp(at_secs as i64, 0)
        .unwrap()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    let outcome = run_pitr(
        &PitrRequest {
            target_uri: Secret::new(uri.clone()),
            mongorestore_program: mongorestore_program(),
            timeout_secs: None,
            at: at_rfc3339,
            force: false,
            dry_run: true,
            skip_precheck: true,
        },
        &storage,
        false,
        |_| panic!("dry-run confirm 미호출"),
    )
    .await
    .expect("dry-run 성공");

    assert_eq!(outcome.plan.base_id, full.backup_id);
    assert_eq!(outcome.replayed_slices, 0, "dry-run은 재생하지 않음");
    // 대상은 그대로 비어 있어야 한다(무부작용).
    assert!(
        !doc_exists(&client, 1).await,
        "dry-run이 대상을 변경함(무부작용 위반)"
    );
}
