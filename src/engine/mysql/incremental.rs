//! MySQL 증분 — binlog ROW 이벤트를 캡처해 아카이브하고, 복구 시 멱등 DML로 적용한다.
//!
//! PG의 logical decoding 증분과 동형이되 MySQL은 서버측 슬롯이 없어 **binlog file:pos**를
//! 체인 좌표로 쓴다(manifest에 기록):
//! - 풀 백업이 스냅샷 시점 좌표([`current_coords`])를 기록 → 첫 증분의 시작점.
//! - [`capture`]가 `get_binlog_stream`(non-blocking)으로 start 이후 ROW 이벤트를 디코드 →
//!   `xb-mysql-incr-v1` 아카이브. 반환 좌표(end)가 다음 증분의 시작점이다(선형 체인).
//! - [`apply`]가 복구 대상에 변경을 멱등 DML로 적용(I=upsert / U·D=키 또는 전체 행 매칭).
//!   `--at`은 커밋 ts(초 정밀도) 필터.
//! - [`binlog_available`]이 base binlog 파일의 잔존을 확인(purge 시 gap → 풀 승격).
//!
//! ROW 이벤트 값은 위치 기준으로 디코드하고(컬럼 이름·PK·generated 여부는 **대상 스키마**에서
//! 조회 — 복구된 base와 컬럼 순서가 일치), 적용은 `FOREIGN_KEY_CHECKS=0`으로 순서·순환을 우회한다.
//!
//! **서버 요건:** `log_bin=ON`, `binlog_format=ROW`, `binlog_row_image=FULL`(전 컬럼 before/after
//! 이미지 — 키 매칭·idempotency 전제), 고유 `server_id`, REPLICATION SLAVE/CLIENT 권한.

use std::collections::HashMap;

use bson::{Bson, Document};
use futures::StreamExt;
use mysql_async::binlog::events::{EventData, RowsEventData};
use mysql_async::binlog::row::BinlogRow;
use mysql_async::consts::ColumnType;
use mysql_async::prelude::Queryable;
use mysql_async::{BinlogStreamRequest, Conn, Row, Value};
use tokio::io::{AsyncRead, AsyncReadExt};

use super::conn::MysqlClient;
use super::util::quote_ident;
use super::value::render_binlog_value;
use crate::config::secret::Secret;
use crate::error::{Result, XBackupError};
use crate::manifest::schema::MysqlBinlogCoords;

/// 증분 아카이브 포맷 식별자(manifest.archive_format).
pub const INCR_FORMAT_ID: &str = "xb-mysql-incr-v1";

/// 한 번의 capture가 디코드할 최대 변경 수 — 메모리 상한. 초과분은 다음 증분이 이어간다.
const MAX_CHANGES_PER_CAPTURE: u64 = 200_000;

const TAG_HEADER: u8 = b'H';
const TAG_CHANGE: u8 = b'C';
const TAG_END: u8 = b'E';

/// 변경 종류.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Insert,
    Update,
    Delete,
}

/// 한 행 변경 — 값은 이미 렌더링된 SQL 리터럴(위치 순서). before=이전 이미지(U/D), after=신규(I/U).
#[derive(Debug, Clone)]
struct Change {
    op: Op,
    db: String,
    table: String,
    before: Vec<String>,
    after: Vec<String>,
    commit_unix_micros: i64,
}

impl MysqlBinlogCoords {
    /// binlog 비활성 등 좌표 부재 표현.
    pub(crate) fn empty() -> Self {
        Self {
            file: String::new(),
            position: 0,
            gtid_executed: String::new(),
        }
    }
}

/// 프로파일에서 안정적인 binlog replica server_id를 만든다(다른 replica와 충돌 회피용 범위).
pub fn server_id_for(profile: &str) -> u32 {
    let mut h: u32 = 2_166_136_261;
    for b in profile.bytes() {
        h = h.wrapping_mul(16_777_619) ^ b as u32;
    }
    100_000 + (h % 1_000_000)
}

