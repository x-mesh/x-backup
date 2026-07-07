//! 통합 테스트 — PostgreSQL 엔진(드라이버 COPY 백업/복구 + logical decoding 증분).
//!
//! 환경에 `wal_level=logical`인 PostgreSQL이 필요하므로 `pg-integration` feature로 격리한다
//! (기본 CI 단위 테스트에는 포함되지 않는다). `make postgres-up`이 docker로 적합한 PG를 띄운다.
//!
//! 실행:
//! ```bash
//! make postgres-up
//! XB_PG_TEST_URI="postgres://xbackup:xbackup-dev@localhost:5432/xbackup" \
//!   cargo test --features pg-integration --test pg_integration -- --ignored --nocapture --test-threads=1
//! make postgres-down
//! ```
//!
//! 검증 항목(이번 감사 수정 회귀):
//! - **H3** — UPDATE가 건드리지 않은 out-of-line TOAST 컬럼('u')이 복구+증분 적용 후
//!   NULL로 덮이지 않고 보존되는지(조용한 데이터 파손 가드).
//! - **C2** — replication slot 건강도(`slot_health`)가 Missing/Active를 정확히 판정하는지
//!   (handler가 Missing/Lost면 풀 백업으로 승격하는 전제).
//! - **풀→복구 라운드트립** — 풀 백업(H1 REPEATABLE READ 스냅샷) → 빈 대상으로 복구(H5
//!   가드 happy path) 후 행 수·콘텐츠가 일치하는지.

#![cfg(feature = "pg-integration")]

use tokio_postgres::{Client, NoTls};

use x_backup::config::secret::Secret;
use x_backup::engine::postgres::incremental::{
    self, capture, recreate_slot_and_publication, slot_health, SlotHealth,
};
use x_backup::pipeline::backup::{run_pg_full_backup, BackupMeta};
use x_backup::pipeline::restore::{run_restore, RestoreRequest};
use x_backup::pipeline::stage::StageStack;
use x_backup::storage::LocalFs;

/// 소스 접속 URI(기본 — docker-compose.postgres.yaml 자격증명). 쿼리스트링 없음 가정.
fn src_uri() -> String {
    std::env::var("XB_PG_TEST_URI")
        .unwrap_or_else(|_| "postgres://xbackup:xbackup-dev@localhost:5432/xbackup".to_string())
}

/// URI의 데이터베이스명만 교체한다(`.../olddb` → `.../newdb`). 쿼리스트링이 없다고 가정.
fn swap_db(uri: &str, db: &str) -> String {
    let (base, _old) = uri.rsplit_once('/').expect("uri에 '/' 없음");
    format!("{base}/{db}")
}

/// NoTls 평문 연결(테스트 자체 SQL용). 백그라운드 연결 task를 띄운다.
async fn connect(uri: &str) -> Client {
    let (client, conn) = tokio_postgres::connect(uri, NoTls)
        .await
        .expect("PG 연결 실패 — make postgres-up 했는지 확인");
    tokio::spawn(async move {
        let _ = conn.await;
    });
    client
}

/// 슬롯이 있으면 drop(없으면 무시).
async fn drop_slot_if_exists(c: &Client, slot: &str) {
    let exists: bool = c
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM pg_replication_slots WHERE slot_name=$1)",
            &[&slot],
        )
        .await
        .map(|r| r.get(0))
        .unwrap_or(false);
    if exists {
        let _ = c
            .execute("SELECT pg_drop_replication_slot($1)", &[&slot])
            .await;
    }
}

/// 대상 데이터베이스를 깨끗이 (재)생성한다 — 기존 연결 종료 후 DROP/CREATE(autocommit).
async fn recreate_database(admin: &Client, db: &str) {
    let _ = admin
        .execute(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
             WHERE datname=$1 AND pid <> pg_backend_pid()",
            &[&db],
        )
        .await;
    // CREATE/DROP DATABASE는 트랜잭션 밖에서만 — batch_execute(simple query)는 자동 커밋.
    let _ = admin
        .batch_execute(&format!("DROP DATABASE IF EXISTS {db}"))
        .await;
    admin
        .batch_execute(&format!("CREATE DATABASE {db}"))
        .await
        .expect("대상 DB 생성 실패");
}

