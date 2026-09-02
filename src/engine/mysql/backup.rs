//! MySQL 네이티브 백업 — 드라이버 SQL(`SHOW CREATE` + `SELECT`)로 [`archive`](super::archive)
//! 스트림을 낸다(외부 mysqldump 불필요).
//!
//! PG 엔진과 동일 패턴: 별도 task가 스키마를 introspection해 DDL을 쓰고, 테이블마다 행을
//! 스트리밍하며 렌더링한 INSERT 튜플을 [`DuplexStream`]에 흘린다(상수 메모리).
//!
//! ## 일관성
//! 모든 introspection + SELECT를 **단일 `START TRANSACTION WITH CONSISTENT SNAPSHOT`**
//! (REPEATABLE READ, InnoDB) 안에서 수행한다. 그 직후 binlog 좌표(file:pos + gtid_executed)를
//! 캡처해 증분 base로 [`finish`](MysqlDumpHandle::finish)를 통해 반환한다. **주의:** MySQL의
//! consistent snapshot은 동시 DDL을 격리하지 못한다(InnoDB 전용) — 백업 중 DDL은 피해야 한다.
//!
//! ## 스코프
//! 잡는 것: BASE TABLE의 DDL(`SHOW CREATE TABLE` — 컬럼·제약·인덱스·AUTO_INCREMENT 내장)·행
//! 데이터(generated 컬럼 제외, invisible 포함)·뷰·트리거·루틴(프로시저/함수)·이벤트.
//! 잡지 않는 것: 사용자/권한·테이블스페이스·객체별 sql_mode 재현(복구는 고정 permissive 모드).

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use futures::StreamExt;
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Row, Value};
use tokio::io::{AsyncRead, AsyncWriteExt, DuplexStream, ReadBuf};
use tokio::task::JoinHandle;

use super::archive::{self, HeaderFrame, TableFrame};
use super::conn::MysqlClient;
use super::util::{quote_ident, quote_qualified, strip_definer};
use super::value::{render_value, ColCategory};
use crate::config::secret::Secret;
use crate::error::{Result, XBackupError};
use crate::manifest::schema::MysqlBinlogCoords;

/// cursor→스트림 경계 버퍼(바이트). 백프레셔용.
const PIPE_BUFFER_BYTES: usize = 256 * 1024;

/// 드라이버로 MySQL 데이터 백업을 수행하는 덤퍼.
pub struct MysqlDumper {
    client: MysqlClient,
}

impl MysqlDumper {
    /// URI 시크릿으로 연결한다.
    pub async fn connect(uri: &Secret, timeout_secs: Option<u64>) -> Result<Self> {
        Ok(Self {
            client: MysqlClient::connect(uri, timeout_secs).await?,
        })
    }

    /// 서버 버전 문자열(manifest 기록용). dump_stream(self) 전에 호출한다.
    pub async fn server_version(&mut self) -> Option<String> {
        self.client.server_version().await.ok().flatten()
    }

    /// 연결된 데이터베이스의 사용자 테이블을 아카이브 스트림으로 낸다.
    ///
    /// `table_filter`(핸들러의 `--collection` 대응)가 있으면 그 테이블만. 별도 task가
    /// introspection+SELECT를 구동하고, 호출자는 [`AsyncRead`]로 받아 파이프라인에 흘린다.
    /// EOF 후 [`handle`](MysqlDumpStream::handle)로 결과·binlog 좌표를 회수한다.
    pub fn dump_stream(self, table_filter: Option<String>) -> MysqlDumpStream {
        let (mut writer, reader) = tokio::io::duplex(PIPE_BUFFER_BYTES);
        let outcome: Arc<Mutex<Option<Result<()>>>> = Arc::new(Mutex::new(None));
        let coords: Arc<Mutex<Option<MysqlBinlogCoords>>> = Arc::new(Mutex::new(None));
        let outcome_task = Arc::clone(&outcome);
        let coords_task = Arc::clone(&coords);
        let mut conn = self.client.into_conn();

        let handle = tokio::spawn(async move {
            let res = write_archive(&mut conn, &mut writer, table_filter, &coords_task).await;
            let _ = writer.shutdown().await;
            let _ = conn.disconnect().await;
            *outcome_task.lock().expect("mysql dump outcome poisoned") = Some(res);
        });

        MysqlDumpStream {
            reader,
            outcome,
            coords,
            task: Arc::new(Mutex::new(Some(handle))),
        }
    }
}

