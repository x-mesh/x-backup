//! 잡 인자 검증 — 웹에서 들어온 문자열이 자식 argv로 나가기 전에 통과해야 하는 관문.
//!
//! ## 이 파일이 지키는 것
//! [`super::spec::JobSpec`]의 빌더는 **사용자 입력을 값 위치에만** 놓는다(플래그 이름은
//! 전부 이 크레이트 안의 문자열 리터럴이다 — [`super::spec`] 헤더 참조). 그 "값 위치"에
//! 무엇이 들어갈 수 있는지를 정하는 것이 이 파일이고, 여기 있는 함수가 웹 입력이 argv에
//! 닿는 **유일한** 경로다.
//!
//! 모든 검증은 **화이트리스트**다 — "위험한 문자를 지운다"가 아니라 "허용한 문자 외에는
//! 전부 거부한다". 블랙리스트는 새 위험 문자가 알려질 때마다 조용히 뚫리고, 이 값들은
//! 파일 경로·프로세스 인자로 흘러가므로 뚫리면 되돌릴 수 없다.
//!
//! ## 왜 프로파일명이 가장 엄격한가
//! 프로파일명은 argv에만 쓰이는 게 아니다. 자식이 [`crate::lock`]으로 파일 락을 잡을 때
//! **락 파일 이름의 일부**(`$XDG_RUNTIME_DIR/x-backup/<profile>.lock`)가 되고, 상태·로그
//! 경로에도 들어간다. 즉 프로파일명은 경로 조각이며, 경로 탈출(`../`)·숨김 파일(`.`)·
//! 디렉터리 구분자가 그대로 통하면 임의 경로에 락 파일을 만들거나 남의 락을 밟을 수 있다.
//! 그래서 [`ProfileName`]은 ASCII 영숫자 + `_` + `-`만 받고, **첫 글자는 영숫자여야** 한다.
//! `.`이 애초에 허용 문자에 없으므로 `.`·`..`·`.hidden`은 전부 이 규칙 하나로 함께 막힌다.
//!
//! 유니코드는 통째로 거부한다. 시각적으로 같아 보이는 다른 코드포인트(호모글리프 —
//! 예: 키릴 `а` U+0430 vs 라틴 `a` U+0061)가 있으면 "운영자가 화면에서 본 프로파일"과
//! "실제로 락이 걸리는 프로파일"이 갈라질 수 있다. 정규화(NFKC)로 접는 방법도 있지만,
//! 정규화 규칙 자체가 유니코드 버전에 따라 바뀌므로 판정이 시간에 따라 흔들린다 —
//! ASCII만 받으면 그 흔들림이 원천적으로 없다.
//!
//! ## 옵션 주입을 왜 "`--` 구분자"가 아니라 "거부"로 막는가
//! 값이 `-`로 시작하면 **무조건 거부**한다(`--force`, `-f`, `--config /etc/passwd` 등).
//! `--` 구분자를 쓰지 않는 이유는 세 가지다:
//!
//! 1. **`--`는 이 문제를 풀지 못한다.** `--`는 "이 뒤는 전부 위치 인자"라는 뜻이다.
//!    우리가 만드는 argv는 전부 `--flag <VALUE>` 쌍이므로, `--at -- --force`처럼 값 앞에
//!    `--`를 끼우면 파싱이 아예 깨진다. 값이 선행 플래그에 속하는 구조에서는 `--`로
//!    보호할 자리가 없다.
//! 2. **거부는 downstream 파서에 의존하지 않는다.** clap은 기본값(`allow_hyphen_values`
//!    off)에서 `-`로 시작하는 값을 플래그로 보고 사용법 오류를 낸다 — 즉 지금은 조용히
//!    주입되지 않는다. 하지만 그건 clap의 *기본값*이고, 우리 안전 성질이 남의 기본값에
//!    걸려 있으면 언젠가 바뀐다. 웹 경계에서 거부하면 그 성질이 우리 코드 안에서 닫힌다.
//! 3. **진단이 정확해진다.** 경계에서 거부하면 운영자는 "값이 `-`로 시작할 수 없다"는
//!    문장을 즉시 받는다. clap까지 흘려보내면 exit 2와 함께 자식의 사용법 오류만 남아,
//!    무엇이 잘못됐는지 화면에서 되짚어야 한다.
//!
//! 덧붙여, 아래 어떤 규칙도 `-`를 첫 글자로 허용하지 않으므로 이 거부는 정상 입력을
//! 하나도 잃지 않는다(공짜로 얻는 방어다).
//!
//! ## 의미 판정은 하지 않는다
//! 형태만 본다. "그 백업 ID가 존재하는가", "그 시점이 oplog 범위 안인가", "그 프로파일이
//! config에 있는가"는 **전부 CLI가 판정한다**. 웹이 같은 판정을 다시 구현하면 두 구현이
//! 갈라지는 순간 웹이 CLI와 다르게 동작한다 — [`crate::web`] 모듈 헤더의 최상위 불변식이
//! 정확히 그것을 금지한다. 그래서 [`validate_timestamp`]는 RFC3339 *형태*만 확인하고
//! 원본 문자열을 그대로 넘긴다(정규화조차 하지 않는다 — 정규화는 이미 판정의 일부다).

//! ## 오류 문구는 ko/en 둘 다 낸다 — 그래서 `lang`을 받는다
//! 이 파일의 메시지는 **화면에 그대로 렌더된다**(폼 제출이 400으로 되돌아올 때). 그래서
//! [`crate::i18n`] 규약이 그대로 적용된다: 라벨·기술용어는 영문, 설명은 ko/en. 검증 함수가
//! 순수 함수인데도 [`Lang`]을 인자로 받는 이유가 그것이다 — 문구를 만드는 자리가 곧
//! 언어를 알아야 하는 자리다.
//!
//! 조사는 [`crate::i18n::GA`]로 고른다. 라벨을 문장에 끼우는 구조라 조사를 고정으로 박으면
//! 라벨이 바뀔 때 문장이 깨진다(실제로 `"백업 ID이 비어 있습니다"`가 화면에 나가고 있었다).

