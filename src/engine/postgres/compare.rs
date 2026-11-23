//! PostgreSQL의 행 수 세기(`verify --against`) — 판정은 [`crate::engine::compare`]가 맡는다.
//!
//! manifest에 행 수가 없는 백업은 아카이브의 `T`/`D`/`X` 프레임을 끝까지 읽어 센다. `D` 프레임은
//! 행이 아니라 COPY text 청크라서 줄바꿈 수로 센다(값 안의 줄바꿈은 `\n`으로 이스케이프되므로
//! 줄바꿈 수의 합이 행 수와 같다). 대상 DB는 테이블마다 `count(*)`를 **하나씩 순차로** 실행한다.
//!
//! 키는 `스키마.테이블`이다. 파티션 부모(`relkind = 'p'`)는 행을 자식에 두므로 비교에서 뺀다.

use std::collections::{BTreeMap, BTreeSet};

use tokio::io::AsyncRead;
use tokio_postgres::Client;

use super::archive::{self, Frame};
use crate::error::{Result, XBackupError};

/// `backup.rs`·`meta.rs`와 같은 시스템 스키마 목록(백업이 덤프하지 않는 스키마).
const SYSTEM_SCHEMAS: &str = "'pg_catalog','information_schema','pg_toast'";

/// 대상 DB의 비교 대상 테이블과, 비교에서 빼야 하는 파티션 부모.
#[derive(Debug, Default)]
pub struct LiveTables {
    /// `스키마.테이블` → (스키마, 테이블). 파티션 부모는 들어 있지 않다.
    pub tables: BTreeMap<String, (String, String)>,
    /// 파티션 부모의 `스키마.테이블`. 백업 아카이브에는 0행짜리 테이블로 남아 있다.
    pub partitioned_parents: BTreeSet<String>,
}

/// 식별자를 쌍따옴표로 quote한다(내부 쌍따옴표는 두 배).
pub(super) fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// COPY text 청크에 들어 있는 행 수(줄바꿈 수).
fn copy_text_rows(chunk: &[u8]) -> u64 {
    chunk.iter().filter(|&&b| b == b'\n').count() as u64
}

/// 아카이브 스트림을 끝까지 읽어 테이블별 행 수를 센다. 키는 `스키마.테이블`이다.
pub async fn count_archive_rows<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<BTreeMap<String, u64>> {
    match archive::read_frame(reader).await? {
        Frame::Header(h) => {
            let fmt = h.get_str("format").unwrap_or("");
            if fmt != archive::FORMAT_ID {
                return Err(XBackupError::Failure(crate::tr!(
                    "PostgreSQL archive format mismatch: '{fmt}' (expected '{}')",
                    "PostgreSQL 아카이브 포맷 불일치: '{fmt}'(기대 '{}')",
                    archive::FORMAT_ID
                )));
            }
        }
        other => {
            return Err(XBackupError::Failure(crate::tr!(
                "PostgreSQL archive header missing (first frame: {other:?})",
                "PostgreSQL 아카이브 헤더 누락(첫 프레임: {other:?})"
            )))
        }
    }

    let mut counts = BTreeMap::new();
    let mut current: Option<String> = None;
    loop {
        match archive::read_frame(reader).await? {
            Frame::Table(doc) => {
                let ns = doc.get_str("ns").unwrap_or("").to_string();
                counts.entry(ns.clone()).or_insert(0u64);
                current = Some(ns);
            }
            Frame::Data(chunk) => {
                let Some(ns) = current.as_ref() else {
                    return Err(XBackupError::Failure(crate::tr!(
                        "PostgreSQL archive data frame appeared before any table frame",
                        "PostgreSQL 아카이브에서 테이블 프레임 없이 데이터 프레임이 나왔습니다"
                    )));
                };
                if let Some(n) = counts.get_mut(ns) {
                    *n += copy_text_rows(&chunk);
                }
            }
            Frame::TableEnd => current = None,
            Frame::End => return Ok(counts),
            Frame::Header(_) | Frame::Pre(_) | Frame::Post(_) | Frame::Sequence(_) => {}
        }
    }
}