/// 현재 binlog 좌표를 조회한다(`SHOW MASTER STATUS`, 8.4는 `SHOW BINARY LOG STATUS`).
pub async fn current_coords(conn: &mut Conn) -> MysqlBinlogCoords {
    for stmt in ["SHOW MASTER STATUS", "SHOW BINARY LOG STATUS"] {
        if let Ok(rows) = conn.query::<Row, _>(stmt).await {
            return match rows.into_iter().next() {
                Some(mut row) => MysqlBinlogCoords {
                    file: row.take::<String, _>(0).unwrap_or_default(),
                    position: row.take::<u64, _>(1).unwrap_or(0),
                    gtid_executed: row
                        .take_opt::<String, _>(4)
                        .and_then(|r| r.ok())
                        .unwrap_or_default(),
                },
                None => MysqlBinlogCoords::empty(),
            };
        }
    }
    MysqlBinlogCoords::empty()
}

/// base binlog 파일이 아직 서버에 남아 있는지(purge 안 됨) — gap 판정. 없으면 풀 승격 필요.
pub async fn binlog_available(conn: &mut Conn, base: &MysqlBinlogCoords) -> Result<bool> {
    if base.file.is_empty() {
        return Ok(false);
    }
    let logs: Vec<Row> = conn.query("SHOW BINARY LOGS").await.map_err(|e| {
        XBackupError::Failure(crate::tr!(
            "SHOW BINARY LOGS failed: {e}",
            "SHOW BINARY LOGS 실패: {e}"
        ))
    })?;
    Ok(logs
        .iter()
        .filter_map(|r| r.get::<String, usize>(0))
        .any(|name| name == base.file))
}

/// 캡처 결과 — 아카이브 바이트, end 좌표(다음 증분 시작점), 변경 수, backlog 잔여 여부.
pub struct CaptureOutcome {
    pub archive: Vec<u8>,
    pub end: MysqlBinlogCoords,
    pub count: u64,
    pub more_pending: bool,
}

