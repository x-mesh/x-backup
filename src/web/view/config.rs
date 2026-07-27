//! `/config` 화면의 **표시 모델과 마크업** — 프로파일 CRUD 폼.
//!
//! ## 왜 표시 모델이 뷰 계층에 있는가
//! 이 화면의 가장 중요한 성질은 "시크릿 *값*이 응답에 없다"는 것이고, 그 성질은 마스킹이
//! 아니라 **타입에 값 필드를 두지 않는 것**으로 지켜야 한다(`src/web/mask.rs` 헤더의 1차
//! 방어). 그 타입이 렌더 경로 바깥(라우트)에 있으면 "화면에 넣을 수 있는 것"과 "화면에
//! 넣으면 안 되는 것"의 경계가 두 파일로 갈라진다. 그래서 화면이 받아들일 수 있는 모양
//! ([`SecretEnvRef`]·[`RedactedUri`]·[`FieldView`])을 이 파일이 정의하고,
//! [`crate::web::routes::config`]는 `crate::config`가 준 값을 **이 모양으로 접어서만**
//! 넘긴다. 접는 함수가 하나뿐이면 그 함수만 보면 누출 여부를 판정할 수 있다.
//!
//! ## 이 화면의 함정 두 개 — 마크업이 직접 막는다
//!
//! ### 1. 원본 문법(v1/v2)을 조용히 바꾸지 않는다
//! x-backup config는 두 문법을 모두 읽는다: v1(복수 `[profiles.p.source]` 중첩)과
//! v2(단수 `[profile.p]` flat + `[defaults]`/`[base.*]`/`extends`). 폼이 저장할 때
//! 한쪽으로 통일해 버리면 사용자는 값 하나만 고쳤는데 파일 전체가 다른 문법으로 다시
//! 써진다. 그래서 화면은 [`ConfigSyntax`]를 **눈에 보이게** 표시한다 — 운영자가 "내
//! 파일은 v1이다"를 알고 있어야 저장 결과를 검증할 수 있다.
//!
//! ### 2. 상속받은 값을 프로파일에 박아 넣지 않는다
//! v2에서 `[defaults]`/`extends`로 한 번만 정의한 공통 정책이, 폼이 "펼쳐진 최종 값"을
//! 각 프로파일 입력에 미리 채워 넣고 그대로 제출받으면 **모든 프로파일에 복제되어** 공통
//! 정책 한 곳을 고치는 일이 영구히 불가능해진다. 이 파일이 그걸 막는 방법은 UI 규약이다:
//!
//! | 값의 출처([`Origin`]) | 입력 칸 | 빈 입력의 뜻 |
//! |---|---|---|
//! | [`Origin::Direct`] | 현재 값을 미리 채운다 | 그 키를 **지운다** |
//! | [`Origin::Defaults`]·[`Origin::Inherited`]·[`Origin::Builtin`] | **비워 둔다**(실효값은 옆에 텍스트로) | 상속/기본값을 **유지**한다 |
//!
//! 즉 상속된 값은 애초에 입력 칸에 들어가지 않으므로, 아무것도 타이핑하지 않은 제출이
//! 상속을 끊을 수 없다. 상속을 끊는 것은 **타이핑이라는 명시적 행위**뿐이다.
//!
//! ## 시크릿 — 마스킹이 아니라 부재
//! [`SecretEnvRef`]는 env 변수 **이름**과 "그 env가 실제로 설정돼 있는가"(불리언)만 담고,
//! 값을 담을 필드가 아예 없다. [`RedactedUri`]는 생성 경로가
//! [`RedactedUri::from_raw`] 하나뿐이고 그 함수가 [`crate::cli::output::redact_uri`]로
//! userinfo·민감 쿼리를 제거하므로, 이 타입의 값에는 자격증명이 들어갈 수 없다.
//! 두 타입 모두 값 접근자(`value()`류)를 제공하지 않는다 — 제공하지 않는 것이 요점이다.
//!
//! ## 모양은 담지 않는다
//! [`components`](super::components) 헤더 규칙을 그대로 따른다 — 색·인라인 스타일 리터럴을
//! 두지 않고, 상태는 `data-level`, 출처는 `data-origin` 의미 토큰으로만 싣는다. 출처는
//! 배지에 **글자로도** 적는다([`Origin::label`]) — `app.css`에 `[data-origin="…"]` 규칙이
//! 아직 없어도(그 파일은 이 태스크 소관 밖이다) 상속/직접 구분이 화면에서 읽힌다.

use maud::{html, Markup};

use super::components::{self, Level};
use crate::i18n::Lang;
use crate::web::routes::config as route;

/// 원본 config가 어떤 문법으로 적혀 있는가.
///
/// 판별 규칙은 [`crate::config::v2::normalize_v2`]와 같다 — 루트에 `profiles`(복수)가
/// 있으면 v1, `profile`/`base`/`defaults`(단수)가 있으면 v2, 둘 다면 에러(그 에러는
/// 로더가 낸다), 둘 다 없으면 [`ConfigSyntax::Empty`].
///
/// 화면이 이걸 표시하는 이유는 모듈 헤더 "함정 1" 참조.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSyntax {
    /// v1 — 복수 `[profiles.<name>.<...>]` 중첩 트리.
    V1,
    /// v2 — 단수 `[profile.<name>]` flat + `[defaults]`/`[base.<name>]`/`extends`.
    V2,
    /// 프로파일 섹션이 없다(빈 config 또는 ENV-only).
    Empty,
}

impl ConfigSyntax {
    /// `data-syntax` 속성 토큰. 라벨과 달리 CSS가 아는 어휘다.
    pub fn token(self) -> &'static str {
        match self {
            ConfigSyntax::V1 => "v1",
            ConfigSyntax::V2 => "v2",
            ConfigSyntax::Empty => "empty",
        }
    }

    /// 배지 글자. 기술용어이므로 영문 고정([`crate::i18n`] 규약).
    pub fn label(self) -> &'static str {
        match self {
            ConfigSyntax::V1 => "v1 nested",
            ConfigSyntax::V2 => "v2 flat",
            ConfigSyntax::Empty => "no profiles",
        }
    }

    /// 설명 문장 — 저장이 무엇을 보존해야 하는지 운영자에게 알린다.
    pub fn explain(self, lang: Lang) -> &'static str {
        match self {
            ConfigSyntax::V1 => lang.sel(
                "This file uses the v1 nested syntax. Saving preserves that syntax — it will not be rewritten as v2.",
                "이 파일은 v1 중첩 문법입니다. 저장은 그 문법을 보존합니다 — v2로 다시 쓰지 않습니다.",
            ),
            ConfigSyntax::V2 => lang.sel(
                "This file uses the v2 flat syntax with inheritance. Values coming from [defaults] or extends stay where they are defined.",
                "이 파일은 상속을 쓰는 v2 flat 문법입니다. [defaults]·extends에서 온 값은 정의된 자리에 그대로 남습니다.",
            ),
            ConfigSyntax::Empty => lang.sel(
                "This config declares no profiles yet.",
                "이 config에는 아직 프로파일이 없습니다.",
            ),
        }
    }
}

/// 실효값이 **어디서 왔는가**.
///
/// 이 열거형이 이 화면의 핵심이다 — 모듈 헤더 "함정 2"의 표가 곧 이 값에 대한 분기다.
/// 저장(t27)·형식 보존(t28)도 이 값을 근거로 "프로파일에 이 키를 써야 하는가"를 판단한다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// 이 프로파일 테이블에 **직접** 적혀 있다.
    Direct,
    /// `[defaults]`에서 왔다(모든 프로파일에 걸리는 공통 정책).
    Defaults,
    /// `extends`가 가리키는 `[base.<name>]` 또는 다른 `[profile.<name>]`에서 왔다.
    /// `from`은 그 키를 **실제로 적어 둔** 엔티티 이름이다(중간 경유지가 아니라 최종 출처).
    Inherited {
        /// 값을 적어 둔 base/profile 이름.
        from: String,
    },
    /// config에 아예 없고 내장 기본값이 채웠다(예: `compress_level = 10`).
    ///
    /// [`Defaults`](Origin::Defaults)와 반드시 구분해야 한다 — 전자는 파일 안 `[defaults]`
    /// 섹션이고 이건 코드 안 상수다. 운영자가 "고치려면 어디를 봐야 하는가"가 완전히 다르다.
    Builtin,
}

impl Origin {
    /// `data-origin` 속성 토큰 — CSS가 아는 유일한 어휘.
    pub fn token(&self) -> &'static str {
        match self {
            Origin::Direct => "direct",
            Origin::Defaults => "defaults",
            Origin::Inherited { .. } => "inherited",
            Origin::Builtin => "builtin",
        }
    }

    /// 배지에 찍히는 글자. 상속의 경우 **어디서 왔는지**까지 적는다 — "상속됨"만 적으면
    /// 운영자가 고칠 곳을 찾으려고 파일 전체를 훑어야 한다.
    pub fn label(&self) -> String {
        match self {
            Origin::Direct => "direct".to_string(),
            Origin::Defaults => "defaults".to_string(),
            Origin::Inherited { from } => format!("extends: {from}"),
            Origin::Builtin => "builtin".to_string(),
        }
    }

    /// 이 프로파일에 직접 적힌 값인지. 입력 칸을 미리 채울지 가르는 유일한 기준이다.
    pub fn is_direct(&self) -> bool {
        matches!(self, Origin::Direct)
    }

    /// 빈 입력이 무슨 뜻인지 한 문장으로. 폼의 각 칸 아래에 붙는다.
    pub fn empty_means(&self, lang: Lang) -> &'static str {
        match self {
            Origin::Direct => lang.sel(
                "Clearing this box removes the key from the profile.",
                "이 칸을 비우면 프로파일에서 이 키를 지웁니다.",
            ),
            Origin::Defaults => lang.sel(
                "Leave empty to keep inheriting from [defaults].",
                "비워 두면 [defaults] 상속을 유지합니다.",
            ),
            Origin::Inherited { .. } => lang.sel(
                "Leave empty to keep inheriting through extends.",
                "비워 두면 extends 상속을 유지합니다.",
            ),
            Origin::Builtin => lang.sel(
                "Leave empty to keep the built-in default.",
                "비워 두면 내장 기본값을 유지합니다.",
            ),
        }
    }

    /// `<select>`의 빈 선택지에 붙는 짧은 라벨.
    ///
    /// [`Builtin`](Origin::Builtin)을 "상속 유지"로 뭉개지 않는다 — 내장 기본값은 파일 안
    /// `[defaults]` 섹션이 아니라 코드 안 상수이고, 운영자가 고칠 곳이 완전히 다르다.
    pub fn empty_option_label(&self, lang: Lang) -> &'static str {
        match self {
            Origin::Direct => lang.sel("(remove this key)", "(이 키를 지움)"),
            Origin::Defaults => lang.sel("(keep [defaults])", "([defaults] 유지)"),
            Origin::Inherited { .. } => lang.sel("(keep inherited)", "(상속 유지)"),
            Origin::Builtin => lang.sel("(keep built-in default)", "(내장 기본값 유지)"),
        }
    }
}

