//! 로컬 파일시스템 스토리지 백엔드(PRD FR-4).
//!
//! object_store `LocalFileSystem`을 [`Storage`] trait로 래핑한다. 동일 trait를
//! S3 백엔드(t7)도 구현하므로, 파이프라인은 백엔드를 런타임에 교체할 수 있다.
//!
//! ## 스트림 경계
//! - `put_stream`: [`BoxAsyncRead`] → [`ReaderStream`]으로 청크화 → object_store
//!   [`WriteMultipart`]에 공급. `WriteMultipart`가 내부 버퍼링·파트 분할을
//!   담당하므로 디스크 전체를 메모리에 적재하지 않는다(PRD §7 스트리밍).
//! - `get_stream`: object_store `GetResult::into_stream`(바이트 스트림) →
//!   [`StreamReader`]로 [`BoxAsyncRead`] 복원.
//!
//! ## 부분 산출물 정리(PRD §11 신뢰성)
//! `put_stream`은 정상 완료 시에만 [`WriteMultipart::finish`]로 커밋한다. 도중
//! 에러(reader I/O 실패 등)가 나면 같은 함수 안에서 [`WriteMultipart::abort`]를
//! 호출해 미완료 산출물을 제거한다. 함수 경로를 벗어나는 패닉/조기 반환에
//! 대비해 [`UploadGuard`] RAII 가드를 추가로 둔다.

use std::path::Path as FsPath;
use std::pin::Pin;

use futures::StreamExt;
use object_store::local::LocalFileSystem;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, ObjectStoreExt, WriteMultipart};
use tokio::io::AsyncRead;
use tokio_util::io::{ReaderStream, StreamReader};

use super::{BoxAsyncRead, Storage, StorageEntry};
use crate::error::XBackupError;

/// `put_stream`이 reader에서 한 번에 읽어 업로드 버퍼로 넘기는 청크 크기.
///
/// 멀티파트 파트 크기가 아니라 reader 폴링 단위다. `WriteMultipart`가 이 청크를
/// 모아 내부 파트로 다시 분할하므로, 백프레셔/메모리 사용의 균형점으로 8MiB를
/// 택한다(리서치 pitfalls §5-1의 권장 파트 크기대와 정합).
const READ_CHUNK_BYTES: usize = 8 * 1024 * 1024;

/// 로컬 파일시스템 스토리지 백엔드.
///
/// 생성 시 지정한 루트 디렉터리를 prefix로 삼아, 모든 trait 경로를 그 아래에
/// 매핑한다. object_store `LocalFileSystem`은 `put` 시 부모 디렉터리를 자동
/// 생성하므로 `<backup-id>/` 하위 경로를 그대로 쓸 수 있다.
#[derive(Debug)]
pub struct LocalFs {
    inner: LocalFileSystem,
}

impl LocalFs {
    /// `root` 디렉터리를 백엔드 루트로 하는 LocalFs를 만든다.
    ///
    /// `root`는 미리 존재해야 한다(object_store `new_with_prefix` 제약). 호출자가
    /// 대상 디렉터리를 사전 보장한다(config destination.path).
    pub fn new(root: impl AsRef<FsPath>) -> Result<Self, XBackupError> {
        let inner = LocalFileSystem::new_with_prefix(root.as_ref()).map_err(|e| {
            XBackupError::StorageUpload(format!(
                "로컬 스토리지 루트 초기화 실패({}): {e}",
                root.as_ref().display()
            ))
        })?;
        Ok(Self { inner })
    }

    /// trait의 `&str` 경로를 object_store `Path`로 변환한다.
    fn object_path(path: &str) -> Result<ObjectPath, XBackupError> {
        ObjectPath::parse(path)
            .map_err(|e| XBackupError::StorageUpload(format!("잘못된 스토리지 경로 '{path}': {e}")))
    }
}

