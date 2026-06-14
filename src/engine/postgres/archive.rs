//! PostgreSQL 백업 아카이브 포맷 `xb-pg-v1` — pg_dump 없이 드라이버(COPY)로 자체 직렬화.
//!
//! native(Mongo)와 동일한 **태그 + 프레임** 스트리밍 구조다(상수 메모리). 다만 PG는
//! 행 데이터를 COPY 바이너리 **불투명 바이트**로 다루므로(해석하지 않음), 데이터 프레임은
//! BSON처럼 self-delimiting이 아니라 **길이 프리픽스**를 둔다.
//!
//! ```text
//! [H 헤더] [Q 시퀀스]* ( [T 테이블] [D 데이터청크]* [X 테이블끝] )* [E 끝]
//! ```
//! - `H`(헤더): `{ format, created_at, pg_version }` — 1회.
//! - `Q`(시퀀스): `{ name, last_value }` — 테이블 생성 전에 만들어 nextval 기본값을 해소.
//! - `T`(테이블): `{ ns, quoted, schema, create_sql, constraints:[], indexes:[], copy_cols:[],
//!   identity_cols:[] }` — 테이블마다 1회. quoted/schema로 복구가 정확히 식별, copy_cols로 COPY
//!   컬럼을 한정(STORED generated 제외), identity_cols로 복구 후 시퀀스 리셋.
//! - `D`(데이터): COPY 바이너리 raw 청크(길이 프리픽스 + 바이트). 테이블당 N개.
//! - `X`(테이블끝): payload 없음 — COPY IN을 종료시키는 경계.
//! - `E`(끝): payload 없음.

use bson::Document;
use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::{Result, XBackupError};

/// 이 포맷의 식별자(manifest.archive_format에 기록 — 복구가 PG 엔진을 고르는 기준).
pub const FORMAT_ID: &str = "xb-pg-v1";

const TAG_HEADER: u8 = b'H';
const TAG_SEQUENCE: u8 = b'Q';
const TAG_TABLE: u8 = b'T';
const TAG_DATA: u8 = b'D';
const TAG_TABLE_END: u8 = b'X';
const TAG_END: u8 = b'E';

/// BSON/데이터 프레임 안전 상한(과대 할당 방지). COPY 청크는 보통 수십 KiB다.
const MAX_FRAME_BYTES: u32 = 256 * 1024 * 1024;

/// 읽어 들인 한 프레임.
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    /// 스트림 헤더.
    Header(Document),
    /// 시퀀스 메타.
    Sequence(Document),
    /// 테이블 메타(DDL).
    Table(Document),
    /// COPY 바이너리 raw 데이터 청크.
    Data(Vec<u8>),
    /// 현재 테이블의 데이터 끝(COPY IN 종료 경계).
    TableEnd,
    /// 스트림 끝.
    End,
}

/// 헤더 프레임을 쓴다(스트림 시작 1회).
pub async fn write_header<W: AsyncWrite + Unpin>(
    w: &mut W,
    created_at: &str,
    pg_version: &str,
) -> Result<()> {
    let doc =
        bson::doc! { "format": FORMAT_ID, "created_at": created_at, "pg_version": pg_version };
    write_doc_frame(w, TAG_HEADER, &doc).await
}

/// 시퀀스 메타 프레임을 쓴다(`name`, `schema`, `last_value`, `is_called`).
///
/// `schema`(quote됨)는 복구가 시퀀스 생성 전 `CREATE SCHEMA IF NOT EXISTS`로 비-기본 스키마를
/// 먼저 만들게 한다. `is_called=false`(미호출 시퀀스)면 `setval(.., last_value, false)`로 복원해
/// 첫 `nextval`이 `last_value`가 되게 한다(off-by-one 방지).
pub async fn write_sequence<W: AsyncWrite + Unpin>(
    w: &mut W,
    name: &str,
    schema: &str,
    last_value: i64,
    is_called: bool,
) -> Result<()> {
    let doc = bson::doc! {
        "name": name, "schema": schema, "last_value": last_value, "is_called": is_called,
    };
    write_doc_frame(w, TAG_SEQUENCE, &doc).await
}

/// 테이블 메타 프레임에 담을 정보(생성 DDL·제약·인덱스 + 복구가 쓸 정확한 식별자/COPY 목록).
pub struct TableFrame<'a> {
    /// `schema.table`(사람용·setval 인자).
    pub ns: &'a str,
    /// 정확히 quote된 전체 식별자(`"sch"."tbl"`) — COPY/DROP에 사용(create_sql 재파싱 불필요).
    pub quoted: &'a str,
    /// quote된 스키마(`"sch"`) — 복구 전 `CREATE SCHEMA IF NOT EXISTS`.
    pub schema: &'a str,
    pub create_sql: &'a str,
    pub constraints: &'a [String],
    pub indexes: &'a [String],
    /// COPY에 쓸 컬럼 목록(quote됨) — STORED generated 컬럼은 제외한다(COPY 불가).
    pub copy_cols: &'a [String],
    /// IDENTITY 컬럼의 평문 이름 — 복구 후 시퀀스를 max로 리셋(setval pg_get_serial_sequence).
    pub identity_cols: &'a [String],
}

