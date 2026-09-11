//! 네이티브 풀 백업 — 드라이버로 컬렉션을 읽어 [`archive`](super::archive) 스트림으로 낸다.
//!
//! [`OplogCaptureStream`](super::super::mongo::oplog) 과 동일한 패턴: 별도 task가 드라이버
//! 커서를 구동해 [`DuplexStream`]에 아카이브 프레임을 쓰고, 호출자는 [`AsyncRead`]로 받아
//! 그대로 파이프라인(압축→암호화→저장)에 흘린다. 전 구간 스트리밍(상수 메모리).

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use bson::{doc, Document, RawDocumentBuf};
use chrono::Utc;
use futures::TryStreamExt;
use mongodb::Client;
use tokio::io::{AsyncRead, AsyncWriteExt, DuplexStream, ReadBuf};
use tokio::task::JoinHandle;

use super::archive;
use crate::config::secret::Secret;
use crate::error::{Result, XBackupError};

/// 시스템 DB(백업 대상에서 제외).
const SYSTEM_DBS: [&str; 3] = ["admin", "config", "local"];

/// cursor→스트림 경계 버퍼(바이트). 백프레셔용.
const PIPE_BUFFER_BYTES: usize = 256 * 1024;

/// 드라이버로 네이티브 풀 백업을 수행하는 덤퍼.
pub struct NativeDumper {
    client: Client,
}

impl NativeDumper {
    /// URI 시크릿으로 연결한다([`MongoMeta::connect`](super::super::mongo::meta::MongoMeta::connect)와 동일 정책).
    pub async fn connect(uri: &Secret, timeout_secs: Option<u64>) -> Result<Self> {
        let options = super::super::mongo::conn::client_options(uri, timeout_secs).await?;
        let client = Client::with_options(options).map_err(|e| {
            XBackupError::Failure(crate::tr!(
                "failed to create the MongoDB client: {e}",
                "MongoDB 클라이언트 생성 실패: {e}"
            ))
        })?;
        Ok(Self { client })
    }

    /// 백업 대상 네임스페이스를 아카이브 스트림으로 낸다(`db`/`db.coll` 필터 가능).
    ///
    /// 별도 task가 드라이버 커서를 구동해 프레임을 [`DuplexStream`]에 쓴다 — 반환된
    /// [`NativeDumpStream`]을 파이프라인에 흘리고, EOF 후 [`finish`](NativeDumpStream::handle)로
    /// 결과(에러)를 회수한다.
    pub fn dump_stream(&self, db: Option<String>, collection: Option<String>) -> NativeDumpStream {
        let (mut writer, reader) = tokio::io::duplex(PIPE_BUFFER_BYTES);
        let outcome: Arc<Mutex<Option<Result<()>>>> = Arc::new(Mutex::new(None));
        let client = self.client.clone();
        let outcome_task = Arc::clone(&outcome);

        let handle = tokio::spawn(async move {
            let res = write_archive(&client, &mut writer, db, collection).await;
            // 쓰기 측을 닫아 EOF를 보낸다(에러 여부와 무관히).
            let _ = writer.shutdown().await;
            *outcome_task.lock().expect("native dump outcome poisoned") = Some(res);
        });

        NativeDumpStream {
            reader,
            outcome,
            task: Arc::new(Mutex::new(Some(handle))),
        }
    }
}

