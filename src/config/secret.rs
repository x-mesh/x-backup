//! 시크릿 래퍼 — 로그·Debug 출력에 시크릿 값이 새지 않도록 보호한다(PRD §11 보안).
//!
//! `uri_env`/`credentials_env`로 해석된 실제 시크릿 값은 항상 [`Secret`]에 담아
//! 전달한다. `Debug`는 `[REDACTED]`만 출력하므로 `tracing`·`dbg!`·panic 메시지에
//! 평문이 노출되지 않는다.

use std::fmt;

/// Debug 출력을 억제하는 문자열 시크릿 래퍼.
///
/// 직렬화하지 않는다(시크릿은 config 파일·로그에 절대 기록되지 않아야 함).
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// 시크릿 값으로 래퍼를 만든다.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// 내부 평문 값을 노출한다 — 자식 프로세스 env 주입 등 실제 사용 시점에만 호출한다.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 평문 대신 마스킹된 표식만 출력한다.
        f.write_str("Secret([REDACTED])")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Display 경로(로그 포맷 등)에서도 평문을 막는다.
        f.write_str("[REDACTED]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_does_not_leak_value() {
        let s = Secret::new("mongodb://user:p@ss@host");
        let rendered = format!("{s:?}");
        assert!(
            !rendered.contains("p@ss"),
            "Debug에 시크릿 노출: {rendered}"
        );
        assert_eq!(rendered, "Secret([REDACTED])");
    }

    #[test]
    fn display_does_not_leak_value() {
        let s = Secret::new("topsecret");
        assert_eq!(format!("{s}"), "[REDACTED]");
    }

    #[test]
    fn expose_returns_plaintext() {
        let s = Secret::new("topsecret");
        assert_eq!(s.expose(), "topsecret");
    }
}