/// 시크릿 env 참조 — **이름과 설정 여부만** 담는다.
///
/// 값 필드가 없다는 것이 이 타입의 존재 이유다(모듈 헤더 "시크릿" 참조). `present`는
/// 값을 읽어서 보관한 결과가 아니라 "설정돼 있는가"라는 판정 결과 하나다 — 라우트가
/// [`std::env::var_os`]로 존재 여부만 확인해 넘긴다.
///
/// `Debug`를 derive해도 안전하다(담고 있는 것이 변수 이름과 불리언뿐이다). 오히려
/// derive하는 편이 낫다 — 나중에 값 필드를 추가하려는 변경이 이 doc과 충돌해 리뷰에서
/// 드러난다.
///
/// 값 접근자가 없다는 성질을 doctest로 고정한다(`src/web/mask.rs`의 `ApiView`가 같은
/// 방식으로 "필드 부재"를 잠근다) — 누군가 `value()`를 붙이면 이 doctest가 컴파일되면서
/// 깨지고 CI에서 잡힌다.
///
/// ```compile_fail
/// use x_backup::web::view::config::SecretEnvRef;
/// let reference = SecretEnvRef::new("uri_env", "MONGO_URI", true);
/// // 이 타입에는 시크릿 값을 내주는 접근자가 없다 — 이 줄은 컴파일되지 않는다.
/// let _leak = reference.value();
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretEnvRef {
    /// config에 적힌 환경변수 이름(예: `MONGO_URI`).
    name: String,
    /// 그 이름의 env가 이 서버 프로세스에 설정돼 있고 비어 있지 않은가.
    present: bool,
    /// 어느 config 키가 이 이름을 가리키는가(`uri_env`/`read_uri_env`/`s3_creds`).
    key: &'static str,
}

impl SecretEnvRef {
    /// env 이름과 설정 여부로 만든다. **값은 인자로 받지 않는다.**
    pub fn new(key: &'static str, name: impl Into<String>, present: bool) -> Self {
        Self {
            name: name.into(),
            present,
            key,
        }
    }

    /// 환경변수 이름.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 그 env가 실제로 설정돼 있는지. 운영자가 "왜 안 되나"를 화면에서 알아야 한다 —
    /// 이름은 맞는데 env가 없는 상황이 가장 흔한 실패 원인이다.
    pub fn is_present(&self) -> bool {
        self.present
    }

    /// 이 이름을 가리키는 config 키.
    pub fn key(&self) -> &'static str {
        self.key
    }

    /// 설정 여부의 화면 레벨. 미설정은 [`Level::Warn`]이다 — 차단성 실패인지는
    /// `doctor`가 판정할 일이고(이 화면은 config 편집기다), 조용히 초록으로 칠하면 안 된다.
    fn level(&self) -> Level {
        if self.present {
            Level::Ok
        } else {
            Level::Warn
        }
    }
}

/// 화면에 실을 수 있는 URI 표기.
///
/// 생성 경로가 [`RedactedUri::from_raw`] 하나뿐이고 그 함수가 userinfo·민감 쿼리를
/// 제거하므로, 이 타입의 값에 자격증명이 들어갈 수 없다. 원문을 돌려주는 접근자는 없다.
///
/// ## 왜 `uri`를 아예 안 보여주지 않는가
/// config의 `uri`는 설계상 자격증명을 담지 않는 필드다(`src/config/file.rs`의 `uri` doc은
/// "비밀번호가 포함된 URI는 여기 쓰지 말 것"이라고 못박는다). 하지만 운영자가 실제로
/// 그렇게 쓴 파일이 존재할 수 있고, 그 경우 화면이 "여기에 자격증명이 들어 있다"고
/// 말해주지 않으면 운영자는 고칠 이유를 모른다. 그래서 **가린 표기 + 경고**를 함께 낸다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactedUri {
    /// userinfo·민감 쿼리가 제거된 표기.
    shown: String,
    /// 원문에 자격증명(userinfo)이 있었는가.
    had_credentials: bool,
}

impl RedactedUri {
    /// 원문 URI를 가린 표기로 접는다. **원문은 이 함수 밖으로 나가지 않는다.**
    pub fn from_raw(raw: &str) -> Self {
        let shown = crate::cli::output::redact_uri(raw);
        // `redact_uri`는 userinfo가 있으면 `***@host` 형태를 만든다 — 그 흔적으로 판정하지
        // 않고 원문을 직접 본다(가린 표기의 모양에 판정을 걸면 그 함수가 바뀔 때 조용히
        // 어긋난다). authority 구간에 `@`가 있으면 자격증명이 있었다는 뜻이다.
        let authority = raw.split_once("://").map_or(raw, |(_, rest)| rest);
        let authority = &authority[..authority.find(['/', '?']).unwrap_or(authority.len())];
        Self {
            shown,
            had_credentials: authority.contains('@'),
        }
    }

    /// 화면에 그대로 실을 수 있는 표기.
    pub fn shown(&self) -> &str {
        &self.shown
    }

    /// 원문에 자격증명이 있었는지(운영자에게 `uri_env`로 옮기라고 알리는 근거).
    pub fn had_credentials(&self) -> bool {
        self.had_credentials
    }
}

/// 폼 필드를 묶는 섹션. 라벨은 영문 고정.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldGroup {
    /// 접속 대상(`[profile.x]`의 `uri`·`uri_env`·…).
    Source,
    /// 동작 모드(`engine`·`backup_type`·…).
    Mode,
    /// 백업 위치(`dest`·`s3_*`).
    Destination,
    /// 압축.
    Compression,
    /// 암호화.
    Encryption,
    /// 증분.
    Incremental,
    /// 보존 정책.
    Retention,
}

impl FieldGroup {
    /// 섹션 제목(영문 라벨 고정).
    pub fn label(self) -> &'static str {
        match self {
            FieldGroup::Source => "source",
            FieldGroup::Mode => "mode",
            FieldGroup::Destination => "destination",
            FieldGroup::Compression => "compression",
            FieldGroup::Encryption => "encryption",
            FieldGroup::Incremental => "incremental",
            FieldGroup::Retention => "retention",
        }
    }

    /// 렌더 순서. 배열로 두면 필드 목록이 늘어도 화면 순서가 한 곳에서만 바뀐다.
    pub const ORDER: [FieldGroup; 7] = [
        FieldGroup::Source,
        FieldGroup::Mode,
        FieldGroup::Destination,
        FieldGroup::Compression,
        FieldGroup::Encryption,
        FieldGroup::Incremental,
        FieldGroup::Retention,
    ];
}

/// 입력 칸의 종류 — 렌더 모양과 검증 규칙을 함께 가른다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    /// 자유 텍스트(경로·알고리즘 이름·간격 표기).
    Text,
    /// 정수.
    Number,
    /// 참/거짓.
    ///
    /// 체크박스를 쓰지 않는다 — HTML 체크박스는 꺼져 있으면 **아예 제출되지 않아서**
    /// "명시적 false"와 "상속 유지"를 구분할 수 없다. 그 구분이 이 화면의 핵심이므로
    /// (모듈 헤더 "함정 2") 빈 값을 표현할 수 있는 `<select>`로 그린다.
    Bool,
    /// 정해진 값 중 하나.
    Choice(&'static [&'static str]),
    /// 환경변수 **이름**(값이 아니다).
    EnvName,
    /// destination compact 표기(`local:/path` | `s3:bucket/prefix`).
    Dest,
    /// 자격증명이 섞일 수 있는 URI.
    ///
    /// **절대 미리 채우지 않는다.** 가린 표기를 미리 채우면 아무것도 고치지 않은 제출이
    /// 원본 URI를 가린 문자열로 덮어써 접속 정보를 파괴한다. 그래서 이 종류만 빈 입력이
    /// "변경 없음"이고, 지우려면 별도 체크박스(`<key>_clear`)를 요구한다.
    Uri,
}

/// 편집 가능한 필드 하나의 화면 모델.
#[derive(Debug, Clone)]
pub struct FieldView {
    /// 폼 input 이름 = v2 flat 키. 폼이 config 문법을 그대로 가르치도록 같은 어휘를 쓴다.
    pub key: &'static str,
    /// 소속 섹션.
    pub group: FieldGroup,
    /// 입력 종류.
    pub kind: FieldKind,
    /// 실효값의 출처.
    pub origin: Origin,
    /// 실효값의 표시 문자열. `None`이면 "설정 없음".
    ///
    /// [`FieldKind::Uri`]에서는 **이미 가려진** 표기가 들어온다(라우트가
    /// [`RedactedUri`]로 접어 넘긴다).
    pub effective: Option<String>,
    /// 이 필드에 붙는 경고 문장(예: `uri`에 자격증명이 섞여 있다).
    pub warning: Option<String>,
}

impl FieldView {
    /// 입력 칸에 미리 채울 값.
    ///
    /// 이 함수가 모듈 헤더 "함정 2" 표의 구현체다 — 직접 적힌 값만 채우고, 상속·기본값·
    /// URI는 비워 둔다.
    pub fn prefill(&self) -> Option<&str> {
        if self.kind == FieldKind::Uri || !self.origin.is_direct() {
            return None;
        }
        self.effective.as_deref()
    }
}

/// destination 하나의 표시 요약(읽기 전용 표에 쓴다).
#[derive(Debug, Clone)]
pub struct DestinationView {
    /// 보고·선택용 표시 이름(`name` 또는 `type` + 색인).
    pub label: String,
    /// 백엔드 유형(`local`/`s3`) 또는 미설정.
    pub kind: Option<String>,
    /// 위치 — local 경로 또는 `bucket/prefix`.
    pub location: Option<String>,
    /// S3 리전.
    pub region: Option<String>,
    /// S3 엔드포인트.
    pub endpoint: Option<String>,
    /// S3 자격증명 env 참조(값 없음 — [`SecretEnvRef`]).
    pub credentials: Option<SecretEnvRef>,
}

/// 프로파일 하나의 화면 모델.
#[derive(Debug, Clone)]
pub struct ProfileView {
    /// 프로파일 이름(config에 적힌 그대로).
    pub name: String,
    /// 이 이름이 웹에서 편집 가능한지 — [`crate::web::job::args::ProfileName`] 규칙을
    /// 통과하지 못하는 이름은 링크·폼 대상이 되지 못한다.
    pub editable: bool,
    /// 편집 불가 이유(있으면 화면에 그대로 표시한다).
    pub name_error: Option<String>,
    /// destination이 없는 endpoint 전용 프로파일인지(복구 대상 전용 — 백업 잡이 아니다).
    pub endpoint_only: bool,
    /// 시크릿 env 참조 목록(값 없음).
    pub secrets: Vec<SecretEnvRef>,
    /// destination 전부(표시 전용).
    pub destinations: Vec<DestinationView>,
    /// destination을 폼에서 편집할 수 있는지.
    ///
    /// 편집을 막는 이유는 전부 "폼이 조용히 사본을 떨어뜨릴 수 있다"로 모인다 — 여러
    /// destination을 통째로 다시 쓰거나, compact 표기로 접히지 않는 destination에
    /// 미리 채울 올바른 값이 없는 경우. 판정과 이유 문장은 라우트가 만든다.
    pub destination_editable: bool,
    /// 편집할 수 없는 이유(있으면 화면에 그대로 표시한다).
    pub destination_note: Option<String>,
    /// 편집 필드 전부(그룹 순서와 무관하게 담기고, 렌더가 그룹으로 묶는다).
    pub fields: Vec<FieldView>,
}

