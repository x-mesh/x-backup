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
pub mod s3;

pub use local::LocalFs;
pub use s3::S3Compatible;

use std::pin::Pin;

use tokio::io::AsyncRead;

use crate::config::file::DestinationConfig;
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

/// `DestinationConfig`로부터 적절한 [`Storage`] 백엔드를 생성하는 팩토리.
///
/// `destination.type`(`local` | `s3`)에 따라 [`LocalFs`] 또는 [`S3Compatible`]을
/// `Box<dyn Storage>`로 반환한다 — 호출자(파이프라인 핸들러)는 백엔드 종류를
/// 몰라도 동일 trait로 다룬다.
///
/// ## 이 함수의 책임 경계
/// - 백엔드 **생성**만 담당한다. CLI 핸들러 배선(어떤 명령이 이 팩토리를 호출할지)은
///   후속 태스크의 몫이다.
/// - S3 자격증명은 **env에서 직접 조회**한다 — config의
///   `destination.s3.credentials_env`가 가리키는 환경변수 이름을 읽어 그 값을
///   `S3Compatible::new`에 넘긴다. 값 형식은 `"ACCESS_KEY:SECRET_KEY"`다.
///   (시크릿 평문은 이 경로에서만 잠깐 다루며 로그/Debug에 남기지 않는다.)
///
/// ## 에러
/// - `type` 누락/미지원, 필수 키 누락, credentials env 미설정 등은
///   [`XBackupError::Config`](exit 2)로 반환한다.
pub fn from_config(dest: &DestinationConfig) -> Result<Box<dyn Storage>, XBackupError> {
    let backend = dest.r#type.as_deref().ok_or_else(|| {
        XBackupError::Config("destination.type이 필요합니다(local | s3)".to_string())
    })?;

    match backend {
        "local" => {
            let path = dest.path.as_deref().ok_or_else(|| {
                XBackupError::Config("local destination에 path가 필요합니다".to_string())
            })?;
            Ok(Box::new(LocalFs::new(path)?))
        }
        "s3" => {
            let s3_cfg = dest.s3.as_ref().ok_or_else(|| {
                XBackupError::Config(
                    "s3 destination에 [destination.s3] 블록이 필요합니다".to_string(),
                )
            })?;
            let creds_env = s3_cfg.credentials_env.as_deref().ok_or_else(|| {
                XBackupError::Config(
                    "s3 destination에 credentials_env가 필요합니다".to_string(),
                )
            })?;
            // credentials_env가 가리키는 환경변수에서 "ACCESS:SECRET" 값을 읽는다.
            let creds_raw = std::env::var(creds_env).map_err(|_| {
                XBackupError::Config(format!(
                    "S3 자격증명 환경변수 '{creds_env}'가 설정되지 않았습니다"
                ))
            })?;
            Ok(Box::new(S3Compatible::new(s3_cfg, &creds_raw)?))
        }
        other => Err(XBackupError::Config(format!(
            "지원하지 않는 destination.type: '{other}'(local | s3만 지원)"
        ))),
    }
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

    /// from_config: type="local" + 존재하는 path → LocalFs를 만든다.
    #[tokio::test]
    async fn from_config_builds_local_backend() {
        let dir = tempfile::tempdir().unwrap();
        let dest = DestinationConfig {
            r#type: Some("local".to_string()),
            path: Some(dir.path().to_string_lossy().to_string()),
            s3: None,
        };
        let storage = from_config(&dest).expect("local 백엔드 생성 성공");
        // 동일 trait로 put/get round-trip이 되는지 확인(LocalFs↔dyn Storage 교체 가능).
        storage
            .put_stream(
                "bkp/data.bin",
                Box::pin(std::io::Cursor::new(b"hello".to_vec())),
                None,
            )
            .await
            .unwrap();
        let entries = storage.list("bkp").await.unwrap();
        assert_eq!(entries.len(), 1);
    }

    /// from_config: type="s3" + 완비된 설정 + credentials env → S3Compatible을 만든다.
    /// (네트워크 호출 없음 — 빌더 구성까지만 검증.)
    #[test]
    fn from_config_builds_s3_backend() {
        use crate::config::file::S3Config;

        // 테스트 격리를 위해 고유 env 이름 사용.
        let env_name = "XBACKUP_TEST_S3_CREDS_FROM_CONFIG";
        // SAFETY: 단일 스레드 테스트에서만 설정/해제하며 다른 테스트와 이름이 겹치지 않는다.
        unsafe {
            std::env::set_var(env_name, "AKIATEST:secretvalue");
        }

        let dest = DestinationConfig {
            r#type: Some("s3".to_string()),
            path: None,
            s3: Some(S3Config {
                endpoint: Some("http://localhost:9000".to_string()),
                bucket: Some("test-bucket".to_string()),
                prefix: Some("mongo/test".to_string()),
                region: Some("us-east-1".to_string()),
                credentials_env: Some(env_name.to_string()),
            }),
        };
        let result = from_config(&dest);
        unsafe {
            std::env::remove_var(env_name);
        }
        assert!(result.is_ok(), "s3 백엔드 생성 실패: {:?}", result.err());
    }

    /// `from_config`의 에러 종료 코드를 확인하는 헬퍼.
    ///
    /// 성공 타입(`Box<dyn Storage>`)이 `Debug`를 구현하지 않아 `unwrap_err`를 쓸 수
    /// 없으므로, 결과를 직접 매칭해 에러 exit code만 단언한다.
    fn assert_config_error(dest: &DestinationConfig) {
        match from_config(dest) {
            Err(e) => assert_eq!(e.exit_code(), 2),
            Ok(_) => panic!("Config 에러를 기대했으나 백엔드 생성에 성공했다"),
        }
    }

    /// from_config: type 누락은 Config 에러(exit 2).
    #[test]
    fn from_config_rejects_missing_type() {
        assert_config_error(&DestinationConfig {
            r#type: None,
            path: None,
            s3: None,
        });
    }

    /// from_config: 미지원 type은 Config 에러(exit 2).
    #[test]
    fn from_config_rejects_unknown_type() {
        assert_config_error(&DestinationConfig {
            r#type: Some("gcs".to_string()),
            path: None,
            s3: None,
        });
    }

    /// from_config: local인데 path 누락 → Config 에러(exit 2).
    #[test]
    fn from_config_local_requires_path() {
        assert_config_error(&DestinationConfig {
            r#type: Some("local".to_string()),
            path: None,
            s3: None,
        });
    }

    /// from_config: s3인데 credentials env 미설정 → Config 에러(exit 2).
    #[test]
    fn from_config_s3_requires_credentials_env_set() {
        use crate::config::file::S3Config;
        assert_config_error(&DestinationConfig {
            r#type: Some("s3".to_string()),
            path: None,
            s3: Some(S3Config {
                endpoint: Some("http://localhost:9000".to_string()),
                bucket: Some("b".to_string()),
                prefix: None,
                region: None,
                credentials_env: Some("XBACKUP_DEFINITELY_UNSET_ENV_VAR_12345".to_string()),
            }),
        });
    }
}
