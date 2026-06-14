//! PostgreSQL 네이티브 백업 — 드라이버 COPY로 테이블 데이터를 [`archive`](super::archive)
//! 스트림으로 낸다(외부 pg_dump 불필요).
//!
//! native(Mongo)와 동일 패턴: 별도 task가 스키마를 introspection해 DDL을 쓰고, 테이블마다
//! `COPY ... TO STDOUT (FORMAT text)`의 불투명 바이트를 [`DuplexStream`]에 흘린다. 전 구간
//! 스트리밍(상수 메모리). text 포맷이라 메이저 버전 간 이식성이 안전하다.
//!
//! ## 스코프
//! 잡는 것: 멀티 스키마 테이블(컬럼·타입·NOT NULL·DEFAULT·IDENTITY·STORED generated)·
//! 제약(PK/UNIQUE/FK/CHECK)·인덱스·시퀀스(파라미터+last_value/is_called)·확장·사용자 정의
//! 타입(enum/도메인/복합)·함수/프로시저·트리거·뷰/머티뷰·파티셔닝(부모 PARTITION BY +
//! 자식 PARTITION OF, 다중 레벨)·행 데이터(COPY text).
//! 잡지 않는 것(후속): 소유권/권한·코멘트·집계/윈도우 함수·user-defined base/range 타입·증분/PITR(WAL).

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
    /// 스키마.테이블(보고·로그용·setval 인자).
    ns: String,
    /// 서버가 만든 정확히 quote된 식별자(`"sch"."tbl"`) — DDL·COPY에 사용.
    quoted: String,
    /// quote된 스키마(`"sch"`) — 복구 전 CREATE SCHEMA.
    schema_quoted: String,
    create_sql: String,
    constraints: Vec<String>,
    indexes: Vec<String>,
    /// COPY 컬럼 목록(quote됨, STORED generated 제외).
    copy_cols: Vec<String>,
    /// IDENTITY 컬럼 평문 이름(복구 후 시퀀스 리셋용).
    identity_cols: Vec<String>,
    /// 직접 데이터가 있는지 — 파티션 부모(relkind='p')는 false(데이터는 자식에).
    has_data: bool,
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

    // 선행 DDL — 확장 → 타입 → 함수/프로시저. 테이블 컬럼·DEFAULT·CHECK·트리거가 이들을 쓰므로
    // 테이블보다 먼저. 복구는 의존성 순서를 재시도로 흡수하므로 여기 순서는 best-effort.
    write_extensions(client, writer).await?;
    write_types(client, writer, schema_filter.as_deref()).await?;
    write_functions(client, writer, schema_filter.as_deref()).await?;

    // 시퀀스 — 테이블 생성 전에 만들어 nextval 기본값을 해소한다.
    write_sequences(client, writer, schema_filter.as_deref()).await?;

    // 테이블 목록.
    let tables = list_tables(client, schema_filter.as_deref(), table_filter.as_deref()).await?;
    for t in &tables {
        let def = introspect_table(client, t).await?;
        archive::write_table(
            writer,
            &archive::TableFrame {
                ns: &def.ns,
                quoted: &def.quoted,
                schema: &def.schema_quoted,
                create_sql: &def.create_sql,
                constraints: &def.constraints,
                indexes: &def.indexes,
                copy_cols: &def.copy_cols,
                identity_cols: &def.identity_cols,
            },
        )
        .await?;

        // 데이터 — COPY text 불투명 바이트를 그대로 흘린다(해석 없음). text는 메이저 버전 간
        // 이식성이 안전하다(리뷰 #2). STORED generated 컬럼은 COPY 불가라 copy_cols에서 제외.
        // 파티션 부모(has_data=false)는 직접 데이터가 없어 COPY를 건너뛴다(자식 데이터 중복 방지).
        if def.has_data && !def.copy_cols.is_empty() {
            let cols = def.copy_cols.join(", ");
            let copy_sql = format!("COPY {} ({cols}) TO STDOUT (FORMAT text)", def.quoted);
            let stream = client
                .copy_out(copy_sql.as_str())
                .await
                .map_err(|e| XBackupError::Failure(format!("{} COPY OUT 실패: {e}", def.ns)))?;
            futures::pin_mut!(stream);
            while let Some(chunk) = stream.try_next().await.map_err(|e| {
                XBackupError::Failure(format!("{} COPY 청크 읽기 실패: {e}", def.ns))
            })? {
                archive::write_data_bytes(writer, &chunk).await?;
            }
        }
        archive::write_table_end(writer).await?;
        tracing::debug!(ns = %def.ns, "PG 백업: 테이블 직렬화 완료");
    }

    // 후행 DDL — 뷰 → 머티리얼라이즈드뷰(WITH DATA) → 트리거(테이블·함수가 다 존재한 뒤).
    write_views(client, writer, schema_filter.as_deref()).await?;
    write_triggers(client, writer, schema_filter.as_deref()).await?;

    archive::write_end(writer).await
}