impl ProfileView {
    /// 상속·기본값에서 온 필드 개수 — 목록·폼 상단 요약에 쓴다.
    pub fn inherited_count(&self) -> usize {
        self.fields
            .iter()
            .filter(|f| !f.origin.is_direct() && f.effective.is_some())
            .count()
    }

    /// 이 프로파일에 직접 적힌 필드 개수.
    pub fn direct_count(&self) -> usize {
        self.fields.iter().filter(|f| f.origin.is_direct()).count()
    }

    /// 설정되지 않은 시크릿 env가 하나라도 있는지.
    pub fn has_missing_secret(&self) -> bool {
        self.secrets.iter().any(|s| !s.is_present())
    }

    /// 키로 필드를 찾는다.
    pub fn field(&self, key: &str) -> Option<&FieldView> {
        self.fields.iter().find(|f| f.key == key)
    }

    /// 목록 요약에 쓰는 실효값 — 없으면 `None`.
    fn effective_of(&self, key: &str) -> Option<&str> {
        self.field(key)?.effective.as_deref()
    }
}

/// v1→v2 전환 미리보기의 화면 모델.
///
/// **여기 담기는 문자열은 전부 이미 가려진 값이다.** 전환은 config 파일 텍스트를 그대로
/// 보여주는 유일한 화면이고, v1 파일에는 평문 `uri`에 자격증명이 들어 있을 수 있다
/// (`src/config/file.rs`의 `uri` doc은 금지하지만 그렇게 쓴 파일이 실제로 존재한다). 그래서
/// [`crate::web::routes::config`]가 [`RedactedUri`]와 시크릿 레지스트리를 통과시킨 뒤에만 이
/// 모양으로 접는다 — 계획 쪽 타입([`crate::web::config_write::KeyMove`])은 원본 값을 들고
/// 있고, 이 타입은 가려진 문자열만 들고 있다. 타입이 갈라져 있으므로 마스킹을 건너뛴 값이
/// 화면 모델에 들어갈 자리가 없다.
#[derive(Debug, Clone)]
pub struct ConvertPreview {
    /// 원본 파일 지문 — 확인 폼이 그대로 되돌려 보낸다(미리보기를 봤다는 증표).
    pub digest: String,
    /// 프로파일별 키 이동 계획.
    pub profiles: Vec<ConvertedProfile>,
    /// 저장될 v2 텍스트(가려진 표기).
    pub new_text: String,
    /// 화면에서 잘린 줄 수(0이면 전부 보여준다).
    pub truncated_lines: usize,
    /// 원본 바이트 수.
    pub bytes_before: usize,
    /// 결과 바이트 수.
    pub bytes_after: usize,
}

/// 전환되는 프로파일 하나.
#[derive(Debug, Clone)]
pub struct ConvertedProfile {
    /// 프로파일 이름.
    pub name: String,
    /// 이 프로파일에서 옮겨지는 키들.
    pub moves: Vec<MovedKey>,
}

/// v1 nested 경로 → v2 flat 키 한 건(값은 가려진 표기).
#[derive(Debug, Clone)]
pub struct MovedKey {
    /// v1 nested 경로(compact로 접히면 여러 개가 쉼표로 이어진다).
    pub from: String,
    /// v2 flat 키.
    pub to: String,
    /// 옮겨지는 값(가려진 표기).
    pub value: String,
}

/// 폼이 생성용인지 수정용인지.
///
/// **이름 변경은 지원하지 않는다.** 프로파일명은 락 파일·상태 파일 경로 조각이므로
/// (`src/web/job/args.rs` 헤더) 이름을 바꾸는 것은 값 편집이 아니라 이동이다 —
/// 돌고 있는 잡의 락, 쌓인 상태 파일, 감사 로그의 과거 기록이 전부 옛 이름을 가리킨 채
/// 남는다. 이름을 바꾸려면 새로 만들고 지우는 두 단계를 명시적으로 밟는다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormMode {
    /// 새 프로파일.
    Create,
    /// 기존 프로파일 수정(이름 고정).
    Update,
}

// ---------------------------------------------------------------------------
// 렌더 — 전부 순수 함수(파일시스템·env를 건드리지 않는다)
// ---------------------------------------------------------------------------

/// 프로파일 목록 화면.
pub fn list_page(
    lang: Lang,
    syntax: ConfigSyntax,
    default_profile: Option<&str>,
    profiles: &[ProfileView],
) -> Markup {
    let subtitle = lang.sel(
        "Profiles are read from the config file. Secret values never appear here — only the environment variable names that hold them.",
        "프로파일은 config 파일에서 읽습니다. 시크릿 값은 여기 나오지 않습니다 — 값을 담은 환경변수 이름만 표시합니다.",
    );
    html! {
        (components::page_head(route::CONFIG_TITLE, Some(subtitle)))
        (syntax_banner(lang, syntax))
        (components::meta_list(&[
            ("profiles", profiles.len().to_string()),
            ("default_profile", default_profile.unwrap_or("(unset)").to_string()),
            ("syntax", syntax.label().to_string()),
        ]))
        @for profile in profiles {
            (profile_summary_panel(lang, profile))
        }
        @if profiles.is_empty() {
            (components::notice(Level::Warn, lang.sel("No profiles", "프로파일 없음"), html! {
                p { (lang.sel(
                    "This config has no profiles to show. Create one below.",
                    "이 config에는 표시할 프로파일이 없습니다. 아래에서 하나 만드세요.",
                )) }
            }))
        }
        (conversion_panel(lang, syntax))
        (components::panel(
            html! { h3 class="panel__title" { (lang.sel("New profile", "새 프로파일")) } },
            html! {
                p class="muted" {
                    (lang.sel(
                        "A new profile needs a name and a source — either a plain uri or the name of an environment variable that holds it (uri_env).",
                        "새 프로파일에는 이름과 소스가 필요합니다 — 평문 uri 또는 그 값을 담은 환경변수 이름(uri_env) 중 하나.",
                    ))
                }
                p {
                    a href=(route::NEW_PATH) { (lang.sel("Open the create form", "생성 폼 열기")) }
                }
            },
        ))
    }
}

/// 문법(v1/v2) 배너 — 저장이 무엇을 보존하는지 화면 최상단에서 말한다.
fn syntax_banner(lang: Lang, syntax: ConfigSyntax) -> Markup {
    // 문법 자체는 문제가 아니다 — 정보다. 그래서 Ok 레벨로 그리고, 빈 config만 경고로 둔다
    // (프로파일이 없으면 이 화면에서 할 수 있는 일이 생성뿐이라는 사실을 알려야 한다).
    let level = match syntax {
        ConfigSyntax::Empty => Level::Warn,
        _ => Level::Ok,
    };
    html! {
        section class="verdict" data-level=(level.token()) data-syntax=(syntax.token()) {
            span class="verdict__badge" { (syntax.label()) }
            div class="verdict__text" {
                p class="verdict__headline" { (syntax.explain(lang)) }
            }
        }
    }
}

/// 목록에 들어가는 프로파일 카드 — 요약 + 시크릿·destination 표.
fn profile_summary_panel(lang: Lang, profile: &ProfileView) -> Markup {
    let head = html! {
        h3 class="panel__title mono" { (profile.name) }
        @if profile.endpoint_only {
            span class="tag" { "endpoint-only" }
        }
        span class="counts" {
            (format!(
                "{} direct · {} inherited · {} dest",
                profile.direct_count(),
                profile.inherited_count(),
                profile.destinations.len(),
            ))
        }
        @if profile.has_missing_secret() {
            (components::badge(Level::Warn))
        }
    };
    let body = html! {
        @if let Some(reason) = &profile.name_error {
            (components::notice(Level::Fail, lang.sel("Name not editable here", "여기서 편집할 수 없는 이름"), html! {
                p { (reason) }
                p class="muted" {
                    (lang.sel(
                        "The profile keeps working in the CLI. Rename it in the config file to edit it from the console.",
                        "CLI에서는 그대로 동작합니다. 콘솔에서 편집하려면 config 파일에서 이름을 바꾸세요.",
                    ))
                }
            }))
        }
        (summary_meta(lang, profile))
        (secret_table(lang, &profile.secrets))
        (destination_table(lang, profile))
        @if profile.editable {
            p class="actions" {
                a href=(route::edit_href(&profile.name)) { (lang.sel("Edit", "편집")) }
                " · "
                a href=(route::delete_href(&profile.name)) { (lang.sel("Delete", "삭제")) }
            }
        }
    };
    components::panel(head, body)
}

/// 목록 카드의 요약 메타 — "이 프로파일이 무엇에 붙어 무엇을 하는가"를 한눈에.
///
/// 프로파일이 대여섯 개 있으면 이름만으로는 구분이 안 된다. `uri`는 [`RedactedUri`]가 이미
/// 가린 표기이므로 그대로 실을 수 있다(라우트가 그 타입을 거쳐 넣는다).
fn summary_meta(lang: Lang, profile: &ProfileView) -> Markup {
    let source = match profile.effective_of("uri") {
        Some(uri) => uri.to_string(),
        None => match profile
            .field("uri_env")
            .and_then(|f| f.effective.as_deref())
        {
            Some(env) => format!("(env {env})"),
            None => lang.sel("(unset)", "(미설정)").to_string(),
        },
    };
    components::meta_list(&[
        ("source", source),
        (
            "engine",
            profile.effective_of("engine").unwrap_or("-").to_string(),
        ),
        (
            "encrypt",
            profile.effective_of("encrypt").unwrap_or("-").to_string(),
        ),
        (
            "compress",
            match (
                profile.effective_of("compress_algorithm"),
                profile.effective_of("compress_level"),
            ) {
                (Some(algorithm), Some(level)) => format!("{algorithm}:{level}"),
                (Some(algorithm), None) => algorithm.to_string(),
                _ => "-".to_string(),
            },
        ),
    ])
}

