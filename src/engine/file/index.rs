//! 파일 엔진 스냅샷 인덱스(P2-2) — 증분 diff의 기준이 되는 트리 스냅샷.
//!
//! 백업(풀·증분)마다 그 시점의 트리 인덱스(경로·종류·크기·mtime·mode·링크 대상)를
//! `<id>/index.json.zst` 사이드카로 기록한다. 다음 증분은 **이전 체인 헤드의 인덱스**와
//! 현재 트리를 대조해 변경/신규/삭제를 계산한다 — 파일 해시 없이 (종류, 크기, mtime,
//! mode, 링크)의 변화로 감지한다(rsync 기본 휴리스틱과 동일; 내용만 바뀌고 메타가 전부
//! 동일한 조작은 감지하지 못하는 트레이드오프를 문서화한다).
//!
//! ## 왜 비암호화인가
//! 백업 호스트는 age **공개키만** 가지므로(§8.1 키 격리) 자기가 쓴 암호문을 읽을 수
//! 없다. 다음 증분이 읽어야 하는 인덱스를 암호화하면 키 격리 배포에서 증분이 불가능해
//! 진다. 경로·크기·mtime 메타데이터가 평문(zstd)으로 남는 트레이드오프는 README에
//! 문서화한다 — 파일 **내용**은 여전히 암호화된다.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;

use crate::error::{Result, XBackupError};

/// 인덱스 포맷 버전(비호환 변경 시 올린다 — 구버전은 gap으로 취급돼 풀 승격).
pub const INDEX_VERSION: u32 = 1;

/// 항목 종류.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryKind {
    /// 일반 파일.
    #[serde(rename = "f")]
    File,
    /// 디렉터리.
    #[serde(rename = "d")]
    Dir,
    /// 심링크(따라가지 않음 — 링크 자체).
    #[serde(rename = "l")]
    Symlink,
}

/// 인덱스 항목 — diff 판정에 쓰는 최소 메타.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexEntry {
    /// 종류.
    pub k: EntryKind,
    /// 크기(바이트; 디렉터리는 0).
    pub s: u64,
    /// mtime(unix nanos; 조회 불가 플랫폼은 0).
    pub mt: i128,
    /// 권한 mode 비트(하위 12비트).
    pub mo: u32,
    /// 심링크 대상(심링크만).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub lt: Option<String>,
}

/// 트리 스냅샷 인덱스 — 루트 기준 상대 경로("a/b.txt") → 항목. BTreeMap으로 결정적
/// 순서를 보장한다(직렬화 안정성·테스트 용이).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileIndex {
    /// 포맷 버전.
    pub version: u32,
    /// 상대 경로 → 항목.
    pub entries: BTreeMap<String, IndexEntry>,
}

/// 이전 인덱스 대비 변경 집합.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexDiff {
    /// 신규 또는 변경된 경로(정렬 순서) — 증분 tar에 담는다.
    pub changed: Vec<String>,
    /// 삭제된 경로(정렬 순서) — tombstone으로 기록한다.
    pub deleted: Vec<String>,
}

impl IndexDiff {
    /// 변경이 전혀 없는지(빈 슬라이스 판정).
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.deleted.is_empty()
    }

    /// 총 변경 엔트리 수(manifest의 엔트리 카운트).
    pub fn count(&self) -> u64 {
        (self.changed.len() + self.deleted.len()) as u64
    }
}

