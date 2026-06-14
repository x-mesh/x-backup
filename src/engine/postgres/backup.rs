//! PostgreSQL 네이티브 백업 — 드라이버 COPY로 테이블 데이터를 [`archive`](super::archive)
//! 스트림으로 낸다(외부 pg_dump 불필요).
//!
//! native(Mongo)와 동일 패턴: 별도 task가 스키마를 introspection해 DDL을 쓰고, 테이블마다
//! `COPY ... TO STDOUT (FORMAT text)`의 불투명 바이트를 [`DuplexStream`]에 흘린다. 전 구간
//! 스트리밍(상수 메모리). text 포맷이라 메이저 버전 간 이식성이 안전하다.
//!
//! ## 1차 스코프(데이터 중심)
//! 잡는 것: 테이블(컬럼·타입·NOT NULL·DEFAULT)·제약(PK/UNIQUE/FK/CHECK, `pg_get_constraintdef`)·
//! 비제약 인덱스(`pg_get_indexdef`)·시퀀스(last_value/is_called)·행 데이터(COPY text).
//! 잡지 않는 것(후속): 뷰·머티리얼라이즈드뷰·함수·트리거·확장·소유권/권한·파티셔닝·코멘트.

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use futures::TryStreamExt;
use tokio::io::{AsyncRead, AsyncWriteExt, DuplexStream, ReadBuf};
use tokio::task::JoinHandle;
use tokio_postgres::Client;

use super::archive;
use super::conn::PgClient;
use crate::config::secret::Secret;
use crate::error::{Result, XBackupError};

/// cursor→스트림 경계 버퍼(바이트). 백프레셔용.
const PIPE_BUFFER_BYTES: usize = 256 * 1024;

/// 시스템 스키마(백업 대상에서 제외).
const SYSTEM_SCHEMAS: &str = "'pg_catalog','information_schema','pg_toast'";

/// 드라이버로 PostgreSQL 데이터 백업을 수행하는 덤퍼.
pub struct PgDumper {
    pg: PgClient,
}

impl PgDumper {
    /// URI 시크릿으로 연결한다.
    pub async fn connect(uri: &Secret, timeout_secs: Option<u64>) -> Result<Self> {
        Ok(Self {
            pg: PgClient::connect(uri, timeout_secs).await?,
        })
    }

    /// 서버 버전 문자열(manifest 기록용). dump_stream(self) 전에 호출한다.
    pub async fn server_version(&self) -> Option<String> {
        self.pg
            .client()
            .query_one("SHOW server_version", &[])
            .await
            .ok()
            .map(|r| r.get(0))
    }

    /// 연결된 데이터베이스의 사용자 테이블을 아카이브 스트림으로 낸다.
    ///
    /// `schema_filter`/`table_filter`(핸들러의 `--db`/`--collection` 대응)가 있으면 그 범위만.
    /// 별도 task가 introspection+COPY를 구동하고, 호출자는 [`AsyncRead`]로 받아 파이프라인에
    /// 흘린다. EOF 후 [`handle`](PgDumpStream::handle)로 결과(에러)를 회수한다.
    pub fn dump_stream(
        self,
        schema_filter: Option<String>,
        table_filter: Option<String>,
    ) -> PgDumpStream {
        let (mut writer, reader) = tokio::io::duplex(PIPE_BUFFER_BYTES);
        let outcome: Arc<Mutex<Option<Result<()>>>> = Arc::new(Mutex::new(None));
        let outcome_task = Arc::clone(&outcome);
        let pg = self.pg;

        let handle = tokio::spawn(async move {
            let res = write_archive(pg.client(), &mut writer, schema_filter, table_filter).await;
            let _ = writer.shutdown().await;
            *outcome_task.lock().expect("pg dump outcome poisoned") = Some(res);
        });

        PgDumpStream {
            reader,
            outcome,
            task: Arc::new(Mutex::new(Some(handle))),
        }
    }
}

/// 한 테이블의 메타(introspection 결과).
struct TableDef {
    /// 스키마.테이블(보고·로그용).
    ns: String,
    /// 서버가 만든 정확히 quote된 식별자(`"sch"."tbl"`) — DDL·COPY에 사용.
    quoted: String,
    create_sql: String,
    constraints: Vec<String>,
    indexes: Vec<String>,
}

