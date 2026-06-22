//! 네이티브 아카이브(`xb-native-v1`)를 **mongodump `--out` 레이아웃**으로 풀어 쓰는 익스포터.
//!
//! `restore --to-dir`의 코어 — 서버(mongorestore) 없이 백업을 표준 도구로 쓸 수 있는 파일로
//! 추출한다. 입력은 복호화·압축해제된 네이티브 프레임 스트림([`super::archive`]), 출력은:
//!
//! ```text
//! <out>/<db>/<collection>.bson           # 연속 BSON 문서(mongodump .bson과 동일)
//! <out>/<db>/<collection>.metadata.json  # {options, indexes, ...} (Extended JSON)
//! ```
//!
//! 그대로 `mongorestore <out>`로 복원할 수 있다. 입력은 비동기 스트림(storage)에서 오고
//! 출력은 로컬 파일이라, 문서는 메모리에서 BSON 바이트로 직렬화한 뒤 비동기로 파일에 흘린다.

use std::path::Path;

use bson::{doc, Bson, Document};
use tokio::fs;
use tokio::io::{AsyncRead, AsyncWriteExt};

use super::archive::{self, Frame};
use crate::error::{Result, XBackupError};

/// 익스포트 결과 집계(사람용 요약·테스트 검증).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ExportSummary {
    /// 추출한 컬렉션 수.
    pub collections: u64,
    /// 추출한 문서 수.
    pub documents: u64,
    /// `.bson` 파일에 쓴 총 바이트(문서 BSON 합).
    pub bytes: u64,
}

/// 진행 중인 한 컬렉션의 출력 상태 — `file`이 `None`이면 필터로 건너뛰는 컬렉션.
struct CurrentColl {
    file: Option<fs::File>,
}

/// 네이티브 프레임 스트림을 `out_dir`에 mongodump 레이아웃으로 푼다.
///
/// `ns_include`가 `Some(db.coll)`이면 그 네임스페이스만 추출한다(나머지는 건너뜀).
/// 첫 프레임은 반드시 헤더여야 하며 포맷이 다르면 거부한다(복구 경로와 동일 검증).
pub async fn native_export_to_dir<R: AsyncRead + Unpin>(
    reader: &mut R,
    out_dir: &Path,
    ns_include: Option<&str>,
) -> Result<ExportSummary> {
    // 헤더 검증(포맷 일치) — native_restore와 동일 기준.
    match archive::read_frame(reader).await? {
        Frame::Header(h) => {
            let fmt = h.get_str("format").unwrap_or("");
            if fmt != archive::FORMAT_ID {
                return Err(XBackupError::Failure(format!(
                    "네이티브 아카이브 포맷 불일치: '{fmt}'(기대 '{}')",
                    archive::FORMAT_ID
                )));
            }
        }
        other => {
            return Err(XBackupError::Failure(format!(
                "네이티브 아카이브 헤더 누락(첫 프레임: {other:?})"
            )))
        }
    }

    fs::create_dir_all(out_dir).await.map_err(|e| {
        XBackupError::Failure(format!(
            "출력 디렉터리 생성 실패({}): {e}",
            out_dir.display()
        ))
    })?;

    let mut summary = ExportSummary::default();
    let mut current: Option<CurrentColl> = None;

    loop {
        match archive::read_frame(reader).await? {
            Frame::Header(_) => {
                return Err(XBackupError::Failure("아카이브에 헤더가 중복됩니다".into()))
            }
            Frame::Collection(meta) => {
                // 이전 컬렉션 파일을 flush·close.
                if let Some(c) = current.take() {
                    finish_file(c).await?;
                }
                let ns = meta
                    .get_str("ns")
                    .map_err(|_| XBackupError::Failure("컬렉션 프레임에 ns가 없습니다".into()))?;
                let (db_name, coll_name) = ns
                    .split_once('.')
                    .ok_or_else(|| XBackupError::Failure(format!("ns 형식 오류: '{ns}'")))?;

                if ns_include.is_some_and(|want| want != ns) {
                    // 필터 비매칭 — 파일을 만들지 않고 문서만 흘려보낸다(skip).
                    current = Some(CurrentColl { file: None });
                    continue;
                }

                let db_dir = out_dir.join(db_name);
                fs::create_dir_all(&db_dir).await.map_err(|e| {
                    XBackupError::Failure(format!(
                        "DB 디렉터리 생성 실패({}): {e}",
                        db_dir.display()
                    ))
                })?;

                // metadata.json(options/indexes) — mongorestore가 인덱스·옵션 복원에 사용.
                write_metadata(&db_dir, coll_name, &meta).await?;

                let bson_path = db_dir.join(format!("{coll_name}.bson"));
                let file = fs::File::create(&bson_path).await.map_err(|e| {
                    XBackupError::Failure(format!(".bson 생성 실패({}): {e}", bson_path.display()))
                })?;
                summary.collections += 1;
                current = Some(CurrentColl { file: Some(file) });
            }
            Frame::Document(d) => {
                let Some(c) = current.as_mut() else {
                    return Err(XBackupError::Failure(
                        "문서 프레임이 컬렉션보다 먼저 나왔습니다".into(),
                    ));
                };
                let Some(file) = c.file.as_mut() else {
                    continue; // skip 컬렉션
                };
                let mut bytes = Vec::new();
                d.to_writer(&mut bytes)
                    .map_err(|e| XBackupError::Failure(format!("문서 BSON 직렬화 실패: {e}")))?;
                file.write_all(&bytes)
                    .await
                    .map_err(|e| XBackupError::Failure(format!(".bson 쓰기 실패: {e}")))?;
                summary.documents += 1;
                summary.bytes += bytes.len() as u64;
            }
            Frame::End => {
                if let Some(c) = current.take() {
                    finish_file(c).await?;
                }
                break;
            }
        }
    }
    Ok(summary)
}