/// 대상 데이터베이스 전체를 아카이브 프레임으로 직렬화해 writer에 쓴다(단일 스냅샷 트랜잭션).
async fn write_archive(
    conn: &mut Conn,
    writer: &mut DuplexStream,
    table_filter: Option<String>,
    coords_cell: &Arc<Mutex<Option<MysqlBinlogCoords>>>,
) -> Result<()> {
    // 세션 설정 — UTF-8 + UTC(TIMESTAMP 정합, mysqldump --tz-utc 등가).
    for stmt in [
        "SET SESSION TRANSACTION ISOLATION LEVEL REPEATABLE READ",
        "SET NAMES utf8mb4",
        "SET SESSION time_zone = '+00:00'",
    ] {
        conn.query_drop(stmt).await.map_err(|e| {
            XBackupError::Failure(format!("스냅샷 트랜잭션 설정 실패({stmt}): {e}"))
        })?;
    }

    // 스냅샷↔binlog 좌표 **원자성**(mysqldump --single-transaction --master-data 방식):
    // FTWRL로 쓰기를 멈춘 상태에서 일관 스냅샷을 열고 좌표를 읽은 뒤 잠금을 푼다. 그래야
    // 스냅샷 read-view와 기록 좌표가 같은 시점이 되어, 동시 커밋이 첫 증분에서 누락되지 않는다.
    // RELOAD 권한이 없으면 FTWRL는 실패할 수 있으므로 best-effort(좌표 race 가능, 동시 쓰기 없으면 무해).
    let ftwrl = conn.query_drop("FLUSH TABLES WITH READ LOCK").await.is_ok();
    conn.query_drop("START TRANSACTION WITH CONSISTENT SNAPSHOT")
        .await
        .map_err(|e| XBackupError::Failure(format!("일관 스냅샷 시작 실패: {e}")))?;
    let coords = super::incremental::current_coords(conn).await;
    if ftwrl {
        let _ = conn.query_drop("UNLOCK TABLES").await;
    }

    let res = write_archive_in_snapshot(conn, writer, table_filter, &coords, coords_cell).await;
    let _ = conn
        .query_drop(if res.is_ok() { "COMMIT" } else { "ROLLBACK" })
        .await;
    res
}

/// 스냅샷 트랜잭션 안에서 실행되는 본문. `coords`는 스냅샷과 원자적으로 캡처된 binlog 좌표.
async fn write_archive_in_snapshot(
    conn: &mut Conn,
    writer: &mut DuplexStream,
    table_filter: Option<String>,
    coords: &MysqlBinlogCoords,
    coords_cell: &Arc<Mutex<Option<MysqlBinlogCoords>>>,
) -> Result<()> {
    let db: String = conn
        .query_first("SELECT DATABASE()")
        .await
        .map_err(|e| XBackupError::Failure(format!("현재 데이터베이스 조회 실패: {e}")))?
        .flatten()
        .ok_or_else(|| {
            XBackupError::Usage(
                "MySQL 백업은 URI에 데이터베이스가 지정되어야 합니다(mysql://user@host/<db>)"
                    .into(),
            )
        })?;

    let version: String = conn
        .query_first("SELECT VERSION()")
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    let (database_charset, database_collation): (String, String) = conn
        .exec_first(
            "SELECT DEFAULT_CHARACTER_SET_NAME, DEFAULT_COLLATION_NAME \
             FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = ?",
            (&db,),
        )
        .await
        .map_err(|e| XBackupError::Failure(format!("데이터베이스 문자셋 조회 실패: {e}")))?
        .ok_or_else(|| XBackupError::Failure(format!("데이터베이스 '{db}' metadata가 없습니다")))?;

    archive::write_header(
        writer,
        &HeaderFrame {
            created_at: &chrono::Utc::now().to_rfc3339(),
            mysql_version: &version,
            database_charset: &database_charset,
            database_collation: &database_collation,
            binlog_file: &coords.file,
            binlog_pos: coords.position,
            gtid_executed: &coords.gtid_executed,
        },
    )
    .await?;
    *coords_cell.lock().expect("mysql coords poisoned") =
        if coords.file.is_empty() && coords.gtid_executed.is_empty() {
            None
        } else {
            Some(coords.clone())
        };

    // BASE TABLE 목록.
    let tables = list_tables(conn, &db, table_filter.as_deref()).await?;
    for table in &tables {
        dump_table(conn, writer, &db, table).await?;
    }

    // 후행 DDL — 뷰·트리거·루틴·이벤트(데이터 적재 후). 의존성 순서는 복구가 재시도로 흡수.
    // **선택적 백업(--collection)**에서는 DB 전역 객체를 덤프하지 않는다 — 한 테이블만 담는데
    // 다른 테이블을 참조하는 뷰/트리거를 넣으면 복구가 누락 테이블로 실패한다. 단일 테이블
    // 백업은 데이터 전용으로 둔다.
    if table_filter.is_none() {
        write_views(conn, writer, &db).await?;
        write_triggers(conn, writer, &db).await?;
        write_routines(conn, writer, &db).await?;
        write_events(conn, writer, &db).await?;
    }

    archive::write_end(writer).await
}