/// start 좌표 이후의 binlog ROW 변경을 캡처해 `xb-mysql-incr-v1` 아카이브로 만든다.
///
/// non-blocking 스트림이라 현재 binlog 끝에서 자연히 종료한다 — 그 시점 좌표가 end가 된다.
/// 새 연결을 쓰며(binlog 스트림이 연결을 소비), `server_id`는 고유해야 한다.
pub async fn capture(
    uri: &Secret,
    timeout_secs: Option<u64>,
    start: &MysqlBinlogCoords,
    server_id: u32,
) -> Result<CaptureOutcome> {
    let mut client = MysqlClient::connect(uri, timeout_secs).await?;
    // binlog는 서버 전역이므로 소스 DB의 이벤트만 캡처한다(다른 DB·시스템 테이블 제외).
    let source_db: String = client
        .conn_mut()
        .query_first::<Option<String>, _>("SELECT DATABASE()")
        .await
        .ok()
        .flatten()
        .flatten()
        .unwrap_or_default();
    let conn = client.into_conn();

    let start_pos = start.position.max(4); // binlog 매직(4) 미만 금지.
    let req = BinlogStreamRequest::new(server_id)
        .with_non_blocking()
        .with_filename(start.file.as_bytes())
        .with_pos(start_pos);
    let mut stream = conn.get_binlog_stream(req).await.map_err(|e| {
        XBackupError::Failure(crate::tr!(
            "failed to start the binlog stream: {e}",
            "binlog 스트림 시작 실패: {e}"
        ))
    })?;

    let mut archive = Vec::new();
    write_header(&mut archive, &chrono::Utc::now().to_rfc3339());

    let mut cur_file = start.file.clone();
    // boundary = 마지막으로 **완결된 트랜잭션**(XID) 직후 위치 = 다음 증분 시작점. 이벤트/트랜잭션
    // 중간에서는 절대 전진하지 않는다(중간 위치를 기록하면 미캡처 행이 영구 누락된다).
    let mut boundary_file = start.file.clone();
    let mut boundary_pos = start.position;
    // 현재 트랜잭션의 변경 버퍼 — XID에서 한꺼번에 flush한다(부분 적용 방지·시점 일관성).
    let mut tx_buf: Vec<Change> = Vec::new();
    // 현재 트랜잭션 커밋 시각(GTID immediate_commit_timestamp, 마이크로초). GTID 없으면 0.
    let mut cur_commit_micros: i64 = 0;
    let mut count = 0u64;
    let mut more_pending = false;

    while let Some(ev) = stream.next().await {
        let ev = ev.map_err(|e| {
            XBackupError::Failure(crate::tr!(
                "failed to read a binlog event: {e}",
                "binlog 이벤트 읽기 실패: {e}"
            ))
        })?;
        let header = ev.header();
        let ts_secs = header.timestamp();
        let log_pos = header.log_pos() as u64;

        let data = match ev.read_data().map_err(|e| {
            XBackupError::Failure(crate::tr!(
                "failed to decode a binlog event: {e}",
                "binlog 이벤트 디코드 실패: {e}"
            ))
        })? {
            Some(d) => d,
            None => continue,
        };

        match data {
            EventData::RotateEvent(r) => {
                // 실제 파일 회전(다음 파일)이면 파일명만 갱신(위치는 트랜잭션 경계에서만).
                let name = r.name().into_owned();
                if name != cur_file {
                    cur_file = name;
                }
            }
            EventData::GtidEvent(g) => {
                // 트랜잭션 시작 — 커밋 시각을 GTID에서 받는다(마이크로초 정밀, 시점 필터 기준).
                cur_commit_micros = g.immediate_commit_timestamp() as i64;
            }
            EventData::RowsEvent(re) => {
                let op = match &re {
                    RowsEventData::WriteRowsEvent(_) | RowsEventData::WriteRowsEventV1(_) => {
                        Op::Insert
                    }
                    RowsEventData::UpdateRowsEvent(_) | RowsEventData::UpdateRowsEventV1(_) => {
                        Op::Update
                    }
                    RowsEventData::DeleteRowsEvent(_) | RowsEventData::DeleteRowsEventV1(_) => {
                        Op::Delete
                    }
                    RowsEventData::PartialUpdateRowsEvent(_) => {
                        tracing::warn!(
                            "partial JSON update 이벤트는 미지원 — 건너뜀(binlog_row_value_options 비활성 권장)"
                        );
                        continue;
                    }
                };
                let tme = match stream.get_tme(re.table_id()) {
                    Some(t) => t,
                    None => continue,
                };
                let db = tme.database_name().into_owned();
                // binlog는 서버 전역 — 소스 DB의 변경만 캡처한다(다른 DB는 무시).
                if !source_db.is_empty() && db != source_db {
                    continue;
                }
                let table = tme.table_name().into_owned();
                // 컬럼 타입(위치 순) — BIT/SET/TIMESTAMP 등 타입별 렌더링에 쓴다.
                let n_cols = tme.columns_count() as usize;
                let col_types: Vec<Option<ColumnType>> = (0..n_cols)
                    .map(|i| tme.get_column_type(i).ok().flatten())
                    .collect();
                // GTID 없으면(gtid_mode=OFF) row 헤더 ts(초 정밀)로 폴백.
                let commit_unix_micros = if cur_commit_micros > 0 {
                    cur_commit_micros
                } else {
                    ts_secs as i64 * 1_000_000
                };

                for row in re.rows(tme) {
                    let (before, after) = row.map_err(|e| {
                        XBackupError::Failure(crate::tr!(
                            "failed to decode a binlog row: {e}",
                            "binlog 행 디코드 실패: {e}"
                        ))
                    })?;
                    tx_buf.push(Change {
                        op,
                        db: db.clone(),
                        table: table.clone(),
                        before: render_binlog_row(before, &col_types)?,
                        after: render_binlog_row(after, &col_types)?,
                        commit_unix_micros,
                    });
                }
            }
            EventData::XidEvent(_) => {
                // 트랜잭션 커밋 — 버퍼를 한꺼번에 flush하고 boundary를 이 위치로 전진.
                for c in tx_buf.drain(..) {
                    write_change(&mut archive, &c)?;
                    count += 1;
                }
                boundary_file = cur_file.clone();
                boundary_pos = log_pos;
                cur_commit_micros = 0;
                // 한도 체크는 **트랜잭션 경계에서만** — 이벤트/트랜잭션 중간 중단 금지.
                if count >= MAX_CHANGES_PER_CAPTURE {
                    more_pending = true;
                    break;
                }
            }
            _ => {}
        }
    }
    // 미완결 버퍼(XID 없이 스트림 종료 — 커밋된 binlog에선 드묾)는 버린다. boundary가 마지막
    // 완결 트랜잭션을 가리키므로 다음 증분이 그 지점부터 다시 읽는다(idempotent — 손실 없음).

    write_end(&mut archive);
    if more_pending {
        tracing::warn!(
            "MySQL 증분 capture가 {MAX_CHANGES_PER_CAPTURE} 변경 한도 도달 — 남은 backlog는 \
             다음 증분이 이어서 캡처합니다(메모리 보호)"
        );
    }
    Ok(CaptureOutcome {
        archive,
        end: MysqlBinlogCoords {
            file: boundary_file,
            position: boundary_pos,
            gtid_executed: String::new(),
        },
        count,
        more_pending,
    })
}