use crate::error::{Result, XBackupError};
use crate::i18n::{Lang, GA};

/// 프로파일명 최대 길이(바이트).
///
/// 락 파일 경로 조각이 되므로 대부분 파일시스템의 파일명 상한(255바이트)에 `.lock`
/// 접미사를 더해도 넉넉히 남는 값으로 잡는다. 사람이 고르는 이름이 64자를 넘을 이유가
/// 없고, 상한이 없으면 긴 이름으로 경로 길이 상한(PATH_MAX)을 건드릴 수 있다.
pub const MAX_PROFILE_NAME_LEN: usize = 64;

/// 프로파일명이 아닌 일반 값의 최대 길이(바이트).
///
/// 네임스페이스·백업 ID·시각 문자열은 모두 이보다 훨씬 짧다. 상한의 목적은 정상 입력을
/// 자르는 게 아니라, 검증을 통과한 거대한 문자열이 argv 총 길이 상한(`E2BIG`)을
/// 건드려 spawn 자체를 실패시키는 경로를 막는 것이다.
pub const MAX_VALUE_LEN: usize = 256;

/// 검증 대상 필드의 이름 — 두 언어 표기를 함께 들고 다닌다.
///
/// 라벨을 `&str` 하나로 넘기면 호출부마다 `lang.sel(...)`을 되풀이해야 하고, 그중 하나를
/// 빠뜨리면 **한쪽 언어에서만 문장이 깨진다**(테스트가 두 언어를 다 보지 않는 한 조용히
/// 통과한다). 짝을 타입으로 묶으면 빠뜨릴 자리가 없다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field {
    /// 영문 표기 — 기술용어이므로 그대로 라벨이 된다.
    en: &'static str,
    /// 한국어 표기.
    ko: &'static str,
}

impl Field {
    /// 프로파일 이름.
    pub const PROFILE: Self = Self {
        en: "profile name",
        ko: "프로파일 이름",
    };
    /// 데이터베이스 이름.
    pub const DB: Self = Self {
        en: "database name",
        ko: "DB 이름",
    };
    /// 컬렉션/테이블 이름.
    pub const COLLECTION: Self = Self {
        en: "collection name",
        ko: "컬렉션 이름",
    };
    /// 네임스페이스(`db.collection`).
    pub const NAMESPACE: Self = Self {
        en: "namespace",
        ko: "네임스페이스",
    };
    /// 백업 ID.
    pub const BACKUP_ID: Self = Self {
        en: "backup ID",
        ko: "백업 ID",
    };
    /// destination 참조.
    pub const DESTINATION: Self = Self {
        en: "destination name",
        ko: "destination 이름",
    };
    /// PITR 목표 시각.
    pub const TIMESTAMP: Self = Self {
        en: "timestamp",
        ko: "시각",
    };

    /// 이 언어에서의 표기.
    fn label(self, lang: Lang) -> &'static str {
        lang.sel(self.en, self.ko)
    }

    /// 한국어 표기 뒤에 붙일 주격 조사(`이`/`가`).
    fn ga(self) -> &'static str {
        GA.after(self.ko)
    }
}

/// 검증을 통과한 프로파일 이름.
///
/// 이 타입의 값이 존재한다는 것은 [`ProfileName::parse`]를 통과했다는 뜻이다(필드가
/// 비공개이므로 다른 생성 경로가 없다). 프로파일명이 경로 조각이 된다는 사실 때문에
/// [`String`]과 구분되는 별도 타입으로 둔다 — 검증되지 않은 문자열이 실수로 같은 자리에
/// 들어가면 컴파일이 되지 않는다.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ProfileName(String);

impl ProfileName {
    /// 프로파일명을 검증해 접는다. 규칙과 근거는 모듈 헤더 "왜 프로파일명이 가장
    /// 엄격한가" 참조.
    ///
    /// 앞뒤 공백은 잘라내지 **않는다** — `" prod"`와 `"prod"`가 같은 프로파일로 접히면,
    /// 감사 로그에 남은 문자열과 실제로 락이 걸린 이름이 달라진다. 공백이 섞였다면
    /// 입력 경로(폼·URL)에 문제가 있는 것이므로 거부해서 드러내는 편이 낫다.
    pub fn parse(raw: &str, lang: Lang) -> Result<Self> {
        ensure_common(raw, Field::PROFILE, lang, MAX_PROFILE_NAME_LEN)?;
        ensure_charset(raw, Field::PROFILE, lang, is_profile_char, CHARSET_ALNUM)?;
        ensure_alnum_start(raw, Field::PROFILE, lang)?;
        Ok(Self(raw.to_string()))
    }

