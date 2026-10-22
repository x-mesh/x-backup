//! 파일 엔진 status — 소스 경로의 백업 준비 상태를 점검한다(읽기 전용·무부작용).
//!
//! DB status(연결·권한·토폴로지)와 달리 점검 대상이 로컬 경로다: URI 형식 → 존재/종류
//! → 읽기 가능 → 예상 크기(트리 합산). [`StatusReport`] 표면을 DB 엔진들과 공유하므로
//! `status --all`에 파일 프로파일이 섞여 있어도 동일하게 보고된다.

use std::path::Path;

use crate::config::secret::Secret;
use crate::engine::mongo::status::{CheckItem, StatusReport};

/// 사람용 바이트 표기(GiB/MiB/KiB/B) — 예상 크기 항목용.
fn human_bytes(n: u64) -> String {
    const GIB: u64 = 1 << 30;
    const MIB: u64 = 1 << 20;
    const KIB: u64 = 1 << 10;
    if n >= GIB {
        format!("{:.1} GiB", n as f64 / GIB as f64)
    } else if n >= MIB {
        format!("{:.1} MiB", n as f64 / MIB as f64)
    } else if n >= KIB {
        format!("{:.1} KiB", n as f64 / KIB as f64)
    } else {
        format!("{n} B")
    }
}

/// 파일 소스의 전체 status 보고서를 만든다.
pub async fn full_report(profile: &str, uri: &Secret, lang: crate::i18n::Lang) -> StatusReport {
    let path = match crate::engine::file::path_from_uri(uri.expose()) {
        Ok(p) => p,
        Err(e) => {
            return StatusReport::new(
                profile,
                vec![CheckItem::fail(
                    "source",
                    "source",
                    lang.sel(
                        &format!("invalid file:// URI: {e}"),
                        &format!("file:// URI 형식 오류: {e}"),
                    ),
                )],
            )
        }
    };

    // 트리 합산은 blocking IO — 큰 트리에서 워커를 막지 않게 blocking task에서 돈다.
    let report = tokio::task::spawn_blocking(move || scan(&path)).await;
    let items = match report {
        Ok(Ok(s)) => {
            let kind = if s.is_dir {
                lang.sel("directory", "디렉터리")
            } else {
                lang.sel("file", "파일")
            };
            vec![
                CheckItem::ok("source", "source", format!("{} ({kind})", s.display))
                    .with_value(s.display.clone()),
                CheckItem::ok(
                    "estimated_size",
                    "est. size",
                    lang.sel(
                        &format!("{} in {} entries", human_bytes(s.bytes), s.entries),
                        &format!("{} (항목 {}개)", human_bytes(s.bytes), s.entries),
                    ),
                )
                .with_value(human_bytes(s.bytes)),
            ]
        }
        Ok(Err(item)) => vec![item],
        Err(e) => vec![CheckItem::fail(
            "source",
            "source",
            format!("scan task join 실패: {e}"),
        )],
    };
    StatusReport::new(profile, items)
}

/// 스캔 결과 — 경로 표시·종류·총 바이트·항목 수.
struct Scan {
    display: String,
    is_dir: bool,
    bytes: u64,
    entries: u64,
}

/// 경로를 재귀 순회해 크기를 합산한다(심링크는 따라가지 않음 — 백업 의미론과 일치).
fn scan(path: &Path) -> std::result::Result<Scan, CheckItem> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| {
        CheckItem::fail(
            "source",
            "source",
            format!("경로 접근 실패('{}'): {e}", path.display()),
        )
    })?;
    let mut s = Scan {
        display: path.display().to_string(),
        is_dir: meta.is_dir(),
        bytes: 0,
        entries: 0,
    };
    if meta.is_dir() {
        walk(path, &mut s);
    } else {
        s.bytes = meta.len();
        s.entries = 1;
    }
    Ok(s)
}

/// 디렉터리 재귀 합산(읽기 실패 항목은 건너뛴다 — status는 관대, 백업 시 오류로 드러난다).
fn walk(dir: &Path, s: &mut Scan) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        s.entries += 1;
        if meta.is_dir() {
            walk(&entry.path(), s);
        } else {
            s.bytes += meta.len();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::mongo::status::CheckStatus;

    #[tokio::test]
    async fn report_ok_for_existing_tree() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x"), vec![0u8; 2048]).unwrap();
        let uri = Secret::new(format!("file://{}", dir.path().display()));
        let r = full_report("p", &uri, crate::i18n::Lang::En).await;
        assert_eq!(r.overall, CheckStatus::Ok, "{:?}", r.items);
    }

    #[tokio::test]
    async fn report_fails_for_missing_path() {
        let uri = Secret::new("file:///nonexistent/xb-status".to_string());
        let r = full_report("p", &uri, crate::i18n::Lang::En).await;
        assert_eq!(r.overall, CheckStatus::Fail);
    }
}