/// binlog 한 행(이미지)을 SQL 리터럴 벡터로 렌더링한다(위치 순서, 컬럼 타입 반영). None이면 빈 벡터.
fn render_binlog_row(
    row: Option<BinlogRow>,
    col_types: &[Option<ColumnType>],
) -> Result<Vec<String>> {
    let Some(br) = row else { return Ok(Vec::new()) };
    let r: Row = br.try_into().map_err(|e| {
        XBackupError::Failure(crate::tr!(
            "failed to convert a binlog row to Row: {e}",
            "binlog 행→Row 변환 실패: {e}"
        ))
    })?;
    let mut out = Vec::with_capacity(r.len());
    for i in 0..r.len() {
        let ct = col_types.get(i).copied().flatten();
        out.push(render_binlog_value(r.as_ref(i).unwrap_or(&Value::NULL), ct));
    }
    Ok(out)
}

/// 대상 컬럼 메타(이름·생성여부·PK) — 위치 순서. virtual generated는 binlog 이미지에 없으므로 제외.
#[derive(Debug, Clone)]
struct ColInfo {
    name: String,
    /// STORED generated(이미지엔 있으나 INSERT/SET 불가).
    generated: bool,
    is_pk: bool,
}

/// 증분 아카이브를 복구 대상에 적용한다. `max_commit_micros`가 Some이면 그 시각 이하만(PITR).
/// 반환은 적용한 변경 수.
pub async fn apply<R: AsyncRead + Unpin>(
    reader: &mut R,
    conn: &mut Conn,
    max_commit_micros: Option<i64>,
) -> Result<u64> {
    // 헤더 확인.
    match read_frame(reader).await? {
        IncrFrame::Header(h) => {
            let fmt = h.get_str("format").unwrap_or("");
            if fmt != INCR_FORMAT_ID {
                return Err(XBackupError::Failure(crate::tr!(
                    "MySQL incremental format mismatch: '{fmt}'",
                    "MySQL 증분 포맷 불일치: '{fmt}'"
                )));
            }
        }
        other => {
            return Err(XBackupError::Failure(crate::tr!(
                "MySQL incremental header missing: {other:?}",
                "MySQL 증분 헤더 누락: {other:?}"
            )))
        }
    }
    for stmt in [
        "SET FOREIGN_KEY_CHECKS = 0",
        "SET UNIQUE_CHECKS = 0",
        "SET SQL_MODE = 'NO_AUTO_VALUE_ON_ZERO'",
        "SET SESSION time_zone = '+00:00'",
        "SET NAMES utf8mb4",
    ] {
        let _ = conn.query_drop(stmt).await;
    }

    let mut meta_cache: HashMap<String, Vec<ColInfo>> = HashMap::new();
    let mut applied = 0u64;
    loop {
        match read_frame(reader).await? {
            IncrFrame::Header(_) => {
                return Err(XBackupError::Failure(crate::tr!(
                    "duplicate MySQL incremental header",
                    "MySQL 증분 헤더 중복"
                )))
            }
            IncrFrame::Change(c) => {
                if let Some(max) = max_commit_micros {
                    if c.commit_unix_micros > max {
                        continue; // PITR 목표 이후 — 건너뜀.
                    }
                }
                // 대상의 현재 DB 기준 테이블명으로 메타를 캐시한다(교차 DB PITR 대응).
                let key = c.table.clone();
                if !meta_cache.contains_key(&key) {
                    let cols = target_columns(conn, &c.table).await?;
                    meta_cache.insert(key.clone(), cols);
                }
                let cols = &meta_cache[&key];
                if let Some(sql) = build_dml(&c, cols) {
                    conn.query_drop(&sql).await.map_err(|e| {
                        XBackupError::Failure(crate::tr!(
                            "{}.{} incremental apply failed: {e}
 SQL: {sql}",
                            "{}.{} 증분 적용 실패: {e}\n  SQL: {sql}",
                            c.db,
                            c.table
                        ))
                    })?;
                    applied += 1;
                }
            }
            IncrFrame::End => break,
        }
    }
    Ok(applied)
}