/// 대상 데이터베이스 전체를 아카이브 프레임으로 직렬화해 writer에 쓴다.
async fn write_archive(
    client: &Client,
    writer: &mut DuplexStream,
    schema_filter: Option<String>,
    table_filter: Option<String>,
) -> Result<()> {
    let version: String = client
        .query_one("SHOW server_version", &[])
        .await
        .map(|r| r.get(0))
        .map_err(|e| XBackupError::Failure(format!("server_version 조회 실패: {e}")))?;
    archive::write_header(writer, &chrono::Utc::now().to_rfc3339(), &version).await?;

    // 파티션 부모 테이블은 1차 미지원 — 조용히 빠지지 않게 경고한다(리뷰 #6). 자식 파티션은
    // 일반 테이블로 잡혀 데이터는 보존되나 파티션 구조는 복원되지 않는다.
    warn_partitioned(client, schema_filter.as_deref()).await;

    // 시퀀스 — 테이블 생성 전에 만들어 nextval 기본값을 해소한다.
    write_sequences(client, writer, schema_filter.as_deref()).await?;

    // 테이블 목록.
    let tables = list_tables(client, schema_filter.as_deref(), table_filter.as_deref()).await?;
    for t in &tables {
        let def = introspect_table(client, t).await?;
        archive::write_table(
            writer,
            &def.ns,
            &def.create_sql,
            &def.constraints,
            &def.indexes,
        )
        .await?;

        // 데이터 — COPY text 불투명 바이트를 그대로 흘린다(해석 없음). text는 메이저 버전
        // 간 이식성이 안전하다(바이너리는 버전 의존적 — 리뷰 #2). pg_dump 기본도 text.
        let copy_sql = format!("COPY {} TO STDOUT (FORMAT text)", def.quoted);
        let stream = client
            .copy_out(copy_sql.as_str())
            .await
            .map_err(|e| XBackupError::Failure(format!("{} COPY OUT 실패: {e}", def.ns)))?;
        futures::pin_mut!(stream);
        while let Some(chunk) = stream
            .try_next()
            .await
            .map_err(|e| XBackupError::Failure(format!("{} COPY 청크 읽기 실패: {e}", def.ns)))?
        {
            archive::write_data_bytes(writer, &chunk).await?;
        }
        archive::write_table_end(writer).await?;
        tracing::debug!(ns = %def.ns, "PG 백업: 테이블 직렬화 완료");
    }

    archive::write_end(writer).await
}

/// 사용자 시퀀스를 introspection해 프레임으로 쓴다(last_value 보존).
async fn write_sequences(
    client: &Client,
    writer: &mut DuplexStream,
    schema_filter: Option<&str>,
) -> Result<()> {
    // last_value가 NULL이면 한 번도 호출 안 된 시퀀스 → is_called=false, 값은 1로 본다(리뷰 #5).
    // 식별자 quote도 같은 쿼리에서(format('%I.%I')) 처리해 라운드트립을 줄인다.
    let sql = format!(
        "SELECT format('%I.%I', schemaname, sequencename), \
                coalesce(last_value, 1), (last_value IS NOT NULL) \
         FROM pg_sequences WHERE schemaname NOT IN ({SYSTEM_SCHEMAS}) \
         AND ($1::text IS NULL OR schemaname = $1) ORDER BY schemaname, sequencename"
    );
    let rows = client
        .query(sql.as_str(), &[&schema_filter])
        .await
        .map_err(|e| XBackupError::Failure(format!("시퀀스 목록 조회 실패: {e}")))?;
    for row in rows {
        let quoted: String = row.get(0);
        let last_value: i64 = row.get(1);
        let is_called: bool = row.get(2);
        archive::write_sequence(writer, &quoted, last_value, is_called).await?;
    }
    Ok(())
}

/// 파티션 부모 테이블(relkind='p')을 찾아 경고한다 — 1차 미지원이라 구조가 복원되지 않는다.
async fn warn_partitioned(client: &Client, schema_filter: Option<&str>) {
    let sql = format!(
        "SELECT n.nspname || '.' || c.relname FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relkind = 'p' AND n.nspname NOT IN ({SYSTEM_SCHEMAS}) \
         AND ($1::text IS NULL OR n.nspname = $1)"
    );
    if let Ok(rows) = client.query(sql.as_str(), &[&schema_filter]).await {
        for row in rows {
            let ns: String = row.get(0);
            tracing::warn!(
                ns = %ns,
                "PG 백업: 파티션 부모 테이블 — 파티션 구조는 1차 미지원. 자식 데이터는 \
                 개별 테이블로 백업되나 복구 시 파티셔닝이 재구성되지 않습니다"
            );
        }
    }
}

/// 대상 사용자 테이블 (schema, table) 목록.
async fn list_tables(
    client: &Client,
    schema_filter: Option<&str>,
    table_filter: Option<&str>,
) -> Result<Vec<(String, String)>> {
    let sql = format!(
        "SELECT schemaname, tablename FROM pg_tables \
         WHERE schemaname NOT IN ({SYSTEM_SCHEMAS}) \
         AND ($1::text IS NULL OR schemaname = $1) \
         AND ($2::text IS NULL OR tablename = $2) \
         ORDER BY schemaname, tablename"
    );
    let rows = client
        .query(sql.as_str(), &[&schema_filter, &table_filter])
        .await
        .map_err(|e| XBackupError::Failure(format!("테이블 목록 조회 실패: {e}")))?;
    Ok(rows
        .into_iter()
        .map(|r| (r.get::<_, String>(0), r.get::<_, String>(1)))
        .collect())
}

