//! MySQL 백업 아카이브 포맷 `xb-mysql-v1` — mysqldump 없이 드라이버로 자체 직렬화.
//!
//! PostgreSQL 엔진의 `xb-pg-v1`과 동일한 **태그 + 프레임** 스트리밍 구조다(상수 메모리).
//! 다만 MySQL은 COPY 같은 서버측 텍스트 덤프가 없어, 데이터 프레임은 **백업 시점에 렌더링한
//! SQL value 튜플**(`(v1, v2, ...)`)을 길이 프리픽스로 담는다 — 복구는 이 튜플을
//! `INSERT INTO tbl (cols) VALUES (...),(...)`로 묶기만 하면 된다(복구 측 타입 해석 불필요).
//!
//! ```text
//! [H 헤더] [R 선행DDL]* ( [T 테이블] [D 행튜플]* [X 테이블끝] )* [O 후행DDL]* [E 끝]
//! ```
//! - `H`(헤더): `{ format, created_at, mysql_version, binlog_file, binlog_pos, gtid_executed }` — 1회.
//!   binlog 좌표는 증분 base 식별을 위해 아카이브에도 내장한다(manifest와 이중 기록).
//! - `R`(선행 DDL): `{ sql }` — 테이블보다 먼저 실행(현재는 미사용, 향후 확장 여지).
//! - `T`(테이블): `{ ns, quoted, create_sql, insert_cols:[] }` — 테이블마다 1회.
//!   `create_sql`은 `SHOW CREATE TABLE` 결과(제약·인덱스·AUTO_INCREMENT 내장).
//!   `insert_cols`는 INSERT 컬럼 목록(generated 컬럼 제외, invisible 포함, quote됨).
//! - `D`(데이터): 렌더링된 한 행의 value 튜플 `(...)`(길이 프리픽스). 테이블당 N개.
//! - `X`(테이블끝): payload 없음 — 한 테이블의 행 경계.
//! - `O`(후행 DDL): `{ sql }` — 데이터 적재 후 실행(뷰·트리거·루틴·이벤트). 의존성 순서는 복구가 재시도로 흡수.
//! - `E`(끝): payload 없음.

use bson::Document;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::{Result, XBackupError};

/// 이 포맷의 식별자(manifest.archive_format에 기록 — 복구가 MySQL 엔진을 고르는 기준).
pub const FORMAT_ID: &str = "xb-mysql-v1";

const TAG_HEADER: u8 = b'H';
const TAG_PRE: u8 = b'R';
const TAG_POST: u8 = b'O';
const TAG_TABLE: u8 = b'T';
const TAG_DATA: u8 = b'D';
const TAG_TABLE_END: u8 = b'X';
const TAG_END: u8 = b'E';

/// BSON/데이터 프레임 안전 상한(과대 할당 방지).
const MAX_FRAME_BYTES: u32 = 256 * 1024 * 1024;

/// 읽어 들인 한 프레임.
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    /// 스트림 헤더.
    Header(Document),
    /// 선행 DDL — 테이블 전 실행.
    Pre(Document),
    /// 후행 DDL(뷰·트리거·루틴·이벤트) — 데이터 후 실행.
    Post(Document),
    /// 테이블 메타(DDL + INSERT 컬럼).
    Table(Document),
    /// 렌더링된 한 행의 value 튜플 `(...)` 바이트.
    Data(Vec<u8>),
    /// 현재 테이블의 데이터 끝 경계.
    TableEnd,
    /// 스트림 끝.
    End,
}

/// 헤더 프레임에 담을 정보.
pub struct HeaderFrame<'a> {
    pub created_at: &'a str,
    pub mysql_version: &'a str,
    /// 스냅샷 시점 binlog 파일명(없으면 빈 문자열).
    pub binlog_file: &'a str,
    /// 스냅샷 시점 binlog 위치.
    pub binlog_pos: u64,
    /// 스냅샷 시점 gtid_executed(없으면 빈 문자열).
    pub gtid_executed: &'a str,
}

/// 헤더 프레임을 쓴다(스트림 시작 1회).
pub async fn write_header<W: AsyncWrite + Unpin>(w: &mut W, h: &HeaderFrame<'_>) -> Result<()> {
    let doc = bson::doc! {
        "format": FORMAT_ID,
        "created_at": h.created_at,
        "mysql_version": h.mysql_version,
        "binlog_file": h.binlog_file,
        "binlog_pos": h.binlog_pos as i64,
        "gtid_executed": h.gtid_executed,
    };
    write_doc_frame(w, TAG_HEADER, &doc).await
}

