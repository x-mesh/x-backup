//! 파일/디렉터리 백업 엔진(`file://`) — DB가 아닌 로컬 경로를 tar 스트림으로 백업한다
//! (로드맵 Phase 2 / P2-1).
//!
//! 별도 도구를 만드는 게 아니라 **엔진 하나를 추가**한다: tar 아카이브 스트림이 기존
//! 파이프라인(compress→encrypt→store, manifest/verify/prune/list)을 그대로 탄다.
//! 덤프는 [`backup`]이 tar 스트림(AsyncRead)을 만들고, 복구는 [`restore`]가 스트림을
//! 대상 디렉터리에 풀어놓는다. tar 크레이트는 동기 IO라 blocking task에서 돌리고
//! [`tokio_util::io::SyncIoBridge`]로 async 경계를 잇는다.
//!
//! ## 범위(P2-1)
//! 풀 백업/복구만. 증분(스냅샷 인덱스, P2-2)·xattr 보존은 로드맵 잔여. 권한·mtime·
//! 심링크는 tar가 보존한다. status/peek/migrate/PITR은 파일 엔진에 해당 없음 —
//! 핸들러가 명확히 거부하거나 최소 보고만 한다.

pub mod backup;
pub mod restore;
pub mod status;

use std::path::PathBuf;

use crate::error::{Result, XBackupError};

/// 이 엔진의 아카이브 포맷 식별자(manifest.archive_format) — 내용물은 tar 스트림이다.
pub const FORMAT_ID: &str = "xb-file-tar-v1";

/// `file://` URI에서 로컬 절대 경로를 얻는다.
///
/// 형식: `file:///abs/path`(호스트 없음) 또는 `file://localhost/abs/path`.
/// 퍼센트 인코딩(`%20` 등)은 최소한으로 디코드한다. 상대 경로·원격 호스트는 거부(exit 2).
pub fn path_from_uri(uri: &str) -> Result<PathBuf> {
    let rest = uri
        .trim()
        .strip_prefix("file://")
        .ok_or_else(|| XBackupError::Usage(format!("file:// URI가 아닙니다: '{uri}'")))?;
    // 호스트 부분 분리: "" 또는 "localhost"만 허용.
    let (host, path) = match rest.find('/') {
        Some(idx) => rest.split_at(idx),
        None => (rest, ""),
    };
    if !host.is_empty() && host != "localhost" {
        return Err(XBackupError::Usage(format!(
            "file:// URI는 로컬 경로만 지원합니다(호스트 '{host}' 불가): '{uri}'"
        )));
    }
    let decoded = percent_decode(path);
    if !decoded.starts_with('/') {
        return Err(XBackupError::Usage(format!(
            "file:// URI에 절대 경로가 필요합니다(예: file:///var/data): '{uri}'"
        )));
    }
    Ok(PathBuf::from(decoded))
}

/// 최소 퍼센트 디코딩 — `%XX` 시퀀스만 바이트로 되돌린다(잘못된 시퀀스는 그대로 둔다).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_from_uri_accepts_absolute_and_localhost() {
        assert_eq!(
            path_from_uri("file:///var/data").unwrap(),
            PathBuf::from("/var/data")
        );
        assert_eq!(
            path_from_uri("file://localhost/var/data").unwrap(),
            PathBuf::from("/var/data")
        );
        // 퍼센트 디코딩(공백).
        assert_eq!(
            path_from_uri("file:///var/my%20data").unwrap(),
            PathBuf::from("/var/my data")
        );
    }

    #[test]
    fn path_from_uri_rejects_remote_relative_and_non_file() {
        for bad in [
            "file://nas01/share", // 원격 호스트.
            "file://relative",    // 경로 없음(호스트로 해석).
            "mongodb://h/db",     // 스킴 불일치.
        ] {
            let err = path_from_uri(bad).unwrap_err();
            assert_eq!(err.exit_code(), 2, "{bad}는 exit 2여야 함: {err}");
        }
    }
}