/// 연결된 DB의 사용자 테이블을 나열한다(백업과 같은 시스템 스키마 제외).
pub async fn live_tables(client: &Client) -> Result<LiveTables> {
    let rows = client
        .query(
            format!(
                "SELECT n.nspname, c.relname, c.relkind::text FROM pg_class c \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE c.relkind IN ('r','p') AND n.nspname NOT IN ({SYSTEM_SCHEMAS})"
            )
            .as_str(),
            &[],
        )
        .await
        .map_err(|e| {
            XBackupError::Failure(crate::tr!(
                "failed to list target tables: {e}",
                "대상 테이블 목록 조회 실패: {e}"
            ))
        })?;
    let mut live = LiveTables::default();
    for row in rows {
        let schema: String = row.get(0);
        let table: String = row.get(1);
        let kind: String = row.get(2);
        let ns = format!("{schema}.{table}");
        if kind == "p" {
            live.partitioned_parents.insert(ns);
        } else {
            live.tables.insert(ns, (schema, table));
        }
    }
    Ok(live)
}

/// 테이블 하나의 행 수(`count(*)`). `ONLY`로 상속·파티션 자식을 합치지 않는다.
pub async fn count_live_rows(client: &Client, schema: &str, table: &str) -> Result<u64> {
    let sql = format!(
        "SELECT count(*) FROM ONLY {}.{}",
        quote_ident(schema),
        quote_ident(table)
    );
    let row = client.query_one(sql.as_str(), &[]).await.map_err(|e| {
        XBackupError::Failure(crate::tr!(
            "failed to count rows of {schema}.{table}: {e}",
            "{schema}.{table} 행 수 조회 실패: {e}"
        ))
    })?;
    let n: i64 = row.get(0);
    Ok(n.max(0) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::postgres::archive::{
        write_data, write_end, write_header, write_table, write_table_end, TableFrame,
    };

    async fn archive_with(tables: &[(&str, &[&[u8]])]) -> Vec<u8> {
        let mut buf = Vec::new();
        write_header(&mut buf, "2026-10-28T00:00:00Z", "16.0")
            .await
            .unwrap();
        for (ns, chunks) in tables {
            write_table(
                &mut buf,
                &TableFrame {
                    ns,
                    quoted: "\"s\".\"t\"",
                    schema: "\"s\"",
                    create_sql: "CREATE TABLE s.t (id int)",
                    constraints: &[],
                    indexes: &[],
                    copy_cols: &["\"id\"".to_string()],
                    identity_cols: &[],
                },
            )
            .await
            .unwrap();
            for chunk in *chunks {
                write_data(&mut buf, chunk).await.unwrap();
            }
            write_table_end(&mut buf).await.unwrap();
        }
        write_end(&mut buf).await.unwrap();
        buf
    }

    #[test]
    fn copy_text_rows_ignores_escaped_newlines() {
        assert_eq!(copy_text_rows(b""), 0);
        assert_eq!(copy_text_rows(b"1\tline\\nstill one\n2\t\\N\n"), 2);
    }

    /// 청크 경계에 걸친 행은 끝난 청크에서 한 번만 세고, 청크 없는 테이블은 0행이다.
    #[tokio::test]
    async fn counts_rows_across_chunks_and_keeps_empty_tables() {
        let buf = archive_with(&[
            ("public.orders", &[b"1\tpar", b"tial\n2\tb", b"\n"]),
            ("public.empty", &[]),
            ("sales.items", &[b"7\n8\n9\n"]),
        ])
        .await;
        let counts = count_archive_rows(&mut &buf[..]).await.unwrap();
        assert_eq!(counts.get("public.orders"), Some(&2));
        assert_eq!(counts.get("public.empty"), Some(&0));
        assert_eq!(counts.get("sales.items"), Some(&3));
        assert_eq!(counts.len(), 3);
    }

    #[tokio::test]
    async fn rejects_archive_without_header() {
        let mut buf = Vec::new();
        write_end(&mut buf).await.unwrap();
        assert!(count_archive_rows(&mut &buf[..]).await.is_err());
    }

    #[test]
    fn quote_ident_doubles_embedded_quotes() {
        assert_eq!(quote_ident("orders"), "\"orders\"");
        assert_eq!(quote_ident("we\"ird"), "\"we\"\"ird\"");
    }
}