#[async_trait::async_trait]
impl Storage for LocalFs {
    async fn put_stream(
        &self,
        path: &str,
        reader: BoxAsyncRead,
        _content_length_hint: Option<u64>,
    ) -> Result<(), XBackupError> {
        let object_path = Self::object_path(path)?;

        // 커밋 전 Drop되면(패닉·조기 반환) best-effort로 삭제를 시도하는 가드.
        let mut guard = UploadGuard::new(&self.inner, object_path.clone());

        let upload =
            self.inner.put_multipart(&object_path).await.map_err(|e| {
                XBackupError::StorageUpload(format!("'{path}' 업로드 시작 실패: {e}"))
            })?;
        let mut writer = WriteMultipart::new(upload);

        // reader를 청크 스트림으로 변환해 업로드 버퍼에 순차 공급한다.
        let mut chunks = ReaderStream::with_capacity(reader, READ_CHUNK_BYTES);
        while let Some(chunk) = chunks.next().await {
            match chunk {
                Ok(bytes) => writer.put(bytes),
                Err(read_err) => {
                    // reader 측 I/O 실패: 미완료 멀티파트를 abort해 부분 산출물 제거.
                    let _ = writer.abort().await;
                    // abort가 처리했으므로 가드의 추가 삭제는 불필요.
                    guard.disarm();
                    return Err(XBackupError::StorageUpload(format!(
                        "'{path}' 입력 스트림 읽기 실패: {read_err}"
                    )));
                }
            }
        }

        // 마지막 파트 flush + 완료. 실패 시 WriteMultipart::finish 내부에서 abort한다.
        writer
            .finish()
            .await
            .map_err(|e| XBackupError::StorageUpload(format!("'{path}' 업로드 완료 실패: {e}")))?;

        // 정상 커밋 — 가드 해제(삭제 시도하지 않음).
        guard.disarm();
        Ok(())
    }

    async fn get_stream(&self, path: &str) -> Result<BoxAsyncRead, XBackupError> {
        let object_path = Self::object_path(path)?;
        let result =
            self.inner.get(&object_path).await.map_err(|e| {
                XBackupError::StorageDownload(format!("'{path}' 다운로드 실패: {e}"))
            })?;

        // object_store 바이트 스트림(에러 타입)을 io::Error로 매핑해 StreamReader에 연결.
        let byte_stream = result
            .into_stream()
            .map(|r| r.map_err(std::io::Error::other));
        let reader = StreamReader::new(byte_stream);
        Ok(Box::pin(reader) as Pin<Box<dyn AsyncRead + Send>>)
    }

    async fn list(&self, prefix: &str) -> Result<Vec<StorageEntry>, XBackupError> {
        // 빈 prefix는 루트 전체(None)로 매핑한다.
        let object_prefix = if prefix.is_empty() {
            None
        } else {
            Some(Self::object_path(prefix)?)
        };

        let mut stream = self.inner.list(object_prefix.as_ref());
        let mut entries = Vec::new();
        while let Some(meta) = stream.next().await {
            let meta = meta.map_err(|e| {
                XBackupError::StorageDownload(format!("'{prefix}' 목록 조회 실패: {e}"))
            })?;
            entries.push(StorageEntry {
                path: meta.location.to_string(),
                size: meta.size,
                last_modified: Some(meta.last_modified.to_rfc3339()),
            });
        }
        Ok(entries)
    }

    async fn delete(&self, path: &str) -> Result<(), XBackupError> {
        let object_path = Self::object_path(path)?;
        self.inner
            .delete(&object_path)
            .await
            .map_err(|e| XBackupError::StorageDownload(format!("'{path}' 삭제 실패: {e}")))
    }
}

