//! 파일 엔진 덤프 — 로컬 경로를 tar 스트림(AsyncRead)으로 직렬화한다(P2-1).
//!
//! tar 직렬화는 동기 IO(blocking task)에서 수행하고, [`tokio::io::duplex`] 파이프의
//! 쓰기 절반을 [`SyncIoBridge`]로 감싸 async 리더에 잇는다 — 전체 트리를 메모리에
//! 적재하지 않는 스트리밍이다(PRD §11 상수 메모리). 스트림 소비(EOF) 후
//! [`FileDumpHandle::finish`]로 직렬화 task의 오류를 회수한다(native 엔진의
//! `NativeDumpHandle`과 동일 계약 — [`DumpTermination`] 훅으로 공통 코어에 접속).
//!
//! ## 아카이브 레이아웃
//! - 디렉터리 소스: 내용물을 `.` 기준 상대 경로로 담는다 — 복구가 대상 디렉터리에
//!   내용물을 그대로 풀어놓는다(소스/대상 경로가 달라도 됨).
//! - 단일 파일 소스: 파일명 하나를 담는다.
//! - 심링크는 링크 자체를 보존한다(`follow_symlinks(false)` — 링크 대상을 따라가
//!   중복/루프에 빠지지 않는다). 권한·mtime은 tar 헤더에 보존된다.
//!
//! [`DumpTermination`]: crate::pipeline::backup
//! [`SyncIoBridge`]: tokio_util::io::SyncIoBridge

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, DuplexStream, ReadBuf};
use tokio::task::JoinHandle;
use tokio_util::io::SyncIoBridge;

use crate::error::{Result, XBackupError};

/// duplex 파이프 버퍼(백프레셔 경계).
const PIPE_BUFFER_BYTES: usize = 256 * 1024;

/// 파일/디렉터리 덤퍼 — 소스 경로 검증 후 tar 스트림을 만든다.
#[derive(Debug)]
pub struct FileDumper {
    root: PathBuf,
}

impl FileDumper {
    /// 소스 경로를 검증한다(존재·접근 가능). 부재/권한 오류는 사전 점검 실패(exit 3)다.
    pub fn open(root: PathBuf) -> Result<Self> {
        std::fs::metadata(&root).map_err(|e| {
            XBackupError::PrecheckFailed(format!(
                "백업 소스 경로 접근 실패('{}'): {e}",
                root.display()
            ))
        })?;
        Ok(Self { root })
    }

    /// tar 직렬화 task를 시작하고 아카이브 바이트 스트림을 반환한다.
    pub fn dump_stream(self) -> FileDumpStream {
        let (reader, writer) = tokio::io::duplex(PIPE_BUFFER_BYTES);
        // SyncIoBridge는 런타임 컨텍스트에서 만들어 blocking task로 넘긴다.
        let bridge = SyncIoBridge::new(writer);
        let root = self.root;
        let task = tokio::task::spawn_blocking(move || build_tar(root, bridge));
        FileDumpStream {
            reader,
            task: Arc::new(Mutex::new(Some(task))),
        }
    }

    /// 증분 tar 직렬화 task(P2-2) — 변경/신규 경로만 담고, 삭제 목록은
    /// [`TOMBSTONE_ENTRY`](super::TOMBSTONE_ENTRY) 엔트리로 함께 담는다(암호화 스트림
    /// 내부이므로 경로가 평문 노출되지 않는다 — 인덱스 사이드카와 다른 점).
    pub fn dump_incremental_stream(
        self,
        changed: Vec<String>,
        deleted: Vec<String>,
    ) -> FileDumpStream {
        let (reader, writer) = tokio::io::duplex(PIPE_BUFFER_BYTES);
        let bridge = SyncIoBridge::new(writer);
        let root = self.root;
        let task =
            tokio::task::spawn_blocking(move || build_incr_tar(root, changed, deleted, bridge));
        FileDumpStream {
            reader,
            task: Arc::new(Mutex::new(Some(task))),
        }
    }
}