/// 대상 테이블의 컬럼 메타(virtual generated 제외, 위치 순서) — binlog 이미지와 정렬된다.
/// 대상 연결의 **현재 DB**(`DATABASE()`)에서 조회한다(교차 DB PITR 대응).
async fn target_columns(conn: &mut Conn, table: &str) -> Result<Vec<ColInfo>> {
    let rows: Vec<(String, String, String)> = conn
        .exec(
            "SELECT COLUMN_NAME, COALESCE(EXTRA, ''), COLUMN_KEY FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION",
            (table,),
        )
        .await
        .map_err(|e| {
            XBackupError::Failure(crate::tr!(
                "{table}: failed to query column metadata: {e}",
                "{table} 컬럼 메타 조회 실패: {e}"
            ))
        })?;
    Ok(rows
        .into_iter()
        // virtual generated만 row 이미지에서 빠지므로 제외. STORED generated는 이미지엔 있으나
        // INSERT/SET 불가라 generated로 표시. **DEFAULT_GENERATED**(식·CURRENT_TIMESTAMP 기본값)는
        // 일반 삽입 가능 컬럼이므로 generated가 아니다 — "GENERATED" 부분일치로 잡으면 안 된다.
        .filter(|(_, extra, _)| !extra.to_ascii_uppercase().contains("VIRTUAL GENERATED"))
        .map(|(name, extra, key)| ColInfo {
            generated: extra.to_ascii_uppercase().contains("STORED GENERATED"),
            is_pk: key == "PRI",
            name,
        })
        .collect())
}