/// 한 테이블을 introspection해 DDL + 행 데이터를 쓴다.
async fn dump_table(
    conn: &mut Conn,
    writer: &mut DuplexStream,
    db: &str,
    table: &str,
) -> Result<()> {
    // 소스 질의는 db로 정규화(현재 USE db와 무관히 정확). 아카이브에는 **bare** 테이블명을 담아
    // 복구가 대상 연결의 현재 DB로 들어가게 한다(mysqldump와 동일 — 교차 DB 복구 가능).
    let quoted_src = quote_qualified(db, table);
    let quoted = quote_ident(table);
    let ns = format!("{db}.{table}");

    // DDL — SHOW CREATE TABLE(컬럼·제약·인덱스·AUTO_INCREMENT 내장).
    let create_sql: String = conn
        .query_first::<(String, String), _>(format!("SHOW CREATE TABLE {quoted_src}"))
        .await
        .map_err(|e| XBackupError::Failure(format!("{ns} SHOW CREATE TABLE 실패: {e}")))?
        .map(|(_, ddl)| ddl)
        .ok_or_else(|| XBackupError::Failure(format!("{ns} CREATE TABLE 결과 없음")))?;

    // 컬럼 — generated 제외(GENERATION_EXPRESSION<>''), invisible 포함.
    let col_rows: Vec<(String, String, String)> = conn
        .exec(
            "SELECT COLUMN_NAME, DATA_TYPE, COALESCE(GENERATION_EXPRESSION, '') \
             FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION",
            (db, table),
        )
        .await
        .map_err(|e| XBackupError::Failure(format!("{ns} 컬럼 조회 실패: {e}")))?;

    let mut insert_cols: Vec<String> = Vec::new();
    let mut render_plan: Vec<ColCategory> = Vec::new();
    let mut select_exprs: Vec<String> = Vec::new();
    for (name, data_type, gen_expr) in &col_rows {
        if !gen_expr.is_empty() {
            continue; // generated 컬럼 — INSERT 불가, SELECT 제외.
        }
        let q = quote_ident(name);
        insert_cols.push(q.clone());
        select_exprs.push(q);
        render_plan.push(ColCategory::from_data_type(data_type));
    }

    archive::write_table(
        writer,
        &TableFrame {
            ns: &ns,
            quoted: &quoted,
            create_sql: &create_sql,
            insert_cols: &insert_cols,
        },
    )
    .await?;

    // 데이터 — 명시 컬럼 SELECT 스트림(text protocol). 컬럼이 모두 generated면 SELECT 생략.
    if !select_exprs.is_empty() {
        let select = format!("SELECT {} FROM {quoted_src}", select_exprs.join(", "));
        let mut result = conn
            .query_iter(select)
            .await
            .map_err(|e| XBackupError::Failure(format!("{ns} SELECT 실패: {e}")))?;
        let stream_opt = result
            .stream::<Row>()
            .await
            .map_err(|e| XBackupError::Failure(format!("{ns} 행 스트림 실패: {e}")))?;
        if let Some(mut stream) = stream_opt {
            while let Some(row) = stream.next().await {
                let row =
                    row.map_err(|e| XBackupError::Failure(format!("{ns} 행 읽기 실패: {e}")))?;
                let tuple = render_row(&row, &render_plan);
                archive::write_row(writer, tuple.as_bytes()).await?;
            }
        }
    }

    archive::write_table_end(writer).await?;
    tracing::debug!(ns = %ns, "MySQL 백업: 테이블 직렬화 완료");
    Ok(())
}

/// 한 행을 `(v1, v2, ...)` 튜플로 렌더링한다.
fn render_row(row: &Row, plan: &[ColCategory]) -> String {
    let mut tuple = String::with_capacity(plan.len() * 8 + 2);
    tuple.push('(');
    for (i, cat) in plan.iter().enumerate() {
        if i > 0 {
            tuple.push(',');
        }
        let val = row.as_ref(i).unwrap_or(&Value::NULL);
        tuple.push_str(&render_value(val, *cat));
    }
    tuple.push(')');
    tuple
}

