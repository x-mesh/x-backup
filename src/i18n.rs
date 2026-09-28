//! 출력 언어(설명/서술 텍스트 i18n).
//!
//! **라벨·기술용어는 언어와 무관하게 항상 영문**(checksum/id/size/location/…)이며, 이
//! 모듈은 설명·안내 문구의 ko/en 선택만 담당한다. 우선순위:
//! CLI `--lang` > env `XB_LANG` > config `[output].language` > 기본 `En`
//! (`--lang`/env는 clap에서 한 플래그로 합쳐지므로 여기서는 flag·config·기본만 본다).
//!
//! 사용 패턴: 라벨은 영문 리터럴 그대로 두고, 설명만 [`Lang::sel`]로 고른다.
//! ```text
//! println!("  checksum:  sha256:{cs}");              // 라벨: 항상 영문
//! println!("{}", lang.sel("Backup complete", "백업 완료"));  // 설명: 토글
//! ```

use std::sync::Mutex;

use clap::ValueEnum;

use crate::config::file::Config;

/// 출력 설명 언어.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum Lang {
    /// English (default).
    #[default]
    En,
    /// 한국어.
    Ko,
}

impl Lang {
    /// 설명 텍스트를 언어에 맞게 고른다. **라벨/기술용어에는 쓰지 않는다**(항상 영문 고정).
    #[inline]
    pub fn sel<'a>(self, en: &'a str, ko: &'a str) -> &'a str {
        match self {
            Lang::En => en,
            Lang::Ko => ko,
        }
    }

    /// 값을 끼워 만든 문장을 언어에 맞게 고른다 — [`Lang::sel`]의 소유 문자열 판.
    ///
    /// `sel`은 빌린 `&str`을 돌려주므로 `format!` 결과를 바로 넘길 수 없다(임시 값이 그
    /// 자리에서 죽는다). 그래서 호출부마다 두 문장을 지역 변수로 묶어 두는 군더더기가
    /// 생겼는데, 값이 들어가는 문장은 오히려 흔하다 — 검증 오류가 전부 그렇다.
    ///
    /// **양쪽을 다 만든 뒤 하나를 버린다.** 값이 들어가는 문장은 대부분 오류 경로에 있어
    /// 드물게 실행되므로, 여기서 문자열 하나를 더 만드는 비용보다 호출부가 읽히는 편이 낫다.
    #[inline]
    pub fn sel_string(self, en: String, ko: String) -> String {
        match self {
            Lang::En => en,
            Lang::Ko => ko,
        }
    }

    /// config 문자열 등에서 언어를 파싱한다(대소문자 무시). 인식 못 하면 `None`.
    pub fn parse_str(s: &str) -> Option<Lang> {
        match s.trim().to_ascii_lowercase().as_str() {
            "en" | "english" => Some(Lang::En),
            "ko" | "kr" | "korean" => Some(Lang::Ko),
            _ => None,
        }
    }
}

/// 한국어 조사 짝 — 앞말에 받침이 있을 때와 없을 때.
///
/// 앞말이 값에 따라 달라지는 문장(`"{label}이 비어 있습니다"`)에서 쓴다.
pub struct Josa {
    /// 받침 있는 말 뒤(`이`/`은`/`을`/`과`).
    pub with: &'static str,
    /// 받침 없는 말 뒤(`가`/`는`/`를`/`와`).
    pub without: &'static str,
}

/// 주격 조사 `이`/`가`.
pub const GA: Josa = Josa {
    with: "이",
    without: "가",
};
/// 목적격 조사 `을`/`를`.
pub const EUL: Josa = Josa {
    with: "을",
    without: "를",
};

impl Josa {
    /// `word` 뒤에 붙일 조사를 고른다.
    pub fn after(&self, word: &str) -> &'static str {
        if has_final_consonant(word) {
            self.with
        } else {
            self.without
        }
    }
}

