//! mongodump/mongorestore용 0600 임시 URI config 파일(PRD §11, pitfall 6-3).
//!
//! 시크릿 URI를 argv로 넘기면 `ps`에 평문 노출되므로(보안), Database Tools가
//! 지원하는 `--config <file>`의 `uri:` 필드로 전달한다. 파일은:
//! - 권한 **0600**으로 생성(소유자만 read/write).
//! - [`tempfile::NamedTempFile`]이라 핸들 Drop 시 자동 삭제된다 — 정상·에러·패닉
//!   경로 모두에서 잔재가 남지 않는다(종료 후 삭제 요구사항).
//!
//! `--config` 파일 포맷은 YAML이며, Database Tools 100.x는 `uri:` 키를 인식한다.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;

use tempfile::NamedTempFile;

use crate::config::secret::Secret;
use crate::error::{Result, XBackupError};

/// 자동 삭제되는 0600 URI config 파일 핸들.
///
/// [`path`](Self::path)를 dump/restore 스폰의 `--config` 인자로 넘긴다. 이 핸들이
/// 살아 있는 동안만 파일이 존재하므로, 자식 프로세스가 종료된 *뒤* Drop해야 한다.
pub struct UriConfigFile {
    file: NamedTempFile,
}

impl UriConfigFile {
    /// URI 시크릿을 담은 0600 임시 YAML config를 만든다.
    pub fn create(uri: &Secret) -> Result<Self> {
        let mut file = NamedTempFile::with_prefix("x-backup-uri-")
            .map_err(|e| XBackupError::Failure(format!("임시 config 생성 실패: {e}")))?;

        // 생성 직후 권한을 0600으로 좁힌다(기본 tempfile도 0600이나 명시적으로 보장).
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(file.path(), perms)
            .map_err(|e| XBackupError::Failure(format!("임시 config 권한 설정 실패: {e}")))?;

        // Database Tools --config YAML: `uri: <connection-string>`.
        // 값에 특수문자가 있을 수 있으므로 작은따옴표로 감싸고 내부 ' 를 '' 로 이스케이프.
        let escaped = uri.expose().replace('\'', "''");
        writeln!(file, "uri: '{escaped}'")
            .map_err(|e| XBackupError::Failure(format!("임시 config 쓰기 실패: {e}")))?;
        file.flush()
            .map_err(|e| XBackupError::Failure(format!("임시 config flush 실패: {e}")))?;

        Ok(Self { file })
    }

    /// `--config` 인자로 넘길 파일 경로.
    pub fn path(&self) -> &str {
        // NamedTempFile 경로는 생성 시 유효한 UTF-8(tempfile 기본 prefix).
        self.file.path().to_str().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_0600_file_with_uri() {
        let secret = Secret::new("mongodb://user:p%40ss@host:27017/?replicaSet=rs0");
        let cfg = UriConfigFile::create(&secret).unwrap();

        // 권한이 0600인지.
        let mode = std::fs::metadata(cfg.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "권한이 0600이 아님: {:o}", mode & 0o777);

        // 내용에 uri 필드가 있고 평문 URI가 들어있는지(자식에 전달할 목적).
        let content = std::fs::read_to_string(cfg.path()).unwrap();
        assert!(content.starts_with("uri: '"));
        assert!(content.contains("mongodb://user:p%40ss@host:27017"));
    }

    #[test]
    fn file_is_deleted_on_drop() {
        let secret = Secret::new("mongodb://host/db");
        let path = {
            let cfg = UriConfigFile::create(&secret).unwrap();
            cfg.path().to_string()
        };
        // Drop 후에는 파일이 사라져야 한다(종료 후 삭제).
        assert!(!std::path::Path::new(&path).exists(), "Drop 후에도 파일 잔존: {path}");
    }

    #[test]
    fn single_quotes_are_escaped() {
        // URI에 ' 가 들어가도 YAML이 깨지지 않아야 한다.
        let secret = Secret::new("mongodb://host/db?x=a'b");
        let cfg = UriConfigFile::create(&secret).unwrap();
        let content = std::fs::read_to_string(cfg.path()).unwrap();
        assert!(content.contains("a''b"), "작은따옴표 이스케이프 누락: {content}");
    }
}