/// 선행 DDL 프레임을 쓴다.
pub async fn write_pre<W: AsyncWrite + Unpin>(w: &mut W, sql: &str) -> Result<()> {
    write_doc_frame(w, TAG_PRE, &bson::doc! { "sql": sql }).await
}

/// 후행 DDL 프레임을 쓴다(뷰·트리거·루틴·이벤트 — 데이터 후 실행).
///
/// `kind`(view|trigger|procedure|function|event)와 `name`은 복구가 `drop=true`일 때
/// `DROP <kind> IF EXISTS <name>`을 먼저 실행해 멱등 재생성을 가능하게 한다.
pub async fn write_post<W: AsyncWrite + Unpin>(
    w: &mut W,
    kind: &str,
    name: &str,
    sql: &str,
) -> Result<()> {
    write_doc_frame(w, TAG_POST, &bson::doc! { "kind": kind, "name": name, "sql": sql }).await
}

/// 테이블 메타 프레임에 담을 정보.
pub struct TableFrame<'a> {
    /// `db.table`(사람용·로그).
    pub ns: &'a str,
    /// 정확히 quote된 전체 식별자(`` `db`.`tbl` ``) — INSERT/DROP에 사용.
    pub quoted: &'a str,
    /// `SHOW CREATE TABLE` 결과(제약·인덱스·AUTO_INCREMENT 내장).
    pub create_sql: &'a str,
    /// INSERT 컬럼 목록(quote됨) — generated 컬럼 제외, invisible 포함.
    pub insert_cols: &'a [String],
}

/// 테이블 메타 프레임을 쓴다.
pub async fn write_table<W: AsyncWrite + Unpin>(w: &mut W, t: &TableFrame<'_>) -> Result<()> {
    let doc = bson::doc! {
        "ns": t.ns,
        "quoted": t.quoted,
        "create_sql": t.create_sql,
        "insert_cols": t.insert_cols.iter().map(|s| bson::Bson::String(s.clone())).collect::<Vec<_>>(),
    };
    write_doc_frame(w, TAG_TABLE, &doc).await
}

/// 한 행의 렌더링된 value 튜플 `(...)`를 쓴다(길이 프리픽스 + 바이트).
pub async fn write_row<W: AsyncWrite + Unpin>(w: &mut W, tuple: &[u8]) -> Result<()> {
    if tuple.is_empty() {
        return Ok(());
    }
    // read 측 상한(MAX_FRAME_BYTES)과 대칭 — 초과 행은 백업 시점에 실패시켜(복구 가능) 읽을 수
    // 없는 아카이브 생성을 막는다.
    if tuple.len() > MAX_FRAME_BYTES as usize {
        return Err(XBackupError::Failure(format!(
            "행 튜플이 프레임 상한({} MiB)을 초과합니다: {}바이트 — 거대 BLOB 행은 현재 미지원",
            MAX_FRAME_BYTES / (1024 * 1024),
            tuple.len()
        )));
    }
    let len = tuple.len() as u32;
    w.write_all(&[TAG_DATA])
        .await
        .map_err(io_err("데이터 태그 쓰기"))?;
    w.write_all(&len.to_le_bytes())
        .await
        .map_err(io_err("데이터 길이 쓰기"))?;
    w.write_all(tuple)
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

/// BSON 문서 한 개를 태그와 함께 쓴다.
async fn write_doc_frame<W: AsyncWrite + Unpin>(w: &mut W, tag: u8, doc: &Document) -> Result<()> {
    let mut buf = Vec::new();
    doc.to_writer(&mut buf)
        .map_err(|e| XBackupError::Failure(format!("MySQL 아카이브 프레임 직렬화 실패: {e}")))?;
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
        TAG_PRE => Ok(Frame::Pre(read_doc(r).await?)),
        TAG_POST => Ok(Frame::Post(read_doc(r).await?)),
        TAG_TABLE => Ok(Frame::Table(read_doc(r).await?)),
        TAG_DATA => Ok(Frame::Data(read_data(r).await?)),
        other => Err(XBackupError::Failure(format!(
            "MySQL 아카이브 프레임 태그 손상: 0x{other:02x}"
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
            "MySQL 아카이브 문서 길이 비정상: {len}바이트"
        )));
    }
    let mut buf = vec![0u8; len as usize];
    buf[..4].copy_from_slice(&len_buf);
    r.read_exact(&mut buf[4..])
        .await
        .map_err(io_err("프레임 본문 읽기"))?;
    Document::from_reader(&buf[..])
        .map_err(|e| XBackupError::Failure(format!("MySQL 아카이브 프레임 파싱 실패: {e}")))
}