/// 한 변경을 멱등 DML로 만든다(값은 이미 렌더링된 리터럴). 이미지 길이가 컬럼 수와 다르면 None(스킵).
///
/// 테이블은 **bare**(현재 DB)로 참조한다 — apply가 대상 연결의 현재 DB에서 동작하므로 소스와
/// 대상의 DB 이름이 달라도(교차 DB PITR) 올바른 테이블에 적용된다.
fn build_dml(c: &Change, cols: &[ColInfo]) -> Option<String> {
    let q = quote_ident(&c.table);
    match c.op {
        Op::Insert => {
            if c.after.len() != cols.len() {
                return None;
            }
            let insert_idx: Vec<usize> = (0..cols.len()).filter(|&i| !cols[i].generated).collect();
            if insert_idx.is_empty() {
                return None;
            }
            let collist = insert_idx
                .iter()
                .map(|&i| quote_ident(&cols[i].name))
                .collect::<Vec<_>>()
                .join(", ");
            let vals = insert_idx
                .iter()
                .map(|&i| c.after[i].clone())
                .collect::<Vec<_>>()
                .join(", ");
            // 멱등: 비-PK 컬럼은 ON DUPLICATE KEY UPDATE로 덮어쓴다. 전부 PK면 IGNORE.
            let upd = insert_idx
                .iter()
                .filter(|&&i| !cols[i].is_pk)
                .map(|&i| {
                    let col = quote_ident(&cols[i].name);
                    format!("{col}=VALUES({col})")
                })
                .collect::<Vec<_>>()
                .join(", ");
            if upd.is_empty() {
                Some(format!(
                    "INSERT IGNORE INTO {q} ({collist}) VALUES ({vals})"
                ))
            } else {
                Some(format!(
                    "INSERT INTO {q} ({collist}) VALUES ({vals}) ON DUPLICATE KEY UPDATE {upd}"
                ))
            }
        }
        Op::Update => {
            if c.after.len() != cols.len() || c.before.len() != cols.len() {
                return None;
            }
            // SET은 STORED generated만 제외한다. PK는 **포함**해야 한다 — MySQL은 PK 값을 바꾸는
            // UPDATE를 허용하며, 제외하면 PK 변경이 대상에 반영되지 않아 발산한다(WHERE는 before-PK).
            let set = (0..cols.len())
                .filter(|&i| !cols[i].generated)
                .map(|i| format!("{} = {}", quote_ident(&cols[i].name), c.after[i]))
                .collect::<Vec<_>>()
                .join(", ");
            if set.is_empty() {
                return None;
            }
            let whr = where_clause(c, cols);
            // PK 없는 테이블은 WHERE가 전 컬럼 매칭이라 중복 행을 다 건드린다 — RBR 1행 의미에 맞춰
            // LIMIT 1로 한 행만(중복 행 다중 변경 방지).
            let limit = if has_pk(cols) { "" } else { " LIMIT 1" };
            Some(format!("UPDATE {q} SET {set} WHERE {whr}{limit}"))
        }
        Op::Delete => {
            if c.before.len() != cols.len() {
                return None;
            }
            let whr = where_clause(c, cols);
            let limit = if has_pk(cols) { "" } else { " LIMIT 1" };
            Some(format!("DELETE FROM {q} WHERE {whr}{limit}"))
        }
    }
}

/// 테이블에 PK 컬럼이 있는지.
fn has_pk(cols: &[ColInfo]) -> bool {
    cols.iter().any(|c| c.is_pk)
}

/// before 이미지로 WHERE 절을 만든다 — PK가 있으면 PK로, 없으면 비-generated 전 컬럼 매칭.
fn where_clause(c: &Change, cols: &[ColInfo]) -> String {
    let pk: Vec<usize> = (0..cols.len()).filter(|&i| cols[i].is_pk).collect();
    let idx = if pk.is_empty() {
        (0..cols.len()).filter(|&i| !cols[i].generated).collect()
    } else {
        pk
    };
    idx.iter()
        .map(|&i| eq_predicate(&quote_ident(&cols[i].name), &c.before[i]))
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// `col = literal` 또는 NULL이면 `col IS NULL`.
fn eq_predicate(col: &str, literal: &str) -> String {
    if literal == "NULL" {
        format!("{col} IS NULL")
    } else {
        format!("{col} = {literal}")
    }
}

// ───────────────────────── 증분 아카이브 프레이밍(BSON) ─────────────────────────

enum IncrFrame {
    Header(Document),
    Change(Change),
    End,
}

impl std::fmt::Debug for IncrFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IncrFrame::Header(_) => write!(f, "Header"),
            IncrFrame::Change(_) => write!(f, "Change"),
            IncrFrame::End => write!(f, "End"),
        }
    }
}

fn write_header(buf: &mut Vec<u8>, created_at: &str) {
    write_doc(
        buf,
        TAG_HEADER,
        &bson::doc! { "format": INCR_FORMAT_ID, "created_at": created_at },
    );
}

fn write_change(buf: &mut Vec<u8>, c: &Change) -> Result<()> {
    write_doc(buf, TAG_CHANGE, &change_to_doc(c));
    Ok(())
}

fn write_end(buf: &mut Vec<u8>) {
    buf.push(TAG_END);
}

fn write_doc(buf: &mut Vec<u8>, tag: u8, doc: &Document) {
    let mut d = Vec::new();
    doc.to_writer(&mut d).expect("BSON 직렬화");
    buf.push(tag);
    buf.extend_from_slice(&d);
}