/// 증분 tar를 bridge에 쓴다(blocking 컨텍스트) — 변경 경로들 + tombstone 엔트리.
fn build_incr_tar(
    root: PathBuf,
    changed: Vec<String>,
    deleted: Vec<String>,
    bridge: SyncIoBridge<DuplexStream>,
) -> Result<()> {
    let io_err = |ctx: &str, e: std::io::Error| {
        XBackupError::Failure(format!("파일 증분 {ctx} 실패('{}'): {e}", root.display()))
    };

    let mut builder = tar::Builder::new(bridge);
    builder.follow_symlinks(false);

    // 1) tombstone 엔트리(있으면) — 복구가 삭제 지시로 해석한다.
    if !deleted.is_empty() {
        let payload = serde_json::to_vec(&deleted)
            .map_err(|e| XBackupError::Failure(format!("tombstone 직렬화 실패: {e}")))?;
        let mut header = tar::Header::new_gnu();
        header.set_size(payload.len() as u64);
        header.set_mode(0o600);
        header.set_cksum();
        builder
            .append_data(&mut header, super::TOMBSTONE_ENTRY, payload.as_slice())
            .map_err(|e| io_err("tombstone 기록", e))?;
    }

    // 2) 변경/신규 경로 — 디렉터리는 메타만, 파일/심링크는 내용/링크째 담는다.
    for rel in &changed {
        let abs = root.join(rel);
        let meta = std::fs::symlink_metadata(&abs).map_err(|e| io_err("항목 조회", e))?;
        if meta.is_dir() {
            builder
                .append_dir(rel, &abs)
                .map_err(|e| io_err("디렉터리 기록", e))?;
        } else {
            builder
                .append_path_with_name(&abs, rel)
                .map_err(|e| io_err("항목 기록", e))?;
        }
    }

    let mut bridge = builder
        .into_inner()
        .map_err(|e| io_err("아카이브 종료", e))?;
    bridge.shutdown().map_err(|e| io_err("스트림 종료", e))?;
    Ok(())
}

/// tar 아카이브를 bridge에 쓴다(blocking 컨텍스트). 리더가 먼저 drop되면 쓰기가
/// BrokenPipe로 끝난다 — 실패 경로의 자연스러운 종료다.
fn build_tar(root: PathBuf, bridge: SyncIoBridge<DuplexStream>) -> Result<()> {
    let io_err = |ctx: &str, e: std::io::Error| {
        XBackupError::Failure(format!("파일 백업 {ctx} 실패('{}'): {e}", root.display()))
    };

    let mut builder = tar::Builder::new(bridge);
    builder.follow_symlinks(false);

    let meta = std::fs::metadata(&root).map_err(|e| io_err("소스 조회", e))?;
    if meta.is_dir() {
        builder
            .append_dir_all(".", &root)
            .map_err(|e| io_err("디렉터리 직렬화", e))?;
    } else {
        let name = root
            .file_name()
            .ok_or_else(|| XBackupError::Usage(format!("파일명 없는 경로: '{}'", root.display())))?
            .to_os_string();
        builder
            .append_path_with_name(&root, name)
            .map_err(|e| io_err("파일 직렬화", e))?;
    }
    let mut bridge = builder
        .into_inner()
        .map_err(|e| io_err("아카이브 종료", e))?;
    // async writer를 flush+shutdown해 리더에 EOF를 전달한다.
    bridge.shutdown().map_err(|e| io_err("스트림 종료", e))?;
    Ok(())
}

/// tar 아카이브 바이트 스트림([`AsyncRead`]) — 파이프라인에 그대로 흘린다.
pub struct FileDumpStream {
    reader: DuplexStream,
    task: Arc<Mutex<Option<JoinHandle<Result<()>>>>>,
}

impl FileDumpStream {
    /// 스트림 소비(EOF) 후 직렬화 결과를 회수할 핸들.
    pub fn handle(&self) -> FileDumpHandle {
        FileDumpHandle {
            task: Arc::clone(&self.task),
        }
    }
}

impl AsyncRead for FileDumpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.reader).poll_read(cx, buf)
    }
}

/// [`FileDumpStream`]의 직렬화 task 결과를 회수하는 핸들.
pub struct FileDumpHandle {
    task: Arc<Mutex<Option<JoinHandle<Result<()>>>>>,
}

impl FileDumpHandle {
    /// 직렬화 task의 결과를 회수한다(IO 오류 전파). 중복 호출은 Ok.
    pub async fn finish(self) -> Result<()> {
        let task = self.task.lock().expect("file dump task poisoned").take();
        match task {
            Some(task) => task
                .await
                .map_err(|e| XBackupError::Failure(format!("파일 백업 task join 실패: {e}")))?,
            None => Ok(()),
        }
    }
}