    /// 검증된 이름 문자열.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ProfileName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 데이터베이스 이름(`--db`)을 검증한다.
///
/// MongoDB가 DB 이름에 금지하는 문자(`/\. "$*<>:|?`)와 PostgreSQL 식별자 관행을 함께
/// 만족하는 교집합만 허용한다 — 엔진별로 다른 규칙을 웹이 판정하면 엔진이 늘 때마다
/// 이 파일이 갈라진다. `.`을 막는 것은 특히 중요하다: `--db a.b`를 허용하면 운영자가
/// 네임스페이스를 DB 자리에 넣은 것을 자식이 조용히 "그런 이름의 DB"로 받아들인다.
pub fn validate_db_name(raw: &str, lang: Lang) -> Result<String> {
    ensure_common(raw, Field::DB, lang, MAX_VALUE_LEN)?;
    ensure_charset(raw, Field::DB, lang, is_identifier_char, CHARSET_ALNUM)?;
    ensure_alnum_start(raw, Field::DB, lang)?;
    Ok(raw.to_string())
}

/// 컬렉션/테이블 이름(`--collection`)을 검증한다.
///
/// DB 이름과 달리 `.`을 허용한다 — MongoDB 컬렉션 이름에는 점이 들어갈 수 있고
/// (`system.profile` 같은 관행적 이름), 그런 컬렉션을 웹에서 다룰 수 없게 만들 이유가
/// 없다. 단 첫 글자는 영숫자여야 하므로 `.`·`..`은 여기서도 통과하지 못한다.
/// `$`는 허용하지 않는다 — MongoDB의 내부/시스템 네임스페이스 표기라서 웹 콘솔이
/// 다룰 대상이 아니다.
pub fn validate_collection_name(raw: &str, lang: Lang) -> Result<String> {
    ensure_common(raw, Field::COLLECTION, lang, MAX_VALUE_LEN)?;
    ensure_charset(
        raw,
        Field::COLLECTION,
        lang,
        is_collection_char,
        CHARSET_ALNUM_DOT,
    )?;
    ensure_alnum_start(raw, Field::COLLECTION, lang)?;
    Ok(raw.to_string())
}

/// 네임스페이스(`db.collection`)를 검증한다 — `restore --only`, `peek --ns`.
///
/// **첫** `.`에서 자른다. MongoDB DB 이름에는 점이 들어갈 수 없고 컬렉션 이름에는 들어갈
/// 수 있으므로, 첫 점이 유일하게 모호하지 않은 경계다(마지막 점에서 자르면
/// `app.system.profile`이 DB `app.system`으로 잘못 접힌다).
pub fn validate_namespace(raw: &str, lang: Lang) -> Result<String> {
    ensure_common(raw, Field::NAMESPACE, lang, MAX_VALUE_LEN)?;
    let (db, coll) = raw.split_once('.').ok_or_else(|| {
        usage(lang.sel_string(
            format!("namespace '{raw}' has no `.` — write it as `<db>.<collection>`."),
            format!(
                "네임스페이스 '{raw}'에 `.`이 없습니다 — `<db>.<collection>` 형태로 지정하세요."
            ),
        ))
    })?;
    validate_db_name(db, lang)?;
    validate_collection_name(coll, lang)?;
    Ok(raw.to_string())
}

/// 백업 ID(`--id`)를 검증한다.
///
/// 백업 ID는 UUID v7 문자열이다([`crate::manifest`]). 그 형태를 완전히 파싱하지 않고
/// 16진수 + `-`만 허용하는 느슨한 화이트리스트를 쓴다 — "이 ID가 실제로 존재하는
/// 백업인가"는 카탈로그를 아는 CLI만 판정할 수 있고(모듈 헤더 "의미 판정은 하지
/// 않는다"), 여기서 UUID 문법까지 이중 구현하면 ID 표기가 바뀔 때 웹이 먼저 막는다.
/// 안전에 필요한 것은 "경로·플래그로 해석될 문자가 없다"이고, 그건 이 화이트리스트로
/// 이미 닫힌다.
pub fn validate_backup_id(raw: &str, lang: Lang) -> Result<String> {
    ensure_common(raw, Field::BACKUP_ID, lang, MAX_VALUE_LEN)?;
    ensure_charset(raw, Field::BACKUP_ID, lang, is_backup_id_char, CHARSET_HEX)?;
    ensure_hex_start(raw, Field::BACKUP_ID, lang)?;
    Ok(raw.to_string())
}

/// destination 참조(`restore --from`)를 검증한다.
///
/// 이름(`--from cold-s3`) 또는 `type#idx`(`--from s3#1`) 두 표기를 받으므로 `#`을
/// 허용한다([`crate::config::file::DestinationConfig::label`]가 만드는 표기와 같은
/// 어휘다).
pub fn validate_destination_ref(raw: &str, lang: Lang) -> Result<String> {
    ensure_common(raw, Field::DESTINATION, lang, MAX_VALUE_LEN)?;
    ensure_charset(
        raw,
        Field::DESTINATION,
        lang,
        is_destination_char,
        CHARSET_ALNUM_HASH,
    )?;
    ensure_alnum_start(raw, Field::DESTINATION, lang)?;
    Ok(raw.to_string())
}

/// PITR 목표 시점(`--at`)의 **형태만** 검증한다.
///
/// RFC3339로 파싱되는지만 보고 원본 문자열을 그대로 돌려준다. "그 시점이 oplog 보존
/// 범위 안인가", "체인에 gap이 없는가"는 백업 카탈로그를 아는 CLI의 판정이다 — 모듈
/// 헤더 "의미 판정은 하지 않는다" 참조.
///
/// 정규화(예: `+09:00` → UTC)도 하지 않는다. 정규화한 값을 넘기면 감사 로그에 남는
/// 문자열과 운영자가 입력한 문자열이 달라지고, 시각 해석이라는 판정을 웹이 한 번 더
/// 하게 된다. 형태 검증에 [`chrono`]를 쓰는 것은 자식이 쓰는 것과 같은 파서라 두 판정이
/// 갈라질 여지가 없기 때문이다.
pub fn validate_timestamp(raw: &str, lang: Lang) -> Result<String> {
    ensure_common(raw, Field::TIMESTAMP, lang, MAX_VALUE_LEN)?;
    // 문자 집합을 먼저 좁힌다 — chrono가 받아들이는 형태 안에도 argv로 내보내기 전에
    // 확인해 두고 싶은 문자(공백 등)가 섞일 수 있고, 화이트리스트가 통과 조건을 우리
    // 코드 안에 명시적으로 남긴다.
    ensure_charset(
        raw,
        Field::TIMESTAMP,
        lang,
        is_timestamp_char,
        CHARSET_TIMESTAMP,
    )?;
    ensure_digit_start(raw, Field::TIMESTAMP, lang)?;
    chrono::DateTime::parse_from_rfc3339(raw).map_err(|e| {
        usage(lang.sel_string(
            format!(
                "timestamp '{raw}' is not RFC3339 ({e}) — write it like \
                 `2026-07-25T13:00:00Z`."
            ),
            format!(
                "시각 '{raw}'이 RFC3339 형식이 아닙니다({e}) — `2026-07-25T13:00:00Z`처럼 \
                 지정하세요."
            ),
        ))
    })?;
    Ok(raw.to_string())
}

// ---------------------------------------------------------------------------
// 공통 검사 — 모든 값이 반드시 통과한다.
// ---------------------------------------------------------------------------

/// 허용 문자 집합의 사람이 읽는 표기 — [`Field`]와 같은 이유로 두 언어를 함께 든다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Charset {
    en: &'static str,
    ko: &'static str,
}