/// 낱말의 마지막 글자에 받침이 있는가.
///
/// ## 왜 이 판정이 필요한가
/// 검증 메시지는 `"{label}이 비어 있습니다"`처럼 라벨을 문장에 끼운다. 조사를 고정으로
/// 박아 두면 라벨이 바뀌는 순간 문장이 깨진다 — 실제로 `"백업 ID이 비어 있습니다"`,
/// `"네임스페이스이 비어 있습니다"`가 화면에 나가고 있었다.
///
/// ## 한글이 아닌 끝글자를 다루는 방식
/// 조사는 **소리**를 따라간다. 그래서 라틴 문자·숫자로 끝나면 그 글자를 한국어로 읽었을 때
/// 받침이 있는지를 본다 — `ID`는 "아이디"로 끝나 받침이 없고(→ `가`), `SQL`은 "에스큐엘"로
/// 끝나 받침이 있다(→ `이`). 읽는 방식이 갈리는 글자는 표준 표기(외래어 표기법)를 따른다.
///
/// ## 읽히지 않는 끝글자는 건너뛴다
/// `'x'(U+0058)` 같은 표기는 닫는 괄호로 끝나지만 소리로는 `팔`로 끝난다. 그래서 뒤쪽의
/// 구두점·공백을 지나쳐 **실제로 읽히는 마지막 글자**를 찾는다. 이 표기는 검증 메시지가
/// 거부된 문자를 지목할 때 실제로 쓰인다(`web::job::args::describe_char`).
///
/// 판정할 수 없는 끝글자는 받침 없음으로 본다 — 어느 쪽도 확실하지 않을 때 `가`/`를` 쪽이
/// 덜 어색하다.
pub fn has_final_consonant(word: &str) -> bool {
    let Some(last) = word
        .chars()
        .rev()
        .find(|c| c.is_alphanumeric() || ('가'..='힣').contains(c))
    else {
        return false;
    };
    match last {
        // 한글 음절: (코드 - 가) % 28 이 0이 아니면 종성이 있다.
        '가'..='힣' => !(last as u32 - 0xAC00).is_multiple_of(28),
        // 라틴 문자 — 한국어 자모 읽기의 종성 유무.
        // 받침 있음: L(엘) M(엠) N(엔) R(알) 그리고 모음 읽기가 받침으로 끝나는 것들.
        'l' | 'L' | 'm' | 'M' | 'n' | 'N' | 'r' | 'R' => true,
        // 나머지 알파벳은 받침 없이 끝난다(비=B, 시=C, 디=D, 이=E, 에프…는 프로 끝난다).
        'a'..='z' | 'A'..='Z' => false,
        // 숫자 — 영(0) 일(1) 삼(3) 육(6) 칠(7) 팔(8)은 받침으로 끝난다.
        '0' | '1' | '3' | '6' | '7' | '8' => true,
        '2' | '4' | '5' | '9' => false,
        _ => false,
    }
}

/// 플래그(CLI/env) > config language > 기본 `En` 순으로 최종 언어를 정한다.
pub fn resolve(flag: Option<Lang>, config_language: Option<&str>) -> Lang {
    flag.or_else(|| config_language.and_then(Lang::parse_str))
        .unwrap_or_default()
}

/// config.toml 문자열에서 `[output].language`를 읽어 [`resolve`]한다.
/// 파싱 실패/부재는 조용히 무시하고 플래그·기본값으로 떨어진다(언어 결정이 백업을 막지 않는다).
pub fn resolve_from_toml(flag: Option<Lang>, config_toml: Option<&str>) -> Lang {
    if let Some(l) = flag {
        return l;
    }
    let cfg_lang = config_toml
        .and_then(|s| Config::from_toml_str(s).ok())
        .and_then(|c| c.output)
        .and_then(|o| o.language);
    resolve(None, cfg_lang.as_deref())
}