/// 한 테이블의 DDL(생성문·제약·인덱스)을 introspection한다.
async fn introspect_table(client: &Client, (schema, table): &(String, String)) -> Result<TableDef> {
    let ns = format!("{schema}.{table}");
    // 정확히 quote된 식별자 + oid(카탈로그 질의 키).
    let row = client
        .query_one(
            "SELECT format('%I.%I', $1::text, $2::text), \
                    (quote_ident($1)||'.'||quote_ident($2))::regclass::oid",
            &[schema, table],
        )
        .await
        .map_err(|e| XBackupError::Failure(format!("{ns} 식별자/oid 조회 실패: {e}")))?;
    let quoted: String = row.get(0);
    let oid: u32 = row.get(1);

    // 컬럼 → CREATE TABLE. quote_ident를 같은 쿼리에서 처리(컬럼당 라운드트립 제거 — 리뷰 #4).
    let col_rows = client
        .query(
            "SELECT quote_ident(a.attname), pg_catalog.format_type(a.atttypid, a.atttypmod), \
                    a.attnotnull, pg_get_expr(d.adbin, d.adrelid) \
             FROM pg_attribute a \
             LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
             WHERE a.attrelid = $1 AND a.attnum > 0 AND NOT a.attisdropped \
             ORDER BY a.attnum",
            &[&oid],
        )
        .await
        .map_err(|e| XBackupError::Failure(format!("{ns} 컬럼 조회 실패: {e}")))?;
    let mut cols = Vec::new();
    for c in &col_rows {
        let ident: String = c.get(0);
        let typ: String = c.get(1);
        let notnull: bool = c.get(2);
        let default: Option<String> = c.get(3);
        let mut def = format!("{ident} {typ}");
        if let Some(d) = default {
            def.push_str(&format!(" DEFAULT {d}"));
        }
        if notnull {
            def.push_str(" NOT NULL");
        }
        cols.push(def);
    }
    let create_sql = format!("CREATE TABLE {quoted} (\n  {}\n)", cols.join(",\n  "));

    // 제약(PK/UNIQUE/FK/CHECK 등) — pg_get_constraintdef로 정확히. quote_ident도 같은 쿼리에서.
    let con_rows = client
        .query(
            "SELECT quote_ident(conname), pg_get_constraintdef(oid) FROM pg_constraint \
             WHERE conrelid = $1 ORDER BY oid",
            &[&oid],
        )
        .await
        .map_err(|e| XBackupError::Failure(format!("{ns} 제약 조회 실패: {e}")))?;
    let mut constraints = Vec::new();
    for c in &con_rows {
        let cident: String = c.get(0);
        let def: String = c.get(1);
        constraints.push(format!(
            "ALTER TABLE {quoted} ADD CONSTRAINT {cident} {def}"
        ));
    }

    // 비제약 인덱스 — 제약이 만드는 인덱스는 제외(중복 방지).
    let idx_rows = client
        .query(
            "SELECT pg_get_indexdef(i.indexrelid) FROM pg_index i \
             WHERE i.indrelid = $1 \
             AND NOT EXISTS (SELECT 1 FROM pg_constraint c WHERE c.conindid = i.indexrelid) \
             ORDER BY i.indexrelid",
            &[&oid],
        )
        .await
        .map_err(|e| XBackupError::Failure(format!("{ns} 인덱스 조회 실패: {e}")))?;
    let indexes = idx_rows.iter().map(|r| r.get::<_, String>(0)).collect();

    Ok(TableDef {
        ns,
        quoted,
        create_sql,
        constraints,
        indexes,
    })
}

/// PG 백업 아카이브 바이트 스트림([`AsyncRead`]). 파이프라인에 그대로 흘린다.
pub struct PgDumpStream {
    reader: DuplexStream,
    outcome: Arc<Mutex<Option<Result<()>>>>,
    task: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl PgDumpStream {
    /// 스트림 소비(EOF) 후 백업 결과(에러)를 회수할 핸들을 떠 둔다.
    pub fn handle(&self) -> PgDumpHandle {
        PgDumpHandle {
            outcome: Arc::clone(&self.outcome),
            task: Arc::clone(&self.task),
        }
    }
}

/// [`PgDumpStream`]의 결과를 회수하는 핸들.
pub struct PgDumpHandle {
    outcome: Arc<Mutex<Option<Result<()>>>>,
    task: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl PgDumpHandle {
    /// 스트림 EOF 후 백업 task의 결과를 회수한다(드라이버/직렬화 오류 전파).
    pub async fn finish(self) -> Result<()> {
        let task = self.task.lock().expect("pg dump task poisoned").take();
        if let Some(task) = task {
            let _ = task.await;
        }
        self.outcome
            .lock()
            .expect("pg dump outcome poisoned")
            .take()
            .unwrap_or(Ok(()))
    }
}

impl AsyncRead for PgDumpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.reader).poll_read(cx, buf)
    }
}