/// 업로드 부분 산출물 정리용 RAII 가드(리서치 architecture §4).
///
/// `put_stream`이 정상 커밋하면 [`disarm`](Self::disarm)으로 무장 해제된다.
/// 커밋 전에 Drop되면(패닉, `?` 조기 반환 등) `Drop`에서 best-effort 삭제를
/// 시도해 부분 파일이 남지 않게 한다.
///
/// ## 비동기 Drop 제약과 한계 (주석 명시 요구사항)
/// Rust에는 async `Drop`이 없고, `Drop`은 동기 컨텍스트다. tokio 런타임 핸들
/// 위에서 비동기 `delete`를 직접 await할 수 없으므로, **현재 런타임이 살아 있고
/// 멀티스레드일 때만** [`tokio::runtime::Handle`]로 정리 태스크를 spawn하는
/// best-effort 전략을 쓴다. 다음 한계가 있다:
/// - 런타임 핸들을 얻지 못하면(런타임 종료 직후 등) 삭제를 건너뛴다.
/// - spawn된 정리 태스크는 프로세스가 곧장 종료되면 완료를 보장하지 못한다.
/// - 따라서 가드는 **정상 경로의 abort/finish를 대체하지 않는 2차 안전망**이다.
///   reader I/O 실패 같은 예상 경로는 `put_stream`이 직접 `abort`로 처리하고
///   가드를 disarm한다. 가드는 패닉 등 비정상 경로의 잔재 최소화 목적이다.
///   (영구적 잔재 제거는 다음 실행의 orphan 스캔 + manifest "incomplete"
///   표기가 최종적으로 담보한다 — 리서치 pitfalls §7-2.)
struct UploadGuard {
    /// 정리 대상 object 경로. `disarm` 후에는 `None`.
    target: Option<ObjectPath>,
    /// 삭제를 수행할 백엔드 클론(`LocalFileSystem`은 저렴하게 클론 가능).
    store: LocalFileSystem,
}

impl UploadGuard {
    fn new(store: &LocalFileSystem, target: ObjectPath) -> Self {
        Self {
            target: Some(target),
            store: store.clone(),
        }
    }

    /// 정상 커밋·명시적 abort 후 호출 — Drop 시 삭제를 시도하지 않게 한다.
    fn disarm(&mut self) {
        self.target = None;
    }
}

impl Drop for UploadGuard {
    fn drop(&mut self) {
        let Some(target) = self.target.take() else {
            return; // 무장 해제됨 — 정상 경로.
        };

        // 동기 Drop에서 async delete를 직접 await할 수 없다. 현재 tokio 런타임
        // 핸들이 있으면 best-effort로 정리 태스크를 spawn한다(위 한계 주석 참조).
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let store = self.store.clone();
            handle.spawn(async move {
                // 실패(이미 없음 등)는 무시 — best-effort.
                let _ = store.delete(&target).await;
            });
        }
        // 런타임 핸들이 없으면 동기 삭제 경로가 없으므로 건너뛴다. 잔재는
        // 상위 계층(orphan 스캔/incomplete 표기)이 최종 처리한다.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncRead, AsyncReadExt};

    /// reader 헬퍼: 바이트 슬라이스를 BoxAsyncRead로 감싼다.
    fn reader_from(data: &[u8]) -> BoxAsyncRead {
        Box::pin(std::io::Cursor::new(data.to_vec())) as BoxAsyncRead
    }

    /// 항상 I/O 에러를 내는 reader — put 실패 경로(부분 산출물 정리) 검증용.
    struct FailingReader;

