//! 네이티브 풀 백업 — 드라이버로 컬렉션을 읽어 [`archive`](super::archive) 스트림으로 낸다.
//!
//! [`OplogCaptureStream`](super::super::mongo::oplog) 과 동일한 패턴: 별도 task가 드라이버
//! 커서를 구동해 [`DuplexStream`]에 아카이브 프레임을 쓰고, 호출자는 [`AsyncRead`]로 받아
//! 그대로 파이프라인(압축→암호화→저장)에 흘린다. 전 구간 스트리밍(상수 메모리).

use std::collections::BTreeMap;
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
use crate::engine::digest::RowDigest;
use crate::engine::progress::RowProgress;
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
    pub fn dump_stream(
        &self,
        db: Option<String>,
        collection: Option<String>,
        progress: Option<Arc<RowProgress>>,
        content_digest: bool,
    ) -> NativeDumpStream {
        let (mut writer, reader) = tokio::io::duplex(PIPE_BUFFER_BYTES);
        let outcome: Arc<Mutex<Option<Result<()>>>> = Arc::new(Mutex::new(None));
        let rows: Arc<Mutex<BTreeMap<String, u64>>> = Arc::new(Mutex::new(BTreeMap::new()));
        let digests: Arc<Mutex<BTreeMap<String, String>>> = Arc::new(Mutex::new(BTreeMap::new()));
        let client = self.client.clone();
        let outcome_task = Arc::clone(&outcome);
        let rows_task = Arc::clone(&rows);
        let digests_task = content_digest.then(|| Arc::clone(&digests));

        let handle = tokio::spawn(async move {
            let res = write_archive(
                &client,
                &mut writer,
                db,
                collection,
                &rows_task,
                digests_task.as_ref(),
                progress.as_deref(),
            )
            .await;
            // 쓰기 측을 닫아 EOF를 보낸다(에러 여부와 무관히).
            let _ = writer.shutdown().await;
            *outcome_task.lock().expect("native dump outcome poisoned") = Some(res);
        });

        NativeDumpStream {
            reader,
            outcome,
            rows,
            digests,
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
    rows_cell: &Arc<Mutex<BTreeMap<String, u64>>>,
    digests_cell: Option<&Arc<Mutex<BTreeMap<String, String>>>>,
    progress: Option<&RowProgress>,
) -> Result<()> {
    archive::write_header(writer, &Utc::now().to_rfc3339()).await?;

    // 백업할 컬렉션을 먼저 모두 나열한다 — 진행 표시가 전체 개수(`n/N`)를 알 수 있게 하는
    // 메타데이터 조회뿐이고, 문서는 읽지 않는다.
    let targets = list_targets(client, db_filter.as_deref(), coll_filter.as_deref()).await?;
    if let Some(p) = progress {
        p.set_total(targets.len());
    }

    for (db_name, spec) in targets {
        let db = client.database(&db_name);
        let coll_name = spec.name;
        let ns = format!("{db_name}.{coll_name}");
        if let Some(p) = progress {
            p.start_table(&ns);
        }
        let options = bson::to_document(&spec.options).unwrap_or_default();
        let indexes = collect_indexes(&db, &coll_name).await?;
        archive::write_collection(writer, &ns, &options, &indexes).await?;

        // 문서를 raw로 스트리밍(재직렬화 없이 바이트 그대로).
        let raw_coll = db.collection::<RawDocumentBuf>(&coll_name);
        let mut rows = 0u64;
        let mut digest = digests_cell.map(|_| RowDigest::default());
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
            if let Some(d) = digest.as_mut() {
                d.add(cursor.current().as_bytes());
            }
            rows += 1;
            if let Some(p) = progress {
                p.add_row();
            }
        }
        rows_cell
            .lock()
            .expect("native collection rows poisoned")
            .insert(ns.clone(), rows);
        if let (Some(cell), Some(d)) = (digests_cell, digest) {
            cell.lock()
                .expect("native collection digests poisoned")
                .insert(ns.clone(), d.finish());
        }
        if let Some(p) = progress {
            p.finish_table();
        }
        tracing::debug!(ns = %ns, indexes = indexes.len(), rows, "네이티브 백업: 컬렉션 직렬화 완료");
    }

    archive::write_end(writer).await
}