impl Charset {
    fn label(self, lang: Lang) -> &'static str {
        lang.sel(self.en, self.ko)
    }
}

/// 영숫자 + `_` + `-`.
const CHARSET_ALNUM: Charset = Charset {
    en: "letters, digits, `_`, `-`",
    ko: "영문/숫자/`_`/`-`",
};
/// 영숫자 + `_` + `-` + `.`(컬렉션 이름).
const CHARSET_ALNUM_DOT: Charset = Charset {
    en: "letters, digits, `_`, `-`, `.`",
    ko: "영문/숫자/`_`/`-`/`.`",
};
/// 영숫자 + `_` + `-` + `#`(destination 참조).
const CHARSET_ALNUM_HASH: Charset = Charset {
    en: "letters, digits, `_`, `-`, `#`",
    ko: "영문/숫자/`_`/`-`/`#`",
};
/// 16진수와 `-`(백업 ID).
const CHARSET_HEX: Charset = Charset {
    en: "hex digits and `-`",
    ko: "16진수와 `-`",
};
/// RFC3339 시각에 쓰이는 문자.
const CHARSET_TIMESTAMP: Charset = Charset {
    en: "digits and `-` `:` `.` `+` `T` `Z`",
    ko: "숫자와 `-` `:` `.` `+` `T` `Z`",
};

/// 검증 실패를 사용법 오류로 접는다 — 이 파일의 모든 거부가 같은 종류(exit 2)다.
fn usage(message: String) -> XBackupError {
    XBackupError::Usage(message)
}

/// 빈 값·길이 초과·널 바이트·선행 `-`를 거부한다.
///
/// 널 바이트와 선행 `-`는 아래 문자 집합 검사로도 어차피 걸린다. 그런데도 여기서 따로
/// 보는 이유는 두 가지다: (1) 에러 메시지가 "허용되지 않은 문자가 있습니다"가 아니라
/// 원인을 지목하게 되고, (2) 두 위협(C 문자열 절단, 옵션 주입)이 이 파일에 명시적으로
/// 기록된다 — 문자 집합을 넓히는 미래의 변경이 이 두 방어를 조용히 없애지 못한다.
fn ensure_common(raw: &str, field: Field, lang: Lang, max_len: usize) -> Result<()> {
    let (label, ga) = (field.label(lang), field.ga());
    if raw.is_empty() {
        return Err(usage(lang.sel_string(
            format!("{label} is empty."),
            format!("{label}{ga} 비어 있습니다."),
        )));
    }
    if raw.len() > max_len {
        let len = raw.len();
        return Err(usage(lang.sel_string(
            format!("{label} is too long ({len} bytes) — it must be at most {max_len} bytes."),
            format!("{label}{ga} 너무 깁니다({len}바이트) — {max_len}바이트 이하여야 합니다."),
        )));
    }
    // 널 바이트: 운영체제의 exec 경계는 C 문자열이라 널에서 잘린다. Rust 문자열은 널을
    // 담을 수 있으므로, 검증을 통과한 값이 자식에게는 잘린 형태로 도착하는 불일치가
    // 생긴다(우리가 검증한 것과 자식이 받는 것이 다르다). tokio/std가 이 경우를
    // `InvalidInput`으로 거부하지만, 그러면 spawn 실패라는 뭉갠 결과만 남는다.
    if raw.contains('\0') {
        return Err(usage(lang.sel_string(
            format!("{label} contains a null byte — it cannot be passed as a process argument."),
            format!("{label}에 널 바이트가 있습니다 — 프로세스 인자에 담을 수 없습니다."),
        )));
    }
    // 옵션 주입: 모듈 헤더 "옵션 주입을 왜 ..." 참조.
    if raw.starts_with('-') {
        return Err(usage(lang.sel_string(
            format!(
                "{label} '{raw}' starts with `-` — rejected because the value could be read \
                 as a flag."
            ),
            format!(
                "{label} '{raw}'{ga} `-`로 시작합니다 — 값이 플래그로 해석될 수 있어 \
                 거부합니다."
            ),
        )));
    }
    Ok(())
}

/// 허용 문자 집합을 벗어난 첫 글자를 지목해 거부한다.
fn ensure_charset(
    raw: &str,
    field: Field,
    lang: Lang,
    allowed: fn(char) -> bool,
    charset: Charset,
) -> Result<()> {
    if let Some(bad) = raw.chars().find(|c| !allowed(*c)) {
        let label = field.label(lang);
        let bad = describe_char(bad);
        let allowed_desc = charset.label(lang);
        return Err(usage(lang.sel_string(
            format!(
                "{label} '{raw}' contains a disallowed character {bad} — only {allowed_desc} \
                 are allowed (ASCII)."
            ),
            format!(
                "{label} '{raw}'에 허용되지 않은 문자 {bad}{} 있습니다 — {allowed_desc}만 \
                 쓸 수 있습니다(ASCII).",
                GA.after(&bad)
            ),
        )));
    }
    Ok(())
}

