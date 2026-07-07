//! 통합 테스트 — MySQL 엔진(드라이버 SHOW CREATE+SELECT 백업/복구 + binlog ROW 증분).
//!
//! 환경에 binlog/gtid가 켜진 MySQL이 필요하므로 `mysql-integration` feature로 격리한다(기본 CI
//! 단위 테스트에는 포함되지 않는다). `make mysql-up`이 docker로 적합한 MySQL을 띄운다.
//!
//! 실행:
//! ```bash
//! make mysql-up
//! XB_MYSQL_TEST_URI="mysql://root:xbackup-dev@127.0.0.1:3306/xbackup" \
//!   cargo test --features mysql-integration --test mysql_integration -- --include-ignored --test-threads=1
//! make mysql-down
//! ```
//!
//! 검증 항목:
//! - **풀→복구 라운드트립** — 풀 백업(CONSISTENT SNAPSHOT) → 빈 대상으로 복구 후 행 수·콘텐츠가
//!   일치하는지(decimal·datetime·generated 컬럼 포함).
//! - **binlog 증분 캡처/적용** — 풀 백업이 기록한 좌표 이후의 ROW 변경(INSERT/UPDATE/DELETE)을
//!   캡처해 복구 대상에 멱등 적용하면 대상이 소스와 일치하는지.

#![cfg(feature = "mysql-integration")]

use std::io::Cursor;

use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts};

use x_backup::config::secret::Secret;
use x_backup::engine::mysql::incremental::{self, apply, capture, server_id_for};
use x_backup::manifest::store::ManifestStore;
use x_backup::pipeline::backup::{run_mysql_full_backup, BackupMeta};
use x_backup::pipeline::restore::{run_restore, RestoreRequest};
use x_backup::pipeline::stage::StageStack;
use x_backup::storage::LocalFs;

/// 소스 접속 URI(기본 — docker-compose.mysql.yaml 자격증명).
fn src_uri() -> String {
    std::env::var("XB_MYSQL_TEST_URI")
        .unwrap_or_else(|_| "mysql://root:xbackup-dev@127.0.0.1:3306/xbackup".to_string())
}

/// URI의 데이터베이스명만 교체한다(`.../olddb` → `.../newdb`). 쿼리스트링 없음 가정.
fn swap_db(uri: &str, db: &str) -> String {
    let (base, _old) = uri.rsplit_once('/').expect("uri에 '/' 없음");
    format!("{base}/{db}")
}

/// URI에서 데이터베이스명을 뺀 서버 URI(다른 DB 생성용).
fn server_uri(uri: &str) -> String {
    let (base, _db) = uri.rsplit_once('/').expect("uri에 '/' 없음");
    format!("{base}/")
}

async fn connect(uri: &str) -> Conn {
    let opts = Opts::from_url(uri).expect("URI 파싱");
    Conn::new(opts)
        .await
        .expect("MySQL 연결 실패 — make mysql-up 했는지 확인")
}

/// 대상 데이터베이스를 비우고 새로 만든다(빈 대상 복구·증분 적용용).
async fn recreate_database(server: &str, db: &str) {
    let mut c = connect(server).await;
    c.query_drop(format!("DROP DATABASE IF EXISTS `{db}`"))
        .await
        .unwrap();
    c.query_drop(format!("CREATE DATABASE `{db}`"))
        .await
        .unwrap();
    drop(c);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "MySQL 필요(make mysql-up)"]