/// 사용자 함수/프로시저를 선행 DDL로 쓴다(`pg_get_functiondef`). 확장 소유·집계/윈도우는 제외.
async fn write_functions(
    client: &Client,
    writer: &mut DuplexStream,
    schema_filter: Option<&str>,
) -> Result<()> {
    // 집계('a')/윈도우('w')는 pg_get_functiondef 미지원 — 있으면 경고만(1차 미덤프).
    let warn_sql = format!(
        "SELECT count(*) FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
         WHERE p.prokind IN ('a','w') AND n.nspname NOT IN ({SYSTEM_SCHEMAS}) \
         AND ($1::text IS NULL OR n.nspname = $1) \
         AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.objid = p.oid \
            AND d.refclassid = 'pg_extension'::regclass AND d.deptype = 'e')"
    );
    if let Ok(row) = client.query_one(warn_sql.as_str(), &[&schema_filter]).await {
        let n: i64 = row.get(0);
        if n > 0 {
            tracing::warn!(count = n, "PG 백업: 집계/윈도우 함수는 1차 미덤프(로드맵)");
        }
    }

    let sql = format!(
        "SELECT pg_get_functiondef(p.oid) \
         FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
         WHERE p.prokind IN ('f','p') AND n.nspname NOT IN ({SYSTEM_SCHEMAS}) \
         AND ($1::text IS NULL OR n.nspname = $1) \
         AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.objid = p.oid \
            AND d.refclassid = 'pg_extension'::regclass AND d.deptype = 'e') \
         ORDER BY p.oid"
    );
    for r in client
        .query(sql.as_str(), &[&schema_filter])
        .await
        .map_err(|e| XBackupError::Failure(format!("함수 조회 실패: {e}")))?
    {
        archive::write_pre(writer, &r.get::<_, String>(0)).await?;
    }
    Ok(())
}

/// 사용자 트리거를 후행 DDL로 쓴다(`pg_get_triggerdef`). 내부 트리거(FK·제약 자동 생성)는 제외.
async fn write_triggers(
    client: &Client,
    writer: &mut DuplexStream,
    schema_filter: Option<&str>,
) -> Result<()> {
    let sql = format!(
        "SELECT pg_get_triggerdef(t.oid) \
         FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE NOT t.tgisinternal AND n.nspname NOT IN ({SYSTEM_SCHEMAS}) \
         AND ($1::text IS NULL OR n.nspname = $1) ORDER BY t.oid"
    );
    for r in client
        .query(sql.as_str(), &[&schema_filter])
        .await
        .map_err(|e| XBackupError::Failure(format!("트리거 조회 실패: {e}")))?
    {
        archive::write_post(writer, &r.get::<_, String>(0)).await?;
    }
    Ok(())
}

