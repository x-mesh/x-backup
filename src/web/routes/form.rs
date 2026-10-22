//! `application/x-www-form-urlencoded` 본문 파서 — 화면들이 공유한다.
//!
//! ## 왜 공용으로 뺐나
//! 이 파일이 생기기 전에 `verify`·`backup`·`config`·`schedule`·`prune` 다섯 화면이 각각
//! 이름만 같은 `FormBody`와 `percent_decode`를 들고 있었다. 폼을 받는 화면이 늘 때마다
//! 사본이 하나씩 느는 구조였고, 그 사본들이 **조용히 갈라질 수 있다**는 것이 문제다 —
//! 한 화면에서 `+`를 공백으로 풀고 다른 화면에서는 풀지 않으면, 같은 입력이 화면마다 다른
//! 값으로 해석된다. 파괴적 작업 화면에서 그 어긋남은 "운영자가 타이핑한 이름이 일치하지
//! 않는다"로 나타난다.
//!
//! 새 화면(`prune`·`migrate`)부터 이것을 쓴다. 기존 네 화면의 사본을 여기로 모으는 것은
//! 그 파일들의 테스트를 함께 옮겨야 하는 별도 작업이라 미뤄 뒀다 — 적어도 **사본이 더
//! 늘지는 않는다.**
//!
//! ## 이 파서가 하지 않는 것
//! 중복 키를 배열로 접지 않는다([`FormBody::get`]은 **첫** 값을 준다). HTML 폼에서 같은
//! 이름이 여러 번 오는 것은 체크박스 그룹·다중 선택인데, 이 콘솔의 폼에는 아직 그런 필드가
//! 없다. 생기면 그때 `get_all`을 더한다 — 지금 넣으면 쓰이지 않는 의미를 고정하게 된다.

/// 폼 본문의 얇은 조회기 — 파싱 결과를 `(이름, 값)` 순서대로 들고 있는다.
#[derive(Debug, Clone, Default)]
pub struct FormBody(Vec<(String, String)>);

impl FormBody {
    /// 본문을 파싱한다. 빈 쌍은 버리고, `=`가 없는 조각은 값이 빈 문자열인 키로 본다.
    pub fn parse(body: &str) -> Self {
        Self(
            body.split('&')
                .filter(|pair| !pair.is_empty())
                .map(|pair| {
                    let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                    (percent_decode(k), percent_decode(v))
                })
                .collect(),
        )
    }

    /// 이 이름의 **첫** 값(없으면 `None`).
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// 이 이름의 값이 비어 있지 않은가 — 체크박스 판정.
    ///
    /// HTML 체크박스는 **체크됐을 때만 전송된다**(해제하면 필드 자체가 오지 않는다).
    /// 그래서 "값이 있다 = 켜졌다"가 규약이고, 이 판정을 폼 파싱 옆에 두는 이유는
    /// [`crate::web::guard::ConfirmSubmission::allow_overwrite`] doc이 설명한 그대로다.
    pub fn checked(&self, key: &str) -> bool {
        self.get(key).is_some_and(|v| !v.is_empty())
    }

    /// 값이 있고 공백이 아니면 그 값(trim된), 아니면 `None`.
    ///
    /// 폼의 빈 입력칸은 `field=`로 온다 — "지정하지 않음"과 "빈 문자열을 지정함"을 HTML이
    /// 구분하지 못하므로, 선택 입력은 전부 이 판정을 거쳐야 한다.
    pub fn non_empty(&self, key: &str) -> Option<&str> {
        self.get(key).map(str::trim).filter(|v| !v.is_empty())
    }
}

/// `+`와 `%XX`를 되돌린다.
///
/// 비-UTF8 바이트열은 대체 문자로 접는다 — **패닉하지 않는다.** 폼 본문은 브라우저가
/// 보내는 값이지만 이 콘솔의 POST 경로는 위조 요청도 받으므로(출처 검사가 그 뒤에 있다),
/// 파서가 임의 바이트에서 죽으면 그 자체가 표면이 된다.
///
/// 잘린 이스케이프(`%A` 또는 끝의 `%`)는 그 문자를 **그대로 둔다** — 버리면 `%`가 조용히
/// 사라져 "타이핑한 이름"이 달라 보이고, 그건 이름 재입력 검증을 약하게 만든다.
pub fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pairs_and_returns_the_first_value() {
        let form = FormBody::parse("a=1&b=2&a=3");
        assert_eq!(form.get("a"), Some("1"), "첫 값을 줘야 한다");
        assert_eq!(form.get("b"), Some("2"));
        assert_eq!(form.get("missing"), None);
    }

    #[test]
    fn handles_empty_and_valueless_pairs() {
        let form = FormBody::parse("&a=&&b&c=3&");
        assert_eq!(form.get("a"), Some(""), "빈 값도 '있음'이다");
        assert_eq!(form.get("b"), Some(""), "= 없는 조각은 빈 값 키다");
        assert_eq!(form.get("c"), Some("3"));
        assert_eq!(FormBody::parse("").get("a"), None);
    }

    #[test]
    fn decodes_plus_and_percent_escapes() {
        let form = FormBody::parse("q=hello+world&n=%ED%95%9C%EA%B8%80&s=a%2Bb");
        assert_eq!(form.get("q"), Some("hello world"));
        assert_eq!(form.get("n"), Some("한글"), "UTF-8 다바이트가 깨졌다");
        assert_eq!(form.get("s"), Some("a+b"), "%2B는 리터럴 +다");
    }

    /// 잘린 이스케이프는 **그 문자를 그대로 둔다** — 조용히 지우면 이름 재입력 검증이
    /// 약해진다(원문과 다른 값이 "일치"로 보일 수 있다).
    #[test]
    fn truncated_escapes_keep_the_percent_sign() {
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("a%4"), "a%4");
        assert_eq!(percent_decode("%zz"), "%zz", "hex가 아니면 그대로");
    }

    /// 임의 바이트에서 패닉하지 않는다.
    #[test]
    fn invalid_utf8_folds_to_replacement_without_panicking() {
        let decoded = percent_decode("%FF%FE");
        assert!(!decoded.is_empty());
        assert!(decoded.contains('\u{fffd}'), "대체 문자로 접히지 않았다");
    }

    #[test]
    fn checkbox_semantics_follow_html() {
        // 체크됨: 브라우저가 value를 실어 보낸다.
        assert!(FormBody::parse("agree=1").checked("agree"));
        assert!(FormBody::parse("agree=on").checked("agree"));
        // 해제됨: 필드가 아예 오지 않는다.
        assert!(!FormBody::parse("other=1").checked("agree"));
        // 빈 값은 켜진 것으로 보지 않는다.
        assert!(!FormBody::parse("agree=").checked("agree"));
    }

    #[test]
    fn non_empty_trims_and_folds_blank_to_none() {
        let form = FormBody::parse("a=++7++&b=+++&c=");
        assert_eq!(form.non_empty("a"), Some("7"));
        assert_eq!(form.non_empty("b"), None, "공백뿐인 값은 미지정이다");
        assert_eq!(form.non_empty("c"), None);
        assert_eq!(form.non_empty("missing"), None);
    }
}