/// 첫 글자가 ASCII 영숫자인지 확인한다.
///
/// 문자 집합 검사만으로는 `-`·`_`·`.`·`#`으로 시작하는 값이 통과한다. 첫 글자를 영숫자로
/// 제한하면 세 가지가 한 번에 닫힌다: 옵션처럼 보이는 값, `.`/`..`(경로 탈출),
/// `.hidden`(숨김 파일).
fn ensure_alnum_start(raw: &str, field: Field, lang: Lang) -> Result<()> {
    let first = raw.chars().next().unwrap_or('\0');
    if !first.is_ascii_alphanumeric() {
        let label = field.label(lang);
        return Err(usage(lang.sel_string(
            format!(
                "{label} '{raw}' does not start with a letter or digit — it must, so that names \
                 cannot be read as a path or an option."
            ),
            format!(
                "{label} '{raw}'의 첫 글자가 영문/숫자가 아닙니다 — 경로·옵션으로 해석될 수 \
                 있는 이름을 막기 위해 영문 또는 숫자로 시작해야 합니다."
            ),
        )));
    }
    Ok(())
}

/// 첫 글자가 16진수 숫자인지 확인한다(백업 ID 전용).
fn ensure_hex_start(raw: &str, field: Field, lang: Lang) -> Result<()> {
    let first = raw.chars().next().unwrap_or('\0');
    if !first.is_ascii_hexdigit() {
        let label = field.label(lang);
        return Err(usage(lang.sel_string(
            format!("{label} '{raw}' does not start with a hex digit — a backup ID is a UUID."),
            format!("{label} '{raw}'의 첫 글자가 16진수가 아닙니다 — 백업 ID는 UUID 형태입니다."),
        )));
    }
    Ok(())
}

/// 첫 글자가 숫자인지 확인한다(시각 전용 — RFC3339는 4자리 연도로 시작한다).
fn ensure_digit_start(raw: &str, field: Field, lang: Lang) -> Result<()> {
    let first = raw.chars().next().unwrap_or('\0');
    if !first.is_ascii_digit() {
        let label = field.label(lang);
        return Err(usage(lang.sel_string(
            format!(
                "{label} '{raw}' does not start with a digit — RFC3339 begins with a 4-digit year."
            ),
            format!(
                "{label} '{raw}'의 첫 글자가 숫자가 아닙니다 — RFC3339는 4자리 연도로 \
                 시작합니다."
            ),
        )));
    }
    Ok(())
}

/// 거부된 문자를 사람이 읽을 수 있게 표기한다.
///
/// 제어 문자·비-ASCII를 그대로 에러 메시지에 넣으면 터미널에 보이지 않거나(제어 문자)
/// 호모글리프라 구분이 안 된다(유니코드) — 코드포인트를 함께 적어 무엇이 걸렸는지
/// 운영자가 실제로 알 수 있게 한다.
fn describe_char(c: char) -> String {
    if c.is_ascii_graphic() {
        format!("'{c}'(U+{:04X})", c as u32)
    } else {
        format!("U+{:04X}", c as u32)
    }
}

/// 프로파일명 허용 문자 — ASCII 영숫자 + `_` + `-`.
///
/// ASCII 대문자를 함께 허용한다. 프로파일명은 운영자가 config에 직접 쓴 이름이고,
/// `Prod` 같은 이름을 웹에서만 거부하면 "CLI로는 되는데 콘솔로는 안 되는 프로파일"이
/// 생긴다 — [`crate::web`] 모듈 헤더의 "웹과 CLI가 다르게 동작할 수 없다"는 불변식을
/// 검증 규칙이 스스로 깨는 셈이다. **한계:** 대소문자를 구분하지 않는 파일시스템
/// (macOS 기본 APFS)에서는 `Prod`와 `prod`가 같은 락 파일로 접힌다 — 서로 다른 두
/// 프로파일을 대소문자만 다르게 두면 의도보다 넓게 상호 배제된다(안전한 방향의
/// 실패라 허용한다. 반대 방향, 즉 같은 프로파일이 다른 락을 잡는 일은 없다).
fn is_profile_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// DB 이름 허용 문자 — 프로파일명과 같은 집합(경로가 되지는 않지만 이유 없이 넓힐
/// 필요가 없다).
fn is_identifier_char(c: char) -> bool {
    is_profile_char(c)
}

/// 컬렉션 이름 허용 문자 — 식별자 + `.`.
fn is_collection_char(c: char) -> bool {
    is_identifier_char(c) || c == '.'
}

/// destination 참조 허용 문자 — 식별자 + `#`(`type#idx` 표기).
fn is_destination_char(c: char) -> bool {
    is_identifier_char(c) || c == '#'
}

/// 백업 ID 허용 문자 — 16진수 + `-`(UUID 표기).
fn is_backup_id_char(c: char) -> bool {
    c.is_ascii_hexdigit() || c == '-'
}

