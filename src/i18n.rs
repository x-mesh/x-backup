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

use std::sync::OnceLock;

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

    /// config 문자열 등에서 언어를 파싱한다(대소문자 무시). 인식 못 하면 `None`.
    pub fn parse_str(s: &str) -> Option<Lang> {
        match s.trim().to_ascii_lowercase().as_str() {
            "en" | "english" => Some(Lang::En),
            "ko" | "kr" | "korean" => Some(Lang::Ko),
            _ => None,
        }
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
/// [`OnceLock`]이라 한 번만 정해지고, 정해지기 전에 읽으면 기본값 `En`이다. 테스트는
/// 프로세스를 공유하므로 [`active`]에 의존하는 단정을 쓰지 않는다 — 순수 함수인
/// [`Lang::sel`]에 언어를 직접 넘겨 검증한다.
static ACTIVE: OnceLock<Lang> = OnceLock::new();

/// 전역 출력 언어를 정한다. 첫 호출만 반영되고 이후 호출은 조용히 무시된다.
pub fn set_active(lang: Lang) {
    let _ = ACTIVE.set(lang);
}

/// 전역 출력 언어. 아직 정해지지 않았으면 기본 `En`.
pub fn active() -> Lang {
    ACTIVE.get().copied().unwrap_or_default()
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sel_picks_by_language() {
        assert_eq!(Lang::En.sel("size", "크기"), "size");
        assert_eq!(Lang::Ko.sel("size", "크기"), "크기");
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