/// 백업 대상 컬렉션을 `(db, 명세)`로 나열한다. 시스템 DB·`system.` 컬렉션·뷰/타임시리즈는 건너뛰고
/// (뷰/타임시리즈는 1차 미커버, 경고), `db_filter`/`coll_filter`가 있으면 그 범위만 남긴다.
async fn list_targets(
    client: &Client,
    db_filter: Option<&str>,
    coll_filter: Option<&str>,
) -> Result<Vec<(String, mongodb::results::CollectionSpecification)>> {
    let db_names = client.list_database_names().await.map_err(|e| {
        XBackupError::Failure(crate::tr!(
            "failed to list databases: {e}",
            "데이터베이스 목록 조회 실패: {e}"
        ))
    })?;

    let mut targets = Vec::new();
    for db_name in db_names {
        if SYSTEM_DBS.contains(&db_name.as_str()) {
            continue;
        }
        if db_filter.is_some_and(|want| want != db_name) {
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
            if spec.name.starts_with("system.") {
                continue;
            }
            if !matches!(
                spec.collection_type,
                mongodb::results::CollectionType::Collection
            ) {
                tracing::warn!(
                    ns = %format!("{db_name}.{}", spec.name),
                    kind = ?spec.collection_type,
                    "네이티브 백업: 일반 컬렉션이 아님 — 건너뜀(1차 미커버)"
                );
                continue;
            }
            if coll_filter.is_some_and(|want| want != spec.name) {
                continue;
            }
            targets.push((db_name.clone(), spec));
        }
    }
    Ok(targets)
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
    rows: Arc<Mutex<BTreeMap<String, u64>>>,
    digests: Arc<Mutex<BTreeMap<String, String>>>,
    task: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl NativeDumpStream {
    /// 스트림 소비(EOF) 후 백업 결과(에러)를 회수할 핸들을 떠 둔다(sha256 tee 패턴).
    pub fn handle(&self) -> NativeDumpHandle {
        NativeDumpHandle {
            outcome: Arc::clone(&self.outcome),
            rows: Arc::clone(&self.rows),
            digests: Arc::clone(&self.digests),
            task: Arc::clone(&self.task),
        }
    }
}

/// [`NativeDumpStream`]의 결과를 회수하는 핸들.
pub struct NativeDumpHandle {
    outcome: Arc<Mutex<Option<Result<()>>>>,
    rows: Arc<Mutex<BTreeMap<String, u64>>>,
    digests: Arc<Mutex<BTreeMap<String, String>>>,
    task: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl NativeDumpHandle {
    /// 컬렉션별 내용 다이제스트 저장소. `finish`가 핸들을 소비하므로 미리 복제해 둔다.
    /// 다이제스트를 켜지 않은 덤프에서는 끝까지 비어 있다.
    pub fn digests(&self) -> Arc<Mutex<BTreeMap<String, String>>> {
        Arc::clone(&self.digests)
    }

    /// 스트림 EOF 후 백업 task의 결과를 회수하고, 아카이브에 쓴 컬렉션별 문서 수
    /// (`db.컬렉션` → 문서 수)를 반환한다(드라이버/직렬화 오류 전파).
    pub async fn finish(self) -> Result<BTreeMap<String, u64>> {
        let task = self.task.lock().expect("native dump task poisoned").take();
        if let Some(task) = task {
            let _ = task.await;
        }
        self.outcome
            .lock()
            .expect("native dump outcome poisoned")
            .take()
            .unwrap_or(Ok(()))?;
        Ok(std::mem::take(
            &mut *self.rows.lock().expect("native collection rows poisoned"),
        ))
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
