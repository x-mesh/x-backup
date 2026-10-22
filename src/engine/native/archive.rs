//! 네이티브 백업 아카이브 포맷 `xb-native-v1` — mongodump 없이 자체 직렬화.
//!
//! 전 구간 스트리밍을 위해 **태그 + 프레임** 구조다. 프레임을 하나씩 읽고 쓰므로
//! 데이터 전체를 메모리에 적재하지 않는다(PRD §11 상수 메모리).
//!
//! ```text
//! [H 헤더]  ([C 컬렉션메타]  [D 문서]*)*  [E 끝]
//! ```
//! 각 프레임 = 1바이트 태그 + (H/C/D는) BSON 문서. BSON은 앞 4바이트가 전체 길이라
//! self-delimiting이므로 길이 프리픽스를 따로 두지 않는다.
//! - `H`(헤더): `{ format, created_at }` — 스트림 시작 1회.
//! - `C`(컬렉션): `{ ns, options, indexes:[..] }` — 컬렉션마다 1회.
//! - `D`(문서): 사용자 문서 raw BSON — 컬렉션 헤더 뒤로 N개.
//! - `E`(끝): payload 없음.
//!
//! 이 포맷으로 만든 백업은 **네이티브 엔진으로만** 복구한다(manifest.archive_format로 구분).

use bson::{Document, RawDocument};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::{Result, XBackupError};

/// 이 포맷의 식별자(manifest.archive_format에 기록).
pub const FORMAT_ID: &str = "xb-native-v1";

const TAG_HEADER: u8 = b'H';
const TAG_COLLECTION: u8 = b'C';
const TAG_DOCUMENT: u8 = b'D';
const TAG_END: u8 = b'E';

/// BSON 문서의 안전 상한(16MiB + 여유) — 손상된 스트림의 과대 할당 방지.
const MAX_FRAME_BYTES: u32 = 32 * 1024 * 1024;

/// 읽어 들인 한 프레임.
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    /// 스트림 헤더(format/created_at).
    Header(Document),
    /// 컬렉션 메타(ns/options/indexes).
    Collection(Document),
    /// 사용자 문서.
    Document(Document),
    /// 스트림 끝.
    End,
}

/// 헤더 프레임을 쓴다(스트림 시작 1회).
pub async fn write_header<W: AsyncWrite + Unpin>(w: &mut W, created_at: &str) -> Result<()> {
    let doc = bson::doc! { "format": FORMAT_ID, "created_at": created_at };
    write_doc_frame(w, TAG_HEADER, &doc).await
}

/// 컬렉션 메타 프레임을 쓴다(ns/options/indexes).
pub async fn write_collection<W: AsyncWrite + Unpin>(
    w: &mut W,
    ns: &str,
    options: &Document,
    indexes: &[Document],
) -> Result<()> {
    let doc = bson::doc! {
        "ns": ns,
        "options": options.clone(),
        "indexes": indexes.iter().cloned().map(bson::Bson::Document).collect::<Vec<_>>(),
    };
    write_doc_frame(w, TAG_COLLECTION, &doc).await
}

/// 문서 프레임을 쓴다 — 커서가 준 raw BSON 바이트를 그대로 흘린다(재직렬화 없음).
pub async fn write_raw_document<W: AsyncWrite + Unpin>(w: &mut W, raw: &RawDocument) -> Result<()> {
    w.write_all(&[TAG_DOCUMENT])
        .await
        .map_err(io_err("문서 태그 쓰기"))?;
    w.write_all(raw.as_bytes())
        .await
        .map_err(io_err("문서 본문 쓰기"))?;
    Ok(())
}

/// 끝 프레임을 쓴다(스트림 종료).
pub async fn write_end<W: AsyncWrite + Unpin>(w: &mut W) -> Result<()> {
    w.write_all(&[TAG_END])
        .await
        .map_err(io_err("끝 태그 쓰기"))
}

/// BSON 문서 한 개를 태그와 함께 쓴다(H/C 공용).
async fn write_doc_frame<W: AsyncWrite + Unpin>(w: &mut W, tag: u8, doc: &Document) -> Result<()> {
    let mut buf = Vec::new();
    doc.to_writer(&mut buf).map_err(|e| {
        XBackupError::Failure(crate::tr!(
            "failed to serialize an archive frame: {e}",
            "아카이브 프레임 직렬화 실패: {e}"
        ))
    })?;
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
        // 정상 EOF(E 태그 없이 끝)도 End로 관대히 처리.
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(Frame::End),
        Err(e) => return Err(io_err("프레임 태그 읽기")(e)),
    }
    match tag[0] {
        TAG_END => Ok(Frame::End),
        TAG_HEADER => Ok(Frame::Header(read_doc(r).await?)),
        TAG_COLLECTION => Ok(Frame::Collection(read_doc(r).await?)),
        TAG_DOCUMENT => Ok(Frame::Document(read_doc(r).await?)),
        other => Err(XBackupError::Failure(crate::tr!(
            "corrupt archive frame tag: 0x{other:02x}",
            "아카이브 프레임 태그 손상: 0x{other:02x}"
        ))),
    }
}