/// H3 — UPDATE가 건드리지 않은 TOAST 컬럼('u')이 증분 적용 후 NULL로 덮이지 않고 보존된다.
///
/// 시나리오: out-of-line TOAST 컬럼 `body`를 가진 행을 두고 `note`만 UPDATE → pgoutput이
/// body를 'u'(unchanged TOAST)로 보낸다. 캡처한 증분을 (base 복원 상태를 모사한) 대상에
/// 적용하면, 고친 코드는 body를 SET에서 제외해 기존 값을 보존해야 한다. 회귀 시 body가
/// NULL로 덮인다.
#[tokio::test]
#[ignore = "requires PostgreSQL wal_level=logical; run with --features pg-integration"]
async fn h3_unchanged_toast_preserved_on_apply() {
    let src_uri = src_uri();
    let src = connect(&src_uri).await;

    // clean slate.
    drop_slot_if_exists(&src, "xb_h3").await;
    let _ = src
        .batch_execute("DROP PUBLICATION IF EXISTS xb_h3_pub")
        .await;
    let _ = src
        .batch_execute("DROP TABLE IF EXISTS public.h3docs")
        .await;

    // body를 STORAGE EXTERNAL(압축 없음)로 둬 큰 값이 확실히 out-of-line(TOAST)이 되게 한다.
    src.batch_execute(
        "CREATE TABLE public.h3docs (id int PRIMARY KEY, body text, note text); \
         ALTER TABLE public.h3docs ALTER COLUMN body SET STORAGE EXTERNAL",
    )
    .await
    .unwrap();
    let big = "x".repeat(8000); // > 2KB + EXTERNAL → 확실히 TOAST 외부 저장.
    src.execute(
        "INSERT INTO public.h3docs(id, body, note) VALUES (1, $1, 'a')",
        &[&big],
    )
    .await
    .unwrap();

    // 슬롯은 insert 뒤 생성 — 이후 변경만 캡처한다.
    recreate_slot_and_publication(&src, "xb_h3", "xb_h3_pub")
        .await
        .unwrap();

    // note만 UPDATE → body는 미변경 + TOAST → pgoutput 'u'.
    src.execute("UPDATE public.h3docs SET note='b' WHERE id=1", &[])
        .await
        .unwrap();

    let captured = capture(&src, "xb_h3", "xb_h3_pub").await.unwrap();
    assert!(captured.count >= 1, "UPDATE 변경이 캡처되어야 함");

    // 대상: base 복원 상태(note='a', body=big)를 모사한 신규 DB/테이블.
    recreate_database(&src, "xb_h3_target").await;
    let tgt_uri = swap_db(&src_uri, "xb_h3_target");
    let tgt = connect(&tgt_uri).await;
    tgt.batch_execute(
        "CREATE TABLE public.h3docs (id int PRIMARY KEY, body text, note text); \
         ALTER TABLE public.h3docs ALTER COLUMN body SET STORAGE EXTERNAL",
    )
    .await
    .unwrap();
    tgt.execute(
        "INSERT INTO public.h3docs(id, body, note) VALUES (1, $1, 'a')",
        &[&big],
    )
    .await
    .unwrap();

    // 증분 적용.
    let mut reader = std::io::Cursor::new(captured.archive);
    incremental::apply(&mut reader, &tgt, None)
        .await
        .expect("증분 적용 성공");

    // 검증: note는 'b'로 갱신, body는 보존(NULL 아님).
    let row = tgt
        .query_one("SELECT body, note FROM public.h3docs WHERE id=1", &[])
        .await
        .unwrap();
    let body: Option<String> = row.get(0);
    let note: String = row.get(1);
    assert_eq!(note, "b", "note는 'b'로 갱신되어야 함");
    assert_eq!(
        body.as_deref(),
        Some(big.as_str()),
        "H3: unchanged-TOAST body가 NULL로 덮이지 않고 보존되어야 함"
    );

    // cleanup.
    drop_slot_if_exists(&src, "xb_h3").await;
    let _ = src
        .batch_execute("DROP PUBLICATION IF EXISTS xb_h3_pub")
        .await;
}