/// `<db_dir>/<coll>.metadata.json`을 쓴다 — `{options, indexes, collectionName, type}`를
/// Canonical Extended JSON으로(인덱스 키의 타입 보존). mongorestore가 그대로 읽는다.
async fn write_metadata(db_dir: &Path, coll: &str, meta: &Document) -> Result<()> {
    let options = meta.get_document("options").cloned().unwrap_or_default();
    let indexes: Vec<Bson> = meta
        .get_array("indexes")
        .map(|a| a.to_vec())
        .unwrap_or_default();
    let meta_doc = doc! {
        "options": options,
        "indexes": Bson::Array(indexes),
        "collectionName": coll,
        "type": "collection",
    };
    let ext = Bson::Document(meta_doc).into_canonical_extjson();
    let json = serde_json::to_string_pretty(&ext)
        .map_err(|e| XBackupError::Failure(format!("metadata.json 직렬화 실패: {e}")))?;
    let path = db_dir.join(format!("{coll}.metadata.json"));
    fs::write(&path, json).await.map_err(|e| {
        XBackupError::Failure(format!("metadata.json 쓰기 실패({}): {e}", path.display()))
    })
}

/// 컬렉션 .bson 파일을 flush한다(close는 Drop).
async fn finish_file(c: CurrentColl) -> Result<()> {
    if let Some(mut file) = c.file {
        file.flush()
            .await
            .map_err(|e| XBackupError::Failure(format!(".bson flush 실패: {e}")))?;
    }
    Ok(())
}