/// 프로세스 전역 출력 언어.
///
/// **왜 전역인가**: 언어를 인자로 넘길 수 없는 출력 지점이 두 부류 있다.
/// 하나는 `main`의 최종 에러 출력이다 — 거기서는 config를 다시 읽지 않고
/// [`crate::error::XBackupError`] 하나만 들고 있다. 다른 하나는 파이프라인 깊은 곳의
/// `tracing::warn!` 호출이다 — 로그 한 줄을 위해 `lang`을 수십 단계 함수 시그니처에
/// 끼워 넣는 것은 비용이 이득보다 크다. 그래서 서브커맨드가 시작할 때 언어를 한 번
/// 정해 두고, 그 지점들이 여기서 읽어 간다.
///
/// **한 번만 정해지는 값이 아니라 여러 번 다시 정할 수 있다** — `OnceLock`에서
/// `Mutex`로 바꾼 이유가 그것이다. 서브커맨드 핸들러는 보통 이렇게 두 단계를 거친다:
///
/// 1. `--config`를 읽기 **전에** CLI `--lang`만으로 한 번 정한다. config를 읽다가
///    실패해도(파일 없음 등) 이 시점에 정한 언어로 에러 라벨과 본문이 나온다.
/// 2. config를 성공적으로 읽으면 그 안의 `[output].language`까지 반영해
///    [`activate_from_toml`]로 다시 정한다.
///
/// `OnceLock`이던 시절엔 1번이 2번을 막아 버렸다 — config를 못 읽는 에러 경로에서
/// `--lang ko`를 줘도 라벨(`main`이 붙이는 `usage error:` 등)이 항상 영어로 고정되는
/// 회귀가 실측됐다(1번 호출이 없으면 기본값 `En`으로 굳고, 있으면 그 값에 영원히 갇힘).
///
/// 정해지기 전에 읽으면 기본값 `En`이다. 테스트는 프로세스를 공유하므로 [`active`]에
/// 의존하는 단정을 쓰지 않는다 — 순수 함수인 [`Lang::sel`]에 언어를 직접 넘겨 검증한다.
static ACTIVE: Mutex<Lang> = Mutex::new(Lang::En);

/// 전역 출력 언어를 (다시) 정한다. 이전 값을 덮어쓴다.
pub fn set_active(lang: Lang) {
    if let Ok(mut guard) = ACTIVE.lock() {
        *guard = lang;
    }
    // 락이 poison됐으면(다른 스레드의 panic) 조용히 무시한다 — 언어 설정은
    // best-effort이고, 실패해도 [`active`]는 여전히 기본값을 안전하게 돌려준다.
}

/// 전역 출력 언어. 아직 정해지지 않았으면 기본 `En`.
pub fn active() -> Lang {
    ACTIVE.lock().map(|g| *g).unwrap_or_default()
}

/// [`resolve`]에 [`set_active`]를 붙인 것. 서브커맨드 진입부에서 쓴다.
pub fn activate(flag: Option<Lang>, config_language: Option<&str>) -> Lang {
    let lang = resolve(flag, config_language);
    set_active(lang);
    lang
}

/// [`resolve_from_toml`]에 [`set_active`]를 붙인 것. 서브커맨드 진입부에서 쓴다.
pub fn activate_from_toml(flag: Option<Lang>, config_toml: Option<&str>) -> Lang {
    let lang = resolve_from_toml(flag, config_toml);
    set_active(lang);
    lang
}

/// 전역 언어에 맞는 쪽만 포맷해서 `String`으로 돌려준다.
///
/// [`Lang::sel`]은 이미 만들어진 두 문자열 중 하나를 고르므로, 값이 끼어드는 문구에 쓰면
/// 양쪽을 다 `format!`해 놓고 하나를 버리게 된다. 이 매크로는 고른 쪽만 포맷한다.
/// 인자는 인라인 캡처를 그대로 쓴다 — `tr!("failed: {e}", "실패: {e}")`.
///
/// 위치 인자도 받는다 — `tr!("failed ({}): {e}", "실패({}): {e}", path.display())`.
/// en/ko 두 포맷 문자열이 같은 위치 인자 목록을 공유한다(둘 다 `{}` 개수가 같아야
/// 한다). `path.display()`처럼 인라인 캡처로 옮기기 번거로운 표현식을 위한 경로다 —
/// 대부분은 인라인 캡처 쪽(위 형태)을 쓴다.
///
/// 언어를 인자로 받을 수 있는 자리에서는 이걸 쓰지 말고 [`Lang::sel`]에 그 언어를
/// 직접 넘긴다. 전역 상태에 기대는 범위를 좁게 유지하려는 것이다.
#[macro_export]
macro_rules! tr {
    ($en:expr, $ko:expr $(,)?) => {
        match $crate::i18n::active() {
            $crate::i18n::Lang::En => format!($en),
            $crate::i18n::Lang::Ko => format!($ko),
        }
    };
    ($en:expr, $ko:expr, $($arg:expr),+ $(,)?) => {
        match $crate::i18n::active() {
            $crate::i18n::Lang::En => format!($en, $($arg),+),
            $crate::i18n::Lang::Ko => format!($ko, $($arg),+),
        }
    };
}