/// BSON 문서 한 개를 읽는다 — 앞 4바이트(LE 길이)를 보고 나머지를 채운다.
async fn read_doc<R: AsyncRead + Unpin>(r: &mut R) -> Result<Document> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)
        .await
        .map_err(io_err("프레임 길이 읽기"))?;
    let len = u32::from_le_bytes(len_buf);
    if !(5..=MAX_FRAME_BYTES).contains(&len) {
        return Err(XBackupError::Failure(crate::tr!(
            "invalid archive frame length: {len} bytes",
            "아카이브 프레임 길이 비정상: {len}바이트"
        )));
    }
    let mut buf = vec![0u8; len as usize];
    buf[..4].copy_from_slice(&len_buf);
    r.read_exact(&mut buf[4..])
        .await
        .map_err(io_err("프레임 본문 읽기"))?;
    Document::from_reader(&buf[..]).map_err(|e| {
        XBackupError::Failure(crate::tr!(
            "failed to parse an archive frame: {e}",
            "아카이브 프레임 파싱 실패: {e}"
        ))
    })
}

/// IO 에러를 XBackupError로 감싸는 헬퍼.
fn io_err(ctx: &'static str) -> impl Fn(std::io::Error) -> XBackupError {
    move |e| {
        XBackupError::Failure(crate::tr!(
            "archive {ctx} failed: {e}",
            "아카이브 {ctx} 실패: {e}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 헤더 → 컬렉션(+인덱스) → 문서 2건 → 끝을 쓰고 다시 읽어 동일한지(round-trip).
    #[tokio::test]
    async fn round_trip_full_stream() {
        let mut buf: Vec<u8> = Vec::new();
        write_header(&mut buf, "2026-06-14T00:00:00Z")
            .await
            .unwrap();
        let options = bson::doc! { "capped": true, "size": 1024_i64 };
        let indexes = vec![bson::doc! { "key": { "uid": 1 }, "name": "uid_1", "unique": true }];
        write_collection(&mut buf, "shop.orders", &options, &indexes)
            .await
            .unwrap();
        let d1 = bson::doc! { "_id": 1_i64, "x": "a" };
        let d2 = bson::doc! { "_id": 2_i64, "x": "b" };
        for d in [&d1, &d2] {
            let raw = bson::RawDocumentBuf::from_document(d).unwrap();
            write_raw_document(&mut buf, &raw).await.unwrap();
        }
        write_end(&mut buf).await.unwrap();

        // 다시 읽기.
        let mut r = std::io::Cursor::new(buf);
        match read_frame(&mut r).await.unwrap() {
            Frame::Header(h) => assert_eq!(h.get_str("format").unwrap(), FORMAT_ID),
            f => panic!("헤더 기대, {f:?}"),
        }
        match read_frame(&mut r).await.unwrap() {
            Frame::Collection(c) => {
                assert_eq!(c.get_str("ns").unwrap(), "shop.orders");
                assert!(c
                    .get_document("options")
                    .unwrap()
                    .get_bool("capped")
                    .unwrap());
                assert_eq!(c.get_array("indexes").unwrap().len(), 1);
            }
            f => panic!("컬렉션 기대, {f:?}"),
        }
        assert_eq!(read_frame(&mut r).await.unwrap(), Frame::Document(d1));
        assert_eq!(read_frame(&mut r).await.unwrap(), Frame::Document(d2));
        assert_eq!(read_frame(&mut r).await.unwrap(), Frame::End);
    }

    /// 태그 없이 끝나도(미완료 스트림) End로 관대히 처리한다.
    #[tokio::test]
    async fn eof_without_end_tag_is_end() {
        let mut r = std::io::Cursor::new(Vec::<u8>::new());
        assert_eq!(read_frame(&mut r).await.unwrap(), Frame::End);
    }

    /// 손상된 태그는 명확한 에러.
    #[tokio::test]
    async fn corrupt_tag_errors() {
        let mut r = std::io::Cursor::new(vec![b'Z']);
        assert!(read_frame(&mut r).await.is_err());
    }
}