/// 길이 프리픽스 데이터(행 튜플)를 읽는다.
async fn read_data<R: AsyncRead + Unpin>(r: &mut R) -> Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)
        .await
        .map_err(io_err("데이터 길이 읽기"))?;
    let len = u32::from_le_bytes(len_buf);
    if len == 0 || len > MAX_FRAME_BYTES {
        return Err(XBackupError::Failure(format!(
            "MySQL 아카이브 데이터 길이 비정상: {len}바이트"
        )));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)
        .await
        .map_err(io_err("데이터 본문 읽기"))?;
    Ok(buf)
}

/// IO 에러를 XBackupError로 감싸는 헬퍼.
fn io_err(ctx: &'static str) -> impl Fn(std::io::Error) -> XBackupError {
    move |e| XBackupError::Failure(format!("MySQL 아카이브 {ctx} 실패: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trip_full_stream() {
        let mut buf: Vec<u8> = Vec::new();
        write_header(
            &mut buf,
            &HeaderFrame {
                created_at: "2026-06-14T00:00:00Z",
                mysql_version: "8.0.39",
                binlog_file: "binlog.000003",
                binlog_pos: 4567,
                gtid_executed: "uuid:1-10",
            },
        )
        .await
        .unwrap();
        write_table(
            &mut buf,
            &TableFrame {
                ns: "app.t",
                quoted: "`app`.`t`",
                create_sql: "CREATE TABLE `t` (`id` int NOT NULL, PRIMARY KEY (`id`))",
                insert_cols: &["`id`".to_string(), "`name`".to_string()],
            },
        )
        .await
        .unwrap();
        write_row(&mut buf, b"(1,'a')").await.unwrap();
        write_row(&mut buf, b"(2,0x00FF)").await.unwrap();
        write_table_end(&mut buf).await.unwrap();
        write_post(&mut buf, "view", "v", "CREATE VIEW `v` AS SELECT 1")
            .await
            .unwrap();
        write_end(&mut buf).await.unwrap();

        let mut r = std::io::Cursor::new(buf);
        match read_frame(&mut r).await.unwrap() {
            Frame::Header(h) => {
                assert_eq!(h.get_str("format").unwrap(), FORMAT_ID);
                assert_eq!(h.get_str("mysql_version").unwrap(), "8.0.39");
                assert_eq!(h.get_str("binlog_file").unwrap(), "binlog.000003");
                assert_eq!(h.get_i64("binlog_pos").unwrap(), 4567);
                assert_eq!(h.get_str("gtid_executed").unwrap(), "uuid:1-10");
            }
            f => panic!("헤더 기대, {f:?}"),
        }
        match read_frame(&mut r).await.unwrap() {
            Frame::Table(t) => {
                assert_eq!(t.get_str("ns").unwrap(), "app.t");
                assert_eq!(t.get_str("quoted").unwrap(), "`app`.`t`");
                assert_eq!(t.get_array("insert_cols").unwrap().len(), 2);
            }
            f => panic!("테이블 기대, {f:?}"),
        }
        assert_eq!(
            read_frame(&mut r).await.unwrap(),
            Frame::Data(b"(1,'a')".to_vec())
        );
        assert_eq!(
            read_frame(&mut r).await.unwrap(),
            Frame::Data(b"(2,0x00FF)".to_vec())
        );
        assert_eq!(read_frame(&mut r).await.unwrap(), Frame::TableEnd);
        match read_frame(&mut r).await.unwrap() {
            Frame::Post(p) => assert_eq!(p.get_str("sql").unwrap(), "CREATE VIEW `v` AS SELECT 1"),
            f => panic!("후행 DDL 기대, {f:?}"),
        }
        assert_eq!(read_frame(&mut r).await.unwrap(), Frame::End);
    }

    #[tokio::test]
    async fn empty_row_is_skipped() {
        let mut buf: Vec<u8> = Vec::new();
        write_row(&mut buf, b"").await.unwrap();
        write_end(&mut buf).await.unwrap();
        let mut r = std::io::Cursor::new(buf);
        assert_eq!(read_frame(&mut r).await.unwrap(), Frame::End);
    }
}
