//! 웹 화면 문구가 i18n 규약을 지키는지 소스에서 확인한다(R42).
//!
//! ## 왜 소스를 훑는가 — 실행 테스트로는 닿지 않는 문장이 있다
//! 라우트 단위 테스트는 그 라우트가 실제로 만든 화면만 본다. 그런데 문구는 조건 분기 깊은
//! 곳(특정 오류, 특정 상태)에 숨어 있고, 그 분기를 전부 실행으로 밟는 하네스는 이 프로젝트에
//! 없다. 새로 추가되는 화면은 더 그렇다 — **테스트를 함께 안 쓰면 아무도 안 본다.**
//!
//! 그래서 이 파일은 실행이 아니라 **소스 텍스트**를 본다. 정밀하지는 않지만
//! (`Lang::sel` 호출을 괄호 균형으로 찾는 수준) 빠뜨림이 없다: `src/web/view/`에 파일을
//! 하나 더 만들면 그 순간부터 검사 대상이다.
//!
//! ## 무엇을 검사 대상으로 보는가 — 화면에 렌더되는 문자열만
//! - **대상**: `src/web/view/**`의 문자열 리터럴. 이 디렉터리의 코드는 마크업을 만드는 것이
//!   전부이므로, 여기 있는 사람이 읽는 문장은 정의상 화면에 나간다.
//! - **비대상**: 주석·문서 주석(사용자가 못 본다), `#[cfg(test)]` 이후(테스트 단정 문구),
//!   `Lang::sel`/`sel_string` 인자(그게 바로 규약을 지키는 방식이다).
//!
//! ## 여기서 **막지 않는** 것 — 자식 CLI가 만든 문장
//! `/dashboard`·`/catalog`에는 영문 화면에서도 한국어가 뜬다. 그 문장은 웹이 만든 것이
//! 아니라 자식 `x-backup` 프로세스의 stderr을 **그대로 중계**한 것이다(`src/error.rs`,
//! `src/cli/handlers/`). CLI 오류 메시지는 `--lang en`에서도 한국어이므로 — 즉 제품 전체가
//! 그 규약이므로 — 웹만 영문으로 바꾸면 같은 오류가 화면과 터미널에서 다르게 보이게 된다.
//! 그 결정은 R42의 범위가 아니라 오류 메시지 체계 전체의 문제다.
//!
//! 웹이 통제하는 경계는 명확하다: **웹이 만든 문장은 두 언어를 다 갖는다.** 그 경계가
//! 지켜지는지를 이 파일이 본다.

use std::path::{Path, PathBuf};

/// 한글 음절이 들어 있는가.
fn has_hangul(text: &str) -> bool {
    text.chars().any(|c| ('가'..='힣').contains(&c))
}

/// 소스에서 문자열 리터럴의 (시작, 끝) 바이트 범위와 내용을 모은다.
///
/// 주석과 `sel(...)` 호출 범위도 함께 표시해 돌려준다 — 문자열이 그 안에 있으면 검사에서
/// 뺀다. 러스트 문법을 온전히 파싱하지는 않지만, 이 검사가 놓치는 방향(오탐이 아니라
/// 미탐)으로만 틀리도록 짰다.
struct Scan {
    literals: Vec<(usize, String)>,
    comments: Vec<(usize, usize)>,
    sel_calls: Vec<(usize, usize)>,
    test_start: usize,
}