    impl AsyncRead for FailingReader {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Err(std::io::Error::other("주입된 reader 실패")))
        }
    }

    /// put → get round-trip: 저장한 바이트와 읽은 바이트가 동일해야 한다.
    #[tokio::test]
    async fn put_get_round_trip_preserves_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();

        let payload = b"hello x-backup local storage";
        fs.put_stream(
            "bkp-1/data.bin",
            reader_from(payload),
            Some(payload.len() as u64),
        )
        .await
        .unwrap();

        let mut got = Vec::new();
        fs.get_stream("bkp-1/data.bin")
            .await
            .unwrap()
            .read_to_end(&mut got)
            .await
            .unwrap();

        assert_eq!(got, payload);
    }

    /// 수 MB 스트림 round-trip — 청크 경계를 넘는 멀티파트 경로 검증.
    #[tokio::test]
    async fn put_get_multi_megabyte_stream() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();

        // READ_CHUNK_BYTES(8MiB) 경계를 확실히 넘기는 ~10MiB.
        let size = 10 * 1024 * 1024 + 12_345;
        let payload: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();

        fs.put_stream("bkp-2/data.bin", reader_from(&payload), Some(size as u64))
            .await
            .unwrap();

        let mut got = Vec::new();
        fs.get_stream("bkp-2/data.bin")
            .await
            .unwrap()
            .read_to_end(&mut got)
            .await
            .unwrap();

        assert_eq!(got.len(), payload.len());
        assert_eq!(got, payload);
    }

    /// list는 prefix로 필터링하고, 항목의 path/size를 정확히 보고해야 한다.
    #[tokio::test]
    async fn list_filters_by_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();

        fs.put_stream("bkp-a/data.bin", reader_from(b"aaaa"), None)
            .await
            .unwrap();
        fs.put_stream("bkp-a/manifest.json", reader_from(b"{}"), None)
            .await
            .unwrap();
        fs.put_stream("bkp-b/data.bin", reader_from(b"bbbbbb"), None)
            .await
            .unwrap();

        let mut a_entries = fs.list("bkp-a").await.unwrap();
        a_entries.sort_by(|x, y| x.path.cmp(&y.path));
        assert_eq!(a_entries.len(), 2);
        assert_eq!(a_entries[0].path, "bkp-a/data.bin");
        assert_eq!(a_entries[0].size, 4);
        assert_eq!(a_entries[1].path, "bkp-a/manifest.json");
        assert!(a_entries[1].last_modified.is_some());

        // 전체 목록(빈 prefix)에는 3개 모두 보인다.
        let all = fs.list("").await.unwrap();
        assert_eq!(all.len(), 3);
    }

    /// delete 후 해당 경로의 get은 실패해야 한다.
    #[tokio::test]
    async fn delete_removes_object() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();

        fs.put_stream("bkp-3/data.bin", reader_from(b"data"), None)
            .await
            .unwrap();
        fs.delete("bkp-3/data.bin").await.unwrap();

        assert!(fs.get_stream("bkp-3/data.bin").await.is_err());
        assert!(fs.list("bkp-3").await.unwrap().is_empty());
    }

    /// put 도중 reader가 실패하면 에러를 반환하고 부분 산출물을 남기지 않아야 한다
    /// (멀티파트 abort 경로). 같은 경로의 후속 list가 비어 있음을 확인한다.
    #[tokio::test]
    async fn put_failure_leaves_no_partial_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();

        let result = fs
            .put_stream("bkp-4/data.bin", Box::pin(FailingReader), None)
            .await;
        assert!(result.is_err(), "실패하는 reader는 에러를 반환해야 한다");

        // 부분 파일이 남지 않았는지 확인 — get 실패 + list 비어 있음.
        assert!(fs.get_stream("bkp-4/data.bin").await.is_err());
        let entries = fs.list("bkp-4").await.unwrap();
        assert!(
            entries.is_empty(),
            "실패 시 부분 산출물이 남았다: {entries:?}"
        );
    }

    /// 잘못된 경로(빈 문자열 put 등)는 명확한 에러로 거부한다.
    #[tokio::test]
    async fn invalid_path_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        // object_store Path는 빈 세그먼트/`//` 등을 거부한다.
        let result = fs.put_stream("a//b", reader_from(b"x"), None).await;
        assert!(result.is_err());
    }

    /// UploadGuard가 커밋 없이 Drop되면 best-effort 정리가 동작하는지 확인한다.
    /// (런타임 멀티스레드에서 spawn된 삭제 태스크가 완료될 시간을 준다.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn upload_guard_cleans_up_on_drop_without_commit() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();

        // 먼저 정상 파일을 만들어 둔다.
        fs.put_stream("bkp-5/data.bin", reader_from(b"committed"), None)
            .await
            .unwrap();

        // 무장된 가드를 만들고 disarm 없이 Drop시킨다 → 정리 태스크 spawn.
        {
            let object_path = LocalFs::object_path("bkp-5/data.bin").unwrap();
            let _guard = UploadGuard::new(&fs.inner, object_path);
            // disarm 호출하지 않음 → Drop 시 삭제 시도.
        }

        // spawn된 best-effort 삭제가 완료될 때까지 폴링(고정 sleep 회피).
        let mut deleted = false;
        for _ in 0..50 {
            if fs.list("bkp-5").await.unwrap().is_empty() {
                deleted = true;
                break;
            }
            tokio::task::yield_now().await;
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(deleted, "가드 Drop이 부분 산출물을 정리하지 못했다");
    }
}