/// 설치된 확장을 선행 DDL로 쓴다(`CREATE EXTENSION IF NOT EXISTS`). plpgsql(기본)은 제외.
async fn write_extensions(client: &Client, writer: &mut DuplexStream) -> Result<()> {
    let rows = client
        .query(
            "SELECT format('CREATE EXTENSION IF NOT EXISTS %I WITH SCHEMA %I', e.extname, n.nspname) \
             FROM pg_extension e JOIN pg_namespace n ON n.oid = e.extnamespace \
             WHERE e.extname <> 'plpgsql' ORDER BY e.extname",
            &[],
        )
        .await
        .map_err(|e| XBackupError::Failure(format!("확장 목록 조회 실패: {e}")))?;
    for r in rows {
        archive::write_pre(writer, &r.get::<_, String>(0)).await?;
    }
    Ok(())
}

/// 사용자 정의 타입(enum/도메인/복합)을 선행 DDL로 쓴다. 확장이 제공하는 타입은 제외(확장이 재생성).
async fn write_types(
    client: &Client,
    writer: &mut DuplexStream,
    schema_filter: Option<&str>,
) -> Result<()> {
    // enum — 라벨을 정렬 순서대로.
    let enum_sql = format!(
        "SELECT format('CREATE TYPE %I.%I AS ENUM (%s)', n.nspname, t.typname, \
                string_agg(quote_literal(e.enumlabel), ', ' ORDER BY e.enumsortorder)) \
         FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace \
         JOIN pg_enum e ON e.enumtypid = t.oid \
         WHERE t.typtype = 'e' AND n.nspname NOT IN ({SYSTEM_SCHEMAS}) \
         AND ($1::text IS NULL OR n.nspname = $1) \
         AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.objid = t.oid \
            AND d.refclassid = 'pg_extension'::regclass AND d.deptype = 'e') \
         GROUP BY n.nspname, t.typname ORDER BY n.nspname, t.typname"
    );
    for r in client
        .query(enum_sql.as_str(), &[&schema_filter])
        .await
        .map_err(|e| XBackupError::Failure(format!("enum 타입 조회 실패: {e}")))?
    {
        archive::write_pre(writer, &r.get::<_, String>(0)).await?;
    }

    // 도메인 — base 타입 + NOT NULL + DEFAULT + CHECK 제약.
    let domain_sql = format!(
        "SELECT format('CREATE DOMAIN %I.%I AS %s', n.nspname, t.typname, \
                format_type(t.typbasetype, t.typtypmod)) \
            || coalesce(' DEFAULT ' || t.typdefault, '') \
            || CASE WHEN t.typnotnull THEN ' NOT NULL' ELSE '' END \
            || coalesce((SELECT ' ' || string_agg( \
                            'CONSTRAINT ' || quote_ident(c.conname) || ' ' || pg_get_constraintdef(c.oid), \
                            ' ' ORDER BY c.conname) \
                         FROM pg_constraint c WHERE c.contypid = t.oid AND c.contype = 'c'), '') \
         FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace \
         WHERE t.typtype = 'd' AND n.nspname NOT IN ({SYSTEM_SCHEMAS}) \
         AND ($1::text IS NULL OR n.nspname = $1) \
         AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.objid = t.oid \
            AND d.refclassid = 'pg_extension'::regclass AND d.deptype = 'e') \
         ORDER BY n.nspname, t.typname"
    );
    for r in client
        .query(domain_sql.as_str(), &[&schema_filter])
        .await
        .map_err(|e| XBackupError::Failure(format!("도메인 타입 조회 실패: {e}")))?
    {
        archive::write_pre(writer, &r.get::<_, String>(0)).await?;
    }

    // 복합 타입 — 멤버 컬럼.
    let comp_sql = format!(
        "SELECT format('CREATE TYPE %I.%I AS (%s)', n.nspname, t.typname, \
                (SELECT string_agg(format('%I %s', a.attname, format_type(a.atttypid, a.atttypmod)), ', ' \
                        ORDER BY a.attnum) \
                 FROM pg_attribute a WHERE a.attrelid = t.typrelid AND a.attnum > 0 AND NOT a.attisdropped)) \
         FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace \
         JOIN pg_class c ON c.oid = t.typrelid \
         WHERE t.typtype = 'c' AND c.relkind = 'c' AND n.nspname NOT IN ({SYSTEM_SCHEMAS}) \
         AND ($1::text IS NULL OR n.nspname = $1) \
         AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.objid = t.oid \
            AND d.refclassid = 'pg_extension'::regclass AND d.deptype = 'e') \
         ORDER BY n.nspname, t.typname"
    );
    for r in client
        .query(comp_sql.as_str(), &[&schema_filter])
        .await
        .map_err(|e| XBackupError::Failure(format!("복합 타입 조회 실패: {e}")))?
    {
        archive::write_pre(writer, &r.get::<_, String>(0)).await?;
    }
    Ok(())
}