/// C2 — slot_health가 Missing/Active를 정확히 판정한다(없음→생성→drop→재생성).
///
/// handler는 캡처 전 이 판정을 보고 Missing/Lost면 풀 백업으로 승격한다(exit 4). 여기서는
/// 그 판정 자체와 재생성(매 풀백업의 base 정렬)이 정확한지 검증한다.
#[tokio::test]
#[ignore = "requires PostgreSQL wal_level=logical; run with --features pg-integration"]
async fn c2_slot_health_missing_then_active_then_recreate() {
    let src = connect(&src_uri()).await;
    drop_slot_if_exists(&src, "xb_c2").await;
    let _ = src
        .batch_execute("DROP PUBLICATION IF EXISTS xb_c2_pub")
        .await;

    // 슬롯 없음 → Missing.
    assert_eq!(
        slot_health(&src, "xb_c2").await.unwrap(),
        SlotHealth::Missing,
        "슬롯이 없으면 Missing"
    );

    // 생성 → Active.
    recreate_slot_and_publication(&src, "xb_c2", "xb_c2_pub")
        .await
        .unwrap();
    assert_eq!(
        slot_health(&src, "xb_c2").await.unwrap(),
        SlotHealth::Active,
        "생성 직후 Active"
    );

    // 유실 모사(drop) → Missing(= handler가 풀 승격을 트리거하는 상태).
    drop_slot_if_exists(&src, "xb_c2").await;
    assert_eq!(
        slot_health(&src, "xb_c2").await.unwrap(),
        SlotHealth::Missing,
        "drop 후 Missing"
    );

    // 재생성은 있으면 drop 후 새로 만든다(idempotent — base 재정렬).
    recreate_slot_and_publication(&src, "xb_c2", "xb_c2_pub")
        .await
        .unwrap();
    recreate_slot_and_publication(&src, "xb_c2", "xb_c2_pub")
        .await
        .expect("재호출(기존 슬롯 drop+생성)도 성공해야 함");
    assert_eq!(
        slot_health(&src, "xb_c2").await.unwrap(),
        SlotHealth::Active,
        "재생성 후 Active"
    );

    // cleanup.
    drop_slot_if_exists(&src, "xb_c2").await;
    let _ = src
        .batch_execute("DROP PUBLICATION IF EXISTS xb_c2_pub")
        .await;
}

/// 풀 백업(H1 REPEATABLE READ 스냅샷) → 빈 대상으로 복구(H5 가드 happy path) 후
/// 행 수·콘텐츠가 일치한다. serial 시퀀스 충실도도 함께 통과한다.
#[tokio::test]
#[ignore = "requires PostgreSQL wal_level=logical; run with --features pg-integration"]
async fn full_backup_restore_round_trip_preserves_data() {
    let src_uri = src_uri();
    let src = connect(&src_uri).await;
    let _ = src
        .batch_execute("DROP TABLE IF EXISTS public.rt_accounts")
        .await;
    src.batch_execute(
        "CREATE TABLE public.rt_accounts (id serial PRIMARY KEY, name text NOT NULL, n int)",
    )
    .await
    .unwrap();
    src.batch_execute(
        "INSERT INTO public.rt_accounts(name, n) VALUES ('alice',1),('bob',2),('carol',3)",
    )
    .await
    .unwrap();

    // 정렬된 콘텐츠 지문(원본).
    let content_sql =
        "SELECT coalesce(string_agg(name||':'||coalesce(n::text,'∅'), ',' ORDER BY id), '') \
         FROM public.rt_accounts";
    let src_content: String = src.query_one(content_sql, &[]).await.unwrap().get(0);

    let dir = tempfile::tempdir().unwrap();
    let storage = LocalFs::new(dir.path()).unwrap();

    // 풀 백업(enable_incremental=false → 슬롯 없이 단일 REPEATABLE READ 스냅샷 COPY).
    let full = run_pg_full_backup(
        &Secret::new(src_uri.clone()),
        None,
        None,
        None,
        &storage,
        StageStack::new(),
        BackupMeta::none(),
        None,
        "rt",
        false,
    )
    .await
    .expect("PG 풀 백업 성공");

    // 빈 대상 DB로 복구 — 충돌 없으니 H5 가드를 통과한다(force 불필요).
    recreate_database(&src, "xb_rt_target").await;
    let tgt_uri = swap_db(&src_uri, "xb_rt_target");
    let req = RestoreRequest {
        target_uri: Secret::new(tgt_uri.clone()),
        mongorestore_program: "mongorestore".to_string(), // PG 경로에선 미사용.
        backup_id: Some(full.backup_id.clone()),
        only: None,
        force: false,
        dry_run: false,
        skip_precheck: false,
        timeout_secs: None,
        progress_counter: None,
    };
    run_restore(&req, &storage, false, |_| {
        panic!("빈 대상이라 confirm 미호출")
    })
    .await
    .expect("빈 대상 복구 성공");

    // 검증: 행 수 + 콘텐츠 지문 일치.
    let tgt = connect(&tgt_uri).await;
    let n: i64 = tgt
        .query_one("SELECT count(*)::bigint FROM public.rt_accounts", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(n, 3, "복구 후 행 수 일치");
    let tgt_content: String = tgt.query_one(content_sql, &[]).await.unwrap().get(0);
    assert_eq!(tgt_content, src_content, "복구 후 콘텐츠 지문 일치");
}