/// 대상 BASE TABLE 목록(table_filter가 있으면 그 테이블만).
async fn list_tables(conn: &mut Conn, db: &str, table_filter: Option<&str>) -> Result<Vec<String>> {
    let rows: Vec<String> = match table_filter {
        Some(t) => conn
            .exec(
                "SELECT TABLE_NAME FROM information_schema.TABLES \
                 WHERE TABLE_SCHEMA = ? AND TABLE_TYPE = 'BASE TABLE' AND TABLE_NAME = ?",
                (db, t),
            )
            .await
            .map_err(|e| XBackupError::Failure(format!("테이블 목록 조회 실패: {e}")))?,
        None => conn
            .exec(
                "SELECT TABLE_NAME FROM information_schema.TABLES \
                 WHERE TABLE_SCHEMA = ? AND TABLE_TYPE = 'BASE TABLE' ORDER BY TABLE_NAME",
                (db,),
            )
            .await
            .map_err(|e| XBackupError::Failure(format!("테이블 목록 조회 실패: {e}")))?,
    };
    Ok(rows)
}

/// 뷰를 후행 DDL로 쓴다(`SHOW CREATE VIEW`, DEFINER 제거).
async fn write_views(conn: &mut Conn, writer: &mut DuplexStream, db: &str) -> Result<()> {
    let names: Vec<String> = conn
        .exec(
            "SELECT TABLE_NAME FROM information_schema.VIEWS WHERE TABLE_SCHEMA = ? ORDER BY TABLE_NAME",
            (db,),
        )
        .await
        .map_err(|e| XBackupError::Failure(format!("뷰 목록 조회 실패: {e}")))?;
    for name in &names {
        let quoted = quote_qualified(db, name);
        // SHOW CREATE VIEW: (View, Create View, character_set_client, collation_connection).
        if let Some(mut row) = conn
            .query_first::<Row, _>(format!("SHOW CREATE VIEW {quoted}"))
            .await
            .map_err(|e| XBackupError::Failure(format!("{name} SHOW CREATE VIEW 실패: {e}")))?
        {
            if let Some(ddl) = row.take::<String, _>(1) {
                let charset = row.take::<String, _>(2);
                let collation = row.take::<String, _>(3);
                archive::write_post(
                    writer,
                    "view",
                    name,
                    &strip_definer(&ddl),
                    None,
                    charset.as_deref(),
                    collation.as_deref(),
                    None,
                )
                .await?;
            }
        }
    }
    Ok(())
}

/// 트리거를 후행 DDL로 쓴다(`SHOW CREATE TRIGGER`, DEFINER 제거).
async fn write_triggers(conn: &mut Conn, writer: &mut DuplexStream, db: &str) -> Result<()> {
    let names: Vec<String> = conn
        .exec(
            "SELECT TRIGGER_NAME FROM information_schema.TRIGGERS \
             WHERE TRIGGER_SCHEMA = ? ORDER BY TRIGGER_NAME",
            (db,),
        )
        .await
        .map_err(|e| XBackupError::Failure(format!("트리거 목록 조회 실패: {e}")))?;
    for name in &names {
        let quoted = quote_qualified(db, name);
        // SHOW CREATE TRIGGER: (Trigger, sql_mode, SQL Original Statement, charset, collation, db collation).
        if let Some(mut row) = conn
            .query_first::<Row, _>(format!("SHOW CREATE TRIGGER {quoted}"))
            .await
            .map_err(|e| XBackupError::Failure(format!("{name} SHOW CREATE TRIGGER 실패: {e}")))?
        {
            if let Some(ddl) = row.take::<String, _>(2) {
                let sql_mode = row.take::<String, _>(1);
                let charset = row.take::<String, _>(3);
                let collation = row.take::<String, _>(4);
                archive::write_post(
                    writer,
                    "trigger",
                    name,
                    &strip_definer(&ddl),
                    sql_mode.as_deref(),
                    charset.as_deref(),
                    collation.as_deref(),
                    None,
                )
                .await?;
            }
        }
    }
    Ok(())
}