/// `out_dir`이 이미 존재하고 비어 있지 않은지(덮어쓰기 경고용). 존재하지 않으면 false.
pub fn dir_nonempty(out_dir: &Path) -> bool {
    std::fs::read_dir(out_dir)
        .map(|mut it| it.next().is_some())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::native::archive::{
        write_collection, write_end, write_header, write_raw_document,
    };

    /// 테스트용: `Document`를 raw BSON으로 직렬화해 문서 프레임으로 쓴다(백업 경로는
    /// raw 스트림을 그대로 흘리므로 [`write_raw_document`]만 공개돼 있다).
    async fn write_doc(buf: &mut Vec<u8>, d: &Document) {
        let bytes = bson::to_vec(d).unwrap();
        let raw = bson::RawDocument::from_bytes(&bytes).unwrap();
        write_raw_document(buf, raw).await.unwrap();
    }

    /// 합성 네이티브 스트림 → 임시 dir → .bson/.metadata.json 검증.
    #[tokio::test]
    async fn exports_native_stream_to_mongodump_layout() {
        // 1) 합성 네이티브 아카이브 한 벌 만든다(헤더 + 컬렉션 + 문서 2 + 끝).
        let mut buf: Vec<u8> = Vec::new();
        write_header(&mut buf, "2026-06-22T00:00:00Z")
            .await
            .unwrap();
        let options = doc! {};
        let indexes = vec![doc! { "v": 2, "key": { "_id": 1 }, "name": "_id_" }];
        write_collection(&mut buf, "shop.orders", &options, &indexes)
            .await
            .unwrap();
        write_doc(&mut buf, &doc! { "_id": 1, "item": "a" }).await;
        write_doc(&mut buf, &doc! { "_id": 2, "item": "b" }).await;
        write_end(&mut buf).await.unwrap();

        // 2) 익스포트.
        let dir = tempfile::tempdir().unwrap();
        let mut reader = std::io::Cursor::new(buf);
        let summary = native_export_to_dir(&mut reader, dir.path(), None)
            .await
            .unwrap();

        // 3) 집계 + 산출 파일 검증.
        assert_eq!(summary.collections, 1);
        assert_eq!(summary.documents, 2);
        let bson = dir.path().join("shop").join("orders.bson");
        let meta = dir.path().join("shop").join("orders.metadata.json");
        assert!(bson.is_file(), "orders.bson 생성");
        assert!(meta.is_file(), "orders.metadata.json 생성");

        // .bson은 연속 BSON 2건 — 다시 읽어 문서 수 확인.
        let raw = std::fs::read(&bson).unwrap();
        let mut cur = std::io::Cursor::new(raw);
        let d1 = Document::from_reader(&mut cur).unwrap();
        let d2 = Document::from_reader(&mut cur).unwrap();
        assert_eq!(d1.get_str("item").unwrap(), "a");
        assert_eq!(d2.get_str("item").unwrap(), "b");

        // metadata.json은 인덱스 _id_를 담는다.
        let meta_txt = std::fs::read_to_string(&meta).unwrap();
        assert!(meta_txt.contains("_id_"), "인덱스 메타 포함: {meta_txt}");
    }

    /// ns 필터는 매칭 컬렉션만 추출한다.
    #[tokio::test]
    async fn ns_filter_extracts_only_matching() {
        let mut buf: Vec<u8> = Vec::new();
        write_header(&mut buf, "2026-06-22T00:00:00Z")
            .await
            .unwrap();
        write_collection(&mut buf, "shop.orders", &doc! {}, &[])
            .await
            .unwrap();
        write_doc(&mut buf, &doc! { "_id": 1 }).await;
        write_collection(&mut buf, "shop.users", &doc! {}, &[])
            .await
            .unwrap();
        write_doc(&mut buf, &doc! { "_id": 9 }).await;
        write_end(&mut buf).await.unwrap();

        let dir = tempfile::tempdir().unwrap();
        let mut reader = std::io::Cursor::new(buf);
        let summary = native_export_to_dir(&mut reader, dir.path(), Some("shop.orders"))
            .await
            .unwrap();

        assert_eq!(summary.collections, 1);
        assert_eq!(summary.documents, 1);
        assert!(dir.path().join("shop").join("orders.bson").is_file());
        assert!(!dir.path().join("shop").join("users.bson").exists());
    }
}
