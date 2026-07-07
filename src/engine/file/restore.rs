//! 파일 엔진 복구 — tar 스트림을 대상 디렉터리에 풀어놓는다(P2-1).
//!
//! 덤프의 역방향: 파이프라인 역스택(decrypt→decompress)이 낸 tar 바이트 스트림을
//! [`SyncIoBridge`]로 동기 리더로 바꿔 blocking task의 `tar::Archive`가 소비한다.
//! 항목별 `unpack_in`은 경로 탈출(`..`)을 거부한다 — 조작된 아카이브가 대상 디렉터리
//! 밖에 쓰는 것을 막는다. 권한·mtime은 tar 헤더대로 복원한다.
//!
//! [`SyncIoBridge`]: tokio_util::io::SyncIoBridge

use std::path::{Path, PathBuf};

use tokio_util::io::SyncIoBridge;

use crate::error::{Result, XBackupError};
use crate::storage::BoxAsyncRead;

/// tar 스트림을 `target` 디렉터리에 푼다. 풀어놓은 항목 수를 반환한다.
///
/// `target`이 없으면 만든다(중간 경로 포함). 기존 파일 위 덮어쓰기 가드는 호출자
/// (restore 파이프라인의 --force/확인 가드레일) 책임이다.
pub async fn file_restore(stream: BoxAsyncRead, target: &Path) -> Result<u64> {
    let target: PathBuf = target.to_path_buf();
    tokio::fs::create_dir_all(&target).await.map_err(|e| {
        XBackupError::Failure(format!(
            "복구 대상 디렉터리 생성 실패('{}'): {e}",
            target.display()
        ))
    })?;

    // async 스트림 → 동기 리더 브리지(런타임 컨텍스트에서 생성해 blocking으로 이동).
    let bridge = SyncIoBridge::new(stream);
    tokio::task::spawn_blocking(move || unpack_tar(bridge, &target))
        .await
        .map_err(|e| XBackupError::Failure(format!("파일 복구 task join 실패: {e}")))?
}

/// tar 아카이브를 순회하며 항목을 대상에 푼다(blocking 컨텍스트).
fn unpack_tar(bridge: SyncIoBridge<BoxAsyncRead>, target: &Path) -> Result<u64> {
    let io_err = |ctx: &str, e: std::io::Error| {
        XBackupError::Failure(format!("파일 복구 {ctx} 실패('{}'): {e}", target.display()))
    };

    let mut archive = tar::Archive::new(bridge);
    archive.set_preserve_permissions(true);
    archive.set_preserve_mtime(true);

    let mut unpacked = 0u64;
    let entries = archive.entries().map_err(|e| io_err("아카이브 열기", e))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| io_err("항목 읽기", e))?;
        // unpack_in은 경로 탈출(절대 경로·`..`)을 스킵/거부한다 — 대상 밖 쓰기 방지.
        let ok = entry
            .unpack_in(target)
            .map_err(|e| io_err("항목 풀기", e))?;
        if ok {
            unpacked += 1;
        }
    }
    Ok(unpacked)
}

#[cfg(test)]
mod tests {
    use super::super::backup::FileDumper;
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// 라운드트립: 트리(서브디렉터리·권한·심링크) → tar 스트림 → 다른 디렉터리에 복구
    /// → 내용·권한·링크가 동일해야 한다. DB·외부 도구 없이 완결되는 엔진 검증.
    #[tokio::test]
    async fn dump_restore_round_trip_preserves_tree() {
        let src = tempfile::tempdir().unwrap();
        std::fs::create_dir(src.path().join("sub")).unwrap();
        std::fs::write(src.path().join("a.txt"), b"alpha").unwrap();
        std::fs::write(src.path().join("sub/b.bin"), vec![7u8; 4096]).unwrap();
        let script = src.path().join("run.sh");
        std::fs::write(&script, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("a.txt", src.path().join("link")).unwrap();

        let stream = FileDumper::open(src.path().to_path_buf())
            .unwrap()
            .dump_stream();
        let handle = stream.handle();

        let dst = tempfile::tempdir().unwrap();
        let unpacked = file_restore(Box::pin(stream), dst.path()).await.unwrap();
        handle.finish().await.unwrap();
        assert!(unpacked >= 5, "항목 수 부족: {unpacked}");

        assert_eq!(std::fs::read(dst.path().join("a.txt")).unwrap(), b"alpha");
        assert_eq!(
            std::fs::read(dst.path().join("sub/b.bin")).unwrap(),
            vec![7u8; 4096]
        );
        let mode = std::fs::metadata(dst.path().join("run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755, "실행 권한 보존");
        let link = std::fs::read_link(dst.path().join("link")).unwrap();
        assert_eq!(link, PathBuf::from("a.txt"), "심링크 자체 보존");
    }

    /// 단일 파일 소스도 파일명으로 담겨 복구된다.
    #[tokio::test]
    async fn single_file_round_trip() {
        let src = tempfile::tempdir().unwrap();
        let file = src.path().join("only.dat");
        std::fs::write(&file, b"solo").unwrap();

        let stream = FileDumper::open(file).unwrap().dump_stream();
        let handle = stream.handle();
        let dst = tempfile::tempdir().unwrap();
        file_restore(Box::pin(stream), dst.path()).await.unwrap();
        handle.finish().await.unwrap();

        assert_eq!(std::fs::read(dst.path().join("only.dat")).unwrap(), b"solo");
    }

    /// 부재 소스는 사전 점검 실패(exit 3)로 조기 거부된다.
    #[test]
    fn open_missing_source_is_precheck_failure() {
        let err = FileDumper::open(PathBuf::from("/nonexistent/xb-test")).unwrap_err();
        assert_eq!(err.exit_code(), 3);
    }
}
