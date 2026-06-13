//! 네이티브 풀 복구 — [`archive`](super::archive) 스트림을 드라이버로 복원한다.
//!
//! 프레임을 하나씩 읽어:
//! - `C`(컬렉션): (`--drop`이면 먼저 drop) `create`로 옵션 포함 생성 → `createIndexes`로
//!   인덱스 복원(`_id_` 제외).
//! - `D`(문서): 배치로 모아 `insert_many`(상수 메모리).
//! - `E`(끝): 마지막 배치 flush.
//!
//! `ns_include`(`--only db.coll`)면 그 네임스페이스만 복원한다.

use bson::{doc, Document};
use mongodb::Client;
use tokio::io::AsyncRead;

use super::archive::{self, Frame};
use crate::error::{Result, XBackupError};

/// insert_many 배치 크기(상수 메모리 — 문서를 모두 적재하지 않음).
const INSERT_BATCH: usize = 1000;

/// 아카이브 스트림을 대상 클라이언트로 복원한다.
///
/// `drop`이면 각 컬렉션을 복원 전 drop한다. `ns_include`(`db.coll`)면 그 네임스페이스만.
/// 반환은 삽입한 총 문서 수.
pub async fn native_restore<R: AsyncRead + Unpin>(
    reader: &mut R,
    client: &Client,
    drop: bool,
    ns_include: Option<&str>,
) -> Result<u64> {
    // 헤더 확인(포맷 검증).
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

    let mut inserted = 0u64;
    // 현재 복원 중인 (db, coll) + 배치. skip=true면 ns 필터에 안 맞아 이 컬렉션을 건너뛴다.
    let mut current: Option<(String, String, bool)> = None;
    let mut batch: Vec<Document> = Vec::with_capacity(INSERT_BATCH);

    loop {
        match archive::read_frame(reader).await? {
            Frame::Header(_) => {
                return Err(XBackupError::Failure("아카이브에 헤더가 중복됩니다".into()))
            }
            Frame::Collection(meta) => {
                // 이전 컬렉션의 잔여 배치 flush.
                inserted += flush_batch(client, &current, &mut batch).await?;

                let ns = meta
                    .get_str("ns")
                    .map_err(|_| XBackupError::Failure("컬렉션 프레임에 ns가 없습니다".into()))?;
                let (db_name, coll_name) = ns
                    .split_once('.')
                    .ok_or_else(|| XBackupError::Failure(format!("ns 형식 오류: '{ns}'")))?;

                let skip = ns_include.is_some_and(|want| want != ns);
                if skip {
                    current = Some((db_name.to_string(), coll_name.to_string(), true));
                    continue;
                }

                let options = meta.get_document("options").cloned().unwrap_or_default();
                let indexes = meta
                    .get_array("indexes")
                    .map(|a| {
                        a.iter()
                            .filter_map(|b| b.as_document().cloned())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();

                create_collection(client, db_name, coll_name, &options, &indexes, drop).await?;
                current = Some((db_name.to_string(), coll_name.to_string(), false));
            }
            Frame::Document(d) => {
                // 건너뛰는 컬렉션이면 무시.
                if matches!(&current, Some((_, _, true))) {
                    continue;
                }
                batch.push(d);
                if batch.len() >= INSERT_BATCH {
                    inserted += flush_batch(client, &current, &mut batch).await?;
                }
            }
            Frame::End => {
                inserted += flush_batch(client, &current, &mut batch).await?;
                break;
            }
        }
    }
    Ok(inserted)
}

/// 현재 컬렉션에 배치를 insert_many한다(비었으면 0). 배치를 비운다.
async fn flush_batch(
    client: &Client,
    current: &Option<(String, String, bool)>,
    batch: &mut Vec<Document>,
) -> Result<u64> {
    if batch.is_empty() {
        return Ok(0);
    }
    let (db, coll, skip) = match current {
        Some(c) => c,
        None => {
            batch.clear();
            return Ok(0);
        }
    };
    if *skip {
        batch.clear();
        return Ok(0);
    }
    let docs = std::mem::take(batch);
    let n = docs.len() as u64;
    client
        .database(db)
        .collection::<Document>(coll)
        .insert_many(docs)
        .await
        .map_err(|e| XBackupError::Failure(format!("{db}.{coll} insert_many 실패: {e}")))?;
    Ok(n)
}

/// 컬렉션을 옵션과 함께 생성하고 인덱스를 복원한다(`--drop`이면 먼저 drop).
async fn create_collection(
    client: &Client,
    db_name: &str,
    coll: &str,
    options: &Document,
    indexes: &[Document],
    drop: bool,
) -> Result<()> {
    let db = client.database(db_name);

    if drop {
        // 존재하지 않아도 무해(드라이버가 조용히 처리).
        let _ = db.collection::<Document>(coll).drop().await;
    }

    // create 명령 — 옵션을 그대로 병합한다(capped/size/validator/collation 등).
    let mut create_cmd = doc! { "create": coll };
    for (k, v) in options.iter() {
        create_cmd.insert(k.clone(), v.clone());
    }
    match db.run_command(create_cmd).await {
        Ok(_) => {}
        Err(e) => {
            // --drop 아닌데 이미 존재(NamespaceExists, code 48)면 데이터만 채우도록 허용.
            let already = e
                .get_custom::<bson::Document>()
                .and_then(|d| d.get_i32("code").ok())
                == Some(48);
            if !already {
                return Err(XBackupError::Failure(format!(
                    "{db_name}.{coll} 컬렉션 생성 실패: {e}"
                )));
            }
        }
    }

    if !indexes.is_empty() {
        let cmd = doc! {
            "createIndexes": coll,
            "indexes": indexes.iter().cloned().map(bson::Bson::Document).collect::<Vec<_>>(),
        };
        db.run_command(cmd).await.map_err(|e| {
            XBackupError::Failure(format!("{db_name}.{coll} 인덱스 복원 실패: {e}"))
        })?;
    }
    Ok(())
}