async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<IncrFrame> {
    let mut tag = [0u8; 1];
    match r.read_exact(&mut tag).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(IncrFrame::End),
        Err(e) => {
            return Err(XBackupError::Failure(crate::tr!(
                "failed to read the incremental tag: {e}",
                "증분 태그 읽기 실패: {e}"
            )))
        }
    }
    match tag[0] {
        TAG_END => Ok(IncrFrame::End),
        TAG_HEADER => Ok(IncrFrame::Header(read_doc(r).await?)),
        TAG_CHANGE => Ok(IncrFrame::Change(doc_to_change(&read_doc(r).await?)?)),
        other => Err(XBackupError::Failure(crate::tr!(
            "corrupt incremental frame tag: 0x{other:02x}",
            "증분 프레임 태그 손상: 0x{other:02x}"
        ))),
    }
}

async fn read_doc<R: AsyncRead + Unpin>(r: &mut R) -> Result<Document> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await.map_err(|e| {
        XBackupError::Failure(crate::tr!(
            "failed to read the incremental length: {e}",
            "증분 길이 읽기 실패: {e}"
        ))
    })?;
    let len = u32::from_le_bytes(len_buf);
    if !(5..=256 * 1024 * 1024).contains(&len) {
        return Err(XBackupError::Failure(crate::tr!(
            "invalid incremental frame length: {len}",
            "증분 프레임 길이 비정상: {len}"
        )));
    }
    let mut buf = vec![0u8; len as usize];
    buf[..4].copy_from_slice(&len_buf);
    r.read_exact(&mut buf[4..]).await.map_err(|e| {
        XBackupError::Failure(crate::tr!(
            "failed to read the incremental body: {e}",
            "증분 본문 읽기 실패: {e}"
        ))
    })?;
    Document::from_reader(&buf[..]).map_err(|e| {
        XBackupError::Failure(crate::tr!(
            "failed to parse the incremental frame: {e}",
            "증분 프레임 파싱 실패: {e}"
        ))
    })
}

fn change_to_doc(c: &Change) -> Document {
    let strs = |v: &[String]| {
        v.iter()
            .map(|s| Bson::String(s.clone()))
            .collect::<Vec<_>>()
    };
    let op = match c.op {
        Op::Insert => "I",
        Op::Update => "U",
        Op::Delete => "D",
    };
    bson::doc! {
        "op": op,
        "db": &c.db,
        "table": &c.table,
        "before": strs(&c.before),
        "after": strs(&c.after),
        "ts": c.commit_unix_micros,
    }
}