/// 테이블 메타 프레임을 쓴다.
pub async fn write_table<W: AsyncWrite + Unpin>(w: &mut W, t: &TableFrame<'_>) -> Result<()> {
    let doc = bson::doc! {
        "ns": t.ns,
        "quoted": t.quoted,
        "schema": t.schema,
        "create_sql": t.create_sql,
        "constraints": t.constraints.iter().map(|s| bson::Bson::String(s.clone())).collect::<Vec<_>>(),
        "indexes": t.indexes.iter().map(|s| bson::Bson::String(s.clone())).collect::<Vec<_>>(),
        "copy_cols": t.copy_cols.iter().map(|s| bson::Bson::String(s.clone())).collect::<Vec<_>>(),
        "identity_cols": t.identity_cols.iter().map(|s| bson::Bson::String(s.clone())).collect::<Vec<_>>(),
    };
    write_doc_frame(w, TAG_TABLE, &doc).await
}

/// COPY 데이터 청크를 쓴다(길이 프리픽스 + raw 바이트). 빈 청크는 건너뛴다.
pub async fn write_data<W: AsyncWrite + Unpin>(w: &mut W, chunk: &[u8]) -> Result<()> {
    if chunk.is_empty() {
        return Ok(());
    }
    let len = chunk.len() as u32;
    w.write_all(&[TAG_DATA])
        .await
        .map_err(io_err("데이터 태그 쓰기"))?;
    w.write_all(&len.to_le_bytes())
        .await
        .map_err(io_err("데이터 길이 쓰기"))?;
    w.write_all(chunk)
        .await
        .map_err(io_err("데이터 본문 쓰기"))?;
    Ok(())
}

/// 테이블 데이터 끝 프레임을 쓴다.
pub async fn write_table_end<W: AsyncWrite + Unpin>(w: &mut W) -> Result<()> {
    w.write_all(&[TAG_TABLE_END])
        .await
        .map_err(io_err("테이블끝 태그 쓰기"))
}

/// 끝 프레임을 쓴다(스트림 종료).
pub async fn write_end<W: AsyncWrite + Unpin>(w: &mut W) -> Result<()> {
    w.write_all(&[TAG_END])
        .await
        .map_err(io_err("끝 태그 쓰기"))
}

/// BSON 문서 한 개를 태그와 함께 쓴다(H/Q/T 공용).
async fn write_doc_frame<W: AsyncWrite + Unpin>(w: &mut W, tag: u8, doc: &Document) -> Result<()> {
    let mut buf = Vec::new();
    doc.to_writer(&mut buf)
        .map_err(|e| XBackupError::Failure(format!("PG 아카이브 프레임 직렬화 실패: {e}")))?;
    w.write_all(&[tag])
        .await
        .map_err(io_err("프레임 태그 쓰기"))?;
    w.write_all(&buf)
        .await
        .map_err(io_err("프레임 본문 쓰기"))?;
    Ok(())
}

/// 다음 프레임을 읽는다. 스트림이 깔끔히 끝나면(태그 부재) `End`로 본다.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Frame> {
    let mut tag = [0u8; 1];
    match r.read_exact(&mut tag).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(Frame::End),
        Err(e) => return Err(io_err("프레임 태그 읽기")(e)),
    }
    match tag[0] {
        TAG_END => Ok(Frame::End),
        TAG_TABLE_END => Ok(Frame::TableEnd),
        TAG_HEADER => Ok(Frame::Header(read_doc(r).await?)),
        TAG_SEQUENCE => Ok(Frame::Sequence(read_doc(r).await?)),
        TAG_TABLE => Ok(Frame::Table(read_doc(r).await?)),
        TAG_DATA => Ok(Frame::Data(read_data(r).await?)),
        other => Err(XBackupError::Failure(format!(
            "PG 아카이브 프레임 태그 손상: 0x{other:02x}"
        ))),
    }
}

/// BSON 문서 한 개를 읽는다(앞 4바이트 LE 길이).
async fn read_doc<R: AsyncRead + Unpin>(r: &mut R) -> Result<Document> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)
        .await
        .map_err(io_err("프레임 길이 읽기"))?;
    let len = u32::from_le_bytes(len_buf);
    if !(5..=MAX_FRAME_BYTES).contains(&len) {
        return Err(XBackupError::Failure(format!(
            "PG 아카이브 문서 길이 비정상: {len}바이트"
        )));
    }
    let mut buf = vec![0u8; len as usize];
    buf[..4].copy_from_slice(&len_buf);
    r.read_exact(&mut buf[4..])
        .await
        .map_err(io_err("프레임 본문 읽기"))?;
    Document::from_reader(&buf[..])
        .map_err(|e| XBackupError::Failure(format!("PG 아카이브 프레임 파싱 실패: {e}")))
}