/// RFC3339 시각 표기에 나타날 수 있는 문자.
fn is_timestamp_char(c: char) -> bool {
    c.is_ascii_digit() || matches!(c, '-' | ':' | '.' | '+' | 'T' | 'Z' | 't' | 'z')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (라벨, 검증 함수) 쌍. 라벨은 어느 규칙이 뚫렸는지 실패 메시지에 남기기 위한 것이다.
    type NamedValidator = (&'static str, fn(&str) -> Result<String>);

    /// 모든 검증 함수를 한 번에 돌리는 목록 — "어떤 값 위치에 넣어도 거부되어야 한다"를
    /// 검사하는 테스트가 규칙을 하나씩 빠뜨리지 않게 한다.
    ///
    /// 시각(`validate_timestamp`)은 형태가 완전히 달라(숫자로 시작하는 RFC3339) 여기
    /// 포함하지 않는다 — 별도 테스트가 담당한다.
    fn identifier_validators() -> Vec<NamedValidator> {
        vec![
            ("profile", |s| ProfileName::parse(s, Lang::En).map(|p| p.0)),
            ("db", |s| validate_db_name(s, Lang::En)),
            ("collection", |s| validate_collection_name(s, Lang::En)),
            ("backup_id", |s| validate_backup_id(s, Lang::En)),
            ("destination", |s| validate_destination_ref(s, Lang::En)),
        ]
    }

    /// 이 파일이 아는 모든 필드 — 아래 i18n 정합 테스트가 하나도 빠뜨리지 않게 한다.
    fn all_fields() -> Vec<Field> {
        vec![
            Field::PROFILE,
            Field::DB,
            Field::COLLECTION,
            Field::NAMESPACE,
            Field::BACKUP_ID,
            Field::DESTINATION,
            Field::TIMESTAMP,
        ]
    }

    /// **모든 필드가 두 언어를 다 갖는다**(R42).
    ///
    /// 한쪽을 다른 쪽에 복사해 두는 실수(`ko: "profile name"`)를 잡는다 — 그렇게 되면
    /// 한국어 화면에 영문이 섞여도 컴파일은 통과하고 아무도 모른다.
    #[test]
    fn every_field_has_both_languages() {
        for f in all_fields() {
            assert!(!f.en.is_empty() && !f.ko.is_empty(), "{f:?}: 빈 표기");
            assert_ne!(
                f.en, f.ko,
                "{f:?}: 두 언어 표기가 같다 — 한쪽을 복사해 둔 것이다"
            );
            assert!(
                f.en.is_ascii(),
                "{f:?}: 영문 표기에 비 ASCII가 있다 — 라벨은 영문 고정이다"
            );
        }
    }

    /// 문자 집합 표기도 마찬가지다.
    #[test]
    fn every_charset_has_both_languages() {
        for c in [
            CHARSET_ALNUM,
            CHARSET_ALNUM_DOT,
            CHARSET_ALNUM_HASH,
            CHARSET_HEX,
            CHARSET_TIMESTAMP,
        ] {
            assert!(!c.en.is_empty() && !c.ko.is_empty(), "{c:?}: 빈 표기");
            assert_ne!(c.en, c.ko, "{c:?}: 두 언어 표기가 같다");
        }
    }

    /// **한국어 문장에 조사가 맞게 붙는다.**
    ///
    /// 라벨을 문장에 끼우는 구조라 조사를 고정으로 박으면 라벨이 바뀔 때 깨진다 —
    /// 실제로 `"백업 ID이 비어 있습니다"`가 화면에 나가고 있었다.
    #[test]
    fn korean_messages_use_the_right_particle() {
        let ko = |field: Field| {
            ensure_common("", field, Lang::Ko, MAX_VALUE_LEN)
                .expect_err("빈 값은 거부되어야 함")
                .to_string()
        };
        assert!(
            ko(Field::PROFILE).contains("프로파일 이름이 비어"),
            "{}",
            ko(Field::PROFILE)
        );
        assert!(
            ko(Field::NAMESPACE).contains("네임스페이스가 비어"),
            "{}",
            ko(Field::NAMESPACE)
        );
        assert!(
            ko(Field::BACKUP_ID).contains("백업 ID가 비어"),
            "{}",
            ko(Field::BACKUP_ID)
        );
        assert!(
            ko(Field::TIMESTAMP).contains("시각이 비어"),
            "{}",
            ko(Field::TIMESTAMP)
        );
    }

    /// **거부 사유는 언어와 무관하게 같다** — 영문 화면에서만 통과하는 값이 있으면 안 된다.
    #[test]
    fn the_verdict_never_depends_on_the_language() {
        let inputs = [
            "",
            "-x",
            "a\0b",
            "../etc",
            "프로파일",
            "ok-name",
            &"a".repeat(999),
        ];
        for input in inputs {
            assert_eq!(
                ProfileName::parse(input, Lang::En).is_ok(),
                ProfileName::parse(input, Lang::Ko).is_ok(),
                "'{input}'의 판정이 언어에 따라 갈렸다 — 언어는 문구만 정해야 한다"
            );
        }
    }

    // ---- 정상 입력 ----

    /// 관행적인 프로파일명은 통과한다(대문자·숫자·`_`·`-` 포함).
    #[test]
    fn profile_name_accepts_conventional_names() {
        for name in [
            "p",
            "prod",
            "nightly-full",
            "pg_ops",
            "Prod",
            "db2",
            "a1-b2_c3",
        ] {
            let parsed = ProfileName::parse(name, Lang::En)
                .unwrap_or_else(|e| panic!("'{name}'은 통과해야 함: {e}"));
            assert_eq!(parsed.as_str(), name, "값이 변형되면 안 됨");
        }
    }

    /// 상한 길이 자체는 통과하고, 한 글자 넘으면 거부된다(경계값).
    #[test]
    fn profile_name_length_boundary() {
        let at_limit = "a".repeat(MAX_PROFILE_NAME_LEN);
        assert!(
            ProfileName::parse(&at_limit, Lang::En).is_ok(),
            "상한값은 통과해야 함"
        );
        let over = "a".repeat(MAX_PROFILE_NAME_LEN + 1);
        // 두 언어 모두 길이를 원인으로 지목한다(R42 — 설명은 ko/en 둘 다 존재한다).
        let en = ProfileName::parse(&over, Lang::En).expect_err("상한 초과는 거부되어야 함");
        assert!(en.to_string().contains("too long"), "{en}");
        let ko = ProfileName::parse(&over, Lang::Ko).expect_err("상한 초과는 거부되어야 함");
        assert!(ko.to_string().contains("너무 깁니다"), "{ko}");
    }

    /// 매우 긴 이름(경로 길이 상한을 노리는 입력)은 거부된다.
    #[test]
    fn profile_name_rejects_very_long_name() {
        let huge = "a".repeat(10_000);
        assert!(ProfileName::parse(&huge, Lang::En).is_err());
    }

    // ---- 프로파일명: 경로 탈출 ----

    /// 경로 구분자·`.`·`..`는 전부 거부된다 — 프로파일명이 락 파일 경로 조각이기 때문.
    #[test]
    fn profile_name_rejects_path_traversal() {
        let cases = [
            "..",
            ".",
            "../../etc/passwd",
            "..%2fetc",
            "a/b",
            "a\\b",
            "/abs",
            "./rel",
            ".hidden",
            "p.lock",
            "p\0.lock",
            "C:\\temp",
        ];
        for input in cases {
            let err = ProfileName::parse(input, Lang::En)
                .err()
                .unwrap_or_else(|| panic!("'{input}'은 거부되어야 함(경로 탈출 표면)"));
            assert_eq!(
                err.exit_code(),
                crate::error::exit_codes::USAGE,
                "'{input}'은 사용법 오류(exit 2)여야 함: {err}"
            );
        }
    }

    /// 빈 문자열·공백만 있는 값은 거부된다(공백을 잘라 접지도 않는다).
    #[test]
    fn profile_name_rejects_empty_and_whitespace() {
        for input in ["", " ", "\t", "\n", "  prod", "prod  ", "pro d"] {
            assert!(
                ProfileName::parse(input, Lang::En).is_err(),
                "'{input}'은 거부되어야 함"
            );
        }
    }

    /// 유니코드 호모글리프는 거부된다 — 화면에서 같아 보이는 다른 프로파일이
    /// 만들어지는 것을 막는다.
    #[test]
    fn profile_name_rejects_unicode_homoglyphs() {
        let cases = [
            "рrod",         // 키릴 р(U+0440) + rod
            "prоd",         // 키릴 о(U+043E)
            "ｐrod",        // 전각 ｐ(U+FF50)
            "prod\u{200b}", // 폭 없는 공백(U+200B) — 눈에 보이지 않는다
            "prod\u{00a0}", // NBSP
            "ⅰncr",         // 로마 숫자 ⅰ(U+2170)
            "한글프로파일",
        ];
        for input in cases {
            let err = ProfileName::parse(input, Lang::En)
                .err()
                .unwrap_or_else(|| panic!("'{input}'(호모글리프)은 거부되어야 함"));
            // 메시지에 코드포인트가 보여야 운영자가 무엇이 걸렸는지 알 수 있다.
            assert!(
                err.to_string().contains("U+"),
                "코드포인트 안내 누락: {err}"
            );
        }
    }

    // ---- 셸 메타문자 ----

    /// 셸 메타문자는 모든 값 위치에서 거부된다.
    ///
    /// 자식은 셸을 경유하지 않으므로(`super::super::runner` 헤더) 이 문자들이 통과해도
    /// 명령이 되지는 않는다. 그래도 거부하는 이유: 이 값들은 argv에만 쓰이는 게 아니라
    /// 락/상태 파일 경로와 감사 로그에도 들어가고, 자식이 다시 띄우는 외부 도구
    /// (mongodump 등)의 인자로도 흘러간다 — "셸이 없으니 괜찮다"는 가정이 한 계층만
    /// 깨져도 무방비가 된다.
    #[test]
    fn all_validators_reject_shell_metacharacters() {
        let hostile = [
            "a;rm -rf /",
            "a$(whoami)",
            "a`whoami`",
            "a&&b",
            "a||b",
            "a|b",
            "a>b",
            "a<b",
            "a\nb",
            "a\rb",
            "a b",
            "a'b",
            "a\"b",
            "a*",
            "a?",
            "a{b}",
            "a[b]",
            "a~b",
            "a!b",
            "$HOME",
            "${HOME}",
        ];
        for (label, validate) in identifier_validators() {
            for input in hostile {
                assert!(
                    validate(input).is_err(),
                    "{label} 검증이 '{input}'을 통과시켰다"
                );
            }
        }
    }

    /// 널 바이트는 모든 값 위치에서 거부되고, 메시지가 원인을 지목한다.
    #[test]
    fn all_validators_reject_null_bytes() {
        for (label, validate) in identifier_validators() {
            for input in ["a\0b", "\0", "prod\0", "\0prod", "abc\0--force"] {
                let err = validate(input)
                    .err()
                    .unwrap_or_else(|| panic!("{label}이 널 바이트 '{input}'을 통과시켰다"));
                if input.starts_with('\0') {
                    continue; // 첫 글자 규칙이 먼저 잡을 수 있다 — 거부됐으면 충분.
                }
                assert!(
                    err.to_string().contains("null byte"),
                    "{label}: 널 바이트를 지목하지 않았다: {err}"
                );
            }
        }
        // 시각 검증도 같은 방어를 갖는다.
        assert!(validate_timestamp("2026-07-25T00:00:00Z\0", Lang::En).is_err());
    }

    // ---- 옵션 주입 ----

    /// 플래그처럼 보이는 값은 모든 값 위치에서 거부된다.
    ///
    /// 목록에는 실재하는 파괴적 플래그(`--force`, `--drop`)와 실재하지 않는 이름
    /// (`--allow-overwrite`, `--drop-target`)을 함께 넣는다 — 방어가 "알려진 플래그
    /// 목록"이 아니라 "`-`로 시작하는 모든 값"이라는 것을 고정하기 위함이다. 알려진
    /// 목록에 의존하면 CLI에 새 플래그가 생길 때마다 이 방어가 뒤처진다.
    #[test]
    fn all_validators_reject_option_injection() {
        let injections = [
            "--force",
            "--allow-overwrite",
            "--drop-target",
            "--drop",
            "-f",
            "-v",
            "--config /etc/passwd",
            "--config=/etc/passwd",
            "--json",
            "--profile",
            "--at",
            "-",
            "--",
            "---",
        ];
        for (label, validate) in identifier_validators() {
            for input in injections {
                let err = validate(input)
                    .err()
                    .unwrap_or_else(|| panic!("{label}이 옵션 주입 '{input}'을 통과시켰다"));
                assert_eq!(err.exit_code(), crate::error::exit_codes::USAGE);
            }
        }
        for input in injections {
            assert!(
                validate_timestamp(input, Lang::En).is_err(),
                "시각 검증이 '{input}'을 통과시켰다"
            );
        }
    }

    /// `-`로 시작하는 값의 에러 메시지는 "플래그로 해석될 수 있다"는 이유를 밝힌다 —
    /// 운영자가 화면에서 원인을 바로 알 수 있어야 한다.
    #[test]
    fn leading_dash_error_explains_reason() {
        let en = ProfileName::parse("--force", Lang::En).expect_err("거부되어야 함");
        assert!(
            en.to_string().contains("read as a flag"),
            "이유가 메시지에 없다: {en}"
        );
        let ko = ProfileName::parse("--force", Lang::Ko).expect_err("거부되어야 함");
        assert!(
            ko.to_string().contains("플래그로 해석"),
            "이유가 메시지에 없다: {ko}"
        );
    }

    // ---- 네임스페이스 ----

    /// `db.collection` 형태는 통과하고, 컬렉션 쪽 점은 유지된다.
    #[test]
    fn namespace_accepts_db_dot_collection() {
        assert_eq!(
            validate_namespace("app.users", Lang::En).unwrap(),
            "app.users"
        );
        // 첫 점에서 자르므로 컬렉션 이름에 점이 남아도 통과한다.
        assert_eq!(
            validate_namespace("app.system.profile", Lang::En).unwrap(),
            "app.system.profile"
        );
    }

    /// 점이 없거나 한쪽이 비면 거부된다.
    #[test]
    fn namespace_rejects_malformed() {
        for input in ["app", "app.", ".users", ".", "..", "app..users", "a/b.c"] {
            assert!(
                validate_namespace(input, Lang::En).is_err(),
                "'{input}'은 거부되어야 함"
            );
        }
    }

    // ---- 백업 ID ----

    /// UUID v7 표기는 통과한다.
    #[test]
    fn backup_id_accepts_uuid_v7_text() {
        let id = uuid::Uuid::now_v7().to_string();
        assert_eq!(validate_backup_id(&id, Lang::En).unwrap(), id);
    }

    /// 16진수 밖 문자는 거부된다(경로·플래그 문자를 포함해).
    #[test]
    fn backup_id_rejects_non_hex() {
        for input in [
            "../x",
            "zzzz",
            "id_with_underscore",
            "id.with.dot",
            "g0000000",
        ] {
            assert!(
                validate_backup_id(input, Lang::En).is_err(),
                "'{input}'은 거부되어야 함"
            );
        }
    }

    // ---- destination 참조 ----

    /// 이름과 `type#idx` 표기를 모두 받는다.
    #[test]
    fn destination_ref_accepts_name_and_type_index() {
        assert_eq!(
            validate_destination_ref("cold-s3", Lang::En).unwrap(),
            "cold-s3"
        );
        assert_eq!(validate_destination_ref("s3#1", Lang::En).unwrap(), "s3#1");
    }

    // ---- 시각 ----

    /// RFC3339 표기는 원본 그대로 통과한다(정규화하지 않는다).
    #[test]
    fn timestamp_accepts_rfc3339_without_normalizing() {
        for input in [
            "2026-07-25T13:00:00Z",
            "2026-07-25T13:00:00.123Z",
            "2026-07-25T22:00:00+09:00",
            "2026-07-25T13:00:00-05:00",
        ] {
            assert_eq!(
                validate_timestamp(input, Lang::En).unwrap(),
                input,
                "시각 문자열이 변형되면 감사 로그와 입력이 어긋난다"
            );
        }
    }

    /// RFC3339가 아닌 표기는 거부되고, 메시지가 올바른 예를 보여준다.
    #[test]
    fn timestamp_rejects_non_rfc3339() {
        for input in [
            "2026-07-25",           // 날짜만
            "13:00:00",             // 시각만
            "2026-07-25 13:00:00",  // 공백 구분자(RFC3339는 T)
            "now",                  // 자연어
            "2026-13-45T99:99:99Z", // 범위 밖
            "1753448400",           // epoch 초
        ] {
            assert!(
                validate_timestamp(input, Lang::En).is_err(),
                "'{input}'은 거부되어야 함"
            );
        }
        // 문자 집합은 통과하지만 RFC3339가 아닌 입력(날짜만)은 형태 검사에서 걸리고,
        // 메시지가 올바른 예를 보여준다. `now` 같은 자연어는 그 앞 단계(문자 집합)에서
        // 이미 걸리므로 메시지가 다르다 — 두 방어선이 각자 다른 이유를 말한다.
        let err = validate_timestamp("2026-07-25", Lang::En).expect_err("거부되어야 함");
        assert!(err.to_string().contains("RFC3339"), "형태 안내 누락: {err}");
        assert!(err.to_string().contains("2026-07-25T"), "예시 누락: {err}");
    }

    /// 모든 검증 실패는 exit 2(Usage) 계열이다 — 웹 라우트가 400으로 접을 수 있게
    /// 종류를 하나로 고정한다.
    #[test]
    fn all_validation_failures_are_usage_errors() {
        let failures: Vec<XBackupError> = vec![
            ProfileName::parse("../x", Lang::En).unwrap_err(),
            validate_db_name("a b", Lang::En).unwrap_err(),
            validate_collection_name("--force", Lang::En).unwrap_err(),
            validate_namespace("nodot", Lang::En).unwrap_err(),
            validate_backup_id("zz", Lang::En).unwrap_err(),
            validate_destination_ref("a;b", Lang::En).unwrap_err(),
            validate_timestamp("now", Lang::En).unwrap_err(),
        ];
        for err in failures {
            assert_eq!(
                err.exit_code(),
                crate::error::exit_codes::USAGE,
                "검증 실패가 exit 2가 아니다: {err}"
            );
        }
    }
}
