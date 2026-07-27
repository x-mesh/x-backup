//! 뷰 계층 — maud SSR 마크업과 컴파일 타임 임베드 에셋.
//!
//! ## 왜 maud + 임베드인가
//! x-backup은 외부 의존 없는 단일 정적 바이너리다(README 최상단 약속). 런타임에 템플릿
//! 파일이나 정적 디렉터리를 찾는 순간 그 약속이 깨진다. maud는 `html!` 매크로가 컴파일
//! 타임에 Rust 코드로 펼쳐지므로 템플릿 파일이 아예 없고, CSS 같은 텍스트 에셋은
//! [`include_str!`]로 바이너리에 박는다(R40·R41). 릴리스 스크립트는 변하지 않는다.
//!
//! `rust-embed` 같은 크레이트는 쓰지 않는다 — 에셋이 소수(현재 1개)라 매크로 하나로
//! 충분하고, 의존성을 늘릴 이유가 없다.
//!
//! ## 에셋 캐싱 — 지문(fingerprint) URL
//! 에셋 내용이 바이너리에 고정되므로, **URL에 내용 지문을 붙이면** 그 URL이 가리키는
//! 바이트는 영원히 변하지 않는다. 그래서 `immutable` + 1년 캐시를 안전하게 줄 수 있고,
//! 새 버전을 배포하면 지문이 달라져 브라우저가 자동으로 새로 받는다. 버전 문자열
//! (`CARGO_PKG_VERSION`) 대신 내용 해시를 쓰는 이유는, 개발 중 버전을 올리지 않고 CSS만
//! 고치는 경우가 훨씬 흔하기 때문이다.
//!
//! ## 모듈 구성 — 구조와 스타일의 분리
//! - [`layout`]: 문서 껍데기(`<head>`·헤더·내비게이션). 화면마다 다시 쓰지 않는 것들.
//! - [`components`]: 재사용 조각(배지·판정 배너·카드·표). **의미만 담고 모양은 담지 않는다.**
//!
//! 비주얼 아이덴티티(색·타이포·모션)가 확정되지 않은 상태에서 20여 개 화면이 붙어야 하므로,
//! 마크업이 모양을 아는 만큼 아이덴티티 변경이 화면 수만큼 번진다. 그래서 상태는
//! [`components::Level`]의 `data-level` 토큰으로만 표현하고, 그 토큰을 색으로 바꾸는 곳은
//! `app.css`의 `[data-level="…"]` 규칙 한 군데뿐이다 — 자세한 계약은 [`components`] 헤더 참고.
//!
//! ## 함정
//! - 마크업에 서버 내부 정보(bind 주소·config 경로·state 경로)를 흘리지 않는다. 브라우저에
//!   보내는 순간 스크린샷·캐시·프록시 로그로 번진다.
//! - maud는 보간값을 자동 HTML 이스케이프한다. `PreEscaped`를 쓰는 순간 그 보호가 사라지므로
//!   컴파일 타임 상수에만 쓴다(사용자·DB에서 온 문자열에는 절대 쓰지 않는다).

pub mod backup;
pub mod catalog;
pub mod components;
pub mod config;
pub mod dashboard;
pub mod jobs;
pub mod layout;
pub mod migrate;
pub mod monitor;
pub mod peek;
pub mod prune;
pub mod restore;
pub mod schedule;
pub mod verify;

use std::sync::LazyLock;

/// 임베드된 스타일시트 본문. 컴파일 타임에 바이너리로 들어간다(R41).
pub const APP_CSS: &str = include_str!("../assets/app.css");

/// 스타일시트 라우트 경로(쿼리 없는 부분). 라우터와 `<link href>`가 이 상수를 공유해
/// 경로 문자열이 두 곳에서 어긋나지 않게 한다.
pub const APP_CSS_PATH: &str = "/assets/app.css";

/// 에셋 응답의 `Cache-Control`. 지문 URL이므로 내용이 바뀌면 URL이 바뀐다 → 영구 캐시가 안전하다.
pub const ASSET_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";

/// 지문에 쓰는 sha256 접두 바이트 수(→ hex 8자). 캐시 무효화 용도라 충돌 내성은 이 정도로 충분하다.
const FINGERPRINT_BYTES: usize = 4;

/// 에셋 지문 — `sha256(app.css)`의 앞 [`FINGERPRINT_BYTES`]바이트를 hex로.
///
/// 프로세스 수명 동안 한 번만 계산한다(내용이 컴파일 타임 상수이므로 재계산할 이유가 없다).
static APP_CSS_FINGERPRINT: LazyLock<String> = LazyLock::new(|| {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(APP_CSS.as_bytes());
    hex::encode(&digest[..FINGERPRINT_BYTES])
});

/// `<link href>`에 넣을 지문 포함 스타일시트 URL(`/assets/app.css?v=<hash>`).
pub fn app_css_url() -> String {
    format!("{APP_CSS_PATH}?v={}", &*APP_CSS_FINGERPRINT)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 에셋이 실제로 임베드되어 비어 있지 않다 — `include_str!` 경로가 어긋나면
    /// 컴파일이 깨지지만, 파일이 빈 채로 커밋되는 사고는 잡히지 않는다.
    #[test]
    fn app_css_is_embedded_and_non_empty() {
        assert!(
            APP_CSS.len() > 200,
            "app.css가 비었거나 너무 짧다: {} bytes",
            APP_CSS.len()
        );
        assert!(
            APP_CSS.contains("--c-bg"),
            "색 토큰이 없다 — 잘못된 파일 임베드"
        );
    }

    /// 지문 URL은 경로 상수 + 8자 hex 쿼리 형태이고, 호출마다 같은 값이다.
    #[test]
    fn css_url_carries_stable_fingerprint() {
        let url = app_css_url();
        let expected_prefix = format!("{APP_CSS_PATH}?v=");
        let hash = url
            .strip_prefix(&expected_prefix)
            .expect("지문 URL 형태가 아니다");
        assert_eq!(hash.len(), FINGERPRINT_BYTES * 2, "hex 길이 불일치: {hash}");
        assert!(
            hash.chars().all(|c| c.is_ascii_hexdigit()),
            "hex가 아닌 문자 포함: {hash}"
        );
        assert_eq!(url, app_css_url(), "지문이 호출마다 달라진다");
    }
}