/// 뷰·머티리얼라이즈드뷰를 후행 DDL로 쓴다. pg_class 기반으로 확장 소유 객체는 제외(리뷰 #8).
/// 머티뷰는 WITH DATA(기본)로 생성돼 적재된 테이블에서 채워진다.
async fn write_views(
    client: &Client,
    writer: &mut DuplexStream,
    schema_filter: Option<&str>,
) -> Result<()> {
    // 뷰(relkind='v')와 머티뷰(relkind='m')를 같은 패턴으로 — pg_get_viewdef(oid)로 정의.
    let sql = format!(
        "SELECT CASE c.relkind WHEN 'v' THEN 'CREATE VIEW ' ELSE 'CREATE MATERIALIZED VIEW ' END \
                || format('%I.%I', n.nspname, c.relname) || ' AS ' || pg_get_viewdef(c.oid) \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relkind IN ('v','m') AND n.nspname NOT IN ({SYSTEM_SCHEMAS}) \
         AND ($1::text IS NULL OR n.nspname = $1) \
         AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.objid = c.oid \
            AND d.refclassid = 'pg_extension'::regclass AND d.deptype = 'e') \
         ORDER BY c.relkind DESC, c.oid"
    );
    for r in client
        .query(sql.as_str(), &[&schema_filter])
        .await
        .map_err(|e| XBackupError::Failure(format!("뷰/머티뷰 조회 실패: {e}")))?
    {
        archive::write_post(writer, &r.get::<_, String>(0)).await?;
    }
    Ok(())
}

/// 사용자 시퀀스를 introspection해 프레임으로 쓴다(last_value 보존).
async fn write_sequences(
    client: &Client,
    writer: &mut DuplexStream,
    schema_filter: Option<&str>,
) -> Result<()> {
    // last_value가 NULL이면 한 번도 호출 안 된 시퀀스 → is_called=false, 값은 1로 본다(리뷰 #5).
    // 식별자 quote도 같은 쿼리에서(format('%I.%I')) 처리해 라운드트립을 줄인다.
    // IDENTITY 컬럼의 내부 시퀀스(deptype='i')는 제외 — IDENTITY DDL이 재생성하므로 중복 방지.
    // 파라미터(타입·증분·min/max·start·cache·cycle)를 보존한 CREATE SEQUENCE DDL을 만든다.
    let sql = format!(
        "SELECT format('%I.%I', s.schemaname, s.sequencename), quote_ident(s.schemaname), \
                format('CREATE SEQUENCE %I.%I AS %s INCREMENT BY %s MINVALUE %s MAXVALUE %s \
                        START WITH %s CACHE %s%s', \
                       s.schemaname, s.sequencename, s.data_type, s.increment_by, \
                       s.min_value, s.max_value, s.start_value, s.cache_size, \
                       CASE WHEN s.cycle THEN ' CYCLE' ELSE '' END), \
                coalesce(s.last_value, s.start_value), (s.last_value IS NOT NULL) \
         FROM pg_sequences s \
         WHERE s.schemaname NOT IN ({SYSTEM_SCHEMAS}) \
         AND ($1::text IS NULL OR s.schemaname = $1) \
         AND NOT EXISTS ( \
            SELECT 1 FROM pg_class sc JOIN pg_namespace sn ON sn.oid = sc.relnamespace \
            JOIN pg_depend dep ON dep.objid = sc.oid AND dep.deptype IN ('i','e') \
            WHERE sc.relkind = 'S' AND sn.nspname = s.schemaname AND sc.relname = s.sequencename) \
         ORDER BY s.schemaname, s.sequencename"
    );
    let rows = client
        .query(sql.as_str(), &[&schema_filter])
        .await
        .map_err(|e| XBackupError::Failure(format!("시퀀스 목록 조회 실패: {e}")))?;
    for row in rows {
        let quoted: String = row.get(0);
        let schema: String = row.get(1);
        let create_sql: String = row.get(2);
        let last_value: i64 = row.get(3);
        let is_called: bool = row.get(4);
        archive::write_sequence(writer, &quoted, &schema, &create_sql, last_value, is_called)
            .await?;
    }
    Ok(())
}