/// 루틴(프로시저/함수)을 후행 DDL로 쓴다(`SHOW CREATE PROCEDURE/FUNCTION`, DEFINER 제거).
async fn write_routines(conn: &mut Conn, writer: &mut DuplexStream, db: &str) -> Result<()> {
    let routines: Vec<(String, String)> = conn
        .exec(
            "SELECT ROUTINE_NAME, ROUTINE_TYPE FROM information_schema.ROUTINES \
             WHERE ROUTINE_SCHEMA = ? ORDER BY ROUTINE_NAME",
            (db,),
        )
        .await
        .map_err(|e| XBackupError::Failure(format!("루틴 목록 조회 실패: {e}")))?;
    for (name, rtype) in &routines {
        let quoted = quote_qualified(db, name);
        let is_func = rtype.eq_ignore_ascii_case("FUNCTION");
        let kw = if is_func { "FUNCTION" } else { "PROCEDURE" };
        let kind = if is_func { "function" } else { "procedure" };
        // SHOW CREATE PROCEDURE/FUNCTION: (Name, sql_mode, Create ..., charset, collation, db collation).
        if let Some(mut row) = conn
            .query_first::<Row, _>(format!("SHOW CREATE {kw} {quoted}"))
            .await
            .map_err(|e| XBackupError::Failure(format!("{name} SHOW CREATE {kw} 실패: {e}")))?
        {
            if let Some(ddl) = row.take::<Option<String>, _>(2).flatten() {
                let sql_mode = row.take::<String, _>(1);
                let charset = row.take::<String, _>(3);
                let collation = row.take::<String, _>(4);
                archive::write_post(
                    writer,
                    kind,
                    name,
                    &strip_definer(&ddl),
                    sql_mode.as_deref(),
                    charset.as_deref(),
                    collation.as_deref(),
                    None,
                )
                .await?;
            }
        }
    }
    Ok(())
}

/// 이벤트를 후행 DDL로 쓴다(`SHOW CREATE EVENT`, DEFINER 제거).
async fn write_events(conn: &mut Conn, writer: &mut DuplexStream, db: &str) -> Result<()> {
    let names: Vec<String> = conn
        .exec(
            "SELECT EVENT_NAME FROM information_schema.EVENTS WHERE EVENT_SCHEMA = ? ORDER BY EVENT_NAME",
            (db,),
        )
        .await
        .map_err(|e| XBackupError::Failure(format!("이벤트 목록 조회 실패: {e}")))?;
    for name in &names {
        let quoted = quote_qualified(db, name);
        // SHOW CREATE EVENT: (Event, sql_mode, time_zone, Create Event, charset, collation, db collation).
        if let Some(mut row) = conn
            .query_first::<Row, _>(format!("SHOW CREATE EVENT {quoted}"))
            .await
            .map_err(|e| XBackupError::Failure(format!("{name} SHOW CREATE EVENT 실패: {e}")))?
        {
            if let Some(ddl) = row.take::<Option<String>, _>(3).flatten() {
                let sql_mode = row.take::<String, _>(1);
                let time_zone = row.take::<String, _>(2);
                let charset = row.take::<String, _>(4);
                let collation = row.take::<String, _>(5);
                archive::write_post(
                    writer,
                    "event",
                    name,
                    &strip_definer(&ddl),
                    sql_mode.as_deref(),
                    charset.as_deref(),
                    collation.as_deref(),
                    time_zone.as_deref(),
                )
                .await?;
            }
        }
    }
    Ok(())
}

/// MySQL 백업 아카이브 바이트 스트림([`AsyncRead`]). 파이프라인에 그대로 흘린다.
pub struct MysqlDumpStream {
    reader: DuplexStream,
    outcome: Arc<Mutex<Option<Result<()>>>>,
    coords: Arc<Mutex<Option<MysqlBinlogCoords>>>,
    task: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl MysqlDumpStream {
    /// 스트림 소비(EOF) 후 결과·binlog 좌표를 회수할 핸들.
    pub fn handle(&self) -> MysqlDumpHandle {
        MysqlDumpHandle {
            outcome: Arc::clone(&self.outcome),
            coords: Arc::clone(&self.coords),
            task: Arc::clone(&self.task),
        }
    }
}

/// [`MysqlDumpStream`]의 결과를 회수하는 핸들.
pub struct MysqlDumpHandle {
    outcome: Arc<Mutex<Option<Result<()>>>>,
    coords: Arc<Mutex<Option<MysqlBinlogCoords>>>,
    task: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl MysqlDumpHandle {
    /// 스트림 EOF 후 백업 task의 결과를 회수하고 스냅샷 binlog 좌표를 반환한다(증분 base).
    pub async fn finish(self) -> Result<Option<MysqlBinlogCoords>> {
        let task = self.task.lock().expect("mysql dump task poisoned").take();
        if let Some(task) = task {
            let _ = task.await;
        }
        self.outcome
            .lock()
            .expect("mysql dump outcome poisoned")
            .take()
            .unwrap_or(Ok(()))?;
        Ok(self.coords.lock().expect("mysql coords poisoned").take())
    }
}

impl AsyncRead for MysqlDumpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.reader).poll_read(cx, buf)
    }
}