fn doc_to_change(d: &Document) -> Result<Change> {
    let strs = |arr: &str| -> Vec<String> {
        d.get_array(arr)
            .map(|a| {
                a.iter()
                    .filter_map(|b| b.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    };
    let op = match d.get_str("op").unwrap_or("") {
        "I" => Op::Insert,
        "U" => Op::Update,
        "D" => Op::Delete,
        other => {
            return Err(XBackupError::Failure(crate::tr!(
                "corrupt incremental op: '{other}'",
                "증분 op 손상: '{other}'"
            )))
        }
    };
    Ok(Change {
        op,
        db: d.get_str("db").unwrap_or("").to_string(),
        table: d.get_str("table").unwrap_or("").to_string(),
        before: strs("before"),
        after: strs("after"),
        commit_unix_micros: d.get_i64("ts").unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cols() -> Vec<ColInfo> {
        vec![
            ColInfo {
                name: "id".into(),
                generated: false,
                is_pk: true,
            },
            ColInfo {
                name: "name".into(),
                generated: false,
                is_pk: false,
            },
        ]
    }

    fn ch(op: Op) -> Change {
        Change {
            op,
            db: "app".into(),
            table: "t".into(),
            before: vec!["7".into(), "'old'".into()],
            after: vec!["7".into(), "'new'".into()],
            commit_unix_micros: 1_700_000_000_000_000,
        }
    }

    #[test]
    fn insert_upsert() {
        let sql = build_dml(&ch(Op::Insert), &cols()).unwrap();
        assert_eq!(
            sql,
            "INSERT INTO `t` (`id`, `name`) VALUES (7, 'new') ON DUPLICATE KEY UPDATE `name`=VALUES(`name`)"
        );
    }

    #[test]
    fn update_by_pk() {
        // PK 컬럼도 SET에 포함된다(PK 변경 UPDATE를 대상에 반영하기 위함).
        let sql = build_dml(&ch(Op::Update), &cols()).unwrap();
        assert_eq!(
            sql,
            "UPDATE `t` SET `id` = 7, `name` = 'new' WHERE `id` = 7"
        );
    }

    #[test]
    fn update_changing_pk_emits_new_pk() {
        // before id=5, after id=6 — 새 PK가 SET에 들어가고 WHERE는 before PK.
        let mut c = ch(Op::Update);
        c.before = vec!["5".into(), "'old'".into()];
        c.after = vec!["6".into(), "'new'".into()];
        let sql = build_dml(&c, &cols()).unwrap();
        assert_eq!(
            sql,
            "UPDATE `t` SET `id` = 6, `name` = 'new' WHERE `id` = 5"
        );
    }

    #[test]
    fn delete_by_pk() {
        let sql = build_dml(&ch(Op::Delete), &cols()).unwrap();
        assert_eq!(sql, "DELETE FROM `t` WHERE `id` = 7");
    }

    #[test]
    fn no_pk_matches_full_row() {
        let cols = vec![
            ColInfo {
                name: "a".into(),
                generated: false,
                is_pk: false,
            },
            ColInfo {
                name: "b".into(),
                generated: false,
                is_pk: false,
            },
        ];
        let mut c = ch(Op::Delete);
        c.before = vec!["1".into(), "NULL".into()];
        let sql = build_dml(&c, &cols).unwrap();
        // PK 없는 테이블은 RBR 1행 의미에 맞춰 LIMIT 1(중복 행 다중 삭제 방지).
        assert_eq!(sql, "DELETE FROM `t` WHERE `a` = 1 AND `b` IS NULL LIMIT 1");
    }

    #[test]
    fn stored_generated_excluded_from_insert() {
        let cols = vec![
            ColInfo {
                name: "id".into(),
                generated: false,
                is_pk: true,
            },
            ColInfo {
                name: "g".into(),
                generated: true,
                is_pk: false,
            },
        ];
        let mut c = ch(Op::Insert);
        c.after = vec!["7".into(), "'computed'".into()];
        let sql = build_dml(&c, &cols).unwrap();
        // generated 컬럼 g는 INSERT 목록에서 제외(전부 PK면 IGNORE).
        assert_eq!(sql, "INSERT IGNORE INTO `t` (`id`) VALUES (7)");
    }

    #[test]
    fn length_mismatch_skips() {
        let mut c = ch(Op::Insert);
        c.after = vec!["7".into()]; // 컬럼 2개인데 값 1개.
        assert!(build_dml(&c, &cols()).is_none());
    }

    #[tokio::test]
    async fn archive_round_trip() {
        let mut buf = Vec::new();
        write_header(&mut buf, "2026-06-14T00:00:00Z");
        write_change(&mut buf, &ch(Op::Insert)).unwrap();
        write_change(&mut buf, &ch(Op::Delete)).unwrap();
        write_end(&mut buf);

        let mut r = std::io::Cursor::new(buf);
        assert!(matches!(
            read_frame(&mut r).await.unwrap(),
            IncrFrame::Header(_)
        ));
        match read_frame(&mut r).await.unwrap() {
            IncrFrame::Change(c) => {
                assert_eq!(c.op, Op::Insert);
                assert_eq!(c.after, vec!["7".to_string(), "'new'".to_string()]);
            }
            f => panic!("change 기대, {f:?}"),
        }
        assert!(matches!(
            read_frame(&mut r).await.unwrap(),
            IncrFrame::Change(_)
        ));
        assert!(matches!(read_frame(&mut r).await.unwrap(), IncrFrame::End));
    }

    #[test]
    fn server_id_stable_and_ranged() {
        let a = server_id_for("prod");
        let b = server_id_for("prod");
        assert_eq!(a, b);
        assert!((100_000..1_100_000).contains(&a));
        assert_ne!(server_id_for("prod"), server_id_for("staging"));
    }
}