/// [`tr!`]과 같지만 전역 언어 대신 **주어진 언어**로 고른다.
///
/// 서브커맨드 핸들러는 보통 `--config`를 읽어 `[output].language`까지 본 뒤에야
/// [`activate_from_toml`]로 전역 언어를 정한다. 그런데 그 config를 읽는 도중 실패하면
/// — 파일이 없거나 읽기 권한이 없으면 — 이 에러는 전역 언어가 정해지기 **전에** 만들어
/// 진다. 그 자리에서 [`tr!`]을 쓰면 `active()`가 항상 기본값(`En`)을 돌려주므로
/// `--lang ko`를 줘도 영어로 나온다(회귀로 실측됨: config 부재/읽기 실패 두 경로).
///
/// 이 시점엔 config 내용을 아직 못 읽었으니 그 안의 `[output].language`는 알 수 없고,
/// 알 수 있는 건 CLI `--lang`(`lang_flag`)뿐이다. 그래서 이 매크로는 전역 상태를 읽지
/// 않고 첫 인자로 받은 언어(대개 `lang_flag.unwrap_or_default()`)로 직접 고른다.
#[macro_export]
macro_rules! tr_lang {
    ($lang:expr, $en:expr, $ko:expr $(,)?) => {
        match $lang {
            $crate::i18n::Lang::En => format!($en),
            $crate::i18n::Lang::Ko => format!($ko),
        }
    };
    ($lang:expr, $en:expr, $ko:expr, $($arg:expr),+ $(,)?) => {
        match $lang {
            $crate::i18n::Lang::En => format!($en, $($arg),+),
            $crate::i18n::Lang::Ko => format!($ko, $($arg),+),
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn josa_follows_the_final_consonant_of_hangul() {
        assert_eq!(GA.after("이름"), "이", "받침 있는 말 뒤");
        assert_eq!(GA.after("네임스페이스"), "가", "받침 없는 말 뒤");
        assert_eq!(EUL.after("백업"), "을");
        assert_eq!(EUL.after("프로파일"), "을");
        assert_eq!(EUL.after("스케줄"), "을");
    }

    /// **조사는 글자가 아니라 소리를 따라간다.** 라틴 문자로 끝나는 라벨(`백업 ID`)이
    /// 실제로 화면에 있었고, 여기서 틀리면 `"백업 ID이 비어 있습니다"`가 다시 나온다.
    #[test]
    fn josa_reads_latin_endings_as_korean() {
        assert_eq!(GA.after("백업 ID"), "가", "ID는 '아이디'로 끝난다");
        assert_eq!(GA.after("URL"), "이", "URL은 '유알엘'로 끝난다");
        assert_eq!(GA.after("SQL"), "이", "SQL은 '에스큐엘'로 끝난다");
        assert_eq!(GA.after("DB"), "가", "DB는 '디비'로 끝난다");
        assert_eq!(GA.after("PATH"), "가", "H는 '에이치'로 끝난다");
    }

    #[test]
    fn josa_reads_digit_endings_as_korean() {
        for (word, expected) in [
            ("s3", "이"),   // 삼
            ("v2", "가"),   // 이
            ("rfc6", "이"), // 육
            ("h9", "가"),   // 구
        ] {
            assert_eq!(GA.after(word), expected, "{word}의 조사가 틀렸다");
        }
    }

    /// **읽히지 않는 구두점은 건너뛴다.** `\'x\'(U+0058)`은 소리로 `팔`로 끝난다.
    #[test]
    fn trailing_punctuation_is_not_pronounced() {
        assert_eq!(GA.after("'x'(U+0058)"), "이", "8 = 팔 → 받침");
        assert_eq!(GA.after("U+0020"), "이", "0 = 영 → 받침");
        assert_eq!(GA.after("'a'(U+0041)"), "이", "1 = 일 → 받침");
        assert_eq!(GA.after("'b'(U+0042)"), "가", "2 = 이 → 받침 없음");
        assert_eq!(GA.after("60"), "이", "0 = 영 → 받침");
    }

    /// 판정할 수 없는 끝글자는 받침 없음으로 본다 — 확실하지 않을 때 덜 어색한 쪽.
    #[test]
    fn an_undecidable_ending_falls_back_to_no_final_consonant() {
        assert_eq!(GA.after("---"), "가");
        assert_eq!(GA.after(""), "가");
    }

    #[test]
    fn sel_string_picks_by_language() {
        assert_eq!(
            Lang::En.sel_string("size 3".into(), "크기 3".into()),
            "size 3"
        );
        assert_eq!(
            Lang::Ko.sel_string("size 3".into(), "크기 3".into()),
            "크기 3"
        );
    }

    #[test]
    fn sel_picks_by_language() {
        assert_eq!(Lang::En.sel("size", "크기"), "size");
        assert_eq!(Lang::Ko.sel("size", "크기"), "크기");
    }

    /// `tr!`은 활성 언어 쪽만 포맷한다 — 인라인 캡처, 위치 인자 둘 다 같은 규칙을
    /// 따르는지 본다. `active()`는 프로세스 전역(`OnceLock`)이라 다른 테스트가 먼저
    /// 정했을 수 있으므로, 특정 언어로 고정하지 않고 "지금 활성 언어에 맞는 쪽이
    /// 나온다"만 검증한다.
    #[test]
    fn tr_macro_formats_only_the_active_language() {
        let e = "boom";
        let expect_inline = match active() {
            Lang::En => format!("failed: {e}"),
            Lang::Ko => format!("실패: {e}"),
        };
        assert_eq!(tr!("failed: {e}", "실패: {e}"), expect_inline);

        let expect_positional = match active() {
            Lang::En => format!("failed ({})", 42),
            Lang::Ko => format!("실패({})", 42),
        };
        assert_eq!(tr!("failed ({})", "실패({})", 42), expect_positional);
    }

    #[test]
    fn parse_str_accepts_aliases_and_rejects_unknown() {
        assert_eq!(Lang::parse_str("EN"), Some(Lang::En));
        assert_eq!(Lang::parse_str(" ko "), Some(Lang::Ko));
        assert_eq!(Lang::parse_str("korean"), Some(Lang::Ko));
        assert_eq!(Lang::parse_str("fr"), None);
    }

    #[test]
    fn resolve_priority_flag_over_config_over_default() {
        // flag 최우선.
        assert_eq!(resolve(Some(Lang::Ko), Some("en")), Lang::Ko);
        // flag 없으면 config.
        assert_eq!(resolve(None, Some("ko")), Lang::Ko);
        // 둘 다 없으면 기본 En.
        assert_eq!(resolve(None, None), Lang::En);
        // config 값이 이상하면 기본 En.
        assert_eq!(resolve(None, Some("zzz")), Lang::En);
    }

    #[test]
    fn resolve_from_toml_reads_output_section() {
        let toml = "[output]\nlanguage = \"ko\"\n[profiles.demo]\n";
        assert_eq!(resolve_from_toml(None, Some(toml)), Lang::Ko);
        // flag가 config를 이긴다.
        assert_eq!(resolve_from_toml(Some(Lang::En), Some(toml)), Lang::En);
        // 섹션 없으면 기본 En.
        assert_eq!(resolve_from_toml(None, Some("[profiles.demo]\n")), Lang::En);
        assert_eq!(resolve_from_toml(None, None), Lang::En);
    }
}