/// 대상 사용자 테이블 (schema, table) 목록 — 일반 테이블 + 파티션 부모/자식(relkind 'r','p').
///
/// 파티션 계층 깊이 순으로 정렬해 **부모가 자식보다 먼저** 나오게 한다(자식의 `PARTITION OF`가
/// 부모 존재를 전제). 재귀 CTE로 비-파티션(깊이 0)에서 파티션을 따라 내려간다(다중 레벨 지원).
async fn list_tables(
    client: &Client,
    schema_filter: Option<&str>,
    table_filter: Option<&str>,
) -> Result<Vec<(String, String)>> {
    let sql = format!(
        "WITH RECURSIVE h AS ( \
            SELECT c.oid, 0 AS depth FROM pg_class c \
            WHERE c.relkind IN ('r','p') AND NOT c.relispartition \
          UNION ALL \
            SELECT c.oid, h.depth + 1 FROM pg_class c \
            JOIN pg_inherits i ON i.inhrelid = c.oid \
            JOIN h ON h.oid = i.inhparent WHERE c.relispartition \
         ) \
         SELECT n.nspname, c.relname FROM h \
         JOIN pg_class c ON c.oid = h.oid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname NOT IN ({SYSTEM_SCHEMAS}) \
         AND ($1::text IS NULL OR n.nspname = $1) \
         AND ($2::text IS NULL OR c.relname = $2) \
         ORDER BY h.depth, \
                  (c.relpartbound IS NOT NULL AND pg_get_expr(c.relpartbound, c.oid) = 'DEFAULT'), \
                  n.nspname, c.relname"
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
                    (quote_ident($1)||'.'||quote_ident($2))::regclass::oid, \
                    quote_ident($1)",
            &[schema, table],
        )
        .await
        .map_err(|e| XBackupError::Failure(format!("{ns} 식별자/oid 조회 실패: {e}")))?;
    let quoted: String = row.get(0);
    let oid: u32 = row.get(1);
    let schema_quoted: String = row.get(2);

    // 파티션 정보 — 부모(relkind='p')는 PARTITION BY, 자식(relispartition)은 PARTITION OF.
    let prow = client
        .query_one(
            "SELECT c.relkind = 'p', c.relispartition, \
                    CASE WHEN c.relkind = 'p' THEN pg_get_partkeydef(c.oid) END, \
                    CASE WHEN c.relispartition THEN \
                        (SELECT format('%I.%I', pn.nspname, pc.relname) FROM pg_inherits i \
                         JOIN pg_class pc ON pc.oid = i.inhparent \
                         JOIN pg_namespace pn ON pn.oid = pc.relnamespace WHERE i.inhrelid = c.oid) END, \
                    CASE WHEN c.relispartition THEN pg_get_expr(c.relpartbound, c.oid) END \
             FROM pg_class c WHERE c.oid = $1",
            &[&oid],
        )
        .await
        .map_err(|e| XBackupError::Failure(format!("{ns} 파티션 정보 조회 실패: {e}")))?;
    let is_partitioned_parent: bool = prow.get(0);
    let is_partition_child: bool = prow.get(1);
    let part_key: Option<String> = prow.get(2);
    let part_parent: Option<String> = prow.get(3);
    let part_bound: Option<String> = prow.get(4);

    // 컬럼 → CREATE TABLE. attidentity(IDENTITY)/attgenerated(STORED generated)도 함께 가져온다
    // (리뷰 #3/#4). quote_ident는 같은 쿼리에서(리뷰 #4). enum/도메인/복합 타입은 write_types가
    // 선행 DDL로 덤프하므로 여기서 특별 처리는 없다(사용자 정의 range/base 타입만 미지원).
    let col_rows = client
        .query(
            "SELECT quote_ident(a.attname), pg_catalog.format_type(a.atttypid, a.atttypmod), \
                    a.attnotnull, pg_get_expr(d.adbin, d.adrelid), \
                    a.attidentity, a.attgenerated, a.attname \
             FROM pg_attribute a \
             LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
             WHERE a.attrelid = $1 AND a.attnum > 0 AND NOT a.attisdropped \
             ORDER BY a.attnum",
            &[&oid],
        )
        .await
        .map_err(|e| XBackupError::Failure(format!("{ns} 컬럼 조회 실패: {e}")))?;
    let mut cols = Vec::new();
    let mut copy_cols = Vec::new();
    let mut identity_cols = Vec::new();
    for c in &col_rows {
        let ident: String = c.get(0);
        let typ: String = c.get(1);
        let notnull: bool = c.get(2);
        let default: Option<String> = c.get(3);
        let attidentity: i8 = c.get(4); // 'a'=always 'd'=by default 0=아님
        let attgenerated: i8 = c.get(5); // 's'=STORED generated 0=아님
        let attname: String = c.get(6);

        let mut def = format!("{ident} {typ}");
        if attgenerated as u8 as char == 's' {
            // STORED generated — DEFAULT가 아니라 GENERATED ... STORED로 재현하고 COPY에서 제외.
            let expr = default.clone().unwrap_or_else(|| "NULL".to_string());
            def.push_str(&format!(" GENERATED ALWAYS AS ({expr}) STORED"));
            // copy_cols에 넣지 않는다(COPY 불가).
        } else if matches!(attidentity as u8 as char, 'a' | 'd') {
            let kind = if attidentity as u8 as char == 'a' {
                "ALWAYS"
            } else {
                "BY DEFAULT"
            };
            def.push_str(&format!(" GENERATED {kind} AS IDENTITY"));
            copy_cols.push(ident.clone());
            identity_cols.push(attname.clone());
        } else {
            if let Some(d) = default {
                def.push_str(&format!(" DEFAULT {d}"));
            }
            copy_cols.push(ident.clone());
        }
        if notnull {
            def.push_str(" NOT NULL");
        }
        cols.push(def);
    }

    // 파티션 자식은 PARTITION OF로 부모 정의를 상속(컬럼/제약/인덱스 재선언 불필요·금지).
    // 부모(relkind='p')는 PARTITION BY 절을 붙이고 직접 데이터가 없다(데이터는 자식에 있음).
    if is_partition_child {
        let parent = part_parent
            .ok_or_else(|| XBackupError::Failure(format!("{ns} 파티션 부모를 찾지 못했습니다")))?;
        let bound = part_bound.unwrap_or_else(|| "DEFAULT".to_string());
        // 중간 노드(자식이면서 또 파티션 부모)는 PARTITION BY도 덧붙인다(다중 레벨).
        let mut create_sql = format!("CREATE TABLE {quoted} PARTITION OF {parent} {bound}");
        if is_partitioned_parent {
            if let Some(key) = &part_key {
                create_sql.push_str(&format!(" PARTITION BY {key}"));
            }
        }
        // 부모에서 전파되는 제약·인덱스는 PARTITION OF가 자동 생성하므로 제외하되, **자식 로컬**
        //   제약(conislocal=true)·로컬 인덱스(부모 인덱스에서 상속되지 않은 것)는 보존한다(리뷰 #1/#3).
        let constraints = local_constraints(client, oid, &quoted).await?;
        let indexes = local_partition_indexes(client, oid).await?;
        return Ok(TableDef {
            ns,
            quoted,
            schema_quoted,
            create_sql,
            constraints,
            indexes,
            copy_cols,
            // identity는 부모 컬럼에서 상속되므로 자식에선 리셋 대상이 아니다.
            identity_cols: Vec::new(),
            // 중간 노드도 직접 데이터 없음(리프만 데이터 보유).
            has_data: !is_partitioned_parent,
        });
    }

    let create_sql = if is_partitioned_parent {
        let key = part_key.unwrap_or_default();
        format!(
            "CREATE TABLE {quoted} (\n  {}\n) PARTITION BY {key}",
            cols.join(",\n  ")
        )
    } else {
        format!("CREATE TABLE {quoted} (\n  {}\n)", cols.join(",\n  "))
    };

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
        schema_quoted,
        create_sql,
        constraints,
        indexes,
        copy_cols,
        identity_cols,
        // 파티션 부모는 직접 데이터가 없다(자식에서 COPY) — COPY TO 시 자식 데이터가 중복 라우팅됨.
        has_data: !is_partitioned_parent,
    })
}