/// 대상 네임스페이스 전체를 아카이브 프레임으로 직렬화해 writer에 쓴다.
async fn write_archive(
    client: &Client,
    writer: &mut DuplexStream,
    db_filter: Option<String>,
    coll_filter: Option<String>,
) -> Result<()> {
    archive::write_header(writer, &Utc::now().to_rfc3339()).await?;

    let db_names = client.list_database_names().await.map_err(|e| {
        XBackupError::Failure(crate::tr!(
            "failed to list databases: {e}",
            "데이터베이스 목록 조회 실패: {e}"
        ))
    })?;

    for db_name in db_names {
        if SYSTEM_DBS.contains(&db_name.as_str()) {
            continue;
        }
        if let Some(want) = &db_filter {
            if &db_name != want {
                continue;
            }
        }
        let db = client.database(&db_name);
        // 컬렉션 명세(옵션 포함) — view/timeseries는 1차 미커버(건너뜀, 경고).
        let mut specs = db.list_collections().await.map_err(|e| {
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
            let coll_name = spec.name;
            if coll_name.starts_with("system.") {
                continue;
            }
            if !matches!(
                spec.collection_type,
                mongodb::results::CollectionType::Collection
            ) {
                tracing::warn!(
                    ns = %format!("{db_name}.{coll_name}"),
                    kind = ?spec.collection_type,
                    "네이티브 백업: 일반 컬렉션이 아님 — 건너뜀(1차 미커버)"
                );
                continue;
            }
            if let Some(want) = &coll_filter {
                if &coll_name != want {
                    continue;
                }
            }

            let ns = format!("{db_name}.{coll_name}");
            let options = bson::to_document(&spec.options).unwrap_or_default();
            let indexes = collect_indexes(&db, &coll_name).await?;
            archive::write_collection(writer, &ns, &options, &indexes).await?;

            // 문서를 raw로 스트리밍(재직렬화 없이 바이트 그대로).
            let raw_coll = db.collection::<RawDocumentBuf>(&coll_name);
            let mut cursor = raw_coll.find(doc! {}).await.map_err(|e| {
                XBackupError::Failure(crate::tr!(
                    "{ns}: failed to query documents: {e}",
                    "{ns} 문서 조회 실패: {e}"
                ))
            })?;
            while cursor.advance().await.map_err(|e| {
                XBackupError::Failure(crate::tr!(
                    "{ns}: cursor advance failed: {e}",
                    "{ns} 커서 진행 실패: {e}"
                ))
            })? {
                archive::write_raw_document(writer, cursor.current()).await?;
            }
            tracing::debug!(ns = %ns, indexes = indexes.len(), "네이티브 백업: 컬렉션 직렬화 완료");
        }
    }

    archive::write_end(writer).await
}

/// 컬렉션의 인덱스 명세를 createIndexes 호환 문서로 모은다(`_id_` 기본 인덱스 제외).
async fn collect_indexes(db: &mongodb::Database, coll: &str) -> Result<Vec<Document>> {
    let mut cursor = db
        .collection::<Document>(coll)
        .list_indexes()
        .await
        .map_err(|e| {
            XBackupError::Failure(crate::tr!(
                "{coll}: failed to query indexes: {e}",
                "{coll} 인덱스 조회 실패: {e}"
            ))
        })?;

    let mut out = Vec::new();
    while let Some(model) = cursor.try_next().await.map_err(|e| {
        XBackupError::Failure(crate::tr!(
            "{coll}: index iteration failed: {e}",
            "{coll} 인덱스 순회 실패: {e}"
        ))
    })? {
        // 옵션(name/unique/sparse/...)을 평탄화하고 key를 더해 createIndexes 스펙을 만든다.
        let mut spec = model
            .options
            .as_ref()
            .and_then(|o| bson::to_document(o).ok())
            .unwrap_or_default();
        // _id 기본 인덱스는 컬렉션 생성 시 자동 — 재생성 대상에서 제외.
        if spec.get_str("name").ok() == Some("_id_") {
            continue;
        }
        spec.insert("key", model.keys.clone());
        out.push(spec);
    }
    Ok(out)
}

/// 네이티브 백업 아카이브 바이트 스트림([`AsyncRead`]). 파이프라인에 그대로 흘린다.
pub struct NativeDumpStream {
    reader: DuplexStream,
    outcome: Arc<Mutex<Option<Result<()>>>>,
    task: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl NativeDumpStream {
    /// 스트림 소비(EOF) 후 백업 결과(에러)를 회수할 핸들을 떠 둔다(sha256 tee 패턴).
    pub fn handle(&self) -> NativeDumpHandle {
        NativeDumpHandle {
            outcome: Arc::clone(&self.outcome),
            task: Arc::clone(&self.task),
        }
    }
}

/// [`NativeDumpStream`]의 결과를 회수하는 핸들.
pub struct NativeDumpHandle {
    outcome: Arc<Mutex<Option<Result<()>>>>,
    task: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl NativeDumpHandle {
    /// 스트림 EOF 후 백업 task의 결과를 회수한다(드라이버/직렬화 오류 전파).
    pub async fn finish(self) -> Result<()> {
        let task = self.task.lock().expect("native dump task poisoned").take();
        if let Some(task) = task {
            let _ = task.await;
        }
        self.outcome
            .lock()
            .expect("native dump outcome poisoned")
            .take()
            .unwrap_or(Ok(()))
    }
}

impl AsyncRead for NativeDumpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.reader).poll_read(cx, buf)
    }
}
