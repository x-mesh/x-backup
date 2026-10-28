//! MongoDB native 엔진의 문서 수 세기(`verify --against`) — 판정은 [`crate::engine::compare`]가 맡는다.
//!
//! manifest에 문서 수가 없는 백업은 아카이브의 `C`/`D` 프레임을 끝까지 읽어 센다. 대상 DB는
//! 컬렉션마다 `countDocuments`(정확하지만 컬렉션을 훑는다)를 **하나씩 순차로** 실행한다.
//! `estimate`가 참이면 `estimatedDocumentCount`(메타데이터라 즉시 나오지만 샤드 클러스터나
//! 비정상 종료 뒤에는 부정확할 수 있다)를 쓴다.
//!
//! 키는 `db.컬렉션`이다. native 복구는 아카이브의 네임스페이스 그대로 복원하므로 대상에서도 같다.

use std::collections::{BTreeMap, BTreeSet};

use bson::{doc, Document};
use futures::TryStreamExt;
use mongodb::Client;
use tokio::io::AsyncRead;

use super::archive::{self, Frame};
use crate::config::secret::Secret;
use crate::engine::digest::RowDigest;
use crate::error::{Result, XBackupError};

/// manifest `table_digests.algorithm`에 기록하는 식별자. 해시 입력은 문서의 raw BSON 바이트다.
pub const DIGEST_ALGORITHM: &str = "sha256-sum128-bson-v1";

/// `backup.rs`와 같은 시스템 DB 목록(백업이 덤프하지 않는 DB).
const SYSTEM_DBS: [&str; 3] = ["admin", "config", "local"];

/// 아카이브 스트림을 끝까지 읽어 컬렉션별 문서 수를 센다. 키는 `db.컬렉션`이다.
pub async fn count_archive_docs<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<BTreeMap<String, u64>> {
    match archive::read_frame(reader).await? {
        Frame::Header(h) => {
            let fmt = h.get_str("format").unwrap_or("");
            if fmt != archive::FORMAT_ID {
                return Err(XBackupError::Failure(crate::tr!(
                    "native archive format mismatch: '{fmt}' (expected '{}')",
                    "네이티브 아카이브 포맷 불일치: '{fmt}'(기대 '{}')",
                    archive::FORMAT_ID
                )));
            }
        }
        other => {
            return Err(XBackupError::Failure(crate::tr!(
                "native archive header missing (first frame: {other:?})",
                "네이티브 아카이브 헤더 누락(첫 프레임: {other:?})"
            )))
        }
    }

    let mut counts = BTreeMap::new();
    let mut current: Option<String> = None;
    loop {
        match archive::read_frame(reader).await? {
            Frame::Collection(meta) => {
                let ns = meta.get_str("ns").unwrap_or("").to_string();
                counts.entry(ns.clone()).or_insert(0u64);
                current = Some(ns);
            }
            Frame::Document(_) => {
                let Some(ns) = current.as_ref() else {
                    return Err(XBackupError::Failure(crate::tr!(
                        "native archive document frame appeared before any collection frame",
                        "네이티브 아카이브에서 컬렉션 프레임 없이 문서 프레임이 나왔습니다"
                    )));
                };
                if let Some(n) = counts.get_mut(ns) {
                    *n += 1;
                }
            }
            Frame::End => return Ok(counts),
            Frame::Header(_) => {}
        }
    }
}

/// URI로 연결한다(백업과 같은 타임아웃 정책).
pub async fn connect(uri: &Secret) -> Result<Client> {
    let options = crate::engine::mongo::conn::client_options(uri, None).await?;
    Client::with_options(options).map_err(|e| {
        XBackupError::Failure(crate::tr!(
            "failed to create the MongoDB client: {e}",
            "MongoDB 클라이언트 생성 실패: {e}"
        ))
    })
}