/// 길이 프리픽스 데이터 청크를 읽는다.
async fn read_data<R: AsyncRead + Unpin>(r: &mut R) -> Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)
        .await
        .map_err(io_err("데이터 길이 읽기"))?;
    let len = u32::from_le_bytes(len_buf);
    if len == 0 || len > MAX_FRAME_BYTES {
        return Err(XBackupError::Failure(format!(
            "PG 아카이브 데이터 길이 비정상: {len}바이트"
        )));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)
        .await
        .map_err(io_err("데이터 본문 읽기"))?;
    Ok(buf)
}

/// `Bytes` 청크를 그대로 데이터 프레임으로 쓰는 편의 함수(백업 task에서 사용).
pub async fn write_data_bytes<W: AsyncWrite + Unpin>(w: &mut W, chunk: &Bytes) -> Result<()> {
    write_data(w, chunk).await
}

/// IO 에러를 XBackupError로 감싸는 헬퍼.
fn io_err(ctx: &'static str) -> impl Fn(std::io::Error) -> XBackupError {
    move |e| XBackupError::Failure(format!("PG 아카이브 {ctx} 실패: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trip_full_stream() {
        let mut buf: Vec<u8> = Vec::new();
        write_header(&mut buf, "2026-06-14T00:00:00Z", "16.2")
            .await
            .unwrap();
        write_sequence(&mut buf, "public.t_id_seq", "\"public\"", 1000, true)
            .await
            .unwrap();
        write_table(
            &mut buf,
            &TableFrame {
                ns: "public.t",
                quoted: "\"public\".\"t\"",
                schema: "\"public\"",
                create_sql: "CREATE TABLE public.t (id int, name text)",
                constraints: &[
                    "ALTER TABLE public.t ADD CONSTRAINT t_pkey PRIMARY KEY (id)".to_string(),
                ],
                indexes: &["CREATE INDEX t_name_idx ON public.t (name)".to_string()],
                copy_cols: &["\"id\"".to_string(), "\"name\"".to_string()],
                identity_cols: &[],
            },
        )
        .await
        .unwrap();
        write_data(&mut buf, b"PGCOPY\n\xff\r\n\0").await.unwrap();
        write_data(&mut buf, b"row-bytes").await.unwrap();
        write_table_end(&mut buf).await.unwrap();
        write_end(&mut buf).await.unwrap();

        let mut r = std::io::Cursor::new(buf);
        match read_frame(&mut r).await.unwrap() {
            Frame::Header(h) => {
                assert_eq!(h.get_str("format").unwrap(), FORMAT_ID);
                assert_eq!(h.get_str("pg_version").unwrap(), "16.2");
            }
            f => panic!("헤더 기대, {f:?}"),
        }
        match read_frame(&mut r).await.unwrap() {
            Frame::Sequence(s) => {
                assert_eq!(s.get_i64("last_value").unwrap(), 1000);
                assert!(s.get_bool("is_called").unwrap());
            }
            f => panic!("시퀀스 기대, {f:?}"),
        }
        match read_frame(&mut r).await.unwrap() {
            Frame::Table(t) => {
                assert_eq!(t.get_str("ns").unwrap(), "public.t");
                assert_eq!(t.get_str("quoted").unwrap(), "\"public\".\"t\"");
                assert_eq!(t.get_array("constraints").unwrap().len(), 1);
                assert_eq!(t.get_array("indexes").unwrap().len(), 1);
                assert_eq!(t.get_array("copy_cols").unwrap().len(), 2);
            }
            f => panic!("테이블 기대, {f:?}"),
        }
        assert_eq!(
            read_frame(&mut r).await.unwrap(),
            Frame::Data(b"PGCOPY\n\xff\r\n\0".to_vec())
        );
        assert_eq!(
            read_frame(&mut r).await.unwrap(),
            Frame::Data(b"row-bytes".to_vec())
        );
        assert_eq!(read_frame(&mut r).await.unwrap(), Frame::TableEnd);
        assert_eq!(read_frame(&mut r).await.unwrap(), Frame::End);
    }

    #[tokio::test]
    async fn empty_data_chunk_is_skipped() {
        let mut buf: Vec<u8> = Vec::new();
        write_data(&mut buf, b"").await.unwrap();
        write_end(&mut buf).await.unwrap();
        let mut r = std::io::Cursor::new(buf);
        // 빈 청크는 쓰이지 않으므로 바로 End.
        assert_eq!(read_frame(&mut r).await.unwrap(), Frame::End);
    }
}