impl FileIndex {
    /// 루트를 재귀 순회해 현재 트리의 인덱스를 만든다(심링크는 따라가지 않음).
    ///
    /// 읽기 실패 항목은 **에러**다 — status와 달리 백업 기준 스냅샷이 불완전하면
    /// 다음 diff가 조용히 틀리기 때문에 관대하지 않다.
    pub fn build(root: &Path) -> Result<Self> {
        let mut index = FileIndex {
            version: INDEX_VERSION,
            entries: BTreeMap::new(),
        };
        let meta = std::fs::symlink_metadata(root).map_err(|e| index_err(root, "루트 조회", e))?;
        if meta.is_dir() {
            walk(root, Path::new(""), &mut index)?;
        } else {
            // 단일 파일 소스: 파일명 하나가 인덱스 전체다(덤프 레이아웃과 동일).
            let name = root
                .file_name()
                .ok_or_else(|| {
                    XBackupError::Usage(format!("파일명 없는 경로: '{}'", root.display()))
                })?
                .to_string_lossy()
                .into_owned();
            index.entries.insert(name, entry_from_meta(root, &meta)?);
        }
        Ok(index)
    }

    /// 이전 인덱스 대비 변경/삭제를 계산한다(경로 사전순).
    pub fn diff_from(&self, prev: &FileIndex) -> IndexDiff {
        let mut diff = IndexDiff::default();
        for (path, entry) in &self.entries {
            match prev.entries.get(path) {
                Some(old) if old == entry => {}
                _ => diff.changed.push(path.clone()),
            }
        }
        for path in prev.entries.keys() {
            if !self.entries.contains_key(path) {
                diff.deleted.push(path.clone());
            }
        }
        diff
    }

    /// zstd 압축 JSON 바이트로 직렬화한다(사이드카 기록용).
    pub async fn encode(&self) -> Result<Vec<u8>> {
        let json = serde_json::to_vec(self)
            .map_err(|e| XBackupError::Failure(format!("인덱스 직렬화 실패: {e}")))?;
        let mut encoder =
            async_compression::tokio::bufread::ZstdEncoder::new(std::io::Cursor::new(json));
        let mut out = Vec::new();
        encoder
            .read_to_end(&mut out)
            .await
            .map_err(|e| XBackupError::Failure(format!("인덱스 압축 실패: {e}")))?;
        Ok(out)
    }

    /// 사이드카 바이트에서 역직렬화한다. 버전 불일치는 에러(호출자가 gap으로 취급해
    /// 풀 승격한다).
    pub async fn decode(bytes: &[u8]) -> Result<Self> {
        let mut decoder =
            async_compression::tokio::bufread::ZstdDecoder::new(std::io::Cursor::new(bytes));
        let mut json = Vec::new();
        decoder
            .read_to_end(&mut json)
            .await
            .map_err(|e| XBackupError::Failure(format!("인덱스 해제 실패: {e}")))?;
        let index: FileIndex = serde_json::from_slice(&json)
            .map_err(|e| XBackupError::Failure(format!("인덱스 파싱 실패: {e}")))?;
        if index.version != INDEX_VERSION {
            return Err(XBackupError::Failure(format!(
                "인덱스 버전 불일치(파일 {}, 지원 {INDEX_VERSION})",
                index.version
            )));
        }
        Ok(index)
    }
}

/// 디렉터리 재귀 순회 — `rel`은 루트 기준 상대 경로 프리픽스.
fn walk(abs: &Path, rel: &Path, index: &mut FileIndex) -> Result<()> {
    let entries = std::fs::read_dir(abs).map_err(|e| index_err(abs, "디렉터리 열기", e))?;
    for entry in entries {
        let entry = entry.map_err(|e| index_err(abs, "디렉터리 순회", e))?;
        let abs_child = entry.path();
        let rel_child = rel.join(entry.file_name());
        let meta = std::fs::symlink_metadata(&abs_child)
            .map_err(|e| index_err(&abs_child, "메타 조회", e))?;
        let rel_str = rel_child.to_string_lossy().into_owned();
        index
            .entries
            .insert(rel_str, entry_from_meta(&abs_child, &meta)?);
        if meta.is_dir() {
            walk(&abs_child, &rel_child, index)?;
        }
    }
    Ok(())
}