async fn full_backup_restore_round_trip_preserves_data() {
    let src_uri = src_uri();
    let mut src = connect(&src_uri).await;
    src.query_drop("DROP TABLE IF EXISTS rt_accounts")
        .await
        .unwrap();
    src.query_drop(
        "CREATE TABLE rt_accounts (id INT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(64) NOT NULL, \
         bal DECIMAL(10,2), ts DATETIME(6), tag VARCHAR(80) GENERATED ALWAYS AS (UPPER(name)) STORED)",
    )
    .await
    .unwrap();
    src.query_drop(
        "INSERT INTO rt_accounts(name,bal,ts) VALUES \
         ('alice',1.50,'2026-06-01 10:00:00.123456'),('bob',2.00,'2026-06-02 11:00:00'),('carol',3.25,NULL)",
    )
    .await
    .unwrap();

    let content_sql =
        "SELECT COALESCE(GROUP_CONCAT(CONCAT(name,':',bal,':',COALESCE(ts,'_'),':',tag) ORDER BY id),'') FROM rt_accounts";
    let src_content: String = src.query_first(content_sql).await.unwrap().unwrap();

    let dir = tempfile::tempdir().unwrap();
    let storage = LocalFs::new(dir.path()).unwrap();

    let full = run_mysql_full_backup(
        &Secret::new(src_uri.clone()),
        None,
        None,
        &storage,
        StageStack::new(),
        BackupMeta::none(),
        None,
        false,
    )
    .await
    .expect("MySQL 풀 백업 성공");

    // 빈 대상 DB로 복구.
    recreate_database(&server_uri(&src_uri), "xb_rt_target").await;
    let tgt_uri = swap_db(&src_uri, "xb_rt_target");
    let req = RestoreRequest {
        target_uri: Secret::new(tgt_uri.clone()),
        mongorestore_program: "mongorestore".to_string(), // MySQL 경로에선 미사용.
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

    let mut tgt = connect(&tgt_uri).await;
    let n: i64 = tgt
        .query_first("SELECT COUNT(*) FROM rt_accounts")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(n, 3, "복구 후 행 수 일치");
    let tgt_content: String = tgt.query_first(content_sql).await.unwrap().unwrap();
    assert_eq!(
        tgt_content, src_content,
        "복구 후 콘텐츠 지문 일치(decimal·datetime·generated)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "MySQL 필요(make mysql-up, binlog ON)"]
async fn incremental_binlog_capture_and_apply_round_trip() {
    let src_uri = src_uri();
    let mut src = connect(&src_uri).await;
    src.query_drop("DROP TABLE IF EXISTS incr_t").await.unwrap();
    src.query_drop("CREATE TABLE incr_t (id INT PRIMARY KEY, v VARCHAR(32), n INT)")
        .await
        .unwrap();
    src.query_drop("INSERT INTO incr_t VALUES (1,'a',10),(2,'b',20),(3,'c',30)")
        .await
        .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let storage = LocalFs::new(dir.path()).unwrap();

    // 풀 백업 — 스냅샷 시점 binlog 좌표를 manifest에 기록.
    let full = run_mysql_full_backup(
        &Secret::new(src_uri.clone()),
        None,
        None,
        &storage,
        StageStack::new(),
        BackupMeta::none(),
        None,
        false,
    )
    .await
    .expect("풀 백업 성공");

    // 빈 대상에 base 복구.
    recreate_database(&server_uri(&src_uri), "xb_incr_target").await;
    let tgt_uri = swap_db(&src_uri, "xb_incr_target");
    let req = RestoreRequest {
        target_uri: Secret::new(tgt_uri.clone()),
        mongorestore_program: "mongorestore".to_string(),
        backup_id: Some(full.backup_id.clone()),
        only: None,
        force: false,
        dry_run: false,
        skip_precheck: false,
        timeout_secs: None,
        progress_counter: None,
    };
    run_restore(&req, &storage, false, |_| true)
        .await
        .expect("base 복구 성공");

    // base 좌표 회수.
    let store = ManifestStore::new(&storage);
    let base = store.read(&full.backup_id).await.unwrap();
    let start = base
        .mysql_binlog
        .expect("풀 백업이 binlog 좌표를 기록해야 함");

    // 소스 변경: insert 4, update 1, delete 2.
    src.query_drop("INSERT INTO incr_t VALUES (4,'d',40)")
        .await
        .unwrap();
    src.query_drop("UPDATE incr_t SET n=111 WHERE id=1")
        .await
        .unwrap();
    src.query_drop("DELETE FROM incr_t WHERE id=2")
        .await
        .unwrap();
    // binlog가 디스크에 반영되도록 flush.
    src.query_drop("FLUSH BINARY LOGS").await.ok();

    // 변경 캡처.
    let captured = capture(
        &Secret::new(src_uri.clone()),
        None,
        &start,
        server_id_for("test"),
    )
    .await
    .expect("binlog 캡처 성공");
    assert!(
        captured.count >= 3,
        "최소 3개 변경(insert/update/delete) 캡처: {}",
        captured.count
    );

    // 대상에 멱등 적용.
    let mut tgt = connect(&tgt_uri).await;
    let mut reader = Cursor::new(captured.archive.clone());
    let applied = apply(&mut reader, &mut tgt, None)
        .await
        .expect("증분 적용 성공");
    assert!(applied >= 3, "적용 변경 수 >= 3: {applied}");

    // 멱등 재적용(같은 슬라이스 다시) — 안전해야 한다.
    let mut reader2 = Cursor::new(captured.archive);
    apply(&mut reader2, &mut tgt, None)
        .await
        .expect("멱등 재적용 성공");

    // 검증: 대상이 소스와 일치.
    let q = "SELECT COALESCE(GROUP_CONCAT(CONCAT(id,':',v,':',n) ORDER BY id),'') FROM incr_t";
    let src_state: String = src.query_first(q).await.unwrap().unwrap();
    let tgt_state: String = tgt.query_first(q).await.unwrap().unwrap();
    assert_eq!(tgt_state, src_state, "증분 적용 후 대상이 소스와 일치");
    // 구체 검증: id=1 n=111, id=2 삭제, id=4 추가.
    assert!(tgt_state.contains("1:a:111"), "update 반영: {tgt_state}");
    assert!(!tgt_state.contains("2:b"), "delete 반영: {tgt_state}");
    assert!(tgt_state.contains("4:d:40"), "insert 반영: {tgt_state}");

    let _ = incremental::server_id_for("unused"); // 링크 보장.
}

/// 증분 적용이 까다로운 컬럼 타입(BIT/SET/ENUM/TIMESTAMP/DATETIME/DECIMAL/JSON/VARBINARY/
/// DEFAULT_GENERATED)과 PK 변경 UPDATE를 정확히 재생하는지 — 코드리뷰에서 잡힌 데이터 손상
/// 회귀를 직접 검증한다.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "MySQL 필요(make mysql-up, binlog ON)"]
async fn incremental_typed_columns_round_trip() {
    let src_uri = src_uri();
    let mut src = connect(&src_uri).await;
    src.query_drop("DROP TABLE IF EXISTS typed_t")
        .await
        .unwrap();
    src.query_drop(
        "CREATE TABLE typed_t (\
           id INT PRIMARY KEY, \
           b BIT(8), \
           s SET('a','b','c'), \
           e ENUM('x','y','z'), \
           ts TIMESTAMP NULL, \
           dt DATETIME(6), \
           amt DECIMAL(10,2), \
           js JSON, \
           bin VARBINARY(8), \
           created TIMESTAMP NULL DEFAULT CURRENT_TIMESTAMP)",
    )
    .await
    .unwrap();
    src.query_drop(
        "INSERT INTO typed_t (id,b,s,e,ts,dt,amt,js,bin) VALUES \
         (1, b'10000001', 'a,c', 'y', '2026-06-01 10:00:00', '2026-06-01 10:00:00.123456', 12.34, '{\"k\":1}', 0x00FF)",
    )
    .await
    .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let storage = LocalFs::new(dir.path()).unwrap();
    let full = run_mysql_full_backup(
        &Secret::new(src_uri.clone()),
        None,
        None,
        &storage,
        StageStack::new(),
        BackupMeta::none(),
        None,
        false,
    )
    .await
    .expect("풀 백업");

    recreate_database(&server_uri(&src_uri), "xb_typed_target").await;
    let tgt_uri = swap_db(&src_uri, "xb_typed_target");
    let req = RestoreRequest {
        target_uri: Secret::new(tgt_uri.clone()),
        mongorestore_program: "mongorestore".to_string(),
        backup_id: Some(full.backup_id.clone()),
        only: None,
        force: false,
        dry_run: false,
        skip_precheck: false,
        timeout_secs: None,
        progress_counter: None,
    };
    run_restore(&req, &storage, false, |_| true)
        .await
        .expect("base 복구");

    let store = ManifestStore::new(&storage);
    let start = store
        .read(&full.backup_id)
        .await
        .unwrap()
        .mysql_binlog
        .unwrap();

    // 변경: 모든 타입 들어간 INSERT, 값 UPDATE, PK 변경 UPDATE, DELETE.
    src.query_drop(
        "INSERT INTO typed_t (id,b,s,e,ts,dt,amt,js,bin) VALUES \
         (2, b'00000010', 'b', 'z', '2026-06-10 09:00:00', '2026-06-10 09:00:00.654321', 56.78, '{\"k\":2}', 0xDEAD)",
    )
    .await
    .unwrap();
    src.query_drop("UPDATE typed_t SET amt=99.99, s='a,b,c' WHERE id=1")
        .await
        .unwrap();
    src.query_drop("UPDATE typed_t SET id=3 WHERE id=2") // PK 변경
        .await
        .unwrap();
    src.query_drop("FLUSH BINARY LOGS").await.ok();

    let captured = capture(
        &Secret::new(src_uri.clone()),
        None,
        &start,
        server_id_for("typed"),
    )
    .await
    .expect("캡처");
    assert!(captured.count >= 3, "변경 캡처: {}", captured.count);

    let mut tgt = connect(&tgt_uri).await;
    let mut reader = Cursor::new(captured.archive);
    apply(&mut reader, &mut tgt, None).await.expect("적용");

    // tz 독립 지문 — TIMESTAMP는 UNIX_TIMESTAMP로 비교.
    let fp = "SELECT COALESCE(GROUP_CONCAT(CONCAT_WS('|', \
              id, HEX(b), s, e, COALESCE(UNIX_TIMESTAMP(ts),'_'), dt, amt, js, HEX(bin), \
              COALESCE(UNIX_TIMESTAMP(created),'_')) ORDER BY id),'') FROM typed_t";
    let src_fp: String = src.query_first(fp).await.unwrap().unwrap();
    let tgt_fp: String = tgt.query_first(fp).await.unwrap().unwrap();
    assert_eq!(tgt_fp, src_fp, "타입별 증분 적용 후 소스와 일치(BIT/SET/ENUM/TIMESTAMP/DECIMAL/JSON/VARBINARY/PK변경/DEFAULT)");
    // 구체 검증.
    assert!(
        tgt_fp.contains("1|81|a,b,c|y|"),
        "id=1 SET·BIT·decimal 갱신: {tgt_fp}"
    );
    assert!(
        tgt_fp.contains("3|2|b|z|"),
        "PK 2→3 변경 + BIT(2)·SET·ENUM: {tgt_fp}"
    );
}

/// PK 없는 테이블: 중복 행 중 하나만 DELETE해도 대상에서 한 행만 지워져야 한다(LIMIT 1).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "MySQL 필요(make mysql-up, binlog ON)"]
async fn incremental_no_pk_deletes_single_row() {
    let src_uri = src_uri();
    let mut src = connect(&src_uri).await;
    src.query_drop("DROP TABLE IF EXISTS nopk_t").await.unwrap();
    src.query_drop("CREATE TABLE nopk_t (a INT, b INT)")
        .await
        .unwrap();
    src.query_drop("INSERT INTO nopk_t VALUES (1,2),(1,2),(3,4)")
        .await
        .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let storage = LocalFs::new(dir.path()).unwrap();
    let full = run_mysql_full_backup(
        &Secret::new(src_uri.clone()),
        None,
        None,
        &storage,
        StageStack::new(),
        BackupMeta::none(),
        None,
        false,
    )
    .await
    .expect("풀 백업");
    recreate_database(&server_uri(&src_uri), "xb_nopk_target").await;
    let tgt_uri = swap_db(&src_uri, "xb_nopk_target");
    let req = RestoreRequest {
        target_uri: Secret::new(tgt_uri.clone()),
        mongorestore_program: "mongorestore".to_string(),
        backup_id: Some(full.backup_id.clone()),
        only: None,
        force: false,
        dry_run: false,
        skip_precheck: false,
        timeout_secs: None,
        progress_counter: None,
    };
    run_restore(&req, &storage, false, |_| true)
        .await
        .expect("base 복구");
    let store = ManifestStore::new(&storage);
    let start = store
        .read(&full.backup_id)
        .await
        .unwrap()
        .mysql_binlog
        .unwrap();

    // 중복 (1,2) 두 행 중 하나만 삭제.
    src.query_drop("DELETE FROM nopk_t WHERE a=1 AND b=2 LIMIT 1")
        .await
        .unwrap();
    src.query_drop("FLUSH BINARY LOGS").await.ok();

    let captured = capture(
        &Secret::new(src_uri.clone()),
        None,
        &start,
        server_id_for("nopk"),
    )
    .await
    .expect("캡처");
    let mut tgt = connect(&tgt_uri).await;
    let mut reader = Cursor::new(captured.archive);
    apply(&mut reader, &mut tgt, None).await.expect("적용");

    let src_n: i64 = src
        .query_first("SELECT COUNT(*) FROM nopk_t")
        .await
        .unwrap()
        .unwrap();
    let tgt_n: i64 = tgt
        .query_first("SELECT COUNT(*) FROM nopk_t")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(src_n, 2, "소스는 (1,2) 하나만 남음");
    assert_eq!(
        tgt_n, src_n,
        "대상도 한 행만 삭제(LIMIT 1) — 중복 행 전체 삭제 아님"
    );
}