/// 시크릿 env 참조 표 — **이름과 설정 여부만.**
fn secret_table(lang: Lang, secrets: &[SecretEnvRef]) -> Markup {
    html! {
        div class="dtable-scroll" {
            table class="dtable" {
                caption class="muted" {
                    (lang.sel(
                        "Secret references (names only — values are never read into this page)",
                        "시크릿 참조(이름만 — 값은 이 화면으로 읽어 오지 않습니다)",
                    ))
                }
                thead {
                    tr {
                        th scope="col" { "Status" }
                        th scope="col" { "Key" }
                        th scope="col" { "Env name" }
                    }
                }
                tbody {
                    @if secrets.is_empty() {
                        tr {
                            td colspan="3" class="muted" {
                                (lang.sel(
                                    "No secret environment references in this profile.",
                                    "이 프로파일에는 시크릿 환경변수 참조가 없습니다.",
                                ))
                            }
                        }
                    }
                    @for secret in secrets {
                        @let level = secret.level();
                        tr data-level=(level.token()) {
                            td { (components::badge(level)) }
                            td class="mono key" { (secret.key()) }
                            td class="mono" {
                                (secret.name())
                                @if !secret.is_present() {
                                    " "
                                    span class="muted" {
                                        (lang.sel("(not set in this process)", "(이 프로세스에 설정 안 됨)"))
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// destination 표(표시 전용).
fn destination_table(lang: Lang, profile: &ProfileView) -> Markup {
    html! {
        div class="dtable-scroll" {
            table class="dtable" {
                caption class="muted" { (lang.sel("Destinations", "백업 위치")) }
                thead {
                    tr {
                        th scope="col" { "Name" }
                        th scope="col" { "Type" }
                        th scope="col" { "Location" }
                        th scope="col" { "Region" }
                        th scope="col" { "Endpoint" }
                        th scope="col" { "Credentials env" }
                    }
                }
                tbody {
                    @if profile.destinations.is_empty() {
                        tr {
                            td colspan="6" class="muted" {
                                (lang.sel(
                                    "No destination — this profile is a restore/migrate target only.",
                                    "destination 없음 — 복구·이관 대상 전용 프로파일입니다.",
                                ))
                            }
                        }
                    }
                    @for dest in &profile.destinations {
                        tr {
                            td class="mono key" { (dest.label) }
                            td class="mono" { (dest.kind.as_deref().unwrap_or("(unset)")) }
                            td class="mono" { (dest.location.as_deref().unwrap_or("(unset)")) }
                            td class="mono" { (dest.region.as_deref().unwrap_or("-")) }
                            td class="mono" { (dest.endpoint.as_deref().unwrap_or("-")) }
                            td class="mono" {
                                @match &dest.credentials {
                                    Some(secret) => {
                                        (secret.name())
                                        @if !secret.is_present() { " " (components::badge(Level::Warn)) }
                                    }
                                    None => "-",
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// 생성·수정 폼 화면.
pub fn form_page(
    lang: Lang,
    syntax: ConfigSyntax,
    mode: FormMode,
    profile: &ProfileView,
) -> Markup {
    let title = match mode {
        FormMode::Create => lang.sel("Create a profile", "프로파일 생성"),
        FormMode::Update => lang.sel("Edit the profile", "프로파일 편집"),
    };
    html! {
        (components::page_head(route::CONFIG_TITLE, Some(title)))
        (syntax_banner(lang, syntax))
        (inheritance_legend(lang, mode, profile))
        form method="post" action=(route::SAVE_PATH) class="cfg-form" {
            input type="hidden" name=(route::FIELD_OP) value=(match mode {
                FormMode::Create => route::OP_CREATE,
                FormMode::Update => route::OP_UPDATE,
            });
            (name_field(lang, mode, profile))
            @for group in FieldGroup::ORDER {
                @let fields: Vec<&FieldView> = profile.fields.iter().filter(|f| f.group == group).collect();
                @if !fields.is_empty() {
                    (components::panel(
                        html! { h3 class="panel__title mono" { (group.label()) } },
                        html! {
                            @for field in fields { (field_row(lang, field)) }
                        },
                    ))
                }
            }
            @if let Some(note) = &profile.destination_note {
                (components::notice(
                    Level::Warn,
                    lang.sel("Destinations not editable here", "여기서 편집하지 않는 destination"),
                    html! { p { (note) } },
                ))
            }
            p class="actions" {
                button type="submit" { (lang.sel("Validate and save", "검증하고 저장")) }
                " "
                a href=(route::CONFIG_PATH) { (lang.sel("Cancel", "취소")) }
            }
        }
    }
}

/// 상속 규약 안내 — "빈 칸이 무슨 뜻인가"를 폼 위에서 한 번 못박는다.
fn inheritance_legend(lang: Lang, mode: FormMode, profile: &ProfileView) -> Markup {
    let inherited = profile.inherited_count();
    html! {
        (components::panel(
            html! {
                h3 class="panel__title" { (lang.sel("How empty boxes are read", "빈 칸을 어떻게 읽는가")) }
                span class="counts" { (format!("{inherited} inherited")) }
            },
            html! {
                dl class="meta" {
                    dt class="meta__k" { "direct" }
                    dd class="meta__v" {
                        (lang.sel(
                            "written in this profile — the box is pre-filled, and clearing it removes the key.",
                            "이 프로파일에 적혀 있음 — 칸이 미리 채워져 있고, 비우면 키가 지워집니다.",
                        ))
                    }
                    dt class="meta__k" { "defaults / extends / builtin" }
                    dd class="meta__v" {
                        (lang.sel(
                            "inherited — the box is left empty on purpose. Empty keeps the inheritance; typing a value pins an override into this profile.",
                            "상속된 값 — 칸을 의도적으로 비워 둡니다. 비어 있으면 상속을 유지하고, 값을 적으면 이 프로파일에 override가 박힙니다.",
                        ))
                    }
                    dt class="meta__k" { "uri / read_uri" }
                    dd class="meta__v" {
                        (lang.sel(
                            "never pre-filled (the shown value is redacted). Empty means no change; use the clear checkbox to remove it.",
                            "절대 미리 채우지 않습니다(표시값은 가려진 표기입니다). 비어 있으면 변경 없음이고, 지우려면 clear 체크박스를 쓰세요.",
                        ))
                    }
                }
                @if mode == FormMode::Update && inherited > 0 {
                    p class="muted" {
                        (lang.sel(
                            "Values marked defaults or extends are shared policy. Pinning them here means the shared policy no longer reaches this profile.",
                            "defaults·extends로 표시된 값은 공용 정책입니다. 여기에 박아 넣으면 공용 정책이 이 프로파일에 더 이상 닿지 않습니다.",
                        ))
                    }
                }
            },
        ))
    }
}

/// 프로파일 이름 칸 — 생성일 때만 입력이고, 수정일 때는 고정 표시 + hidden.
fn name_field(lang: Lang, mode: FormMode, profile: &ProfileView) -> Markup {
    html! {
        div class="field" {
            label for="f-profile" class="field__label mono" { (route::FIELD_PROFILE) }
            @match mode {
                FormMode::Create => {
                    input type="text" id="f-profile" name=(route::FIELD_PROFILE)
                        value=(profile.name) autocomplete="off" required
                        maxlength=(crate::web::job::args::MAX_PROFILE_NAME_LEN.to_string());
                    p class="field__hint" {
                        (lang.sel(
                            "ASCII letters, digits, underscore and hyphen only, starting with a letter or digit. The name becomes part of lock and state file paths.",
                            "ASCII 영문/숫자/`_`/`-`만 쓰고 영문 또는 숫자로 시작해야 합니다. 이 이름은 락·상태 파일 경로의 일부가 됩니다.",
                        ))
                    }
                }
                FormMode::Update => {
                    p class="field__value mono" { (profile.name) }
                    input type="hidden" name=(route::FIELD_PROFILE) value=(profile.name);
                    p class="field__hint" {
                        (lang.sel(
                            "Renaming is not supported here — a name change moves lock and state files. Create a new profile and delete this one instead.",
                            "이름 변경은 지원하지 않습니다 — 이름을 바꾸면 락·상태 파일이 이동합니다. 새로 만들고 이 프로파일을 지우세요.",
                        ))
                    }
                }
            }
        }
    }
}

/// 필드 한 줄 — 라벨 + 출처 배지 + 실효값 + 입력 칸 + 안내.
fn field_row(lang: Lang, field: &FieldView) -> Markup {
    let input_id = format!("f-{}", field.key);
    html! {
        div class="field" data-origin=(field.origin.token()) {
            label for=(input_id) class="field__label mono" { (field.key) }
            span class="field__origin" data-origin=(field.origin.token()) { (field.origin.label()) }
            span class="field__value mono" {
                (field.effective.as_deref().unwrap_or("(unset)"))
            }
            (field_input(lang, field, &input_id))
            // 지울 값이 있을 때만 clear 체크박스를 낸다 — 비어 있는 필드에 "지운다"를
            // 붙이면 아무 일도 하지 않는 조작이 화면에 하나 늘어난다.
            @if field.kind == FieldKind::Uri && field.effective.is_some() {
                label class="field__clear" {
                    input type="checkbox" name=(format!("{}{}", field.key, route::CLEAR_SUFFIX)) value="1";
                    " "
                    (lang.sel("clear this value", "이 값을 지운다"))
                }
            }
            p class="field__hint" { (field_hint(lang, field)) }
            @if let Some(warning) = &field.warning {
                p class="field__warn" data-level=(Level::Warn.token()) { (warning) }
            }
        }
    }
}

/// 입력 칸 자체.
fn field_input(lang: Lang, field: &FieldView, input_id: &str) -> Markup {
    let empty_label = field.origin.empty_option_label(lang);
    html! {
        @match field.kind {
            FieldKind::Bool => {
                select id=(input_id) name=(field.key) {
                    option value="" selected[field.prefill().is_none()] { (empty_label) }
                    @for candidate in ["true", "false"] {
                        option value=(candidate) selected[field.prefill() == Some(candidate)] { (candidate) }
                    }
                }
            }
            FieldKind::Choice(options) => {
                select id=(input_id) name=(field.key) {
                    option value="" selected[field.prefill().is_none()] { (empty_label) }
                    @for candidate in options {
                        option value=(candidate) selected[field.prefill() == Some(*candidate)] { (candidate) }
                    }
                }
            }
            FieldKind::Number => {
                input type="number" id=(input_id) name=(field.key)
                    value=[field.prefill()] autocomplete="off";
            }
            // Uri는 prefill()이 항상 None이므로 value 속성이 붙지 않는다(모듈 헤더 함정 2).
            FieldKind::Text | FieldKind::EnvName | FieldKind::Dest | FieldKind::Uri => {
                input type="text" id=(input_id) name=(field.key)
                    value=[field.prefill()] autocomplete="off"
                    maxlength=(route::MAX_FIELD_VALUE_LEN.to_string());
            }
        }
    }
}

/// 필드별 안내 문장 — 종류가 정하는 부분 + 출처가 정하는 부분.
fn field_hint(lang: Lang, field: &FieldView) -> String {
    let kind_hint = match field.kind {
        FieldKind::EnvName => lang.sel(
            "Name of an environment variable, not the secret itself.",
            "환경변수 **이름**입니다 — 시크릿 값이 아닙니다.",
        ),
        FieldKind::Dest => lang.sel(
            "Compact form: local:/path or s3:bucket/prefix.",
            "compact 표기: local:/path 또는 s3:bucket/prefix.",
        ),
        FieldKind::Uri => lang.sel(
            "A URI with a password does not belong here — put it in an environment variable and name it in uri_env.",
            "비밀번호가 들어간 URI를 여기 쓰지 마세요 — 환경변수에 담고 그 이름을 uri_env에 적으세요.",
        ),
        FieldKind::Bool | FieldKind::Choice(_) | FieldKind::Number | FieldKind::Text => "",
    };
    let empty_hint = if field.kind == FieldKind::Uri {
        lang.sel("Empty means no change.", "비어 있으면 변경하지 않습니다.")
    } else {
        field.origin.empty_means(lang)
    };
    if kind_hint.is_empty() {
        empty_hint.to_string()
    } else {
        format!("{kind_hint} {empty_hint}")
    }
}

/// 삭제 확인 화면.
///
/// 삭제는 파괴적이므로 확인 단계를 둔다 — 프로파일 이름을 **직접 타이핑**해야 제출이
/// 통과한다. 재인증·전체 가드는 t21 소관이고, 이 화면은 t21이 승격시킬 최소 확인이다.
pub fn delete_page(lang: Lang, profile: &ProfileView) -> Markup {
    html! {
        (components::page_head(route::CONFIG_TITLE, Some(lang.sel("Delete a profile", "프로파일 삭제"))))
        (components::verdict_banner(
            Level::Fail,
            lang.sel(
                "Deleting a profile is not reversible from this console.",
                "프로파일 삭제는 이 콘솔에서 되돌릴 수 없습니다.",
            ),
            Some(lang.sel(
                "Backups already written are not touched — but nothing will schedule, prune, or restore under this name any more.",
                "이미 쌓인 백업은 손대지 않습니다 — 다만 이 이름으로는 더 이상 어떤 백업·정리·복구도 돌지 않습니다.",
            )),
        ))
        (components::panel(
            html! { h3 class="panel__title mono" { (profile.name) } },
            html! {
                (secret_table(lang, &profile.secrets))
                (destination_table(lang, profile))
                form method="post" action=(route::DELETE_PATH) class="cfg-form" {
                    input type="hidden" name=(route::FIELD_PROFILE) value=(profile.name);
                    div class="field" {
                        label for="f-confirm" class="field__label mono" { (route::FIELD_CONFIRM) }
                        input type="text" id="f-confirm" name=(route::FIELD_CONFIRM)
                            autocomplete="off" required;
                        p class="field__hint" {
                            (lang.sel(
                                "Type the profile name exactly to confirm.",
                                "확인을 위해 프로파일 이름을 그대로 입력하세요.",
                            ))
                        }
                    }
                    p class="actions" {
                        button type="submit" { (lang.sel("Delete this profile", "이 프로파일 삭제")) }
                        " "
                        a href=(route::CONFIG_PATH) { (lang.sel("Cancel", "취소")) }
                    }
                }
            },
        ))
    }
}

// ---------------------------------------------------------------------------
// v1 → v2 전환 화면
// ---------------------------------------------------------------------------

/// 목록 화면에 붙는 전환 안내 패널.
///
/// v1 파일에는 전환 링크를, v2 파일에는 **역방향이 없다는 사실과 그 이유**를 보여준다 —
/// "왜 v1로 되돌리는 버튼은 없나"를 화면이 답해야 운영자가 문서를 찾아 헤매지 않는다.
fn conversion_panel(lang: Lang, syntax: ConfigSyntax) -> Markup {
    html! {
        @match syntax {
            ConfigSyntax::V1 => {
                (components::panel(
                    html! { h3 class="panel__title" { (lang.sel("Convert to v2 flat syntax", "v2 flat 문법으로 전환")) } },
                    html! {
                        p class="muted" {
                            (lang.sel(
                                "The v2 surface writes one [profile.<name>] table per profile with flat keys, and lets shared policy live in [defaults] or [base.<name>]. Converting is an explicit, separate action — saving a field never changes the syntax.",
                                "v2 표면은 프로파일당 [profile.<name>] 테이블 하나 + flat 키로 적고, 공통 정책을 [defaults]·[base.<name>]에 둘 수 있게 합니다. 전환은 명시적인 별개 동작입니다 — 필드를 저장하는 것으로는 문법이 바뀌지 않습니다.",
                            ))
                        }
                        p class="actions" {
                            a href=(route::CONVERT_PATH) {
                                (lang.sel("Preview the conversion", "전환 미리보기 열기"))
                            }
                        }
                    },
                ))
            }
            ConfigSyntax::V2 | ConfigSyntax::Empty => {
                (components::panel(
                    html! { h3 class="panel__title" { (lang.sel("No conversion back to v1", "v1로 되돌리는 전환은 없습니다")) } },
                    html! {
                        p class="muted" { (no_reverse_reason(lang)) }
                    },
                ))
            }
        }
    }
}

/// v2 → v1 역방향을 제공하지 않는 이유. 전환 화면과 목록 화면이 같은 문장을 쓴다.
fn no_reverse_reason(lang: Lang) -> &'static str {
    lang.sel(
        "v2 can express things v1 cannot: [defaults], [base.<name>] and extends. Going back would have to flatten that shared policy into every profile — the file would keep working, but one place to edit would become many, and that loss would not be visible in the resulting file. If you need v1, write it by hand so the choice is yours.",
        "v2는 v1이 표현할 수 없는 것을 담습니다 — [defaults]·[base.<name>]·extends. 되돌리려면 그 공용 정책을 모든 프로파일에 펼쳐 넣어야 하고, 파일은 계속 동작하지만 한 곳에서 고치던 것이 여러 곳으로 흩어집니다. 그 손실은 결과 파일만 봐서는 보이지 않습니다. v1이 필요하면 직접 작성하세요 — 그 선택은 사람이 해야 합니다.",
    )
}

/// v1→v2 전환 미리보기 화면. **확인을 누르기 전까지 파일은 손대지 않는다.**
pub fn convert_page(lang: Lang, preview: &ConvertPreview) -> Markup {
    let moves: usize = preview.profiles.iter().map(|p| p.moves.len()).sum();
    html! {
        (components::page_head(
            route::CONFIG_TITLE,
            Some(lang.sel("Convert v1 nested to v2 flat", "v1 중첩 → v2 flat 전환")),
        ))
        (components::verdict_banner(
            Level::Warn,
            lang.sel(
                "This rewrites the whole config file. Nothing has changed yet.",
                "이 작업은 config 파일 전체를 다시 씁니다. 아직 아무것도 바뀌지 않았습니다.",
            ),
            Some(lang.sel(
                "Review the diff below and confirm. The previous file is kept as a numbered backup, and the change is refused if it does not pass doctor.",
                "아래 diff를 확인한 뒤 실행하세요. 직전 파일은 번호가 붙은 백업으로 보관되며, doctor 점검을 통과하지 못하면 전환은 거부됩니다.",
            )),
        ))
        (components::meta_list(&[
            ("syntax", format!("{} → {}", ConfigSyntax::V1.label(), ConfigSyntax::V2.label())),
            ("profiles", preview.profiles.len().to_string()),
            ("keys moved", moves.to_string()),
            ("bytes", format!("{} → {}", preview.bytes_before, preview.bytes_after)),
        ]))
        (components::notice(
            Level::Warn,
            lang.sel("What this conversion does not do", "이 전환이 하지 않는 일"),
            html! {
                dl class="meta" {
                    dt class="meta__k" { "inheritance" }
                    dd class="meta__v" {
                        (lang.sel(
                            "No [defaults], [base.<name>] or extends is created. v1 has no inheritance surface, so every value is written in its own profile and stays there. This is a syntax conversion — consolidating shared policy is a separate decision only you can make, because the file cannot tell which value is common policy and which is a deliberate exception for that one profile.",
                            "[defaults]·[base.<name>]·extends는 생기지 않습니다. v1에는 상속 표면이 없어서 모든 값이 각 프로파일에 적혀 있고, 전환 후에도 그대로 남습니다. 이것은 문법 변환입니다 — 공통 정책을 한 곳으로 모으는 일은 사람만 할 수 있는 별개의 판단입니다. 어느 값이 공용 정책이고 어느 값이 그 프로파일만의 의도적 예외인지는 파일이 알려주지 않기 때문입니다.",
                        ))
                    }
                    dt class="meta__k" { "values" }
                    dd class="meta__v" {
                        (lang.sel(
                            "No value changes. The result was read back through the same loader the CLI uses and compared to the original; if a single key differed, you would be looking at an error instead of this page.",
                            "값은 하나도 바뀌지 않습니다. 결과를 CLI와 같은 로더로 다시 읽어 원본과 비교했고, 키 하나라도 달랐다면 이 화면 대신 오류가 나왔을 것입니다.",
                        ))
                    }
                    dt class="meta__k" { "comments" }
                    dd class="meta__v" {
                        (lang.sel(
                            "Comments and key order are not preserved — the file is written from its parsed contents, so comments are dropped and keys come out sorted. Copy anything you want to keep before confirming.",
                            "주석과 키 순서는 보존되지 않습니다 — 파일을 파싱된 내용에서 다시 쓰므로 주석이 사라지고 키가 정렬됩니다. 남겨야 할 주석이 있으면 실행 전에 복사해 두세요.",
                        ))
                    }
                }
            },
        ))
        (components::notice(
            Level::Ok,
            lang.sel("There is no conversion back", "역방향 전환은 없습니다"),
            html! { p { (no_reverse_reason(lang)) } },
        ))
        @for profile in &preview.profiles {
            (moves_panel(lang, profile))
        }
        (components::panel(
            html! {
                h3 class="panel__title" { (lang.sel("Resulting file", "저장될 파일")) }
                span class="counts" { (format!("{} bytes", preview.bytes_after)) }
            },
            html! {
                p class="muted" {
                    (lang.sel(
                        "Plain uri values are shown redacted here — the file itself keeps whatever it already had.",
                        "평문 uri 값은 이 화면에서만 가려 보여줍니다 — 파일에는 원래 있던 값이 그대로 쓰입니다.",
                    ))
                }
                pre class="logdump" { (preview.new_text) }
                @if preview.truncated_lines > 0 {
                    p class="muted" {
                        (lang.sel("Lines omitted from this preview: ", "미리보기에서 생략된 줄: "))
                        (preview.truncated_lines)
                    }
                }
            },
        ))
        form method="post" action=(route::CONVERT_PATH) class="cfg-form" {
            input type="hidden" name=(route::FIELD_FROM_DIGEST) value=(preview.digest);
            p class="actions" {
                button type="submit" { (lang.sel("Convert this file to v2", "이 파일을 v2로 전환")) }
                " "
                a href=(route::CONFIG_PATH) { (lang.sel("Cancel", "취소")) }
            }
        }
    }
}

/// 프로파일 하나의 키 이동 표 — v1 경로가 어느 v2 키로 가는지 한 줄씩.
fn moves_panel(lang: Lang, profile: &ConvertedProfile) -> Markup {
    components::panel(
        html! {
            h3 class="panel__title mono" { (profile.name) }
            span class="counts" { (format!("{} keys", profile.moves.len())) }
        },
        html! {
            div class="dtable-scroll" {
                table class="dtable" {
                    caption class="muted" {
                        (lang.sel(
                            "Where each value moves (v1 nested path → v2 flat key)",
                            "각 값이 옮겨지는 자리(v1 중첩 경로 → v2 flat 키)",
                        ))
                    }
                    thead {
                        tr {
                            th scope="col" { "From" }
                            th scope="col" { "To" }
                            th scope="col" { "Value" }
                        }
                    }
                    tbody {
                        @if profile.moves.is_empty() {
                            tr {
                                td colspan="3" class="muted" {
                                    (lang.sel(
                                        "This profile has no keys written in the file.",
                                        "이 프로파일에는 파일에 적힌 키가 없습니다.",
                                    ))
                                }
                            }
                        }
                        @for moved in &profile.moves {
                            tr {
                                td class="mono" { (moved.from) }
                                td class="mono key" { (moved.to) }
                                td class="mono" { (moved.value) }
                            }
                        }
                    }
                }
            }
        },
    )
}

/// 전환 결과 화면.
///
/// 성공했더라도 **끝난 것이 아니라는 사실**을 말한다 — 문법만 바뀌었고 공용 정책은 여전히
/// 프로파일마다 흩어져 있다. 그 말을 하지 않으면 운영자는 "v2로 바꿨는데 왜 [defaults]가
/// 안 생겼나"를 버그로 의심한다.
pub fn convert_result_page(lang: Lang, profiles: usize, outcome: SaveOutcome<'_>) -> Markup {
    let SaveOutcome {
        persisted,
        warnings,
    } = outcome;
    let level = if !persisted || !warnings.is_empty() {
        Level::Warn
    } else {
        Level::Ok
    };
    let headline = if !persisted {
        lang.sel(
            "The conversion passed validation. The config file was NOT written.",
            "전환이 검증을 통과했습니다. config 파일은 아직 쓰이지 않았습니다.",
        )
    } else if warnings.is_empty() {
        lang.sel(
            "The config file now uses the v2 flat syntax.",
            "config 파일이 이제 v2 flat 문법을 씁니다.",
        )
    } else {
        lang.sel(
            "The config file was converted, but doctor reported warnings.",
            "config 파일을 전환했지만 doctor가 경고를 보고했습니다.",
        )
    };
    html! {
        (components::page_head(route::CONFIG_TITLE, None))
        (components::verdict_banner(level, headline, None))
        (components::meta_list(&[
            ("operation", "convert".to_string()),
            ("syntax", format!("{} → {}", ConfigSyntax::V1.label(), ConfigSyntax::V2.label())),
            ("profiles", profiles.to_string()),
        ]))
        @if persisted {
            (components::notice(
                Level::Warn,
                lang.sel("Syntax only — policy is unchanged", "문법만 바뀌었습니다 — 정책은 그대로입니다"),
                html! {
                    p {
                        (lang.sel(
                            "Every profile still carries its own values: no [defaults], no [base.<name>], no extends was created. That is not a bug — v1 had no inheritance to carry over, and guessing which values are shared policy would quietly spread one profile's exception to the others.",
                            "각 프로파일은 여전히 자기 값을 그대로 들고 있습니다 — [defaults]·[base.<name>]·extends는 생기지 않았습니다. 버그가 아닙니다: v1에는 옮겨올 상속이 없었고, 어느 값이 공용 정책인지 추측하면 한 프로파일의 예외가 조용히 다른 프로파일로 번집니다.",
                        ))
                    }
                    p class="muted" {
                        (lang.sel(
                            "Consolidating shared policy is now possible and is a separate, manual step: move the common keys into [defaults] (or a [base.<name>] plus extends) in the config file, then reload this screen and check the origin badges.",
                            "공통 정책을 모으는 일은 이제 가능해졌지만 별개의 수동 작업입니다 — config 파일에서 공통 키를 [defaults](또는 [base.<name>] + extends)로 옮긴 뒤 이 화면을 새로 열어 출처 배지를 확인하세요.",
                        ))
                    }
                },
            ))
        }
        @if !warnings.is_empty() {
            (components::notice(Level::Warn, lang.sel("doctor warnings", "doctor 경고"), html! {
                ul {
                    @for warning in warnings {
                        li class="mono" { (warning) }
                    }
                }
            }))
        }
        p class="actions" {
            a href=(route::CONFIG_PATH) { (lang.sel("Back to profiles", "프로파일 목록으로")) }
        }
    }
}

/// [`result_page`]가 받는 저장 결과 — `persisted`와 `warnings`는 항상 함께 다니는 한 쌍이라
/// (둘 다 [`crate::web::routes::config::ChangeReceipt`]에서 나온다) 묶어서 인자 개수를
/// 줄인다(clippy `too_many_arguments`).
pub struct SaveOutcome<'a> {
    /// 파일이 실제로 바뀌었는지.
    pub persisted: bool,
    /// 저장 전 `doctor` 검증이 exit 4(경고 동반 성공)로 끝났을 때의 경고 메시지.
    /// **exit 4는 저장을 막지 않는다**(`routes::doctor`의 같은 판단) — 그래서
    /// `persisted == true && !warnings.is_empty()`인 조합이 정상이다.
    pub warnings: &'a [String],
}

/// 변경 요청이 검증을 통과한 뒤 보여주는 결과 화면.
///
/// `outcome.persisted`가 false면 "검증만 통과했고 파일은 그대로"라는 사실을
/// **명시적으로** 말한다. 조용히 "저장됐습니다"라고 쓰면 운영자는 반영되지 않은 설정으로
/// 백업을 돌린다. `outcome.warnings`가 있으면(exit 4) 헤드라인은 "저장은 됐다"고 말하되
/// 배지는 [`Level::Warn`]으로 그려 운영자가 경고를 놓치지 않게 한다.
pub fn result_page(
    lang: Lang,
    profile_name: &str,
    operation: &str,
    set: &[(String, String)],
    unset: &[String],
    broke_inheritance: &[String],
    outcome: SaveOutcome<'_>,
) -> Markup {
    let SaveOutcome {
        persisted,
        warnings,
    } = outcome;
    let level = if !persisted || !warnings.is_empty() {
        Level::Warn
    } else {
        Level::Ok
    };
    let headline = if !persisted {
        lang.sel(
            "The change passed validation. The config file was NOT written.",
            "변경이 검증을 통과했습니다. config 파일은 아직 쓰이지 않았습니다.",
        )
    } else if warnings.is_empty() {
        lang.sel(
            "The config file was updated.",
            "config 파일이 갱신되었습니다.",
        )
    } else {
        lang.sel(
            "The config file was updated, but doctor reported warnings.",
            "config 파일이 갱신됐지만 doctor가 경고를 보고했습니다.",
        )
    };
    let detail = if persisted {
        None
    } else {
        Some(lang.sel(
            "This console is running with a validator-only store — the change was checked but nothing was written to disk.",
            "이 콘솔은 검증 전용 저장소로 동작 중입니다 — 변경은 점검했지만 디스크에는 아무것도 쓰지 않았습니다.",
        ))
    };
    html! {
        (components::page_head(route::CONFIG_TITLE, None))
        (components::verdict_banner(level, headline, detail))
        (components::meta_list(&[
            ("profile", profile_name.to_string()),
            ("operation", operation.to_string()),
            ("keys set", set.len().to_string()),
            ("keys removed", unset.len().to_string()),
        ]))
        (components::panel(
            html! { h3 class="panel__title" { (lang.sel("What this change does", "이 변경이 하는 일")) } },
            html! {
                div class="dtable-scroll" {
                    table class="dtable" {
                        thead {
                            tr {
                                th scope="col" { "Action" }
                                th scope="col" { "Key" }
                                th scope="col" { "Value" }
                            }
                        }
                        tbody {
                            @if set.is_empty() && unset.is_empty() {
                                tr {
                                    td colspan="3" class="muted" {
                                        (lang.sel("Nothing would change.", "바뀌는 것이 없습니다."))
                                    }
                                }
                            }
                            @for (key, value) in set {
                                tr {
                                    td class="mono" { "set" }
                                    td class="mono key" { (key) }
                                    td class="mono" { (value) }
                                }
                            }
                            @for key in unset {
                                tr data-level=(Level::Warn.token()) {
                                    td class="mono" { "remove" }
                                    td class="mono key" { (key) }
                                    td class="muted" { "-" }
                                }
                            }
                        }
                    }
                }
            },
        ))
        @if !warnings.is_empty() {
            (components::notice(Level::Warn, lang.sel("doctor warnings", "doctor 경고"), html! {
                p {
                    (lang.sel(
                        "The save went through — doctor's warnings are success with caveats, not a failure. Review these before the next backup:",
                        "저장은 진행됐습니다 — doctor의 경고는 실패가 아니라 단서가 붙은 성공입니다. 다음 백업 전에 아래를 확인하세요:",
                    ))
                }
                ul {
                    @for warning in warnings {
                        li class="mono" { (warning) }
                    }
                }
            }))
        }
        @if !broke_inheritance.is_empty() {
            (components::notice(Level::Warn, lang.sel("Inheritance pinned", "상속이 끊긴 키"), html! {
                p {
                    (lang.sel(
                        "These keys used to come from [defaults] or extends and are now written into this profile. Changing the shared policy will no longer reach them:",
                        "다음 키는 [defaults]·extends에서 오던 값인데 이제 이 프로파일에 직접 적힙니다. 공용 정책을 바꿔도 더 이상 반영되지 않습니다:",
                    ))
                }
                ul {
                    @for key in broke_inheritance {
                        li class="mono" { (key) }
                    }
                }
            }))
        }
        p class="actions" {
            a href=(route::CONFIG_PATH) { (lang.sel("Back to profiles", "프로파일 목록으로")) }
        }
    }
}

/// 화면을 그릴 수 없을 때(config 미연결·파싱 실패·프로파일 없음) 쓰는 오류 화면.
pub fn problem_page(lang: Lang, level: Level, headline: &str, detail: Option<&str>) -> Markup {
    html! {
        (components::page_head(route::CONFIG_TITLE, None))
        (components::verdict_banner(level, headline, detail))
        (components::notice(level, lang.sel("What to check", "확인할 것"), html! {
            p {
                (lang.sel(
                    "The console reads the config file it was started with. Fix the file (or restart the console with --config <PATH>) and reload.",
                    "콘솔은 기동할 때 지정된 config 파일을 읽습니다. 파일을 고치거나(또는 --config <PATH>로 다시 띄우고) 새로 고치세요.",
                ))
            }
        }))
        p class="actions" {
            a href=(route::CONFIG_PATH) { (lang.sel("Back to profiles", "프로파일 목록으로")) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(
        key: &'static str,
        kind: FieldKind,
        origin: Origin,
        effective: Option<&str>,
    ) -> FieldView {
        FieldView {
            key,
            group: FieldGroup::Compression,
            kind,
            origin,
            effective: effective.map(str::to_string),
            warning: None,
        }
    }

    fn sample_profile() -> ProfileView {
        ProfileView {
            name: "prod".to_string(),
            editable: true,
            name_error: None,
            endpoint_only: false,
            secrets: vec![
                SecretEnvRef::new("uri_env", "MONGO_URI", true),
                SecretEnvRef::new("s3_creds", "S3_CREDS", false),
            ],
            destinations: vec![DestinationView {
                label: "primary".to_string(),
                kind: Some("s3".to_string()),
                location: Some("db-backups/mongo/prod".to_string()),
                region: Some("ap-northeast-2".to_string()),
                endpoint: None,
                credentials: Some(SecretEnvRef::new("s3_creds", "S3_CREDS", false)),
            }],
            destination_editable: true,
            destination_note: None,
            fields: vec![
                field(
                    "compress_level",
                    FieldKind::Number,
                    Origin::Direct,
                    Some("6"),
                ),
                field("encrypt", FieldKind::Bool, Origin::Defaults, Some("true")),
                field(
                    "incr_interval",
                    FieldKind::Text,
                    Origin::Inherited {
                        from: "s3prod".to_string(),
                    },
                    Some("30m"),
                ),
                field("keep_full", FieldKind::Number, Origin::Builtin, None),
            ],
        }
    }

    fn sample_preview() -> ConvertPreview {
        ConvertPreview {
            digest: "0f1e2d3c".to_string(),
            profiles: vec![ConvertedProfile {
                name: "prod".to_string(),
                moves: vec![
                    MovedKey {
                        from: "source.uri_env".to_string(),
                        to: "uri_env".to_string(),
                        value: "MONGO_URI".to_string(),
                    },
                    MovedKey {
                        from: "destination.type, destination.s3.bucket".to_string(),
                        to: "dest".to_string(),
                        value: "s3:db-backups".to_string(),
                    },
                ],
            }],
            new_text: "[profile.prod]\nuri_env = \"MONGO_URI\"\ndest = \"s3:db-backups\"\n"
                .to_string(),
            truncated_lines: 0,
            bytes_before: 120,
            bytes_after: 64,
        }
    }

    /// 색·인라인 스타일 리터럴이 마크업에 없다([`components`] 헤더 규칙 1·3).
    ///
    /// `#`을 통째로 금지하지는 않는다 — destination 라벨(`s3#1`)처럼 `#`이 **데이터**로
    /// 들어올 수 있기 때문이다. 금지 대상은 색 리터럴이므로 `#` + 16진수 패턴만 본다.
    #[test]
    fn markup_carries_no_presentation() {
        let rendered = [
            list_page(
                Lang::En,
                ConfigSyntax::V2,
                Some("prod"),
                &[sample_profile()],
            )
            .into_string(),
            form_page(
                Lang::En,
                ConfigSyntax::V2,
                FormMode::Update,
                &sample_profile(),
            )
            .into_string(),
            form_page(
                Lang::Ko,
                ConfigSyntax::V1,
                FormMode::Create,
                &sample_profile(),
            )
            .into_string(),
            delete_page(Lang::En, &sample_profile()).into_string(),
            result_page(
                Lang::En,
                "prod",
                "update",
                &[("a".into(), "b".into())],
                &["c".into()],
                &["d".into()],
                SaveOutcome {
                    persisted: false,
                    warnings: &[],
                },
            )
            .into_string(),
            problem_page(Lang::En, Level::Error, "nope", Some("why")).into_string(),
            convert_page(Lang::Ko, &sample_preview()).into_string(),
            convert_result_page(
                Lang::En,
                2,
                SaveOutcome {
                    persisted: true,
                    warnings: &["p/encryption: disabled".to_string()],
                },
            )
            .into_string(),
        ];
        for out in rendered {
            assert!(!out.contains("style="), "인라인 스타일이 들어갔다: {out}");
            let hex_color = out
                .as_bytes()
                .windows(4)
                .any(|w| w[0] == b'#' && w[1..].iter().all(|b| b.is_ascii_hexdigit()));
            assert!(!hex_color, "색 리터럴로 보이는 값이 있다: {out}");
        }
    }

    /// 직접 적힌 값만 입력 칸에 미리 채워진다 — 상속·기본값·URI는 비어 있다.
    /// 이것이 상속 파괴를 막는 유일한 장치다(모듈 헤더 함정 2).
    #[test]
    fn only_direct_values_are_prefilled() {
        assert_eq!(
            field("k", FieldKind::Text, Origin::Direct, Some("v")).prefill(),
            Some("v")
        );
        assert_eq!(
            field("k", FieldKind::Text, Origin::Defaults, Some("v")).prefill(),
            None,
            "defaults 값이 입력 칸에 박히면 제출만으로 상속이 끊긴다"
        );
        assert_eq!(
            field(
                "k",
                FieldKind::Text,
                Origin::Inherited { from: "b".into() },
                Some("v")
            )
            .prefill(),
            None
        );
        assert_eq!(
            field("k", FieldKind::Text, Origin::Builtin, Some("v")).prefill(),
            None
        );
        assert_eq!(
            field(
                "uri",
                FieldKind::Uri,
                Origin::Direct,
                Some("mongodb://***@h/db")
            )
            .prefill(),
            None,
            "가린 표기가 입력 칸에 채워지면 제출이 원본 URI를 파괴한다"
        );
    }

    /// 상속받은 값과 덮어쓴 값이 마크업에서 구분된다 — 토큰(`data-origin`)과 글자 라벨
    /// 양쪽으로. 색이 없어도(app.css에 규칙이 없어도) 읽혀야 한다.
    #[test]
    fn inherited_and_overridden_are_distinguishable_in_markup() {
        let out = form_page(
            Lang::En,
            ConfigSyntax::V2,
            FormMode::Update,
            &sample_profile(),
        )
        .into_string();
        for token in ["direct", "defaults", "inherited", "builtin"] {
            assert!(
                out.contains(&format!(r#"data-origin="{token}""#)),
                "{token} 출처 토큰 누락: {out}"
            );
        }
        assert!(
            out.contains("extends: s3prod"),
            "상속 출처 이름 누락: {out}"
        );
        assert!(out.contains(">defaults<"), "defaults 글자 라벨 누락");
        assert!(out.contains(">builtin<"), "builtin 글자 라벨 누락");
    }

    /// 네 출처의 토큰·라벨이 서로 겹치지 않는다(겹치면 화면에서 구분이 사라진다).
    #[test]
    fn origins_have_distinct_tokens_and_labels() {
        let all = [
            Origin::Direct,
            Origin::Defaults,
            Origin::Inherited {
                from: "b".to_string(),
            },
            Origin::Builtin,
        ];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a.token(), b.token(), "토큰 충돌: {a:?} vs {b:?}");
                assert_ne!(a.label(), b.label(), "라벨 충돌: {a:?} vs {b:?}");
                for lang in [Lang::En, Lang::Ko] {
                    assert_ne!(
                        a.empty_means(lang),
                        b.empty_means(lang),
                        "빈 입력 설명 충돌: {a:?} vs {b:?}"
                    );
                    assert_ne!(
                        a.empty_option_label(lang),
                        b.empty_option_label(lang),
                        "빈 선택지 라벨 충돌: {a:?} vs {b:?}"
                    );
                }
            }
        }
    }

    /// 설정된 env와 미설정 env가 불리언으로 구분 표시된다.
    #[test]
    fn secret_presence_is_shown_as_a_boolean_state() {
        let out = list_page(Lang::En, ConfigSyntax::V2, None, &[sample_profile()]).into_string();
        assert!(out.contains("MONGO_URI"), "설정된 env 이름 누락");
        assert!(out.contains("S3_CREDS"), "미설정 env 이름 누락");
        assert!(
            out.contains("not set in this process"),
            "미설정 표시 누락: {out}"
        );
        assert!(out.contains(r#"data-level="ok""#), "설정됨 표시 누락");
        assert!(
            out.contains(r#"data-level="warn""#),
            "미설정 표시 레벨 누락"
        );
    }

    /// [`RedactedUri`]는 자격증명을 지운 표기만 담고, 원문 접근자가 없다.
    #[test]
    fn redacted_uri_drops_credentials() {
        const FAKE: &str = "NOT-A-REAL-SECRET-cfgview-71a3";
        let uri = format!("mongodb://admin:{FAKE}@db.internal:27017/app?tls=true");
        let view = RedactedUri::from_raw(&uri);
        assert!(
            !view.shown().contains(FAKE),
            "시크릿이 표기에 남았다: {}",
            view.shown()
        );
        assert!(
            !view.shown().contains("admin"),
            "사용자명이 남았다: {}",
            view.shown()
        );
        assert!(
            view.shown().contains("db.internal"),
            "호스트는 남아야 진단이 된다"
        );
        assert!(view.had_credentials(), "자격증명 존재를 놓쳤다");

        // 자격증명이 없는 URI는 경고를 만들지 않는다(무의미한 경고는 신호를 죽인다).
        let plain = RedactedUri::from_raw("postgres://localhost:5432/app");
        assert!(!plain.had_credentials());
        assert_eq!(plain.shown(), "postgres://localhost:5432/app");
    }

    /// 적대적 문자열(HTML 태그·제어문자)이 전부 이스케이프된다.
    #[test]
    fn hostile_values_are_escaped() {
        let hostile = r#"<img src=x onerror="alert(1)">"#;
        let mut profile = sample_profile();
        profile.name = hostile.to_string();
        profile.name_error = Some(hostile.to_string());
        profile.editable = false;
        profile.secrets = vec![SecretEnvRef::new("uri_env", hostile, false)];
        profile.destinations = vec![DestinationView {
            label: hostile.to_string(),
            kind: Some(hostile.to_string()),
            location: Some(format!("{hostile}\u{0000}\u{001b}[31m")),
            region: None,
            endpoint: None,
            credentials: None,
        }];
        profile.fields = vec![FieldView {
            key: "compress_algorithm",
            group: FieldGroup::Compression,
            kind: FieldKind::Text,
            origin: Origin::Direct,
            effective: Some(hostile.to_string()),
            warning: Some(hostile.to_string()),
        }];
        let rendered = [
            list_page(
                Lang::En,
                ConfigSyntax::V1,
                Some(hostile),
                &[profile.clone()],
            )
            .into_string(),
            form_page(Lang::En, ConfigSyntax::V1, FormMode::Update, &profile).into_string(),
            delete_page(Lang::Ko, &profile).into_string(),
            result_page(
                Lang::En,
                hostile,
                hostile,
                &[(hostile.into(), hostile.into())],
                &[hostile.into()],
                &[hostile.into()],
                SaveOutcome {
                    persisted: true,
                    warnings: &[hostile.into()],
                },
            )
            .into_string(),
        ];
        for out in rendered {
            assert!(!out.contains("<img"), "이스케이프되지 않았다: {out}");
            assert!(!out.contains(r#"onerror=""#), "속성이 살아 있다: {out}");
            assert!(out.contains("&lt;img"), "이스케이프 형태가 아니다: {out}");
        }
    }

    /// 편집 불가한 이름은 링크·폼 대상이 되지 않고, 이유가 화면에 적힌다.
    #[test]
    fn non_editable_name_has_no_action_links() {
        let mut profile = sample_profile();
        profile.name = "../etc".to_string();
        profile.editable = false;
        profile.name_error = Some("경로 구분자가 있습니다".to_string());
        let out = list_page(Lang::En, ConfigSyntax::V1, None, &[profile]).into_string();
        assert!(
            !out.contains(route::CONFIG_EDIT_PREFIX),
            "편집 링크가 남았다: {out}"
        );
        assert!(out.contains("경로 구분자가 있습니다"), "이유 누락: {out}");
    }

    /// 문법 배너가 v1/v2를 서로 다르게 말한다 — 저장이 무엇을 보존하는지 알려야 한다.
    #[test]
    fn syntax_banner_distinguishes_v1_and_v2() {
        let v1 = list_page(Lang::Ko, ConfigSyntax::V1, None, &[]).into_string();
        let v2 = list_page(Lang::Ko, ConfigSyntax::V2, None, &[]).into_string();
        assert!(v1.contains(r#"data-syntax="v1""#), "v1 토큰 누락");
        assert!(v2.contains(r#"data-syntax="v2""#), "v2 토큰 누락");
        assert!(v1.contains("v1 중첩 문법"), "v1 설명 누락: {v1}");
        assert!(v2.contains("v2 flat 문법"), "v2 설명 누락: {v2}");
        assert_ne!(
            ConfigSyntax::V1.explain(Lang::En),
            ConfigSyntax::V2.explain(Lang::En)
        );
    }

    /// destination을 편집하지 않는 프로파일에서는 그 사실과 이유가 화면에 적힌다.
    #[test]
    fn non_editable_destination_says_why() {
        let mut profile = sample_profile();
        profile.destination_editable = false;
        profile.destination_note =
            Some("destination이 여러 개라 폼이 목록을 다시 쓰지 않습니다.".to_string());
        profile
            .fields
            .retain(|f| f.group != FieldGroup::Destination);
        let out = form_page(Lang::En, ConfigSyntax::V2, FormMode::Update, &profile).into_string();
        assert!(
            out.contains("Destinations not editable here"),
            "제목 누락: {out}"
        );
        assert!(out.contains("여러 개라"), "이유 누락: {out}");
    }

    /// 저장되지 않았다는 사실이 결과 화면에 명시된다 — "저장됨"으로 오해하면 운영자가
    /// 반영되지 않은 설정으로 백업을 돌린다.
    #[test]
    fn result_page_says_when_nothing_was_written() {
        let not_persisted = SaveOutcome {
            persisted: false,
            warnings: &[],
        };
        let out =
            result_page(Lang::En, "prod", "update", &[], &[], &[], not_persisted).into_string();
        assert!(out.contains("NOT written"), "미저장 사실 누락: {out}");
        assert!(out.contains(r#"data-level="warn""#), "레벨 누락");
        let not_persisted_ko = SaveOutcome {
            persisted: false,
            warnings: &[],
        };
        let ko =
            result_page(Lang::Ko, "prod", "update", &[], &[], &[], not_persisted_ko).into_string();
        assert!(
            ko.contains("쓰이지 않았습니다"),
            "한국어 미저장 문장 누락: {ko}"
        );
    }

    /// **exit 4(doctor 경고)에서도 저장은 됐다는 사실이 화면에 남는다** — "NOT written"이
    /// 아니라 "저장됐지만 경고가 있다"고 말해야 한다. t27이 이 화면 계층에서 고정하는
    /// 성질이다(저장소 계층의 같은 성질은 `config_write`의 단위·통합 테스트가 고정한다).
    #[test]
    fn result_page_shows_warnings_but_still_says_it_was_written() {
        let warnings = vec!["p/encryption: disabled — plaintext".to_string()];
        let warned = SaveOutcome {
            persisted: true,
            warnings: &warnings,
        };
        let out = result_page(Lang::En, "prod", "update", &[], &[], &[], warned).into_string();
        assert!(
            !out.contains("NOT written"),
            "저장됐는데 미저장이라 말한다: {out}"
        );
        assert!(
            out.contains("doctor reported warnings"),
            "경고 사실이 헤드라인에 없다: {out}"
        );
        assert!(
            out.contains("p/encryption: disabled — plaintext"),
            "경고 메시지 본문이 없다: {out}"
        );
        assert!(
            out.contains(r#"data-level="warn""#),
            "경고가 있는데도 초록(ok) 배지로 그려졌다: {out}"
        );

        // 경고가 없으면 여전히 순수 성공(Ok) 배지다 — 이 테스트가 그 대비를 함께 고정한다.
        let clean = SaveOutcome {
            persisted: true,
            warnings: &[],
        };
        let clean = result_page(Lang::En, "prod", "update", &[], &[], &[], clean).into_string();
        assert!(clean.contains(r#"data-level="ok""#), "{clean}");
        assert!(!clean.contains("doctor reported warnings"), "{clean}");
    }

    /// 삭제 화면은 확인 입력을 요구하고, 무엇이 남고 무엇이 사라지는지 말한다.
    #[test]
    fn delete_page_requires_typed_confirmation() {
        let out = delete_page(Lang::En, &sample_profile()).into_string();
        assert!(out.contains(&format!(r#"name="{}""#, route::FIELD_CONFIRM)));
        assert!(
            out.contains("Type the profile name exactly"),
            "확인 안내 누락: {out}"
        );
        assert!(
            out.contains("Backups already written are not touched"),
            "영향 설명 누락"
        );
        assert!(
            out.contains(r#"data-level="fail""#),
            "파괴적 작업 레벨 누락"
        );
    }

    /// 불리언 필드는 체크박스가 아니라 `<select>`로 그려지고 빈 값을 표현할 수 있다 —
    /// 체크박스로는 "명시적 false"와 "상속 유지"가 구분되지 않는다.
    #[test]
    fn bool_field_uses_select_with_an_empty_option() {
        let inherited = field(
            "pg_logical",
            FieldKind::Bool,
            Origin::Defaults,
            Some("true"),
        );
        let out = field_row(Lang::En, &inherited).into_string();
        assert!(out.contains("<select"), "select가 아니다: {out}");
        assert!(
            !out.contains(r#"type="checkbox""#),
            "체크박스가 쓰였다: {out}"
        );
        assert!(
            out.contains(r#"<option value="" selected>"#),
            "빈 옵션 누락: {out}"
        );
        assert!(
            out.contains("(keep [defaults])"),
            "defaults 유지 라벨 누락: {out}"
        );

        let direct = field("pg_logical", FieldKind::Bool, Origin::Direct, Some("false"));
        let out = field_row(Lang::En, &direct).into_string();
        assert!(
            out.contains(r#"<option value="false" selected>"#),
            "현재 값 선택 누락: {out}"
        );
        assert!(
            out.contains("(remove this key)"),
            "키 삭제 라벨 누락: {out}"
        );
    }

    // -- v1 → v2 전환 화면 -------------------------------------------------

    /// **전환 화면이 "하지 않는 일"을 먼저 말한다.**
    ///
    /// 상속이 생기지 않는다는 사실을 화면이 말하지 않으면 운영자는 "v2로 바꿨는데 왜
    /// [defaults]가 없나"를 버그로 의심하고, 주석이 사라진다는 사실을 말하지 않으면 실행 후에야
    /// 알게 된다. 둘 다 되돌릴 수 없으므로 실행 **전에** 말해야 한다.
    #[test]
    fn convert_page_states_what_it_does_not_do() {
        for lang in [Lang::En, Lang::Ko] {
            let out = convert_page(lang, &sample_preview()).into_string();
            // 1) 상속을 만들지 않는다 = 정책 재구성이 아니다.
            let inheritance = lang.sel("No [defaults]", "[defaults]");
            assert!(
                out.contains(inheritance) || out.contains("defaults"),
                "{out}"
            );
            assert!(
                out.contains(lang.sel("syntax conversion", "문법 변환")),
                "문법 변환일 뿐이라는 말이 없다: {out}"
            );
            // 2) 값이 바뀌지 않는다.
            assert!(
                out.contains(lang.sel("No value changes", "값은 하나도 바뀌지 않습니다")),
                "{out}"
            );
            // 3) 주석은 보존되지 않는다.
            assert!(
                out.contains(lang.sel("Comments and key order", "주석과 키 순서")),
                "{out}"
            );
            // 역방향이 없는 이유.
            assert!(
                out.contains(lang.sel("There is no conversion back", "역방향 전환은 없습니다")),
                "{out}"
            );
            // 아직 아무것도 바뀌지 않았다는 사실.
            assert!(
                out.contains(
                    lang.sel("Nothing has changed yet", "아직 아무것도 바뀌지 않았습니다")
                ),
                "{out}"
            );
        }
    }

    /// 미리보기가 이동 표·결과 텍스트·확인 지문을 함께 싣는다 — 지문이 없으면 실행할 수 없다.
    #[test]
    fn convert_page_carries_the_diff_and_the_confirmation_fingerprint() {
        let out = convert_page(Lang::En, &sample_preview()).into_string();
        assert!(
            out.contains("destination.type, destination.s3.bucket"),
            "{out}"
        );
        assert!(out.contains("s3:db-backups"), "{out}");
        assert!(
            out.contains(r#"class="logdump""#),
            "결과 텍스트 컨테이너 누락"
        );
        assert!(
            out.contains(&format!(
                r#"name="{}" value="0f1e2d3c""#,
                route::FIELD_FROM_DIGEST
            )),
            "확인 지문이 폼에 실리지 않았다: {out}"
        );
        assert!(
            out.contains(&format!(r#"action="{}""#, route::CONVERT_PATH)),
            "{out}"
        );
    }

    /// 결과 화면은 성공했어도 **정책은 그대로**라고 말한다.
    #[test]
    fn convert_result_says_policy_is_unchanged() {
        let done = convert_result_page(
            Lang::Ko,
            2,
            SaveOutcome {
                persisted: true,
                warnings: &[],
            },
        )
        .into_string();
        assert!(done.contains("v2 flat 문법을 씁니다"), "{done}");
        assert!(
            done.contains("문법만 바뀌었습니다"),
            "정책이 그대로라는 말이 없다: {done}"
        );
        assert!(done.contains("버그가 아닙니다"), "{done}");

        // 쓰이지 않은 경우에는 그 사실을 명시한다(저장 화면과 같은 규약).
        let pending = convert_result_page(
            Lang::En,
            1,
            SaveOutcome {
                persisted: false,
                warnings: &[],
            },
        )
        .into_string();
        assert!(pending.contains("NOT written"), "{pending}");
        assert!(
            !pending.contains("Syntax only"),
            "쓰지도 않았는데 전환 후 안내가 나왔다: {pending}"
        );
    }

    /// 목록 화면은 v1이면 전환 링크를, v2면 역방향이 없는 이유를 보여준다.
    #[test]
    fn list_page_offers_conversion_only_for_v1() {
        let v1 = list_page(Lang::En, ConfigSyntax::V1, None, &[]).into_string();
        assert!(
            v1.contains(&format!(r#"href="{}""#, route::CONVERT_PATH)),
            "v1에 전환 링크가 없다: {v1}"
        );

        let v2 = list_page(Lang::En, ConfigSyntax::V2, None, &[]).into_string();
        assert!(
            !v2.contains(&format!(r#"href="{}""#, route::CONVERT_PATH)),
            "v2에 전환 링크가 생겼다: {v2}"
        );
        assert!(
            v2.contains("No conversion back to v1"),
            "역방향이 없다는 사실이 없다: {v2}"
        );
    }

    /// 전환 화면에서도 적대적 문자열이 전부 이스케이프된다(파일 본문을 그대로 그리는 화면이라
    /// 특히 중요하다).
    #[test]
    fn convert_page_escapes_hostile_file_contents() {
        let hostile = r#"<img src=x onerror="alert(1)">"#;
        let preview = ConvertPreview {
            digest: hostile.to_string(),
            profiles: vec![ConvertedProfile {
                name: hostile.to_string(),
                moves: vec![MovedKey {
                    from: hostile.to_string(),
                    to: hostile.to_string(),
                    value: hostile.to_string(),
                }],
            }],
            new_text: format!("{hostile}\u{001b}[31m"),
            truncated_lines: 3,
            bytes_before: 1,
            bytes_after: 1,
        };
        let out = convert_page(Lang::En, &preview).into_string();
        assert!(!out.contains("<img"), "이스케이프되지 않았다: {out}");
        assert!(!out.contains(r#"onerror=""#), "속성이 살아 있다: {out}");
        assert!(out.contains("&lt;img"), "{out}");
    }

    /// URI 칸에는 clear 체크박스가 붙고, 다른 종류에는 붙지 않는다.
    #[test]
    fn only_uri_fields_get_a_clear_checkbox() {
        let uri = field(
            "uri",
            FieldKind::Uri,
            Origin::Direct,
            Some("mongodb://h/db"),
        );
        let out = field_row(Lang::En, &uri).into_string();
        assert!(
            out.contains(&format!(r#"name="uri{}""#, route::CLEAR_SUFFIX)),
            "clear 체크박스 누락: {out}"
        );
        assert!(
            out.contains("Empty means no change"),
            "변경 없음 안내 누락: {out}"
        );

        let text = field(
            "recipient_file",
            FieldKind::Text,
            Origin::Direct,
            Some("/k/age.pub"),
        );
        let out = field_row(Lang::En, &text).into_string();
        assert!(
            !out.contains(route::CLEAR_SUFFIX),
            "clear가 엉뚱한 필드에 붙었다: {out}"
        );
    }
}