fn scan(src: &str) -> Scan {
    let b = src.as_bytes();
    let n = b.len();
    let mut literals = Vec::new();
    let mut comments = Vec::new();
    let mut sel_calls = Vec::new();
    let test_start = src.find("#[cfg(test)]").unwrap_or(n);

    let mut i = 0;
    while i < n {
        // 줄 주석.
        if b[i] == b'/' && i + 1 < n && b[i + 1] == b'/' {
            let end = src[i..].find('\n').map_or(n, |k| i + k);
            comments.push((i, end));
            i = end;
            continue;
        }
        // 블록 주석.
        if b[i] == b'/' && i + 1 < n && b[i + 1] == b'*' {
            let end = src[i + 2..].find("*/").map_or(n, |k| i + 2 + k + 2);
            comments.push((i, end));
            i = end;
            continue;
        }
        // 여기서부터는 `src`를 문자열로 자른다 — 멀티바이트 문자 한가운데면 건너뛴다
        // (한국어 주석이 많은 저장소라 이 경우가 실제로 흔하다).
        if !src.is_char_boundary(i) {
            i += 1;
            continue;
        }
        // raw 문자열 — `r"`, `r#"`, `r##"` …
        if b[i] == b'r' && i + 1 < n && (b[i + 1] == b'#' || b[i + 1] == b'"') {
            let mut k = i + 1;
            let mut hashes = 0;
            while k < n && b[k] == b'#' {
                hashes += 1;
                k += 1;
            }
            if k < n && b[k] == b'"' {
                let close = format!("\"{}", "#".repeat(hashes));
                let end = src[k + 1..]
                    .find(&close)
                    .map_or(n, |p| k + 1 + p + close.len());
                literals.push((i, src[i..end].to_string()));
                i = end;
                continue;
            }
        }
        // 보통 문자열.
        if b[i] == b'"' {
            let mut k = i + 1;
            while k < n {
                if b[k] == b'\\' {
                    k += 2;
                    continue;
                }
                if b[k] == b'"' {
                    k += 1;
                    break;
                }
                k += 1;
            }
            let end = k.min(n);
            // 이스케이프 건너뛰기가 문자 경계를 넘어설 수 있다 — 안전한 경계까지 물린다.
            let end = (end..=n).find(|&e| src.is_char_boundary(e)).unwrap_or(n);
            literals.push((i, src[i..end].to_string()));
            i = end;
            continue;
        }
        // `sel(` / `sel_string(` 호출 범위.
        for name in ["sel(", "sel_string("] {
            if src[i..].starts_with(name)
                && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_'))
            {
                let mut k = i + name.len();
                let mut depth = 1usize;
                while k < n && depth > 0 {
                    match b[k] {
                        b'(' => depth += 1,
                        b')' => depth -= 1,
                        b'"' => {
                            k += 1;
                            while k < n && b[k] != b'"' {
                                if b[k] == b'\\' {
                                    k += 1;
                                }
                                k += 1;
                            }
                        }
                        _ => {}
                    }
                    k += 1;
                }
                sel_calls.push((i, k));
                break;
            }
        }
        i += 1;
    }

    Scan {
        literals,
        comments,
        sel_calls,
        test_start,
    }
}

fn inside(pos: usize, ranges: &[(usize, usize)]) -> bool {
    ranges.iter().any(|&(a, b)| a <= pos && pos < b)
}

/// `src/web/view/` 아래의 모든 `.rs` 파일.
fn view_sources() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(Path::new("src/web/view"), &mut out);
    out.sort();
    out
}

/// **화면 문구는 전부 `sel`을 지난다** — 뷰 계층에 하드코딩된 한국어가 없다(R42).
///
/// 이 검사가 잡는 것: 화면을 하나 더 만들면서 한국어만 적어 두는 실수. 그러면 영문 사용자에게
/// 그 문장만 한국어로 보이는데, 컴파일도 통과하고 기존 테스트도 전부 통과한다.
#[test]
fn no_view_string_carries_korean_outside_sel() {
    let files = view_sources();
    assert!(
        files.len() >= 10,
        "뷰 파일을 찾지 못했다({}개) — 테스트가 저장소 루트에서 도는지 확인하라",
        files.len()
    );

    let mut offenders = Vec::new();
    for path in &files {
        let src = std::fs::read_to_string(path).expect("뷰 소스를 읽지 못했다");
        let s = scan(&src);
        for (pos, text) in &s.literals {
            if *pos >= s.test_start || !has_hangul(text) {
                continue;
            }
            if inside(*pos, &s.comments) || inside(*pos, &s.sel_calls) {
                continue;
            }
            let line = src[..*pos].matches('\n').count() + 1;
            let preview: String = text.chars().take(60).collect();
            offenders.push(format!("{}:{line}  {preview}", path.display()));
        }
    }

    assert!(
        offenders.is_empty(),
        "sel() 밖에 한국어 문구가 있다 — 영문 화면에 그대로 나간다:\n{}",
        offenders.join("\n")
    );
}

