//! Storage 계층 — 추상 스토리지 백엔드(PRD §10, FR-4).
//!
//! [`Storage`] trait: `put_stream` / `get_stream` / `list` / `delete`.
//! 구현체: [`local::LocalFs`](LocalFs)(object_store `LocalFileSystem` 래핑).
//! S3 호환 백엔드는 후속 태스크(t7)가 동일 trait를 구현한다.
//!
//! ## 설계 결정 (리서치 architecture §2)
//! - AFIT(async fn in trait, Rust 1.75)는 `dyn` 객체화를 지원하지 않으므로
//!   `#[async_trait]` 매크로를 채택한다. 파이프라인은 `Box<dyn Storage>`로
//!   런타임 백엔드 교체(local/s3)가 필요하기 때문이다.
//! - 스트림 경계는 [`BoxAsyncRead`](`Pin<Box<dyn AsyncRead + Send>>`)로 통일한다.
//!   `put_stream`은 reader를 소비해 저장하고, `get_stream`은 저장 바이트를
//!   다시 reader로 반환한다 — `tokio::io::copy`의 자연 백프레셔를 활용한다.

pub mod local;

pub use local::LocalFs;

use std::pin::Pin;

use tokio::io::AsyncRead;

use crate::error::XBackupError;

/// 스토리지 스트림 경계 타입.
///
/// `put_stream`의 입력(읽어서 저장)과 `get_stream`의 출력(저장본을 읽기)에
/// 동일하게 쓰인다. `Send`를 요구해 멀티스레드 tokio 런타임의 태스크 간
/// 이동을 허용한다.
pub type BoxAsyncRead = Pin<Box<dyn AsyncRead + Send>>;

/// `list` 결과의 단일 객체 항목.
///
/// 백엔드 중립 메타데이터만 노출한다 — S3 ETag 등 백엔드 고유 필드는
/// 포함하지 않는다(자체 sha256이 무결성의 primary, ETag는 hint일 뿐).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageEntry {
    /// 백엔드 루트(prefix) 기준 객체 경로. 항상 `/` 구분자를 사용한다.
    pub path: String,
    /// 객체 크기(바이트).
    pub size: u64,
    /// 마지막 수정 시각(UTC, RFC3339). 백엔드가 제공하지 않으면 `None`.
    pub last_modified: Option<String>,
}

/// 추상 스토리지 백엔드(PRD §10 trait 경계).
///
/// 모든 경로(`path`/`prefix`)는 백엔드 루트 기준의 상대 경로이며 `/`로
/// 구분한다(예: `<backup-id>/data.bin`). 구현체가 절대 경로/버킷 prefix를
/// 내부에서 합성한다 — 호출자(t4 파이프라인)는 저장 레이아웃만 안다
/// (`<prefix>/<backup-id>/{data.bin, manifest.json, manifest.json.sha256}`).
///
/// t4(파이프라인)·t7(S3)이 이 시그니처를 그대로 구현·소비한다.
#[cfg_attr(test, mockall::automock)]
#[async_trait::async_trait]
pub trait Storage: Send + Sync {
    /// `reader`의 바이트를 `path`에 저장한다.
    ///
    /// `content_length_hint`는 멀티파트 파트 크기 산정 등 최적화 힌트로,
    /// 정확하지 않아도 정합성에 영향을 주지 않는다(스트리밍 백엔드는 무시 가능).
    ///
    /// 실패 시 부분 산출물을 남기지 않아야 한다(PRD §11). 구현체는 내부에서
    /// 미완료 업로드를 정리(로컬: 임시 파일 제거 / S3: multipart abort)한다.
    async fn put_stream(
        &self,
        path: &str,
        reader: BoxAsyncRead,
        content_length_hint: Option<u64>,
    ) -> Result<(), XBackupError>;

    /// `path`에 저장된 바이트를 읽기 스트림으로 반환한다(복구·verify 경로).
    async fn get_stream(&self, path: &str) -> Result<BoxAsyncRead, XBackupError>;

    /// `prefix`로 시작하는 객체 목록을 반환한다(list 카탈로그·prune 기반).
    ///
    /// 빈 문자열 prefix는 전체 객체를 의미한다. 결과 순서는 보장하지 않는다.
    async fn list(&self, prefix: &str) -> Result<Vec<StorageEntry>, XBackupError>;

    /// `path`의 객체를 삭제한다(prune·실패 정리 경로).
    async fn delete(&self, path: &str) -> Result<(), XBackupError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `mockall::automock`이 생성한 `MockStorage`가 `Storage`로 컴파일·동작하는지
    /// 확인한다. t4 파이프라인 단위 테스트가 이 mock에 의존한다(리서치 §8).
    #[tokio::test]
    async fn mock_storage_compiles_and_dispatches() {
        let mut mock = MockStorage::new();
        mock.expect_delete()
            .withf(|p| p == "id/data.bin")
            .times(1)
            .returning(|_| Ok(()));
        mock.expect_list().returning(|_| {
            Ok(vec![StorageEntry {
                path: "id/manifest.json".to_string(),
                size: 42,
                last_modified: None,
            }])
        });

        // dyn 객체화가 되는지(파이프라인은 Box<dyn Storage>를 받는다) 확인한다.
        let storage: Box<dyn Storage> = Box::new(mock);
        storage.delete("id/data.bin").await.unwrap();
        let entries = storage.list("id/").await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].size, 42);
    }
}