/// 메타데이터에서 인덱스 항목을 만든다.
fn entry_from_meta(path: &Path, meta: &std::fs::Metadata) -> Result<IndexEntry> {
    use std::os::unix::fs::MetadataExt;
    let kind = if meta.is_dir() {
        EntryKind::Dir
    } else if meta.file_type().is_symlink() {
        EntryKind::Symlink
    } else {
        EntryKind::File
    };
    let lt = if kind == EntryKind::Symlink {
        Some(
            std::fs::read_link(path)
                .map_err(|e| index_err(path, "링크 대상 조회", e))?
                .to_string_lossy()
                .into_owned(),
        )
    } else {
        None
    };
    Ok(IndexEntry {
        k: kind,
        s: if kind == EntryKind::File {
            meta.len()
        } else {
            0
        },
        mt: (meta.mtime() as i128) * 1_000_000_000 + (meta.mtime_nsec() as i128),
        mo: meta.mode() & 0o7777,
        lt,
    })
}

fn index_err(path: &Path, ctx: &str, e: std::io::Error) -> XBackupError {
    XBackupError::Failure(format!("인덱스 {ctx} 실패('{}'): {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn build_diff_and_codec_round_trip() {
        let src = tempfile::tempdir().unwrap();
        std::fs::create_dir(src.path().join("d")).unwrap();
        std::fs::write(src.path().join("a.txt"), b"one").unwrap();
        std::fs::write(src.path().join("d/b.txt"), b"two").unwrap();
        std::os::unix::fs::symlink("a.txt", src.path().join("ln")).unwrap();

        let base = FileIndex::build(src.path()).unwrap();
        assert_eq!(base.entries.len(), 4, "a.txt, d, d/b.txt, ln");
        assert_eq!(base.entries["ln"].lt.as_deref(), Some("a.txt"));

        // 무변경 diff는 빈 집합.
        assert!(base.diff_from(&base).is_empty());

        // 변경: a.txt 수정(크기), d/b.txt 삭제, 신규 c.txt.
        std::fs::write(src.path().join("a.txt"), b"one-changed").unwrap();
        std::fs::remove_file(src.path().join("d/b.txt")).unwrap();
        std::fs::write(src.path().join("c.txt"), b"new").unwrap();
        let cur = FileIndex::build(src.path()).unwrap();
        let diff = cur.diff_from(&base);
        // "d"도 변경으로 잡힌다 — 내부 파일 삭제로 디렉터리 mtime이 갱신되기 때문
        // (디렉터리 항목은 메타만 담겨 비용이 미미하다).
        assert_eq!(
            diff.changed,
            vec!["a.txt".to_string(), "c.txt".to_string(), "d".to_string()]
        );
        assert_eq!(diff.deleted, vec!["d/b.txt".to_string()]);
        assert_eq!(diff.count(), 4);

        // 직렬화 라운드트립.
        let bytes = cur.encode().await.unwrap();
        let decoded = FileIndex::decode(&bytes).await.unwrap();
        assert_eq!(decoded, cur);
    }

    #[tokio::test]
    async fn decode_rejects_future_version() {
        let mut idx = FileIndex {
            version: INDEX_VERSION + 1,
            entries: BTreeMap::new(),
        };
        // encode는 self.version을 그대로 쓴다 — 미래 버전 파일 흉내.
        idx.version = INDEX_VERSION + 1;
        let bytes = idx.encode().await.unwrap();
        assert!(FileIndex::decode(&bytes).await.is_err());
    }

    #[test]
    fn mode_change_is_detected() {
        use std::os::unix::fs::PermissionsExt;
        let src = tempfile::tempdir().unwrap();
        let f = src.path().join("x");
        std::fs::write(&f, b"same").unwrap();
        let before = FileIndex::build(src.path()).unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o700)).unwrap();
        let after = FileIndex::build(src.path()).unwrap();
        assert_eq!(after.diff_from(&before).changed, vec!["x".to_string()]);
    }
}