/// **검사 하네스가 살아 있는지 스스로 증명한다.**
///
/// 위 테스트는 "아무것도 못 찾았다"로 통과한다. 스캐너가 조용히 망가져도(경로 오타, 파싱
/// 실패) 똑같이 초록이 된다 — 이 프로젝트가 `tests/web_secret_leak.rs`에서 이미 한 번 겪은
/// "죽었는데 초록"이다. 그래서 위반을 일부러 만들어 실제로 걸리는지 확인한다.
#[test]
fn the_scanner_actually_catches_a_violation() {
    let src = r#"
fn body(lang: Lang) -> Markup {
    html! {
        p { (lang.sel("Backup complete", "백업이 끝났습니다")) }
        p { "직접 적은 한국어" }
    }
}
"#;
    let s = scan(src);
    let found: Vec<&String> = s
        .literals
        .iter()
        .filter(|(pos, text)| {
            has_hangul(text) && !inside(*pos, &s.comments) && !inside(*pos, &s.sel_calls)
        })
        .map(|(_, text)| text)
        .collect();

    assert_eq!(
        found.len(),
        1,
        "스캐너가 위반을 정확히 하나 잡아야 한다 — 잡은 것: {found:?}"
    );
    assert!(
        found[0].contains("직접 적은 한국어"),
        "잘못된 것을 잡았다: {found:?}"
    );
}

/// 주석 속 한국어는 위반이 아니다 — 이 저장소의 주석은 전부 한국어다.
#[test]
fn korean_in_comments_is_not_a_violation() {
    let src = r#"
/// 이 함수는 화면을 만든다.
// 여기도 한국어 주석
fn body() -> Markup {
    html! { p { "ok" } }
}
"#;
    let s = scan(src);
    let found = s
        .literals
        .iter()
        .filter(|(pos, text)| {
            has_hangul(text) && !inside(*pos, &s.comments) && !inside(*pos, &s.sel_calls)
        })
        .count();
    assert_eq!(found, 0, "주석을 위반으로 잡았다");
}

/// **웹이 만든 검증 문구는 두 언어를 다 낸다** — 대표 경로를 실제로 호출해 확인한다.
///
/// 위의 소스 스캔은 뷰 계층만 본다. 검증 오류는 뷰가 아니라 `job::args`·`schedule::cron`이
/// 만들고 화면이 그대로 싣는데, 그 경로는 소스 텍스트로 판정하기 어렵다(문장이 `format!`
/// 안에 조각으로 흩어져 있다). 그래서 여기서는 **실제로 불러 본다.**
#[test]
fn validation_messages_differ_between_languages() {
    use x_backup::i18n::Lang;
    use x_backup::web::job::ProfileName;

    for bad in ["", "bad name", "--force", "../etc"] {
        // 화면에 실리는 것은 `Display` 전체가 아니라 `detail()`이다 — 종류 접두
        // ("사용법 오류: ")는 `src/error.rs`에 박힌 리터럴이고 화면은 그 종류를 자기
        // 배너 제목으로 이미 말한다(`XBackupError::detail` 문서 참조).
        let en = ProfileName::parse(bad, Lang::En).expect_err("거부되어야 함");
        let ko = ProfileName::parse(bad, Lang::Ko).expect_err("거부되어야 함");
        let (en, ko) = (en.detail(), ko.detail());

        assert_ne!(
            en, ko,
            "'{bad}': 두 언어 문장이 같다 — 한쪽 분기를 빠뜨렸다"
        );
        assert!(
            !has_hangul(&en),
            "'{bad}': 영문 메시지에 한국어가 섞였다: {en}"
        );
        assert!(
            has_hangul(&ko),
            "'{bad}': 한국어 메시지가 한국어가 아니다: {ko}"
        );
    }
}