/// 대상의 일반 컬렉션을 `db.컬렉션`으로 나열한다(백업과 같은 시스템 DB·`system.`·뷰 제외).
pub async fn live_namespaces(client: &Client) -> Result<BTreeSet<String>> {
    let db_names = client.list_database_names().await.map_err(|e| {
        XBackupError::Failure(crate::tr!(
            "failed to list databases: {e}",
            "데이터베이스 목록 조회 실패: {e}"
        ))
    })?;
    let mut namespaces = BTreeSet::new();
    for db_name in db_names {
        if SYSTEM_DBS.contains(&db_name.as_str()) {
            continue;
        }
        let mut specs = client
            .database(&db_name)
            .list_collections()
            .await
            .map_err(|e| {
                XBackupError::Failure(crate::tr!(
                    "failed to query collection specs ({db_name}): {e}",
                    "컬렉션 명세 조회 실패({db_name}): {e}"
                ))
            })?;
        while let Some(spec) = specs.try_next().await.map_err(|e| {
            XBackupError::Failure(crate::tr!(
                "failed to iterate collection specs ({db_name}): {e}",
                "컬렉션 명세 순회 실패({db_name}): {e}"
            ))
        })? {
            if spec.name.starts_with("system.")
                || !matches!(
                    spec.collection_type,
                    mongodb::results::CollectionType::Collection
                )
            {
                continue;
            }
            namespaces.insert(format!("{db_name}.{}", spec.name));
        }
    }
    Ok(namespaces)
}

/// 컬렉션 하나의 문서 수. `estimate`면 `estimatedDocumentCount`, 아니면 `countDocuments`.
pub async fn count_live_docs(client: &Client, ns: &str, estimate: bool) -> Result<u64> {
    let (db_name, coll_name) = ns.split_once('.').ok_or_else(|| {
        XBackupError::Failure(crate::tr!("malformed ns: '{ns}'", "ns 형식 오류: '{ns}'"))
    })?;
    let coll = client.database(db_name).collection::<Document>(coll_name);
    let count = if estimate {
        coll.estimated_document_count().await
    } else {
        coll.count_documents(doc! {}).await
    };
    count.map_err(|e| {
        XBackupError::Failure(crate::tr!(
            "failed to count documents of {ns}: {e}",
            "{ns} 문서 수 조회 실패: {e}"
        ))
    })
}

/// 컬렉션 하나의 문서를 전부 읽어 내용 다이제스트를 구한다. 백업과 같은 raw BSON 바이트를 해시한다.
pub async fn scan_live_digest(client: &Client, ns: &str) -> Result<String> {
    let (db_name, coll_name) = ns.split_once('.').ok_or_else(|| {
        XBackupError::Failure(crate::tr!("malformed ns: '{ns}'", "ns 형식 오류: '{ns}'"))
    })?;
    let coll = client
        .database(db_name)
        .collection::<bson::RawDocumentBuf>(coll_name);
    let mut cursor = coll.find(doc! {}).await.map_err(|e| {
        XBackupError::Failure(crate::tr!(
            "{ns}: failed to query documents: {e}",
            "{ns} 문서 조회 실패: {e}"
        ))
    })?;
    let mut digest = RowDigest::default();
    while cursor.advance().await.map_err(|e| {
        XBackupError::Failure(crate::tr!(
            "{ns}: cursor advance failed: {e}",
            "{ns} 커서 진행 실패: {e}"
        ))
    })? {
        digest.add(cursor.current().as_bytes());
    }
    Ok(digest.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::native::archive::{write_collection, write_end, write_header};
    use bson::RawDocumentBuf;

    async fn archive_with(collections: &[(&str, usize)]) -> Vec<u8> {
        let mut buf = Vec::new();
        write_header(&mut buf, "2026-10-28T00:00:00Z")
            .await
            .unwrap();
        for (ns, docs) in collections {
            write_collection(&mut buf, ns, &Document::new(), &[])
                .await
                .unwrap();
            for i in 0..*docs {
                let raw = RawDocumentBuf::from_document(&doc! { "n": i as i64 }).unwrap();
                archive::write_raw_document(&mut buf, &raw).await.unwrap();
            }
        }
        write_end(&mut buf).await.unwrap();
        buf
    }

    #[tokio::test]
    async fn counts_documents_per_namespace_and_keeps_empty_collections() {
        let buf = archive_with(&[("shop.orders", 3), ("shop.empty", 0), ("crm.users", 2)]).await;
        let counts = count_archive_docs(&mut &buf[..]).await.unwrap();
        assert_eq!(counts.get("shop.orders"), Some(&3));
        assert_eq!(counts.get("shop.empty"), Some(&0));
        assert_eq!(counts.get("crm.users"), Some(&2));
        assert_eq!(counts.len(), 3);
    }

    #[tokio::test]
    async fn rejects_archive_without_header() {
        let mut buf = Vec::new();
        write_end(&mut buf).await.unwrap();
        assert!(count_archive_docs(&mut &buf[..]).await.is_err());
    }
}