/// 파티션 자식의 **로컬** 제약 — 부모에서 상속된 것(conislocal=false)은 PARTITION OF가 자동
/// 재생성하므로 제외하고, 자식에 직접 정의된 것(conislocal=true)만 ALTER TABLE ADD로 보존(리뷰 #3).
async fn local_constraints(client: &Client, oid: u32, quoted: &str) -> Result<Vec<String>> {
    let rows = client
        .query(
            "SELECT quote_ident(conname), pg_get_constraintdef(oid) FROM pg_constraint \
             WHERE conrelid = $1 AND conislocal ORDER BY oid",
            &[&oid],
        )
        .await
        .map_err(|e| XBackupError::Failure(format!("파티션 로컬 제약 조회 실패: {e}")))?;
    Ok(rows
        .iter()
        .map(|c| {
            format!(
                "ALTER TABLE {quoted} ADD CONSTRAINT {} {}",
                c.get::<_, String>(0),
                c.get::<_, String>(1)
            )
        })
        .collect())
}

/// 파티션 자식의 **로컬** 인덱스 — 부모 파티션 인덱스에서 전파된 것(pg_inherits에 자식으로 등록)과
/// 제약이 만든 인덱스는 제외하고, 자식에 직접 만든 인덱스만 보존(리뷰 #1).
async fn local_partition_indexes(client: &Client, oid: u32) -> Result<Vec<String>> {
    let rows = client
        .query(
            "SELECT pg_get_indexdef(i.indexrelid) FROM pg_index i \
             WHERE i.indrelid = $1 \
             AND NOT EXISTS (SELECT 1 FROM pg_constraint c WHERE c.conindid = i.indexrelid) \
             AND NOT EXISTS (SELECT 1 FROM pg_inherits h WHERE h.inhrelid = i.indexrelid) \
             ORDER BY i.indexrelid",
            &[&oid],
        )
        .await
        .map_err(|e| XBackupError::Failure(format!("파티션 로컬 인덱스 조회 실패: {e}")))?;
    Ok(rows.iter().map(|r| r.get::<_, String>(0)).collect())
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
