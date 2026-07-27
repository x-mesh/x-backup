//! `/config` — 프로파일 CRUD 폼. config를 **읽고·출처를 추적하고·변경을 검증**한다.
//!
//! ## 왜 웹이 `crate::config`를 직접 부르는가 (최상위 불변식과의 관계)
//! [`crate::web`] 모듈 헤더의 불변식은 웹이 `engine`·`storage`·`crypto`를 직접 부르지
//! 않는다는 것이다 — 백업/복구/prune 같은 **도메인 실행**은 전부 `x-backup <cmd> --json`
//! 자식 프로세스가 한다. config 파싱은 그 불변식의 대상이 아니다. 이유:
//!
//! 1. 도메인 실행이 아니라 **설정 텍스트 해석**이다. 락도, 부작용도, 종료 코드도 없다.
//! 2. 오히려 웹이 자체 파서를 두는 쪽이 불변식을 깬다 — v1/v2 판별, `extends` 우선순위,
//!    알 수 없는 키 거부 같은 판정이 두 벌 존재하면 "웹에서는 되는데 CLI에서는 안 되는
//!    config"가 생긴다. 그래서 값 해석은 [`Config::from_toml_str`]에 **전부 맡기고**
//!    (혼합 문법·`extends` 순환·없는 base·미지 키 거부까지 그 함수가 낸 에러를 그대로
//!    보여준다), 이 파일은 그 위에 **출처(provenance)라는 주석**만 얹는다.
//!
//! ## 함정 1 — 원본 문법(v1/v2)을 조용히 바꾸지 않는다
//! config는 두 문법을 모두 읽는다: v1(복수 `[profiles.p.source]` 중첩)과 v2(단수
//! `[profile.p]` flat + `[defaults]`/`[base.*]`/`extends`). 폼이 저장 때 한쪽으로 통일하면
//! 사용자는 값 하나만 고쳤는데 파일 전체가 다른 문법으로 다시 써진다. 그래서
//! [`ConfigDocument`]는 판별한 [`ConfigSyntax`]를 들고 있고, [`ProfileChange`]가 그 값을
//! 그대로 실어 저장 계층에 넘긴다 — **저장이 문법을 추측할 필요가 없다.**
//!
//! 문법을 바꾸는 길은 **하나뿐이고 명시적이다**: [`convert_form`]이 diff를 보여주고
//! [`convert_submit`]이 그 미리보기의 지문을 확인한 뒤에만 실행한다. 저장 경로에서는 문법을
//! 바꿀 방법이 아예 없다 — [`ConfigStore::apply`]는 [`ProfileChange`]만 받고, 그 값에는
//! "문법을 바꿔라"를 표현할 자리가 없다.
//!
//! ## 함정 2 — 상속을 프로파일에 박아 넣지 않는다
//! v2에서 `[defaults]`/`extends`로 한 번만 적은 공통 정책을, 폼이 "펼쳐진 최종 값"을 각
//! 프로파일에 써 버리면 공통 정책 한 곳을 고치는 일이 영구히 불가능해진다. 이 파일이
//! 그것을 막는 장치는 두 겹이다:
//!
//! 1. **출처를 값과 함께 들고 다닌다.** [`ProfileOrigins`]가 v2 flat 키마다
//!    [`Origin`]을 기록한다. 계산 알고리즘은
//!    [`crate::config::v2`]의 `resolve_flat`을 **그대로 거울처럼** 따라간다(우선순위:
//!    `[defaults]` < `extends` 체인 왼→오 < 자신 키). 값 해석과 출처 판정이 같은 순서를
//!    쓰지 않으면 화면이 거짓말을 한다.
//! 2. **상속된 값은 입력 칸에 미리 채우지 않는다.** 그래서 아무것도 타이핑하지 않은
//!    제출은 [`ProfileChange::set`]을 비운 채 나가고, 상속은 그대로 남는다. 규약 표는
//!    [`crate::web::view::config`] 헤더에 있다.
//!
//! ## 시크릿 — 마스킹이 아니라 부재
//! 이 화면은 시크릿 *값*을 읽지 않는다. `uri_env`/`read_uri_env`/`s3_creds`는 env 변수
//! **이름**이고, 화면은 그 이름과 "그 env가 설정돼 있는가"(불리언)만 싣는다
//! ([`SecretEnvRef`] — 값 필드가 없다). 존재 확인조차 [`std::env::var_os`]로 하고 값을
//! `String`으로 꺼내지 않는다. 평문 `uri`/`read_uri`는 [`RedactedUri`]를 통해서만 화면에
//! 닿는다(생성자가 userinfo를 지운다).
//!
//! ## v1↔v2 전환 — 저장의 부작용이 아니라 별개 화면
//! 운영자가 **의도적으로** v1을 v2로 옮기고 싶을 수 있다(v2는 공통 정책을 `[defaults]`·
//! `extends`로 한 곳에 모을 수 있다). 그 경로는 [`CONVERT_PATH`] 하나이고 두 단계다 —
//! `GET`이 무엇이 어떻게 바뀌는지 보여주고, `POST`는 그 미리보기의 지문을 요구한다. 지문은
//! 미리보기 화면만이 알 수 있으므로 "diff를 보여준 뒤에만 실행"이 규약이 아니라 구조다.
//!
//! **전환은 문법 변환이지 정책 재구성이 아니다** — v1에는 상속이 없으므로 v2로 옮겨도
//! `[defaults]`가 생기지 않는다. 화면이 그 사실을 먼저 말한다(그러지 않으면 운영자가 "v2로
//! 바꿨는데 왜 defaults가 없나"를 버그로 의심한다). 역방향(v2→v1)은 제공하지 않고, 그 이유도
//! 화면에 적는다([`crate::web::view::config`]의 `no_reverse_reason`).
//!
//! ## 저장 — t27이 붙였다
//! [`ConfigStore`]가 저장 지점이다. 검증(이 파일의 책임)과 실제 디스크 반영(t27이 만든
//! [`crate::web::config_write::FileConfigStore`])이 분리되어 있다 — [`apply_and_render`]가
//! 요청마다 `ctx.config_path`로 그 구현을 만들어 주입한다. 원자적 쓰기(temp+fsync+rename)·
//! 직전본 세대 보관·저장 전 `doctor` 검증(exit 4는 저장을 막지 않되 경고를 화면에 남기고,
//! exit 2/3은 막는다)은 전부 그 모듈의 책임이고 근거도 그 모듈 헤더에 있다. [`PendingStore`]/
//! [`store`]는 no-op인 채 남아 있다 — 디스크를 건드리지 않고 검증 경로만 확인하고 싶은
//! 단위 테스트가 계속 쓸 수 있게 하기 위함이다.
//!
//! ## 폼 파싱에 `axum::Form`을 쓰지 않는 이유
//! `Cargo.toml`은 axum을 `default-features = false`로 쓴다. `form` feature를 켜면
//! `serde_urlencoded` 의존성이 늘고, 그 위에서 폼을 받으려면 필드 이름을 나열한
//! `Deserialize` 구조체가 필요하다 — 그러면 [`FIELDS`] 표와 **같은 목록이 두 곳에** 생기고
//! 둘이 어긋나는 순간 조용히 값이 유실된다. 필드 이름이 표에서 나오는 이 화면에는
//! `BTreeMap` 같은 평평한 접근이 맞고, 그건 t6이 `POST /login`에서 이미 쓴 방식이다
//! (`src/web/auth.rs`의 `form_field`). 그 함수는 필드 하나만 뽑는 비공개 함수이고
//! 그 파일은 이 태스크 소관 밖이라, 여기서 모든 쌍을 뽑는 [`FormBody`]를 따로 둔다.
//! **`Cargo.toml`은 건드리지 않는다.**
//!
//! ## 삭제 확인은 최소 수준이다
//! [`delete_submit`]은 프로파일 이름을 그대로 타이핑한 `confirm` 필드를 요구한다. 재인증·
//! 이중 확인 같은 전체 가드는 t21 소관이고, 이 확인 단계는 **t21이 승격시킬 대상**이다.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path as FsPath;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use maud::Markup;
use toml::value::{Table, Value};

use crate::config::file::{Config, Profile};
use crate::error::{Result, XBackupError};
use crate::i18n::Lang;
use crate::web::audit::{AuditEvent, AuditOutcome, AuditReceipt};
use crate::web::job::args::{ProfileName, MAX_PROFILE_NAME_LEN};
use crate::web::view::components::Level;
use crate::web::view::config as view;
use crate::web::view::config::{
    ConfigSyntax, DestinationView, FieldGroup, FieldKind, FieldView, FormMode, Origin, ProfileView,
    RedactedUri, SecretEnvRef,
};
use crate::web::view::layout;
use crate::web::ServeConfig;

// ---------------------------------------------------------------------------
// 경로·폼 프로토콜 상수 — 라우터·마크업·테스트가 문자열을 공유한다
// ---------------------------------------------------------------------------

/// 목록 화면 경로.
pub const CONFIG_PATH: &str = "/config";
/// `<title>`·화면 제목·내비게이션 라벨이 공유하는 이름. 기술용어이므로 영문 고정.
pub const CONFIG_TITLE: &str = "Config";
/// 생성 폼 경로.
pub const NEW_PATH: &str = "/config/new";
/// 편집 폼 경로 접두(뒤에 `/<profile>`이 붙는다).
pub const CONFIG_EDIT_PREFIX: &str = "/config/edit";
/// 삭제 확인 화면 경로 접두(뒤에 `/<profile>`이 붙는다).
pub const CONFIG_DELETE_PREFIX: &str = "/config/delete";
/// 편집 폼 라우트 패턴(axum 0.8 `{param}` 문법).
pub const EDIT_ROUTE: &str = "/config/edit/{profile}";
/// 삭제 확인 라우트 패턴.
pub const DELETE_CONFIRM_ROUTE: &str = "/config/delete/{profile}";
/// 생성·수정 제출 경로(`POST`). 프로파일 이름을 **본문**으로 받으므로 경로가 정적이다 —
/// 정적 경로와 동적 경로가 같은 접두를 다투는 상황을 애초에 만들지 않는다.
pub const SAVE_PATH: &str = "/config/save";
/// 삭제 제출 경로(`POST`).
pub const DELETE_PATH: &str = "/config/delete";
/// v1→v2 전환 경로. `GET`은 diff 미리보기, `POST`는 실행이다 — **같은 경로**를 쓰는 이유는
/// 그 둘이 한 작업의 두 단계이고, 미리보기 없이 실행할 수 없다는 규약을 경로 하나로 읽히게
/// 하기 위함이다([`convert_submit`] doc).
pub const CONVERT_PATH: &str = "/config/convert";

/// 대상 프로파일 이름을 싣는 폼 필드.
pub const FIELD_PROFILE: &str = "profile";
/// 작업 종류를 싣는 폼 필드(`create`/`update`).
pub const FIELD_OP: &str = "op";
/// 삭제 확인 입력 필드(프로파일 이름을 그대로 타이핑해야 한다).
pub const FIELD_CONFIRM: &str = "confirm";
/// 전환 확인 필드 — 미리보기가 계산한 **원본 파일 지문**을 그대로 되돌려 보낸다.
///
/// 이 필드가 전환의 관문이다. 값은 미리보기 화면이 아니면 알 수 없으므로(파일 내용의
/// sha256), 미리보기를 열지 않은 요청은 채울 수 없다 — 그래서 "diff를 보여준 뒤에만 실행"이
/// 예의가 아니라 **구조**가 된다. 같은 값이 미리보기와 실제 파일이 동일한지도 증명한다.
pub const FIELD_FROM_DIGEST: &str = "from_digest";
/// [`FIELD_OP`]의 생성 값.
pub const OP_CREATE: &str = "create";
/// [`FIELD_OP`]의 수정 값.
pub const OP_UPDATE: &str = "update";
/// URI 필드를 **명시적으로 지우는** 체크박스의 이름 접미(`uri` → `uri_clear`).
///
/// URI 칸은 빈 값이 "변경 없음"이므로(가린 표기를 미리 채울 수 없기 때문에) 지우는 뜻을
/// 담을 자리가 따로 필요하다.
pub const CLEAR_SUFFIX: &str = "_clear";

/// 폼 값 하나의 최대 길이(바이트).
///
/// config에 들어가는 값은 경로·버킷 이름·env 이름이 대부분이고 전부 이보다 훨씬 짧다.
/// 상한의 목적은 정상 입력을 자르는 게 아니라, 검증을 통과한 거대한 문자열이 config 파일과
/// 그것을 읽는 모든 명령의 메모리로 번지는 경로를 막는 것이다.
pub const MAX_FIELD_VALUE_LEN: usize = 1024;

/// env 변수 **이름**의 최대 길이(바이트). POSIX 관행보다 넉넉하게 잡되 무한은 아니다.
const MAX_ENV_NAME_LEN: usize = 128;

/// 자격증명이 섞일 수 있는 두 필드의 키 — 이 둘만 [`FieldKind::Uri`] 규약을 받는다.
const KEY_URI: &str = "uri";
const KEY_READ_URI: &str = "read_uri";

/// v2 `extends` 키 이름. 출처 계산에서 값 키와 구분해 건너뛴다.
const KEY_EXTENDS: &str = "extends";

/// 편집 폼의 프로파일 링크. **검증을 통과한 이름에만 쓴다** — 허용 문자가 ASCII
/// 영숫자/`_`/`-`뿐이라 퍼센트 인코딩이 필요하지 않다는 전제를 깐다
/// ([`ProfileName`] 규칙).
pub fn edit_href(profile: &str) -> String {
    format!("{CONFIG_EDIT_PREFIX}/{profile}")
}

/// 삭제 확인 화면 링크. [`edit_href`]와 같은 전제.
pub fn delete_href(profile: &str) -> String {
    format!("{CONFIG_DELETE_PREFIX}/{profile}")
}

// ---------------------------------------------------------------------------
// 필드 표 — 화면·검증·출처 추적이 공유하는 단일 목록
// ---------------------------------------------------------------------------

/// 프로파일에서 표시값을 뽑는 함수.
type Getter = fn(&Profile) -> Option<String>;

/// 편집 가능한 필드 하나의 정의.
///
/// 이 표가 **단일 진실 공급원**이다 — 폼 input 이름, 검증 규칙, 출처를 찾을 키, 표시값을
/// 뽑는 방법이 한 줄에 모여 있다. 화면·파싱·변경 조립이 모두 이 표를 순회하므로 필드를
/// 추가할 때 고칠 곳이 하나다.
struct FieldSpec {
    /// 폼 input 이름 = v2 flat 키. 폼이 config 문법을 그대로 가르친다.
    key: &'static str,
    /// 소속 섹션.
    group: FieldGroup,
    /// 입력 종류(렌더 모양 + 검증 규칙).
    kind: FieldKind,
    /// 출처를 찾을 v2 flat 키 후보 — **구체적인 것부터**.
    ///
    /// 첫 히트가 이 필드의 출처다. 예: `compress_level`은 명시 키
    /// `compress_level`이 있으면 그것, 없으면 compact `compress`가 출처다. 그리고 이
    /// 필드를 새로 쓸 때는 뒤쪽 후보 중 **직접 적힌 것**을 지운다 — 그러지 않으면
    /// `compress = "zstd:6"`과 `compress_level = 7`이 한 프로파일에 공존해 파일을 읽는
    /// 사람이 어느 쪽이 이기는지 추측해야 한다.
    origin_keys: &'static [&'static str],
    /// 실효 표시값을 뽑는다.
    get: Getter,
    /// destination 편집 필드인지 — destination이 편집 가능하지 않은 프로파일에서는
    /// 렌더하지도, 제출을 받지도 않는다.
    destination_field: bool,
}

/// `engine` 허용 값. 권위는 `crate::pipeline::backup`의 `Engine::from_str`이다 —
/// 여기 목록은 그 파서가 받는 값의 사본이고, 오타를 폼에서 막기 위한 것이다.
const ENGINES: &[&str] = &["native", "mongodump"];
/// `backup_type` 허용 값(`src/config/file.rs`의 `ModeConfig::backup_type` doc).
const BACKUP_TYPES: &[&str] = &["full", "incr"];
/// `output_mode` 허용 값(같은 doc).
const OUTPUT_MODES: &[&str] = &["progress", "quiet"];
/// `compress_algorithm` 허용 값. 권위는 `crate::compress::ALGORITHM_ZSTD`이며 복구 경로가
/// zstd 외를 거부한다(`src/pipeline/stage.rs`).
const COMPRESS_ALGORITHMS: &[&str] = &["zstd"];
/// `encrypt_algorithm` 허용 값. 권위는 `crate::crypto`의 `ALGORITHM_AGE`/`ALGORITHM_AES_GCM`.
const ENCRYPT_ALGORITHMS: &[&str] = &["age", "aes-256-gcm"];

/// 편집 가능한 필드 전부.
#[allow(clippy::type_complexity)]
const FIELDS: &[FieldSpec] = &[
    // ── source ──
    FieldSpec {
        key: KEY_URI,
        group: FieldGroup::Source,
        kind: FieldKind::Uri,
        origin_keys: &["uri"],
        get: |p| redacted_uri_of(p, KEY_URI).map(|u| u.shown().to_string()),
        destination_field: false,
    },
    FieldSpec {
        key: "uri_env",
        group: FieldGroup::Source,
        kind: FieldKind::EnvName,
        origin_keys: &["uri_env"],
        get: |p| p.source.uri_env.clone(),
        destination_field: false,
    },
    FieldSpec {
        key: KEY_READ_URI,
        group: FieldGroup::Source,
        kind: FieldKind::Uri,
        origin_keys: &["read_uri"],
        get: |p| redacted_uri_of(p, KEY_READ_URI).map(|u| u.shown().to_string()),
        destination_field: false,
    },
    FieldSpec {
        key: "read_uri_env",
        group: FieldGroup::Source,
        kind: FieldKind::EnvName,
        origin_keys: &["read_uri_env"],
        get: |p| p.source.read_uri_env.clone(),
        destination_field: false,
    },
    FieldSpec {
        key: "prefer_secondary",
        group: FieldGroup::Source,
        kind: FieldKind::Bool,
        origin_keys: &["prefer_secondary"],
        get: |p| Some(p.source.prefer_secondary.to_string()),
        destination_field: false,
    },
    FieldSpec {
        key: "connect_timeout_secs",
        group: FieldGroup::Source,
        kind: FieldKind::Number,
        origin_keys: &["connect_timeout_secs"],
        get: |p| p.source.connect_timeout_secs.map(|v| v.to_string()),
        destination_field: false,
    },
    // ── mode ──
    FieldSpec {
        key: "engine",
        group: FieldGroup::Mode,
        kind: FieldKind::Choice(ENGINES),
        origin_keys: &["engine"],
        get: |p| Some(p.mode.engine.clone()),
        destination_field: false,
    },
    FieldSpec {
        key: "backup_type",
        group: FieldGroup::Mode,
        kind: FieldKind::Choice(BACKUP_TYPES),
        origin_keys: &["backup_type"],
        get: |p| Some(p.mode.backup_type.clone()),
        destination_field: false,
    },
    FieldSpec {
        key: "output_mode",
        group: FieldGroup::Mode,
        kind: FieldKind::Choice(OUTPUT_MODES),
        origin_keys: &["output_mode"],
        get: |p| Some(p.mode.output.clone()),
        destination_field: false,
    },
    FieldSpec {
        key: "precheck",
        group: FieldGroup::Mode,
        kind: FieldKind::Bool,
        origin_keys: &["precheck"],
        get: |p| Some(p.mode.precheck.to_string()),
        destination_field: false,
    },
    // ── destination ──
    FieldSpec {
        key: "dest_name",
        group: FieldGroup::Destination,
        kind: FieldKind::Text,
        origin_keys: &["dest_name"],
        get: |p| primary_destination(p).and_then(|d| d.name.clone()),
        destination_field: true,
    },
    FieldSpec {
        key: "dest",
        group: FieldGroup::Destination,
        kind: FieldKind::Dest,
        // compact `dest` 또는 명시 `s3_bucket`/`s3_prefix` 중 무엇으로 적혀 있어도 이
        // 필드가 그 값을 대표한다.
        origin_keys: &["dest", "s3_bucket", "s3_prefix"],
        get: dest_compact_of,
        destination_field: true,
    },
    FieldSpec {
        key: "s3_region",
        group: FieldGroup::Destination,
        kind: FieldKind::Text,
        origin_keys: &["s3_region"],
        get: |p| primary_destination(p).and_then(|d| d.s3.as_ref()?.region.clone()),
        destination_field: true,
    },
    FieldSpec {
        key: "s3_endpoint",
        group: FieldGroup::Destination,
        kind: FieldKind::Text,
        origin_keys: &["s3_endpoint"],
        get: |p| primary_destination(p).and_then(|d| d.s3.as_ref()?.endpoint.clone()),
        destination_field: true,
    },
    FieldSpec {
        key: "s3_creds",
        group: FieldGroup::Destination,
        kind: FieldKind::EnvName,
        origin_keys: &["s3_creds"],
        get: |p| primary_destination(p).and_then(|d| d.s3.as_ref()?.credentials_env.clone()),
        destination_field: true,
    },
    // ── compression ──
    FieldSpec {
        key: "compress_algorithm",
        group: FieldGroup::Compression,
        kind: FieldKind::Choice(COMPRESS_ALGORITHMS),
        origin_keys: &["compress_algorithm", "compress"],
        get: |p| Some(p.features.compression.algorithm.clone()),
        destination_field: false,
    },
    FieldSpec {
        key: "compress_level",
        group: FieldGroup::Compression,
        kind: FieldKind::Number,
        origin_keys: &["compress_level", "compress"],
        get: |p| Some(p.features.compression.level.to_string()),
        destination_field: false,
    },
    // ── encryption ──
    FieldSpec {
        key: "encrypt",
        group: FieldGroup::Encryption,
        kind: FieldKind::Bool,
        origin_keys: &["encrypt"],
        get: |p| Some(p.features.encryption.enabled.to_string()),
        destination_field: false,
    },
    FieldSpec {
        key: "encrypt_algorithm",
        group: FieldGroup::Encryption,
        kind: FieldKind::Choice(ENCRYPT_ALGORITHMS),
        origin_keys: &["encrypt_algorithm", "encrypt"],
        get: |p| Some(p.features.encryption.algorithm.clone()),
        destination_field: false,
    },
    FieldSpec {
        key: "recipient_file",
        group: FieldGroup::Encryption,
        kind: FieldKind::Text,
        origin_keys: &["recipient_file", "encrypt"],
        get: |p| p.features.encryption.recipient_file.clone(),
        destination_field: false,
    },
    // ── incremental ──
    FieldSpec {
        key: "incr_interval",
        group: FieldGroup::Incremental,
        kind: FieldKind::Text,
        origin_keys: &["incr_interval"],
        get: |p| Some(p.features.incremental.interval.clone()),
        destination_field: false,
    },
    FieldSpec {
        // 자유 텍스트로 둔다 — CLI에 `on_gap`의 닫힌 값 목록이 없다. 폼이 목록을
        // 발명하면 웹이 CLI보다 좁아진다(모듈 헤더 "자체 파서를 두지 않는다"와 같은 이유).
        key: "incr_on_gap",
        group: FieldGroup::Incremental,
        kind: FieldKind::Text,
        origin_keys: &["incr_on_gap"],
        get: |p| Some(p.features.incremental.on_gap.clone()),
        destination_field: false,
    },
    FieldSpec {
        key: "pg_logical",
        group: FieldGroup::Incremental,
        kind: FieldKind::Bool,
        origin_keys: &["pg_logical"],
        get: |p| Some(p.features.incremental.pg_logical.to_string()),
        destination_field: false,
    },
    FieldSpec {
        key: "mysql_binlog",
        group: FieldGroup::Incremental,
        kind: FieldKind::Bool,
        origin_keys: &["mysql_binlog"],
        get: |p| Some(p.features.incremental.mysql_binlog.to_string()),
        destination_field: false,
    },
    // ── retention ──
    FieldSpec {
        key: "keep_full",
        group: FieldGroup::Retention,
        kind: FieldKind::Number,
        origin_keys: &["keep_full"],
        get: |p| p.retention.keep_full.map(|v| v.to_string()),
        destination_field: false,
    },
    FieldSpec {
        key: "keep_days",
        group: FieldGroup::Retention,
        kind: FieldKind::Number,
        origin_keys: &["keep_days"],
        get: |p| p.retention.keep_days.map(|v| v.to_string()),
        destination_field: false,
    },
    FieldSpec {
        key: "keep_last",
        group: FieldGroup::Retention,
        kind: FieldKind::Number,
        origin_keys: &["keep_last"],
        get: |p| p.retention.keep_last.map(|v| v.to_string()),
        destination_field: false,
    },
    FieldSpec {
        key: "recovery_window_days",
        group: FieldGroup::Retention,
        kind: FieldKind::Number,
        origin_keys: &["recovery_window_days"],
        get: |p| p.retention.recovery_window_days.map(|v| v.to_string()),
        destination_field: false,
    },
    FieldSpec {
        key: "min_redundancy",
        group: FieldGroup::Retention,
        kind: FieldKind::Number,
        origin_keys: &["min_redundancy"],
        get: |p| p.retention.min_redundancy.map(|v| v.to_string()),
        destination_field: false,
    },
];

/// 평문 URI 두 필드의 표시값을 만든다.
///
/// **원문은 이 함수 밖으로 나가지 않는다** — 반환 타입이 [`RedactedUri`]이고 그 타입에는
/// 원문 접근자가 없다. 그래서 `Profile::source::uri`를 읽는 코드 경로가 이 함수 하나뿐이면
/// 화면에 자격증명이 실릴 수 없다.
fn redacted_uri_of(profile: &Profile, key: &str) -> Option<RedactedUri> {
    let raw = match key {
        KEY_URI => profile.source.uri.as_deref(),
        KEY_READ_URI => profile.source.read_uri.as_deref(),
        _ => None,
    }?;
    Some(RedactedUri::from_raw(raw))
}

/// 첫 destination(primary). `destinations`(복수)가 있으면 그 첫 항목, 없으면 단일
/// `destination`([`Profile::effective_destinations`] 규칙과 동일).
fn primary_destination(profile: &Profile) -> Option<&crate::config::file::DestinationConfig> {
    profile.effective_destinations().into_iter().next()
}

/// primary destination을 compact `dest` 표기로 접는다.
///
/// `local`/`s3` 외의 유형이면 `None`을 돌려준다 — 그런 프로파일은
/// [`destination_editability`]가 편집 대상에서 제외하므로, 여기서 억지로 문자열을 만들어
/// 잘못된 값을 입력 칸에 미리 채우는 일이 없다.
fn dest_compact_of(profile: &Profile) -> Option<String> {
    let dest = primary_destination(profile)?;
    match dest.r#type.as_deref()? {
        "local" => dest.path.as_deref().map(|p| format!("local:{p}")),
        "s3" => {
            let s3 = dest.s3.as_ref()?;
            let bucket = s3.bucket.as_deref()?;
            match s3.prefix.as_deref().filter(|p| !p.is_empty()) {
                Some(prefix) => Some(format!("s3:{bucket}/{prefix}")),
                None => Some(format!("s3:{bucket}")),
            }
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 출처(provenance) — 값이 어디서 왔는가
// ---------------------------------------------------------------------------

/// 프로파일 하나의 v2 flat 키별 출처.
///
/// v1 config에서도 같은 flat 키 어휘를 쓴다([`V1_PATHS`]) — 어휘가 하나여야 폼·검증·저장이
/// 문법마다 갈라지지 않는다. v1에는 상속 표면이 없으므로 기록되는 출처는 전부
/// [`Origin::Direct`]다.
#[derive(Debug, Clone, Default)]
pub struct ProfileOrigins(BTreeMap<String, Origin>);

impl ProfileOrigins {
    /// 후보 키를 앞에서부터 보고 **첫 히트의 출처**를 돌려준다. 하나도 없으면
    /// [`Origin::Builtin`](config에 없고 내장 기본값이 채웠다).
    pub fn origin_of(&self, candidates: &[&str]) -> Origin {
        for key in candidates {
            if let Some(origin) = self.0.get(*key) {
                return origin.clone();
            }
        }
        Origin::Builtin
    }

    /// 이 프로파일 테이블에 **직접** 적힌 키 목록(정렬됨). t28이 "무엇을 그대로 남겨야
    /// 하는가"를 판단하는 근거다.
    pub fn direct_keys(&self) -> Vec<&str> {
        self.0
            .iter()
            .filter(|(_, origin)| origin.is_direct())
            .map(|(key, _)| key.as_str())
            .collect()
    }

    /// 기록된 (키, 출처) 전부.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &Origin)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v))
    }
}

/// v1 nested 경로 → flat 키 대응표.
///
/// [`crate::config::v2`]의 `expand_profile`이 하는 flat→nested 변환의 **역방향**이다.
/// 한 flat 키에 여러 nested 경로가 대응할 수 있다(compact `dest` 하나가 `destination.type`·
/// `destination.path`·`destination.s3.{bucket,prefix}`를 함께 대표한다 — 그 4경로가 갈라지는
/// 이유는 [`crate::web::config_write`] 모듈 헤더에 있다).
///
/// **이 표는 v1 프로파일 스키마 전체를 덮어야 한다.** 읽기(출처 추적)와 쓰기(t27·t28의 저장),
/// 그리고 v1→v2 전환이 전부 이 표를 근거로 돌기 때문이다 — 빠진 경로가 있으면 전환이 그 값을
/// 옮길 자리를 찾지 못한다. 다행히 그 경우 조용히 잃지는 않는다: 전환은 대응되지 않는 경로를
/// 발견하면 중단하고(`config_write`의 `reject_unmapped_paths`), 그래도 새어 나가면 등가성
/// 게이트가 잡는다. 그래도 표를 먼저 채우는 것이 맞다.
///
/// `hooks.*`는 아직 폼에 편집 필드가 없다([`FIELDS`]에 없다). 그래도 대응을 적어 두는 이유는
/// 위와 같다 — v1 파일이 훅을 갖고 있을 수 있고, 그 파일을 v2로 전환할 수 있어야 한다.
pub(crate) const V1_PATHS: &[(&str, &str)] = &[
    ("source.uri", "uri"),
    ("source.uri_env", "uri_env"),
    ("source.read_uri", "read_uri"),
    ("source.read_uri_env", "read_uri_env"),
    ("source.prefer_secondary", "prefer_secondary"),
    ("source.connect_timeout_secs", "connect_timeout_secs"),
    ("mode.backup_type", "backup_type"),
    ("mode.output", "output_mode"),
    ("mode.precheck", "precheck"),
    ("mode.engine", "engine"),
    ("destination.name", "dest_name"),
    ("destination.type", "dest"),
    ("destination.path", "dest"),
    ("destination.s3.bucket", "dest"),
    ("destination.s3.prefix", "dest"),
    ("destination.s3.region", "s3_region"),
    ("destination.s3.endpoint", "s3_endpoint"),
    ("destination.s3.credentials_env", "s3_creds"),
    ("features.compression.algorithm", "compress_algorithm"),
    ("features.compression.level", "compress_level"),
    ("features.encryption.enabled", "encrypt"),
    ("features.encryption.algorithm", "encrypt_algorithm"),
    ("features.encryption.recipient_file", "recipient_file"),
    ("features.incremental.interval", "incr_interval"),
    ("features.incremental.on_gap", "incr_on_gap"),
    ("features.incremental.pg_logical", "pg_logical"),
    ("features.incremental.mysql_binlog", "mysql_binlog"),
    ("retention.keep_full", "keep_full"),
    ("retention.keep_days", "keep_days"),
    ("retention.keep_last", "keep_last"),
    ("retention.recovery_window_days", "recovery_window_days"),
    ("retention.min_redundancy", "min_redundancy"),
    ("hooks.pre_backup", "hook_pre_backup"),
    ("hooks.post_backup", "hook_post_backup"),
    ("hooks.pre_restore", "hook_pre_restore"),
    ("hooks.post_restore", "hook_post_restore"),
    ("hooks.pre_prune", "hook_pre_prune"),
    ("hooks.post_prune", "hook_post_prune"),
    ("hooks.on_error", "hook_on_error"),
    ("hooks.hook_timeout_secs", "hook_timeout_secs"),
];

/// 점으로 구분된 경로를 테이블에서 찾는다.
fn lookup_path<'a>(table: &'a Table, path: &str) -> Option<&'a Value> {
    let mut current = table;
    let mut segments = path.split('.').peekable();
    while let Some(segment) = segments.next() {
        let value = current.get(segment)?;
        if segments.peek().is_none() {
            return Some(value);
        }
        current = value.as_table()?;
    }
    None
}

/// v1 `[profiles.<name>]` 트리에서 프로파일별 출처를 모은다(전부 [`Origin::Direct`]).
fn v1_origins(profiles: &Table) -> BTreeMap<String, ProfileOrigins> {
    let mut out = BTreeMap::new();
    for (name, profile_value) in profiles {
        let mut origins = BTreeMap::new();
        if let Some(table) = profile_value.as_table() {
            for (path, flat) in V1_PATHS {
                if lookup_path(table, path).is_some() {
                    origins.insert((*flat).to_string(), Origin::Direct);
                }
            }
        }
        out.insert(name.clone(), ProfileOrigins(origins));
    }
    out
}

/// v2 표면에서 프로파일별 출처를 모은다.
fn v2_origins(root: &Table) -> BTreeMap<String, ProfileOrigins> {
    let empty = Table::new();
    let defaults = root
        .get("defaults")
        .and_then(Value::as_table)
        .unwrap_or(&empty);
    let bases = root.get("base").and_then(Value::as_table).unwrap_or(&empty);
    let profiles = root
        .get("profile")
        .and_then(Value::as_table)
        .unwrap_or(&empty);

    let mut out = BTreeMap::new();
    for (name, profile_value) in profiles {
        let mut origins = BTreeMap::new();
        if let Some(table) = profile_value.as_table() {
            let mut visiting = vec![name.clone()];
            resolve_origins(
                name,
                table,
                defaults,
                bases,
                profiles,
                &mut visiting,
                true,
                &mut origins,
            );
        }
        out.insert(name.clone(), ProfileOrigins(origins));
    }
    out
}

/// 한 엔티티의 실효 키별 출처를 계산한다.
///
/// **[`crate::config::v2`]의 `resolve_flat`을 그대로 거울처럼 따라간다** — 낮은 우선순위부터
/// `[defaults]` → `extends` 체인(왼→오) → 자신 키. 같은 순서를 쓰지 않으면 "화면이 말하는
/// 출처"와 "실제로 이긴 값"이 갈라진다. 재귀 단계에서도 `[defaults]`를 다시 깔는 것까지
/// 같다(그래서 `extends = ["a","b"]`에서 b가 적지 않은 defaults 키가 a의 기여를 덮는
/// 로더의 성질이 화면에도 그대로 반영된다 — 그 성질을 여기서 "고치면" 화면이 거짓말을 한다).
///
/// `top`이 false면 자신 키는 [`Origin::Inherited`]로 기록된다 — `from`은 그 키를 **실제로
/// 적어 둔** 엔티티 이름이다(중간 경유지가 아니다).
///
/// `extends` 순환·없는 base는 [`Config::from_toml_str`]가 먼저 거부하므로 여기 도달하지
/// 않는다. 그래도 `visiting`으로 끊는다 — 이 함수가 그 검증에 의존해 무한 재귀하지 않도록.
#[allow(clippy::too_many_arguments)]
fn resolve_origins(
    entity_name: &str,
    entity: &Table,
    defaults: &Table,
    bases: &Table,
    profiles: &Table,
    visiting: &mut Vec<String>,
    top: bool,
    out: &mut BTreeMap<String, Origin>,
) {
    // 1) [defaults] — 최하위.
    for key in defaults.keys() {
        if key != KEY_EXTENDS {
            out.insert(key.clone(), Origin::Defaults);
        }
    }

    // 2) extends 체인 — 왼→오, 뒤가 우선.
    if let Some(names) = entity.get(KEY_EXTENDS).map(extends_names) {
        for base_name in names {
            if visiting.contains(&base_name) {
                continue; // 순환 — 로더가 이미 거부했다. 여기서는 재귀만 끊는다.
            }
            let Some(base_entity) = bases
                .get(&base_name)
                .or_else(|| profiles.get(&base_name))
                .and_then(Value::as_table)
            else {
                continue; // 없는 base — 로더가 이미 거부했다.
            };
            visiting.push(base_name.clone());
            resolve_origins(
                &base_name,
                base_entity,
                defaults,
                bases,
                profiles,
                visiting,
                false,
                out,
            );
            visiting.pop();
        }
    }

    // 3) 자신 키 — 최상위 우선.
    let own_origin = if top {
        Origin::Direct
    } else {
        Origin::Inherited {
            from: entity_name.to_string(),
        }
    };
    for key in entity.keys() {
        if key != KEY_EXTENDS {
            out.insert(key.clone(), own_origin.clone());
        }
    }
}

/// `extends` 값(문자열 또는 문자열 배열)을 이름 목록으로. 형태가 어긋난 항목은 건너뛴다 —
/// 그 거부는 로더의 일이다.
fn extends_names(value: &Value) -> Vec<String> {
    match value {
        Value::String(s) => vec![s.clone()],
        Value::Array(items) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// config 문서 읽기
// ---------------------------------------------------------------------------

/// 읽어서 검증까지 끝난 config 문서 — 값은 로더가 해석하고, 출처는 이 파일이 얹는다.
#[derive(Debug, Clone)]
pub struct ConfigDocument {
    /// 원본 문법. 저장이 이것을 **보존해야** 한다(모듈 헤더 함정 1).
    pub syntax: ConfigSyntax,
    /// 로더가 해석한 값(v2면 상속이 이미 펼쳐진 상태).
    pub config: Config,
    /// 프로파일별 출처. 키는 config에 적힌 프로파일 이름.
    pub origins: BTreeMap<String, ProfileOrigins>,
}

/// config를 읽어 오지 못한 이유.
///
/// 세 갈래로 나누는 이유는 운영자가 봐야 할 곳이 각각 다르기 때문이다 — 서버 기동 인자,
/// 파일 권한, config 내용.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
    /// 서버가 `--config` 없이 떴다.
    NotWired,
    /// 파일을 읽을 수 없다(권한·경로).
    Read(String),
    /// 파싱·검증 실패. 로더가 낸 메시지를 그대로 담는다(v1/v2 혼합, `extends` 순환,
    /// 없는 base, 알 수 없는 키가 전부 여기로 온다).
    Parse(String),
}

impl LoadError {
    /// 오류 화면에 그대로 들어가는 설명 문장.
    pub fn explain(&self, lang: Lang) -> String {
        match self {
            LoadError::NotWired => lang
                .sel(
                    "This console was started without a config file, so there is nothing to edit.",
                    "이 콘솔은 config 파일 없이 기동했습니다 — 편집할 대상이 없습니다.",
                )
                .to_string(),
            LoadError::Read(detail) => format!(
                "{} {detail}",
                lang.sel(
                    "The config file could not be read:",
                    "config 파일을 읽을 수 없습니다:",
                )
            ),
            LoadError::Parse(detail) => format!(
                "{} {detail}",
                lang.sel(
                    "The config file could not be parsed:",
                    "config 파일을 해석할 수 없습니다:",
                )
            ),
        }
    }

    /// 화면 레벨. 전부 "점검 결과를 얻지 못했다"이므로 [`Level::Error`]다
    /// (`routes::doctor`의 `Verdict::Misconfigured`와 같은 판단).
    pub fn level(&self) -> Level {
        Level::Error
    }
}

impl ConfigDocument {
    /// 파일에서 읽는다. **[`load_with_source`](Self::load_with_source)와 함께 이 모듈의 유일한
    /// 파일시스템 접점이다.**
    pub fn load(path: Option<&FsPath>) -> std::result::Result<Self, LoadError> {
        Self::load_with_source(path).map(|(document, _)| document)
    }

    /// 파일에서 읽고 **원본 텍스트까지** 돌려준다.
    ///
    /// v1→v2 전환은 파싱 결과가 아니라 원본 텍스트를 입력으로 받는다 — 파일 **전체**를 다시
    /// 쓰므로 `profiles` 밖의 최상위 섹션(`default_profile`·`[output]`)을 그대로 옮겨야 하고,
    /// 미리보기와 실행이 같은 파일을 가리키는지 증명할 지문도 그 텍스트에서 나온다.
    /// [`ConfigDocument`]에 텍스트를 필드로 달지 않은 이유는 그 값이 시크릿(평문 `uri`)을 담을
    /// 수 있어서다 — 목록·폼을 그리는 모든 경로가 그것을 들고 다닐 필요가 없다.
    pub fn load_with_source(
        path: Option<&FsPath>,
    ) -> std::result::Result<(Self, String), LoadError> {
        let path = path.ok_or(LoadError::NotWired)?;
        let text = std::fs::read_to_string(path).map_err(|e| LoadError::Read(e.to_string()))?;
        let document = Self::parse(&text)?;
        Ok((document, text))
    }

    /// TOML 텍스트를 문서로 접는다 — **순수 함수.**
    ///
    /// 값 해석과 모든 거부 판정을 [`Config::from_toml_str`]에 먼저 맡긴다(모듈 헤더
    /// "왜 웹이 `crate::config`를 직접 부르는가"). 그 다음에만 출처를 계산한다 — 검증되지
    /// 않은 입력에 대해 출처를 계산하면 로더가 거부할 파일에 대해 화면이 그럴듯한 표를
    /// 그린다.
    pub fn parse(text: &str) -> std::result::Result<Self, LoadError> {
        let config = Config::from_toml_str(text).map_err(|e| LoadError::Parse(e.to_string()))?;
        let root: Value = toml::from_str(text).map_err(|e| LoadError::Parse(e.to_string()))?;
        let empty = Table::new();
        let table = root.as_table().unwrap_or(&empty);

        let has_v1 = table.contains_key("profiles");
        let has_v2 = table.contains_key("profile")
            || table.contains_key("base")
            || table.contains_key("defaults");
        // 혼합은 위 `from_toml_str`가 이미 거부했다 — 여기 도달하면 둘 중 하나거나 둘 다
        // 아니다. 그래도 v1을 먼저 보는 순서를 고정해 판정이 흔들리지 않게 한다.
        let (syntax, origins) = if has_v1 {
            (
                ConfigSyntax::V1,
                v1_origins(
                    table
                        .get("profiles")
                        .and_then(Value::as_table)
                        .unwrap_or(&empty),
                ),
            )
        } else if has_v2 {
            (ConfigSyntax::V2, v2_origins(table))
        } else {
            (ConfigSyntax::Empty, BTreeMap::new())
        };

        Ok(Self {
            syntax,
            config,
            origins,
        })
    }

    /// 프로파일 하나의 출처(없으면 빈 것 — 전부 [`Origin::Builtin`]으로 접힌다).
    pub fn origins_of(&self, name: &str) -> ProfileOrigins {
        self.origins.get(name).cloned().unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// 화면 모델 조립
// ---------------------------------------------------------------------------

/// env 이름이 이 프로세스에 설정돼 있는지 판정한다.
///
/// [`std::env::var_os`]를 쓰고 값을 `String`으로 꺼내지 않는다 — 이 화면은 시크릿 값을
/// 알 필요가 없고, 알지 않는 것이 가장 확실한 누출 방어다. 빈 값은 "미설정"으로 본다
/// (`crate::config::merged`의 `resolve_uri_pair`가 같은 판정을 한다 — 빈 env는 폴백을
/// 유발하므로 운영자에게는 설정되지 않은 것과 같다).
fn env_is_set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty())
}

/// destination을 폼에서 편집할 수 있는지와, 못 하는 이유.
///
/// 편집을 막는 두 경우 모두 "폼이 조용히 사본을 떨어뜨릴 수 있는" 상황이다:
///
/// 1. destination이 여러 개 — 제출 때 필드 하나가 빠지면 offsite 사본이 사라진다.
/// 2. primary destination의 유형이 `local`/`s3`가 아니거나 compact 표기로 접히지 않는다 —
///    입력 칸에 미리 채울 올바른 값이 없으므로, 채우면 잘못된 값을 채우게 되고 비우면
///    빈 제출이 destination을 지운다.
fn destination_editability(lang: Lang, profile: &Profile) -> (bool, Option<String>) {
    if profile.destinations.len() > 1 {
        return (
            false,
            Some(
                lang.sel(
                    "This profile has more than one destination. The form does not rewrite that list: a single missing field on submit would silently drop an offsite copy. Edit the destination list in the config file.",
                    "이 프로파일에는 destination이 여러 개입니다. 폼은 그 목록을 다시 쓰지 않습니다 — 제출 때 필드 하나가 빠지면 offsite 사본이 조용히 사라집니다. destination 목록은 config 파일에서 편집하세요.",
                )
                .to_string(),
            ),
        );
    }
    if profile.is_endpoint_only() {
        // destination이 아예 없는 endpoint 전용 프로파일 — 새로 하나 붙이는 것은 안전하다.
        return (true, None);
    }
    if dest_compact_of(profile).is_none() {
        return (
            false,
            Some(
                lang.sel(
                    "The destination of this profile does not fold into the compact local:/path or s3:bucket/prefix form, so the form has no correct value to pre-fill. Edit it in the config file.",
                    "이 프로파일의 destination이 compact 표기(local:/path·s3:bucket/prefix)로 접히지 않아 폼에 미리 채울 올바른 값이 없습니다. config 파일에서 편집하세요.",
                )
                .to_string(),
            ),
        );
    }
    (true, None)
}

/// 프로파일 하나의 화면 모델을 만든다 — **순수 함수는 아니다**(env 존재 확인을 한다).
///
/// `probe`는 env 존재 확인 함수다. 테스트가 프로세스 환경을 건드리지 않고 두 경로
/// (설정됨/미설정)를 검증할 수 있게 인자로 받는다(`crate::config::merged::build_with`와
/// 같은 패턴).
fn build_profile_view(
    lang: Lang,
    name: &str,
    profile: &Profile,
    origins: &ProfileOrigins,
    probe: &dyn Fn(&str) -> bool,
) -> ProfileView {
    let name_check = ProfileName::parse(name, lang);
    let (destination_editable, destination_note) = destination_editability(lang, profile);

    let mut secrets = Vec::new();
    for (key, env_name) in [
        ("uri_env", profile.source.uri_env.as_deref()),
        ("read_uri_env", profile.source.read_uri_env.as_deref()),
    ] {
        if let Some(env_name) = env_name {
            secrets.push(SecretEnvRef::new(key, env_name, probe(env_name)));
        }
    }
    for dest in profile.effective_destinations() {
        if let Some(env_name) = dest
            .s3
            .as_ref()
            .and_then(|s3| s3.credentials_env.as_deref())
        {
            secrets.push(SecretEnvRef::new("s3_creds", env_name, probe(env_name)));
        }
    }

    // endpoint 전용 프로파일에서는 `effective_destinations()`가 **빈 기본값 1개**를 돌려준다
    // (단일 `destination` 필드로 폴백하는 규칙). 그걸 표에 그리면 "type 미설정 destination이
    // 하나 있다"는 거짓 정보가 된다 — destination이 없다는 사실을 그대로 보여야 한다.
    let destinations = if profile.is_endpoint_only() {
        Vec::new()
    } else {
        profile
            .effective_destinations()
            .iter()
            .enumerate()
            .map(|(idx, dest)| DestinationView {
                label: dest.label(idx),
                kind: dest.r#type.clone(),
                location: destination_location(dest),
                region: dest.s3.as_ref().and_then(|s3| s3.region.clone()),
                endpoint: dest.s3.as_ref().and_then(|s3| s3.endpoint.clone()),
                credentials: dest
                    .s3
                    .as_ref()
                    .and_then(|s3| s3.credentials_env.as_deref())
                    .map(|env_name| SecretEnvRef::new("s3_creds", env_name, probe(env_name))),
            })
            .collect()
    };

    let fields = FIELDS
        .iter()
        .filter(|spec| !spec.destination_field || destination_editable)
        .map(|spec| FieldView {
            key: spec.key,
            group: spec.group,
            kind: spec.kind,
            origin: origins.origin_of(spec.origin_keys),
            effective: (spec.get)(profile),
            warning: uri_credential_warning(lang, spec, profile),
        })
        .collect();

    ProfileView {
        name: name.to_string(),
        editable: name_check.is_ok(),
        name_error: name_check.err().map(|e| e.to_string()),
        endpoint_only: profile.is_endpoint_only(),
        secrets,
        destinations,
        destination_editable,
        destination_note,
        fields,
    }
}

/// destination의 위치 표기(local 경로 또는 `bucket/prefix`).
fn destination_location(dest: &crate::config::file::DestinationConfig) -> Option<String> {
    if let Some(path) = &dest.path {
        return Some(path.clone());
    }
    let s3 = dest.s3.as_ref()?;
    let bucket = s3.bucket.as_deref()?;
    match s3.prefix.as_deref().filter(|p| !p.is_empty()) {
        Some(prefix) => Some(format!("{bucket}/{prefix}")),
        None => Some(bucket.to_string()),
    }
}

/// 평문 `uri`/`read_uri`에 자격증명이 섞여 있으면 붙이는 경고.
///
/// config에 시크릿을 두는 것은 설계 위반이지만(`src/config/file.rs`의 `uri` doc), 실제로
/// 그렇게 쓴 파일이 존재할 수 있다. 화면이 말해주지 않으면 운영자는 고칠 이유를 모른다.
fn uri_credential_warning(lang: Lang, spec: &FieldSpec, profile: &Profile) -> Option<String> {
    if spec.kind != FieldKind::Uri {
        return None;
    }
    let uri = redacted_uri_of(profile, spec.key)?;
    if !uri.had_credentials() {
        return None;
    }
    Some(
        lang.sel(
            "This value carries credentials in the config file. Move the secret into an environment variable and name it in uri_env / read_uri_env — anyone who can read the file can read the password.",
            "이 값에는 config 파일 안에 자격증명이 들어 있습니다. 시크릿을 환경변수로 옮기고 그 이름을 uri_env·read_uri_env에 적으세요 — 파일을 읽을 수 있는 사람은 누구나 비밀번호를 읽습니다.",
        )
        .to_string(),
    )
}

/// 빈 프로파일의 화면 모델(생성 폼용) — 모든 필드가 [`Origin::Builtin`]이고 비어 있다.
///
/// 새 프로파일에는 상속받을 원본이 없다. `[defaults]`가 있으면 저장 뒤 그 값이 적용되지만,
/// 그건 **저장 결과**이고 폼이 미리 그 값을 채워 넣어서는 안 된다 — 채우면 생성 즉시
/// defaults가 프로파일에 복제된다(모듈 헤더 함정 2가 생성 경로에도 그대로 적용된다).
fn blank_profile_view(lang: Lang) -> ProfileView {
    let profile = Profile::default();
    let origins = ProfileOrigins::default();
    let mut view = build_profile_view(lang, "", &profile, &origins, &|_| false);
    // 빈 프로파일에는 실효값이 없다 — 내장 기본값을 실효값으로 보여주면 "이미 설정된 것"
    // 처럼 읽힌다. 값을 지우고 출처만 builtin으로 남긴다.
    for field in &mut view.fields {
        field.effective = None;
        field.warning = None;
    }
    view.name = String::new();
    view.editable = true;
    view.name_error = None;
    view.destinations = Vec::new();
    view.destination_editable = true;
    view.destination_note = None;
    view.secrets = Vec::new();
    view
}

// ---------------------------------------------------------------------------
// 폼 본문 파싱 — `application/x-www-form-urlencoded`
// ---------------------------------------------------------------------------

/// 폼 본문의 모든 (이름, 값) 쌍.
///
/// 필드 이름이 [`FIELDS`] 표에서 나오므로 이름을 나열한 구조체로 받을 수 없다(모듈 헤더
/// "폼 파싱에 `axum::Form`을 쓰지 않는 이유"). 같은 이름이 여러 번 오면 **첫 값**을 쓴다 —
/// 마지막 값을 쓰면 뒤에 같은 이름을 덧붙여 앞의 값을 덮는 요청이 통한다.
#[derive(Debug, Default)]
struct FormBody {
    pairs: Vec<(String, String)>,
}

impl FormBody {
    /// 본문을 쌍 목록으로 파싱한다. 어떤 입력에도 실패하지 않는다(값을 못 읽으면 없는 것).
    fn parse(body: &str) -> Self {
        let mut pairs = Vec::new();
        for pair in body.as_bytes().split(|&b| b == b'&') {
            if pair.is_empty() {
                continue;
            }
            let (name, value) = match pair.iter().position(|&b| b == b'=') {
                Some(eq) => {
                    let (k, v) = pair.split_at(eq);
                    (percent_decode(k), percent_decode(&v[1..]))
                }
                // 값 없는 이름(`flag`)도 존재로 취급한다.
                None => (percent_decode(pair), String::new()),
            };
            pairs.push((name, value));
        }
        Self { pairs }
    }

    /// 필드 값(앞뒤 공백을 잘라서).
    ///
    /// 잘라내는 이유: 브라우저 자동완성·복사 붙여넣기가 공백을 흔히 끼우고, 앞뒤 공백이
    /// 붙은 경로·env 이름을 config에 남기는 것은 거의 항상 사고다. 프로파일 **이름**은
    /// 반대로 자르지 않는다 — [`ProfileName::parse`]가 공백이 섞인 이름을 거부해서
    /// 드러내는 쪽을 택했고(그 파일 doc 참조), 그 판정을 여기서 무르면 안 된다.
    fn get(&self, name: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.trim())
    }

    /// 필드 값(원본 그대로 — 프로파일 이름 전용).
    fn get_raw(&self, name: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// 체크박스류 — 이름이 존재하고 값이 비어 있지 않은지.
    fn flag(&self, name: &str) -> bool {
        self.get(name).is_some_and(|v| !v.is_empty())
    }
}

/// `+`(공백) / `%XX`(퍼센트 인코딩)를 디코드한다.
///
/// 바이트 단위로만 다룬다 — `&str` 슬라이싱은 UTF-8 경계를 벗어나면 패닉하고, 이 본문은
/// 브라우저가 아닌 무엇이든 보낼 수 있는 입력이다. 마지막에 한 번만
/// [`String::from_utf8_lossy`]로 접는다(t6의 `percent_decode`와 같은 이유·같은 형태).
fn percent_decode(input: &[u8]) -> String {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        match input[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < input.len()
                && input[i + 1].is_ascii_hexdigit()
                && input[i + 2].is_ascii_hexdigit() =>
            {
                out.push((hex_val(input[i + 1]) << 4) | hex_val(input[i + 2]));
                i += 3;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// ASCII 16진 숫자 한 글자를 값으로. 호출부가 이미 걸러내므로 그 외 입력에는 도달하지
/// 않는다(도달해도 0을 돌려줄 뿐 패닉하지 않는다).
fn hex_val(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        b'A'..=b'F' => byte - b'A' + 10,
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// 값 검증
// ---------------------------------------------------------------------------

/// 모든 값이 통과하는 공통 검사 — 길이 상한과 제어문자 거부.
///
/// ## 프로파일 이름과 규칙이 다른 이유
/// 프로파일 이름은 **경로 조각**이 된다(락·상태 파일). 그래서 ASCII 화이트리스트라는
/// 가장 좁은 규칙을 받는다([`ProfileName`], `src/web/job/args.rs` 헤더). 반면 여기 값들은
/// **파일 내용**이 된다 — argv로 나가지 않으므로(자식은 `--config <path> --profile <name>`만
/// 받고 값은 config 파일에서 스스로 읽는다) 옵션 주입 표면이 없고, 경로·버킷 이름에
/// 유니코드가 정당하게 들어갈 수 있다. 그래서 문자 집합을 좁히는 대신 **파일을 망가뜨리는
/// 것**만 막는다:
///
/// - 제어문자(개행·탭·NUL 포함) — TOML 한 줄 구조를 깨거나 이스케이프에 의존하게 만든다.
///   `crate::config::v2`의 emitter가 이스케이프하긴 하지만, 값에 개행이 들어간 경우는
///   거의 전부 붙여넣기 사고이고 거부해서 드러내는 편이 낫다.
/// - 길이 상한.
fn ensure_value_shape(key: &str, raw: &str) -> Result<()> {
    if raw.len() > MAX_FIELD_VALUE_LEN {
        return Err(XBackupError::Usage(format!(
            "'{key}' 값이 너무 깁니다({}바이트) — {MAX_FIELD_VALUE_LEN}바이트 이하여야 합니다.",
            raw.len()
        )));
    }
    if let Some(bad) = raw.chars().find(|c| c.is_control()) {
        return Err(XBackupError::Usage(format!(
            "'{key}' 값에 제어문자(U+{:04X})가 있습니다 — config 파일 한 줄에 담을 수 없습니다.",
            bad as u32
        )));
    }
    Ok(())
}

/// env 변수 **이름**을 검증한다(값이 아니다).
///
/// POSIX 환경변수 이름 문법(`[A-Za-z_][A-Za-z0-9_]*`)만 받는다. 넓힐 이유가 없다 — 이
/// 이름은 `std::env::var`로 조회되고 자식 프로세스 env에 그대로 실린다. `=`이나 공백이
/// 섞이면 env 자체를 만들 수 없고, 소문자·숫자 시작은 셸이 설정하지 못하는 이름이다.
fn validate_env_name(key: &str, raw: &str) -> Result<String> {
    if raw.len() > MAX_ENV_NAME_LEN {
        return Err(XBackupError::Usage(format!(
            "'{key}' 환경변수 이름이 너무 깁니다({}바이트) — {MAX_ENV_NAME_LEN}바이트 이하.",
            raw.len()
        )));
    }
    let mut chars = raw.chars();
    let first = chars
        .next()
        .ok_or_else(|| XBackupError::Usage(format!("'{key}' 환경변수 이름이 비어 있습니다.")))?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(XBackupError::Usage(format!(
            "'{key}' 환경변수 이름 '{raw}'의 첫 글자가 영문 또는 `_`가 아닙니다 — \
             환경변수 이름은 `[A-Za-z_][A-Za-z0-9_]*` 형태여야 합니다."
        )));
    }
    if let Some(bad) = raw
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || *c == '_'))
    {
        return Err(XBackupError::Usage(format!(
            "'{key}' 환경변수 이름 '{raw}'에 허용되지 않은 문자(U+{:04X})가 있습니다 — \
             영문/숫자/`_`만 쓸 수 있습니다.",
            bad as u32
        )));
    }
    Ok(raw.to_string())
}

/// compact destination 표기를 검증한다.
///
/// 권위는 [`crate::config::v2`]의 `parse_dest_compact`다(비공개 함수라 호출할 수 없다).
/// 그 함수가 받는 형태만 통과시키므로, 저장 뒤 되읽을 때 같은 판정이 나온다.
fn validate_dest(key: &str, raw: &str) -> Result<String> {
    let (scheme, rest) = raw.split_once(':').ok_or_else(|| {
        XBackupError::Usage(format!(
            "'{key}' 값 '{raw}'에 스킴이 없습니다 — \"local:/path\" 또는 \
             \"s3:bucket/prefix\" 형태로 지정하세요."
        ))
    })?;
    match scheme {
        "local" => {
            if rest.is_empty() {
                return Err(XBackupError::Usage(format!(
                    "'{key}' 값 '{raw}': local 경로가 비었습니다."
                )));
            }
        }
        "s3" => {
            let bucket = rest.split('/').next().unwrap_or("");
            if bucket.is_empty() {
                return Err(XBackupError::Usage(format!(
                    "'{key}' 값 '{raw}': s3 버킷이 비었습니다."
                )));
            }
        }
        other => {
            return Err(XBackupError::Usage(format!(
                "'{key}' 값 '{raw}': 알 수 없는 스킴 '{other}'(local|s3)."
            )))
        }
    }
    Ok(raw.to_string())
}

/// 평문 URI를 검증한다 — **자격증명이 섞인 URI는 거부한다.**
///
/// 거부하는 것이 이 화면의 핵심 규약이다. 허용하면 웹 콘솔이 config에 시크릿을 써 넣는
/// 경로가 되고, 그 값은 다음 화면에서 가려진 표기로만 보이므로 운영자가 무엇을 저장했는지
/// 확인할 수도 없다. 자격증명이 필요하면 env로 옮기고 `uri_env`에 이름을 적는다.
fn validate_plain_uri(key: &str, raw: &str) -> Result<String> {
    let (_, rest) = raw.split_once("://").ok_or_else(|| {
        XBackupError::Usage(format!(
            "'{key}' 값에 스킴이 없습니다 — `mongodb://…`·`postgres://…`처럼 \
             `<scheme>://`로 시작해야 합니다."
        ))
    })?;
    let authority = &rest[..rest.find(['/', '?']).unwrap_or(rest.len())];
    if authority.contains('@') {
        return Err(XBackupError::Usage(format!(
            "'{key}'에 자격증명이 들어 있습니다 — config 파일에 비밀번호를 쓸 수 없습니다. \
             시크릿을 환경변수에 담고 그 이름을 '{}'에 적으세요.",
            if key == KEY_URI {
                "uri_env"
            } else {
                "read_uri_env"
            }
        )));
    }
    if authority.is_empty() {
        return Err(XBackupError::Usage(format!(
            "'{key}' 값에 호스트가 없습니다."
        )));
    }
    Ok(raw.to_string())
}

/// 제출된 문자열을 그 필드의 TOML 값으로 접는다.
fn parse_field_value(spec: &FieldSpec, raw: &str) -> Result<Value> {
    ensure_value_shape(spec.key, raw)?;
    match spec.kind {
        FieldKind::Text => Ok(Value::String(raw.to_string())),
        FieldKind::EnvName => Ok(Value::String(validate_env_name(spec.key, raw)?)),
        FieldKind::Dest => Ok(Value::String(validate_dest(spec.key, raw)?)),
        FieldKind::Uri => Ok(Value::String(validate_plain_uri(spec.key, raw)?)),
        FieldKind::Bool => match raw {
            "true" => Ok(Value::Boolean(true)),
            "false" => Ok(Value::Boolean(false)),
            other => Err(XBackupError::Usage(format!(
                "'{}' 값 '{other}'는 true 또는 false여야 합니다.",
                spec.key
            ))),
        },
        FieldKind::Number => {
            let parsed: i64 = raw.parse().map_err(|_| {
                XBackupError::Usage(format!(
                    "'{}' 값 '{raw}'를 정수로 읽을 수 없습니다.",
                    spec.key
                ))
            })?;
            // 범위는 config 구조체의 필드 타입이 정한다 — 웹이 임의의 상한을 발명하지 않는다.
            let (min, max) = number_range(spec.key);
            if parsed < min || parsed > max {
                return Err(XBackupError::Usage(format!(
                    "'{}' 값 {parsed}가 허용 범위({min}~{max})를 벗어났습니다.",
                    spec.key
                )));
            }
            Ok(Value::Integer(parsed))
        }
        FieldKind::Choice(options) => {
            if options.contains(&raw) {
                Ok(Value::String(raw.to_string()))
            } else {
                Err(XBackupError::Usage(format!(
                    "'{}' 값 '{raw}'는 허용된 값이 아닙니다 — {} 중 하나여야 합니다.",
                    spec.key,
                    options.join(" | ")
                )))
            }
        }
    }
}

/// 숫자 필드의 허용 범위. **config 구조체 필드 타입에서 그대로 온다** — 웹이 자기 판단으로
/// "합리적인" 상한을 발명하면 CLI로는 되는 값이 콘솔에서만 거부된다.
fn number_range(key: &str) -> (i64, i64) {
    match key {
        // `CompressionConfig::level: i32`.
        "compress_level" => (i64::from(i32::MIN), i64::from(i32::MAX)),
        // `SourceConfig::connect_timeout_secs: Option<u64>` — u64 상한은 i64로 표현할 수
        // 없으므로 i64 상한까지만 받는다(초 단위로 사실상 무한하다).
        "connect_timeout_secs" => (0, i64::MAX),
        // `RetentionConfig`의 모든 필드가 `Option<u32>`.
        _ => (0, i64::from(u32::MAX)),
    }
}

// ---------------------------------------------------------------------------
// 변경 요청 — 검증은 끝났고, 저장은 아직
// ---------------------------------------------------------------------------

/// 변경 종류.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeOp {
    /// 새 프로파일.
    Create,
    /// 기존 프로파일 수정.
    Update,
    /// 프로파일 삭제.
    Delete,
}

impl ChangeOp {
    /// 화면·감사 기록에 쓰는 라벨(영문 고정).
    pub fn label(self) -> &'static str {
        match self {
            ChangeOp::Create => OP_CREATE,
            ChangeOp::Update => OP_UPDATE,
            ChangeOp::Delete => "delete",
        }
    }

    /// 감사 로그 `action` 필드 값.
    ///
    /// 라우트와 같은 어휘를 쓴다(`POST /config/save` → `config.save`) — 감사 로그를 훑는
    /// 사람이 "어느 요청이었나"를 되짚을 때 라벨과 경로가 어긋나면 안 된다. 생성과 수정은
    /// 같은 라우트·같은 저장 경로이므로 action도 하나로 두고, 어느 쪽이었는지는
    /// `args_masked`의 `op=` 항목이 말한다([`audit_args`]).
    ///
    /// [`crate::web::job::spec::JobCommand::audit_action`]이 잡 쪽에서 `<verb>.run` 규칙을
    /// 쓰는 것과 같은 자리의 값이다 — 여기는 잡이 아니라 config 쓰기라 접미가 다르다.
    pub fn audit_action(self) -> &'static str {
        match self {
            ChangeOp::Create | ChangeOp::Update => "config.save",
            ChangeOp::Delete => "config.delete",
        }
    }
}

/// **검증을 통과한 변경 요청.** 아직 파일에 쓰이지 않았다.
///
/// ## 왜 `Profile` 구조체가 아니라 sparse 키맵인가
/// [`crate::config::file::Profile`]은 모든 필드가 채워진 구조체다 — `compress_level: i32`,
/// `precheck: bool`처럼 `Option`이 아닌 필드가 있으므로 **"이 키는 프로파일에 쓰지 마라
/// (상속·기본값에 맡겨라)"를 표현할 수 없다.** 그걸 표현하지 못하면 저장이 상속을 보존할
/// 방법이 없다(모듈 헤더 함정 2). 그래서 변경은 "프로파일 테이블에 직접 적힐 키"의
/// 목록([`set`](Self::set))과 "지울 키"의 목록([`unset`](Self::unset))으로 표현한다.
/// 키 어휘는 v2 flat 키이고, v1 nested 경로로의 대응은 [`V1_PATHS`]가 들고 있다.
///
/// ## 저장 계층(t27·t28)이 이 값에서 얻는 것
/// - [`syntax`](Self::syntax): 원본 문법. v1 파일을 v2로 다시 쓰지 않기 위한 근거.
/// - [`set`](Self::set)/[`unset`](Self::unset): 프로파일 테이블에만 손대면 된다는 사실.
///   여기 **없는 키는 건드리지 않는다** — `[defaults]`·`[base.*]`는 이 변경의 대상이 아니다.
/// - [`origins`](Self::origins): 편집 전 출처 스냅샷. "이 키가 프로파일에 없는 이유"가
///   상속인지 기본값인지 구분되므로, 저장이 상속 구조를 재구성할 수 있다.
/// - [`broke_inheritance`](Self::broke_inheritance): 운영자가 명시적으로 상속을 끊은 키.
///   감사 기록·확인 화면에 그대로 실린다.
#[derive(Debug, Clone)]
pub struct ProfileChange {
    /// 변경 종류.
    pub op: ChangeOp,
    /// 대상 프로파일(검증된 이름).
    pub name: ProfileName,
    /// 원본 config의 문법 — 저장이 **보존해야** 한다.
    pub syntax: ConfigSyntax,
    /// 프로파일 테이블에 직접 적힐 키 → 값. 상속받은 값은 여기 **없다**.
    pub set: BTreeMap<String, Value>,
    /// 프로파일 테이블에서 지울 키.
    pub unset: BTreeSet<String>,
    /// 이번 제출로 상속이 끊긴 키(있던 상속을 override로 바꾼 것).
    pub broke_inheritance: Vec<String>,
    /// 편집 전 출처 스냅샷.
    pub origins: ProfileOrigins,
}

impl ProfileChange {
    /// 화면 표시용 `set` 목록(키, 값 문자열) — 정렬된 순서.
    pub fn set_rows(&self) -> Vec<(String, String)> {
        self.set
            .iter()
            .map(|(key, value)| (key.clone(), display_toml(value)))
            .collect()
    }

    /// 화면 표시용 `unset` 목록.
    pub fn unset_rows(&self) -> Vec<String> {
        self.unset.iter().cloned().collect()
    }

    /// 실제로 바뀌는 것이 있는지.
    pub fn is_noop(&self) -> bool {
        self.op != ChangeOp::Delete && self.set.is_empty() && self.unset.is_empty()
    }
}

/// TOML 값을 화면 문자열로. 문자열은 인용부호 없이 보여준다(폼에 입력한 그대로가 읽혀야
/// 한다).
fn display_toml(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// 폼 본문에서 검증된 변경 요청을 만든다 — **순수 함수**(파일도, env도 건드리지 않는다).
///
/// 필드별 규약(모듈 헤더 함정 2의 표):
/// - 빈 입력 + 출처가 [`Origin::Direct`] → 그 키를 [`ProfileChange::unset`]에 넣는다.
/// - 빈 입력 + 상속/기본값 → **아무것도 하지 않는다**(상속 유지).
/// - 빈 입력 + [`FieldKind::Uri`] → 변경 없음. `<key>_clear` 체크박스가 있을 때만 지운다.
/// - 값 있음 → 검증해 [`ProfileChange::set`]에 넣는다. 상속이었다면
///   [`ProfileChange::broke_inheritance`]에 기록한다.
/// - 값 있음 + 출처가 [`Origin::Direct`] + **현재 값과 같음** → 아무것도 하지 않는다.
///
/// 마지막 규약이 필요한 이유: 브라우저는 직접 적힌 필드의 값을 미리 채운 채로 **전부**
/// 다시 제출한다. 그것을 그대로 `set`에 담으면 아무것도 고치지 않은 제출이 "키 12개를
/// 씁니다"로 보이고, 그러면 결과 화면의 목록이 신호가 아니라 소음이 된다 — 운영자가
/// 실제로 바뀐 한 줄을 그 안에서 찾지 못한다. `current`는 그 비교의 기준값을 담은 편집 전
/// 화면 모델이다(생성이면 `None`).
fn build_change(
    op: ChangeOp,
    name: ProfileName,
    syntax: ConfigSyntax,
    origins: &ProfileOrigins,
    destination_editable: bool,
    current: Option<&ProfileView>,
    form: &FormBody,
) -> Result<ProfileChange> {
    let mut set = BTreeMap::new();
    let mut unset = BTreeSet::new();
    let mut broke_inheritance = Vec::new();

    for spec in FIELDS {
        // destination을 편집하지 않는 프로파일에서는 그 필드를 렌더하지 않았다 —
        // 제출에 섞여 들어와도 무시한다(폼 밖에서 만든 요청이 사본을 지우지 못하게).
        if spec.destination_field && !destination_editable {
            continue;
        }
        let origin = origins.origin_of(spec.origin_keys);
        // 제출에 **없는** 필드는 건드리지 않는다.
        //
        // "빈 값으로 제출됨"과 "아예 제출되지 않음"을 반드시 갈라야 한다. HTML 폼은 렌더된
        // 텍스트 입력·`<select>`를 빈 값이라도 전부 제출하므로, 이름이 없다는 것은 "그
        // 필드가 화면에 없었다"는 뜻이다(destination 비편집 프로파일, 미래에 추가될 부분
        // 폼, 손으로 만든 요청). 그런 필드를 "비웠다"로 읽으면 **부분 제출 하나가 프로파일
        // 절반을 지운다.** 이 화면에서 값이 사라지는 사고의 가장 큰 원인이 여기다.
        let Some(raw) = form.get(spec.key) else {
            continue;
        };

        if raw.is_empty() {
            match spec.kind {
                // URI는 빈 값이 "변경 없음"이다 — 지우려면 명시적 체크박스가 필요하다.
                FieldKind::Uri => {
                    if form.flag(&format!("{}{CLEAR_SUFFIX}", spec.key)) {
                        unset.insert(spec.key.to_string());
                    }
                }
                _ => {
                    if origin.is_direct() {
                        unset.insert(spec.key.to_string());
                    }
                }
            }
            continue;
        }

        let value = parse_field_value(spec, raw)?;

        // 직접 적힌 값이 그대로 다시 왔으면 변경이 아니다(위 doc의 마지막 규약).
        // 상속받은 값에는 이 단축을 적용하지 않는다 — 상속 필드의 입력 칸은 비어 있으므로,
        // 같은 값이 왔다는 것은 운영자가 그것을 **직접 타이핑해** override를 요청했다는 뜻이다.
        let unchanged = origin.is_direct()
            && current
                .and_then(|view| view.field(spec.key))
                .and_then(|field| field.effective.as_deref())
                == Some(raw);
        if unchanged {
            continue;
        }

        if !origin.is_direct() && matches!(origin, Origin::Defaults | Origin::Inherited { .. }) {
            broke_inheritance.push(spec.key.to_string());
        }
        set.insert(spec.key.to_string(), value);

        // 같은 값을 대표하던 **직접 적힌** 다른 키를 지운다. 그러지 않으면
        // `compress = "zstd:6"`과 `compress_level = 7`이 공존해 파일을 읽는 사람이 어느
        // 쪽이 이기는지 추측해야 한다.
        for other in &spec.origin_keys[1..] {
            if origins.origin_of(&[other]).is_direct() {
                unset.insert((*other).to_string());
            }
        }
    }

    // `set`과 `unset`이 같은 키를 다투면 `set`이 이긴다. 이 상황은 정상이다 — 예를 들어
    // compact `encrypt = "age:/k.pub"`을 편집하면 `recipient_file`이 "그 compact 키를
    // 지워라"고 요청하는 동시에 `encrypt` 필드가 새 불리언 값을 쓴다. 지우고 나서 쓰는
    // 순서를 저장 계층에 강요하지 않기 위해 여기서 정리한다.
    unset.retain(|key| !set.contains_key(key));

    // 새 프로파일에는 소스가 필요하다. 이건 웹이 발명한 규칙이 아니라 "이 프로파일로는
    // 아무 명령도 돌 수 없다"는 사실을 저장 전에 말해주는 것이다(t27이 붙일 저장 전
    // `doctor` 검증의 최소 선행 조건이기도 하다).
    if op == ChangeOp::Create && !set.contains_key("uri") && !set.contains_key("uri_env") {
        return Err(XBackupError::Usage(
            "새 프로파일에는 소스가 필요합니다 — 평문 'uri' 또는 시크릿을 담은 환경변수 \
             이름 'uri_env' 중 하나를 채우세요."
                .to_string(),
        ));
    }

    Ok(ProfileChange {
        op,
        name,
        syntax,
        set,
        unset,
        broke_inheritance,
        origins: origins.clone(),
    })
}

// ---------------------------------------------------------------------------
// 저장 지점 — t27이 채운다
// ---------------------------------------------------------------------------

/// 검증된 변경을 실제 config 파일에 반영하는 **저장 지점**.
///
/// ## t27이 채운 계약
/// [`crate::web::config_write::FileConfigStore`]가 이 트레이트의 프로덕션 구현이다. 다음을
/// 지킨다:
///
/// 1. **원자성** — 임시 파일에 쓰고 `fsync` 후 `rename`(+디렉터리 fsync). 중간 상태의
///    config가 디스크에 보이면 그 순간 돌던 cron 백업이 반쪽 설정을 읽는다.
/// 2. **직전본 보존** — 저장마다 `.bak.1..N`으로 세대 회전한다.
/// 3. **문법 보존** — [`ProfileChange::syntax`]가 v1이면 v1 중첩 구조로, v2/Empty면 v2
///    flat 구조로 쓴다.
/// 4. **상속 보존** — [`ProfileChange::set`]/[`unset`](ProfileChange::unset)에 없는 키는
///    건드리지 않고, `[defaults]`·`[base.*]` 섹션에는 손대지 않는다.
/// 5. **저장 전 doctor 검증** — 새 config를 임시 파일에 써서 점검하고, 통과하지 못하면
///    (exit 2/3) 원본에 손대지 않는다. exit 4(경고)는 저장을 막지 않는다.
/// 6. **v1 `dest` 압축 해제** — compact 표기 한 줄을 v1의 nested 4경로(`destination.type`·
///    `destination.path`·`destination.s3.{bucket,prefix}`)로 정확히 펼쳐 쓰고, 반대쪽 스킴의
///    위치 키는 지운다. `dest`가 v1에서 여러 경로로 갈라지는 이유는 그 모듈 헤더에 있다.
///
/// 테스트가 디스크를 건드리지 않아도 되도록 [`PendingStore`]/[`store`]는 여전히 남아 있다
/// (기본값은 계속 no-op). 실제 요청 경로([`apply_and_render`])는 [`PendingStore`]가 아니라
/// `ctx.config_path`로 만든 `FileConfigStore`를 직접 주입해 쓴다.
///
/// ## `AuditReceipt`를 값으로 요구하는 이유 — 기록 없는 config 쓰기가 컴파일되지 않게
/// 이 트레이트가 하는 일은 **운영자의 config 파일을 실제로 덮어쓰는 것**이다. destination을
/// 남의 S3로 바꾸거나 `features.encryption.enabled`를 끄는 변경이 흔적 없이 지나갈 수 있으면
/// 이 콘솔은 감사 불가능한 도구가 된다. 그런데 t8이 만든 관문
/// ([`AuditLog::gate`](crate::web::audit::AuditLog::gate))은 "호출하기로 약속"만으로는
/// 지켜지지 않는다 — 실제로 이 파일의 config 경로에는 관문 호출이 **한 줄도 없었다**.
///
/// 그래서 [`crate::web::audit::AuditReceipt`]를 **값으로** 받는다. 이 값은 감사 로그 append가
/// 성공했을 때만 만들어지고 `audit` 모듈 밖에서는 생성할 방법이 없으므로, 게이트를 건너뛰고
/// config를 쓰는 코드는 넘길 값을 만들 수 없어 컴파일되지 않는다
/// ([`JobRunner::spawn_destructive`](crate::web::job::JobRunner::spawn_destructive)와 같은 패턴).
/// `Clone`이 아니므로 한 번 소비하면 끝이고, 두 번째 저장은 자신의 게이트를 지나야 한다.
///
/// 대가: 저장 경로를 검증하는 단위 테스트도 receipt를 만들어야 한다(임시 디렉터리에 감사
/// 로그를 열고 게이트를 통과한다 — 각 테스트 모듈의 `test_receipt` 헬퍼). 다섯 군데뿐이고,
/// 그 대가로 "다음 사람이 감사 없이 저장 경로를 추가하는" 실수가 문법 수준에서 막힌다.
/// `PendingStore` 같은 no-op 구현조차 receipt를 요구받는 것은 의도적이다 — 예외를 하나
/// 만들면 그 예외가 우회 경로가 된다.
pub trait ConfigStore: Send + Sync {
    /// 변경을 반영한다. `audit`는 이 호출 **직전에** 감사 로그에 기록이 남았다는 증표다.
    fn apply(&self, change: &ProfileChange, audit: AuditReceipt) -> Result<ChangeReceipt>;

    /// v1 문법 파일을 v2 표면으로 **전환한다** — 파일 전체를 다시 쓴다.
    ///
    /// [`apply`](Self::apply)와 별개의 메서드인 이유: 전환은 [`ProfileChange`]로 표현할 수
    /// 없다. 대상이 프로파일 하나가 아니라 파일 전부이고, `set`/`unset`이 비어 있으므로
    /// 변경으로 접으면 "바뀌는 것이 없는 저장"과 구별되지 않는다. 그리고 이 구분이 **저장이
    /// 전환을 부작용으로 일으킬 수 없다**는 성질을 타입으로 만든다 — `apply`를 부르는 코드는
    /// 문법을 바꿀 방법이 없다.
    ///
    /// `plan`은 [`crate::web::config_write::plan_v1_to_v2`]만이 만들 수 있고(생성자가 하나뿐인
    /// 비공개 필드), 그 함수가 "값이 하나도 달라지지 않는다"를 증명한 결과다. `audit`는
    /// `apply`와 같은 감사 게이트 증표다 — 파일 전체를 바꾸는 작업에 예외를 두지 않는다.
    fn convert_to_v2(
        &self,
        plan: &crate::web::config_write::ConversionPlan,
        audit: AuditReceipt,
    ) -> Result<ChangeReceipt>;
}

/// 저장 결과.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeReceipt {
    /// config 파일에 반영됐다(doctor 경고 없음).
    Persisted,
    /// config 파일에 반영됐고, doctor exit 4의 경고 메시지가 함께 있다 — 화면이 보여줘야
    /// 한다. exit 4는 "경고 동반 성공"이므로 저장을 막지 않는다(`routes::doctor`의 같은
    /// 판단과 일치시킨다).
    PersistedWithWarnings(Vec<String>),
    /// 검증만 통과했고 파일은 그대로다(기본 저장소·[`PendingStore`]).
    NotPersisted,
}

impl ChangeReceipt {
    /// 파일이 실제로 바뀌었는지.
    pub fn persisted(&self) -> bool {
        !matches!(self, ChangeReceipt::NotPersisted)
    }

    /// doctor exit 4가 남긴 경고 메시지(없으면 빈 슬라이스).
    pub fn warnings(&self) -> &[String] {
        match self {
            ChangeReceipt::PersistedWithWarnings(warnings) => warnings,
            _ => &[],
        }
    }
}

/// 저장이 아직 배선되지 않았음을 **값으로** 돌려주는 구현.
///
/// 에러가 아니라 [`ChangeReceipt::NotPersisted`]를 돌려준다 — 검증 경로 전체가 지금
/// 동작하고 테스트되어야 하고, 화면은 "파일은 쓰이지 않았다"를 명시적으로 말한다. 에러로
/// 끊으면 검증 결과를 볼 수 없어 이 화면이 아무 쓸모가 없어진다.
///
/// t27 이후에도 이 구현은 남아 있다 — 단위 테스트가 디스크를 건드리지 않고 검증 경로만
/// 확인하고 싶을 때 [`store`]로 계속 쓴다. 실제 저장은 [`crate::web::config_write::FileConfigStore`]가
/// 맡는다.
#[derive(Debug, Clone, Copy, Default)]
pub struct PendingStore;

impl ConfigStore for PendingStore {
    fn apply(&self, _change: &ProfileChange, _audit: AuditReceipt) -> Result<ChangeReceipt> {
        Ok(ChangeReceipt::NotPersisted)
    }

    fn convert_to_v2(
        &self,
        _plan: &crate::web::config_write::ConversionPlan,
        _audit: AuditReceipt,
    ) -> Result<ChangeReceipt> {
        Ok(ChangeReceipt::NotPersisted)
    }
}

/// 기본(no-op) 저장 지점 — 테스트 전용 경로다. 실제 요청은 이 함수를 쓰지 않는다
/// ([`apply_and_render`]가 `FileConfigStore`를 직접 주입한다).
pub fn store() -> &'static dyn ConfigStore {
    &PendingStore
}

// ---------------------------------------------------------------------------
// 핸들러
// ---------------------------------------------------------------------------

/// 오류 화면을 상태 코드와 함께 만든다.
fn problem(
    ctx: &ServeConfig,
    status: StatusCode,
    headline: &str,
    detail: Option<&str>,
) -> Response {
    let level = if status.is_server_error() || status == StatusCode::NOT_FOUND {
        Level::Error
    } else {
        Level::Fail
    };
    let body = view::problem_page(ctx.lang, level, headline, detail);
    (status, layout::shell(ctx.lang, CONFIG_TITLE, body)).into_response()
}

/// config를 읽지 못한 이유를 설명하는 화면 — **읽기 경로용(200)**.
///
/// 200으로 돌려준다 — 서버는 정상이고, 화면이 "무엇이 잘못됐는지"를 설명한다
/// (`routes::doctor`가 config 미연결을 같은 방식으로 다룬다).
///
/// 쓰기 경로에서는 [`write_blocked`]를 쓴다 — 이유는 그쪽 문서에.
fn load_problem(ctx: &ServeConfig, error: LoadError) -> Response {
    let body = view::problem_page(ctx.lang, error.level(), &error.explain(ctx.lang), None);
    (StatusCode::OK, layout::shell(ctx.lang, CONFIG_TITLE, body)).into_response()
}

/// 같은 화면을 **409 Conflict**로 돌려준다 — 쓰기 경로용.
///
/// ## 왜 쓰기에는 200을 쓸 수 없는가
/// config를 읽지 못하면 편집도 할 수 없으므로 저장·삭제·전환은 **아무것도 하지 않고**
/// 끝난다. 그런데 그 응답이 200이면 "저장했다"와 "저장을 거부했다"가 같은 상태 코드가 된다 —
/// 사람은 화면의 배너를 읽지만, `curl`이나 스크립트는 상태 코드만 본다. 실측으로 확인된
/// 실제 모양: v1/v2가 섞인 config에 `POST /config/save`가 **200을 내면서 파일을 바꾸지
/// 않았다.**
///
/// 409를 고른 이유: 요청 자체는 올바른데(문법·권한 문제가 아니다) **자원의 현재 상태와
/// 충돌**한다. 400은 "요청이 잘못됐다"는 뜻이라 운영자가 자기 입력을 뒤지게 만들고,
/// 500은 "서버가 고장났다"는 뜻이라 콘솔을 의심하게 만든다. 고쳐야 할 것은 config 파일이다.
fn write_blocked(ctx: &ServeConfig, error: LoadError) -> Response {
    let body = view::problem_page(ctx.lang, error.level(), &error.explain(ctx.lang), None);
    (
        StatusCode::CONFLICT,
        layout::shell(ctx.lang, CONFIG_TITLE, body),
    )
        .into_response()
}

/// config 문서를 읽는다. 못 읽으면 [`LoadError`]를 그대로 올려 호출부가
/// [`load_problem`]으로 접게 한다 — `Result<_, Response>`로 돌리면 Err 변종이
/// 응답 하나만큼 커져 모든 호출부의 스택이 그만큼 넓어진다(`clippy::result_large_err`).
fn load_document(ctx: &ServeConfig) -> std::result::Result<ConfigDocument, LoadError> {
    ConfigDocument::load(ctx.config_path.as_deref())
}

/// `GET /config` — 프로파일 목록.
pub async fn list(State(ctx): State<Arc<ServeConfig>>) -> Response {
    let document = match load_document(&ctx) {
        Ok(doc) => doc,
        Err(error) => return load_problem(&ctx, error),
    };
    let profiles: Vec<ProfileView> = document
        .config
        .profiles
        .iter()
        .map(|(name, profile)| {
            build_profile_view(
                ctx.lang,
                name,
                profile,
                &document.origins_of(name),
                &env_is_set,
            )
        })
        .collect();
    let body = view::list_page(
        ctx.lang,
        document.syntax,
        document.config.default_profile.as_deref(),
        &profiles,
    );
    layout::shell(ctx.lang, CONFIG_TITLE, body).into_response()
}

/// `GET /config/new` — 생성 폼.
pub async fn new_form(State(ctx): State<Arc<ServeConfig>>) -> Response {
    let document = match load_document(&ctx) {
        Ok(doc) => doc,
        Err(error) => return load_problem(&ctx, error),
    };
    let body = view::form_page(
        ctx.lang,
        document.syntax,
        FormMode::Create,
        &blank_profile_view(ctx.lang),
    );
    layout::shell(ctx.lang, CONFIG_TITLE, body).into_response()
}

/// 이름으로 프로파일 화면 모델을 찾는다.
fn profile_view_of(
    ctx: &ServeConfig,
    document: &ConfigDocument,
    name: &str,
) -> Option<ProfileView> {
    let profile = document.config.profiles.get(name)?;
    Some(build_profile_view(
        ctx.lang,
        name,
        profile,
        &document.origins_of(name),
        &env_is_set,
    ))
}

/// `GET /config/edit/{profile}` — 편집 폼.
pub async fn edit_form(State(ctx): State<Arc<ServeConfig>>, Path(name): Path<String>) -> Response {
    let document = match load_document(&ctx) {
        Ok(doc) => doc,
        Err(error) => return load_problem(&ctx, error),
    };
    // 경로에서 온 이름을 먼저 검증한다 — 검증되지 않은 문자열로 맵을 조회하지 않는다.
    if let Err(error) = ProfileName::parse(&name, ctx.lang) {
        return problem(
            &ctx,
            StatusCode::BAD_REQUEST,
            &error.to_string(),
            Some(ctx.lang.sel(
                "The console only edits profiles whose names are safe to use as file path segments.",
                "콘솔은 파일 경로 조각으로 쓸 수 있는 이름의 프로파일만 편집합니다.",
            )),
        );
    }
    match profile_view_of(&ctx, &document, &name) {
        Some(profile) => {
            let body = view::form_page(ctx.lang, document.syntax, FormMode::Update, &profile);
            layout::shell(ctx.lang, CONFIG_TITLE, body).into_response()
        }
        None => problem(
            &ctx,
            StatusCode::NOT_FOUND,
            ctx.lang.sel(
                "No such profile in this config.",
                "이 config에 그런 프로파일이 없습니다.",
            ),
            None,
        ),
    }
}

/// `GET /config/delete/{profile}` — 삭제 확인 화면.
pub async fn delete_form(
    State(ctx): State<Arc<ServeConfig>>,
    Path(name): Path<String>,
) -> Response {
    let document = match load_document(&ctx) {
        Ok(doc) => doc,
        Err(error) => return load_problem(&ctx, error),
    };
    if let Err(error) = ProfileName::parse(&name, ctx.lang) {
        return problem(&ctx, StatusCode::BAD_REQUEST, &error.to_string(), None);
    }
    match profile_view_of(&ctx, &document, &name) {
        Some(profile) => {
            let body = view::delete_page(ctx.lang, &profile);
            layout::shell(ctx.lang, CONFIG_TITLE, body).into_response()
        }
        None => problem(
            &ctx,
            StatusCode::NOT_FOUND,
            ctx.lang.sel(
                "No such profile in this config.",
                "이 config에 그런 프로파일이 없습니다.",
            ),
            None,
        ),
    }
}

/// 상태 변경 요청의 출처를 검사하고, 아니면 403 화면을 만든다.
///
/// ## 왜 미들웨어만 믿지 않고 핸들러에서 부르는가
/// [`crate::web::auth::require_same_origin`] 미들웨어가 같은 검사를 하지만 그것을 라우터에
/// 얹는 것은 `src/web/server.rs`(이 태스크 소관 밖)의 일이다. 배선 전까지 이 두 경로
/// (config 저장·삭제)는 **가장 되돌릴 수 없는 작업**이므로 먼저 스스로 막는다. 미들웨어가
/// 배선된 뒤에도 이 호출은 남겨 둔다 — 같은 판정을 두 번 하는 비용은 헤더 비교 한 번이고,
/// "라우터 배선을 고치다가 조용히 보호가 빠지는" 사고보다 싸다.
///
/// 검사가 **감사 게이트보다 먼저**다: 위조된 요청이 append-only 감사 로그에 줄을 남기게
/// 하면, 그 로그를 부풀리는 것 자체가 공격이 된다([`apply_and_render`]의 순서 참조).
fn reject_foreign_origin(ctx: &ServeConfig, headers: &axum::http::HeaderMap) -> Option<Response> {
    let refusal = crate::web::auth::verify_same_origin(headers).err()?;
    tracing::warn!(reason = ?refusal, "출처를 확인할 수 없어 config 변경 요청을 거부했습니다");
    Some(problem(
        ctx,
        StatusCode::FORBIDDEN,
        &refusal.explain(ctx.lang),
        Some(ctx.lang.sel(
            "Submit the form from the console's own page. Nothing was changed.",
            "콘솔 화면에서 폼을 제출하세요. 아무것도 바뀌지 않았습니다.",
        )),
    ))
}

/// `POST /config/save` — 생성·수정 제출.
///
/// `HeaderMap`을 받는 이유는 출처 검사([`reject_foreign_origin`])다. axum 핸들러는 마지막
/// 하나(`body: String`)만 본문을 소비하는 추출자면 되므로 라우터 등록은 손대지 않는다.
pub async fn save(
    State(ctx): State<Arc<ServeConfig>>,
    headers: axum::http::HeaderMap,
    body: String,
) -> Response {
    if let Some(rejected) = reject_foreign_origin(&ctx, &headers) {
        return rejected;
    }
    let document = match load_document(&ctx) {
        Ok(doc) => doc,
        Err(error) => return write_blocked(&ctx, error),
    };
    let form = FormBody::parse(&body);

    let op = match form.get(FIELD_OP) {
        Some(OP_CREATE) => ChangeOp::Create,
        Some(OP_UPDATE) => ChangeOp::Update,
        other => {
            return problem(
                &ctx,
                StatusCode::BAD_REQUEST,
                &format!(
                    "{} '{}'",
                    ctx.lang
                        .sel("Unknown form operation:", "알 수 없는 폼 작업:"),
                    other.unwrap_or("")
                ),
                None,
            )
        }
    };

    // 이름은 자르지 않은 원본으로 검증한다([`FormBody::get`] doc 참조).
    let raw_name = form.get_raw(FIELD_PROFILE).unwrap_or("");
    let name = match ProfileName::parse(raw_name, ctx.lang) {
        Ok(name) => name,
        Err(error) => {
            return problem(
                &ctx,
                StatusCode::BAD_REQUEST,
                &error.to_string(),
                Some(&format!(
                    "{} {MAX_PROFILE_NAME_LEN}",
                    ctx.lang.sel(
                        "Names use ASCII letters, digits, underscore and hyphen, start with a letter or digit, and are at most this many bytes:",
                        "이름은 ASCII 영문/숫자/`_`/`-`를 쓰고 영문 또는 숫자로 시작하며, 최대 바이트 수는:",
                    )
                )),
            )
        }
    };

    let exists = document.config.profiles.contains_key(name.as_str());
    match op {
        ChangeOp::Create if exists => {
            return problem(
                &ctx,
                StatusCode::BAD_REQUEST,
                ctx.lang.sel(
                    "A profile with that name already exists.",
                    "그 이름의 프로파일이 이미 있습니다.",
                ),
                None,
            )
        }
        ChangeOp::Update if !exists => {
            return problem(
                &ctx,
                StatusCode::NOT_FOUND,
                ctx.lang.sel(
                    "No such profile in this config.",
                    "이 config에 그런 프로파일이 없습니다.",
                ),
                None,
            )
        }
        _ => {}
    }

    // 편집 전 화면 모델 — "무엇이 편집 가능했는가"와 "값이 그대로인가"의 기준값이다.
    // 폼을 그릴 때와 **같은 함수**로 만들어야 그 두 판정이 어긋나지 않는다.
    let current = profile_view_of(&ctx, &document, name.as_str());
    let destination_editable = current
        .as_ref()
        .is_none_or(|view| view.destination_editable);
    let origins = document.origins_of(name.as_str());
    let change = match build_change(
        op,
        name,
        document.syntax,
        &origins,
        destination_editable,
        current.as_ref(),
        &form,
    ) {
        Ok(change) => change,
        Err(error) => return problem(&ctx, StatusCode::BAD_REQUEST, &error.to_string(), None),
    };

    apply_and_render(&ctx, change).await
}

/// `POST /config/delete` — 삭제 제출(확인 필드 필수).
///
/// `HeaderMap`은 출처 검사용이다([`save`]와 같은 이유).
pub async fn delete_submit(
    State(ctx): State<Arc<ServeConfig>>,
    headers: axum::http::HeaderMap,
    body: String,
) -> Response {
    if let Some(rejected) = reject_foreign_origin(&ctx, &headers) {
        return rejected;
    }
    let document = match load_document(&ctx) {
        Ok(doc) => doc,
        Err(error) => return write_blocked(&ctx, error),
    };
    let form = FormBody::parse(&body);
    let raw_name = form.get_raw(FIELD_PROFILE).unwrap_or("");
    let name = match ProfileName::parse(raw_name, ctx.lang) {
        Ok(name) => name,
        Err(error) => return problem(&ctx, StatusCode::BAD_REQUEST, &error.to_string(), None),
    };
    if !document.config.profiles.contains_key(name.as_str()) {
        return problem(
            &ctx,
            StatusCode::NOT_FOUND,
            ctx.lang.sel(
                "No such profile in this config.",
                "이 config에 그런 프로파일이 없습니다.",
            ),
            None,
        );
    }
    // 확인 입력은 이름과 **정확히** 같아야 한다. 자르지 않은 원본으로 비교한다 — 공백을
    // 관대하게 접으면 "이름을 타이핑했다"는 사실 자체가 약해진다.
    if form.get_raw(FIELD_CONFIRM).unwrap_or("") != name.as_str() {
        return problem(
            &ctx,
            StatusCode::BAD_REQUEST,
            ctx.lang.sel(
                "The confirmation text did not match the profile name — nothing was deleted.",
                "확인 입력이 프로파일 이름과 다릅니다 — 아무것도 삭제하지 않았습니다.",
            ),
            Some(ctx.lang.sel(
                "Type the name exactly as shown.",
                "표시된 이름을 그대로 입력하세요.",
            )),
        );
    }

    let origins = document.origins_of(name.as_str());
    let change = ProfileChange {
        op: ChangeOp::Delete,
        name,
        syntax: document.syntax,
        set: BTreeMap::new(),
        unset: BTreeSet::new(),
        broke_inheritance: Vec::new(),
        origins,
    };
    apply_and_render(&ctx, change).await
}

// ---------------------------------------------------------------------------
// v1 → v2 전환 — 미리보기와 실행
// ---------------------------------------------------------------------------

/// 전환 미리보기에 렌더할 결과 텍스트의 줄 수 상한.
///
/// 이 화면은 저장될 파일을 **그대로** 보여주는 것이 값이므로 자르는 것은 손해다. 그래도
/// 상한을 두는 이유는 config 파일 크기에 상한이 없기 때문이다 — 수만 줄짜리 파일을 HTML로
/// 부풀리면 응답 하나가 브라우저를 멈춘다. 운영자가 손으로 관리하는 config는 수십~수백 줄
/// 규모이므로 이 값이 정상 파일을 자를 일은 없고, 잘린 경우에는 그 사실을 화면에 적는다.
const MAX_PREVIEW_LINES: usize = 500;

/// 전환의 감사 로그 `action` 값. 저장(`config.save`)·삭제(`config.delete`)와 나란히 읽히는
/// 어휘를 쓴다 — 라우트(`POST /config/convert`)와 같은 이름이다.
const AUDIT_ACTION_CONVERT: &str = "config.convert";

/// 전환의 감사 로그 `target` 값.
///
/// 다른 config 작업의 target은 프로파일 이름이지만 전환의 대상은 **파일 전체**다. 프로파일
/// 하나를 골라 적으면 감사 로그를 읽는 사람이 "그 프로파일만 바뀌었다"고 오해한다.
const AUDIT_TARGET_WHOLE_FILE: &str = "*";

/// `GET /config/convert` — v1→v2 전환 diff 미리보기.
///
/// 이 화면은 **아무것도 바꾸지 않는다**. 여기서 계산한 지문([`FIELD_FROM_DIGEST`])이 확인 폼에
/// 실려야 [`convert_submit`]이 실행되므로, 이 화면을 열지 않고 전환하는 경로가 없다.
pub async fn convert_form(State(ctx): State<Arc<ServeConfig>>) -> Response {
    let (document, source) = match ConfigDocument::load_with_source(ctx.config_path.as_deref()) {
        Ok(pair) => pair,
        Err(error) => return load_problem(&ctx, error),
    };
    if let Some(rejected) = reject_non_v1(&ctx, document.syntax) {
        return rejected;
    }
    let plan = match crate::web::config_write::plan_v1_to_v2(&source) {
        Ok(plan) => plan,
        Err(error) => return convert_problem(&ctx, &error),
    };
    let preview = convert_preview(&plan, ctx.jobs.secret_registry());
    layout::shell(
        ctx.lang,
        CONFIG_TITLE,
        view::convert_page(ctx.lang, &preview),
    )
    .into_response()
}

/// `POST /config/convert` — 전환 실행.
///
/// ## 왜 지문을 요구하는가 (미리보기 없이는 실행되지 않는다는 규약의 구현)
/// 이 요청은 config 파일 **전체**를 다시 쓴다. "diff를 보여준 뒤에만 실행한다"는 규약을 문서에
/// 적어 두는 것으로는 지켜지지 않으므로, 미리보기 화면만이 알 수 있는 값을 제출에 요구한다 —
/// 원본 파일 내용의 sha256이다. 이 값이 없거나 다르면 아무것도 하지 않는다.
///
/// 같은 검사가 **TOCTOU도 막는다**: 미리보기와 확인 사이에 다른 요청이 저장을 마쳤거나 누군가
/// 편집기로 파일을 고쳤다면 지문이 달라지므로, 운영자가 화면에서 본 것과 다른 파일을 덮어쓰는
/// 일이 생기지 않는다. 저장소 계층이 파일을 다시 읽어 같은 비교를 한 번 더 한다
/// ([`ConfigStore::convert_to_v2`]) — 그 사이의 창까지 좁히기 위함이다.
///
/// 검사 순서는 [`save`]와 같다: **출처 → 입력 검증 → 감사 게이트 → 쓰기.** 지문 검증이 게이트
/// 앞인 이유는 `save`가 잘못된 폼을 400으로 먼저 끊는 것과 같다 — 실행되지 않은 요청으로
/// append-only 로그를 부풀리지 않는다.
pub async fn convert_submit(
    State(ctx): State<Arc<ServeConfig>>,
    headers: axum::http::HeaderMap,
    body: String,
) -> Response {
    if let Some(rejected) = reject_foreign_origin(&ctx, &headers) {
        return rejected;
    }
    let (document, source) = match ConfigDocument::load_with_source(ctx.config_path.as_deref()) {
        Ok(pair) => pair,
        Err(error) => return write_blocked(&ctx, error),
    };
    if let Some(rejected) = reject_non_v1(&ctx, document.syntax) {
        return rejected;
    }
    let plan = match crate::web::config_write::plan_v1_to_v2(&source) {
        Ok(plan) => plan,
        Err(error) => return convert_problem(&ctx, &error),
    };

    let form = FormBody::parse(&body);
    if form.get(FIELD_FROM_DIGEST) != Some(plan.digest()) {
        return problem(
            &ctx,
            StatusCode::UNPROCESSABLE_ENTITY,
            ctx.lang.sel(
                "This request did not carry the fingerprint of a conversion preview, so nothing was converted.",
                "이 요청에는 전환 미리보기의 지문이 실려 있지 않아 아무것도 전환하지 않았습니다.",
            ),
            Some(ctx.lang.sel(
                "Open the preview, read the diff, and confirm from that page. If you did, the config file changed in the meantime — the preview you saw is no longer what is on disk.",
                "미리보기를 열어 diff를 확인한 뒤 그 화면에서 실행하세요. 이미 그렇게 했다면 그 사이 config 파일이 바뀐 것입니다 — 화면에서 본 내용이 더 이상 디스크의 파일과 같지 않습니다.",
            )),
        );
    }

    apply_conversion_and_render(&ctx, plan).await
}

/// v1이 아닌 config에 대한 전환 요청을 거부한다(이유를 화면에 적는다).
fn reject_non_v1(ctx: &ServeConfig, syntax: ConfigSyntax) -> Option<Response> {
    if syntax == ConfigSyntax::V1 {
        return None;
    }
    Some(problem(
        ctx,
        StatusCode::BAD_REQUEST,
        ctx.lang.sel(
            "This config is not written in the v1 nested syntax, so there is nothing to convert.",
            "이 config는 v1 중첩 문법이 아니어서 전환할 것이 없습니다.",
        ),
        Some(ctx.lang.sel(
            "There is no conversion from v2 back to v1: v2 can express [defaults], [base.<name>] and extends, and flattening that shared policy into every profile would scatter one place to edit into many.",
            "v2에서 v1로 되돌리는 전환은 제공하지 않습니다 — v2는 [defaults]·[base.<name>]·extends를 표현할 수 있고, 그 공용 정책을 모든 프로파일에 펼치면 한 곳에서 고치던 것이 여러 곳으로 흩어집니다.",
        )),
    ))
}

/// 전환 계획을 세우지 못한 이유를 보여준다.
///
/// 422다 — 요청 형태는 정상이고 원인은 **config 파일의 내용**이며(옮길 자리가 없는 키,
/// compact로 접히지 않는 destination), 운영자가 파일을 고치면 같은 요청이 성공한다.
fn convert_problem(ctx: &ServeConfig, error: &XBackupError) -> Response {
    problem(
        ctx,
        StatusCode::UNPROCESSABLE_ENTITY,
        &error.to_string(),
        Some(ctx.lang.sel(
            "Nothing was written. The conversion stops instead of dropping anything it cannot move.",
            "아무것도 쓰지 않았습니다. 전환은 옮길 수 없는 것을 버리는 대신 중단합니다.",
        )),
    )
}

/// 전환 계획을 **가려진** 화면 모델로 접는다.
///
/// 이 함수가 시크릿 경계다. 계획은 원본 값을 들고 있고(`config_write::KeyMove`), 화면 모델은
/// 가려진 문자열만 들고 있다([`view::MovedKey`]) — 그 변환이 여기 한 곳뿐이므로 이 함수만 보면
/// 누출 여부를 판정할 수 있다([`audit_args`]와 같은 구조).
fn convert_preview(
    plan: &crate::web::config_write::ConversionPlan,
    registry: &crate::web::mask::SecretRegistry,
) -> view::ConvertPreview {
    let profiles = plan
        .profiles()
        .iter()
        .map(|profile| view::ConvertedProfile {
            name: profile.name.clone(),
            moves: profile
                .moves
                .iter()
                .map(|moved| view::MovedKey {
                    from: moved.from.clone(),
                    to: moved.to.clone(),
                    value: safe_display(&moved.to, &display_toml(&moved.value), registry),
                })
                .collect(),
        })
        .collect();
    let (new_text, truncated_lines) = redact_config_text(plan.new_text(), registry);
    view::ConvertPreview {
        digest: plan.digest().to_string(),
        profiles,
        new_text,
        truncated_lines,
        bytes_before: plan.original_bytes(),
        bytes_after: plan.new_text().len(),
    }
}

/// 값 하나를 화면에 실을 수 있는 문자열로 접는다.
///
/// 평문 `uri`/`read_uri`만 [`RedactedUri`]로 userinfo를 지우고(그 두 필드가 자격증명을 담을 수
/// 있는 유일한 자리다 — 모듈 헤더 "시크릿"), 마지막에 전체를 시크릿 레지스트리에 통과시킨다
/// (2차 방어 — config가 선언한 시크릿 값이 어느 필드에 붙여넣어졌더라도 지워진다).
fn safe_display(key: &str, value: &str, registry: &crate::web::mask::SecretRegistry) -> String {
    let shown = if key == KEY_URI || key == KEY_READ_URI {
        RedactedUri::from_raw(value).shown().to_string()
    } else {
        value.to_string()
    };
    registry.mask(&shown)
}

/// config 텍스트를 화면에 실을 수 있게 가린다. 돌려주는 두 번째 값은 **잘린 줄 수**다.
///
/// 줄 단위로 `key = value`를 보고 key가 `uri`/`read_uri`면 값을 가린다. TOML 값에 붙은
/// 따옴표는 벗겨서 가린 뒤 다시 씌운다 — [`RedactedUri::from_raw`]는 URI를 받는 함수이고,
/// 따옴표까지 넘기면 authority 판정이 흔들린다.
fn redact_config_text(text: &str, registry: &crate::web::mask::SecretRegistry) -> (String, usize) {
    let total = text.lines().count();
    let shown: Vec<String> = text
        .lines()
        .take(MAX_PREVIEW_LINES)
        .map(|line| redact_config_line(line, registry))
        .collect();
    (shown.join("\n"), total.saturating_sub(shown.len()))
}

/// 한 줄을 가린다.
fn redact_config_line(line: &str, registry: &crate::web::mask::SecretRegistry) -> String {
    let Some((raw_key, raw_value)) = line.split_once('=') else {
        return registry.mask(line);
    };
    let key = raw_key.trim();
    if key != KEY_URI && key != KEY_READ_URI {
        return registry.mask(line);
    }
    let value = raw_value.trim();
    let (quote, inner) = match (
        value.starts_with('"'),
        value.ends_with('"'),
        value.len() >= 2,
    ) {
        (true, true, true) => ("\"", &value[1..value.len() - 1]),
        _ => ("", value),
    };
    registry.mask(&format!(
        "{key} = {quote}{}{quote}",
        RedactedUri::from_raw(inner).shown()
    ))
}

/// 전환을 감사 게이트 뒤에서 실행하고 결과 화면을 만든다.
///
/// [`apply_and_render`]와 같은 순서·같은 이유다(게이트 → 저장 → 완료 기록, `spawn_blocking`으로
/// doctor 자식 대기를 워커 스레드에서 뺀다). 다른 점은 대상이 프로파일 하나가 아니라 파일
/// 전체라는 것뿐이고, 그래서 감사 target이 [`AUDIT_TARGET_WHOLE_FILE`]이다.
async fn apply_conversion_and_render(
    ctx: &ServeConfig,
    plan: crate::web::config_write::ConversionPlan,
) -> Response {
    let Some(config_path) = ctx.config_path.clone() else {
        return load_problem(ctx, LoadError::NotWired);
    };
    let profiles = plan.profiles().len();
    // 감사 인자에는 값을 싣지 않는다 — 전환은 값을 바꾸지 않으므로 남길 정보가 "무엇을 어느
    // 문법으로 옮겼는가"뿐이고, 파일 전체 내용을 append-only 로그에 복사할 이유가 없다.
    let masked_args: Vec<String> = [
        "op=convert".to_string(),
        format!(
            "syntax={}->{}",
            ConfigSyntax::V1.token(),
            ConfigSyntax::V2.token()
        ),
        format!("profiles={profiles}"),
        format!("from-digest={}", plan.digest()),
    ]
    .into_iter()
    .map(|arg| ctx.jobs.secret_registry().mask(&arg))
    .collect();

    let receipt = match ctx
        .audit
        .gate(
            AUDIT_ACTOR,
            AUDIT_ACTION_CONVERT,
            AUDIT_TARGET_WHOLE_FILE,
            &masked_args,
        )
        .await
    {
        Ok(receipt) => receipt,
        Err(error) => {
            tracing::error!(
                action = AUDIT_ACTION_CONVERT,
                error = %error,
                "감사 로그에 기록하지 못해 config 전환을 거부했습니다"
            );
            return problem(
                ctx,
                StatusCode::SERVICE_UNAVAILABLE,
                ctx.lang.sel(
                    "The conversion was refused because it could not be written to the audit log.",
                    "감사 로그에 기록할 수 없어 전환을 거부했습니다 — config 파일은 손대지 않았습니다.",
                ),
                Some(&error.to_string()),
            );
        }
    };

    let store = crate::web::config_write::FileConfigStore::new(config_path);
    let apply_result =
        tokio::task::spawn_blocking(move || store.convert_to_v2(&plan, receipt)).await;

    let result = match apply_result {
        Ok(result) => result,
        Err(join_error) => {
            record_outcome(
                ctx,
                AUDIT_ACTION_CONVERT,
                AUDIT_TARGET_WHOLE_FILE,
                &masked_args,
                AuditOutcome::Failure,
                None,
            )
            .await;
            return problem(
                ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                ctx.lang.sel(
                    "The conversion task panicked unexpectedly.",
                    "전환 작업이 예기치 않게 중단됐습니다.",
                ),
                Some(&join_error.to_string()),
            );
        }
    };

    match result {
        Ok(receipt) => {
            // config가 바뀌면 그 config의 **모든** 프로브를 버린다 — 프로파일이 추가·삭제·
            // 이름 변경될 수 있어 명부까지 낡는다(`crate::web::cache::invalidate_config`).
            if receipt.persisted() {
                crate::web::cache::invalidate_config(ctx);
            }
            let mut warnings = receipt.warnings().to_vec();
            if let Some(note) = record_outcome(
                ctx,
                AUDIT_ACTION_CONVERT,
                AUDIT_TARGET_WHOLE_FILE,
                &masked_args,
                AuditOutcome::Success,
                Some(i32::from(crate::error::exit_codes::SUCCESS)),
            )
            .await
            {
                warnings.push(note);
            }
            let body = view::convert_result_page(
                ctx.lang,
                profiles,
                view::SaveOutcome {
                    persisted: receipt.persisted(),
                    warnings: &warnings,
                },
            );
            layout::shell(ctx.lang, CONFIG_TITLE, body).into_response()
        }
        Err(error) => {
            record_outcome(
                ctx,
                AUDIT_ACTION_CONVERT,
                AUDIT_TARGET_WHOLE_FILE,
                &masked_args,
                AuditOutcome::Failure,
                Some(i32::from(error.exit_code())),
            )
            .await;
            let status = match error.exit_code() {
                crate::error::exit_codes::PRECHECK | crate::error::exit_codes::USAGE => {
                    StatusCode::UNPROCESSABLE_ENTITY
                }
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            problem(ctx, status, &error.to_string(), None)
        }
    }
}

/// 감사 로그 `actor` 필드 값. 이 콘솔에는 세션 토큰 하나만 있고 사용자별 신원이 없으므로
/// (t6), 지금 표현할 수 있는 가장 정직한 값은 "웹에서" 왔다는 사실뿐이다.
///
/// `routes::backup`이 같은 값을 자기 파일에 두고 있다 — 두 곳에 같은 리터럴이 있는 것은
/// 좋지 않지만, 그 파일은 이 태스크의 소유가 아니라 한쪽으로 모으지 못했다(공용 상수로
/// 올리는 것은 후속 정리 대상이다).
const AUDIT_ACTOR: &str = "web";

/// 감사 로그에 남길 인자 목록을 만든다 — **마스킹은 이 함수의 책임이다**
/// ([`crate::web::audit`] 모듈 헤더: 감사 모듈은 받은 문자열을 들여다보지 않는다).
///
/// ## 무엇을 남기고, 무엇을 가리는가
/// 이 화면은 시크릿 *값*을 다루지 않는다(모듈 헤더 "시크릿 — 마스킹이 아니라 부재") —
/// `uri_env`/`credentials_env`는 env 변수 **이름**이므로 그대로 남긴다. 이름이 남아야
/// "누가 자격증명 출처를 바꿨나"를 나중에 되짚을 수 있다.
///
/// 예외가 정확히 둘이다: 평문 `uri`/`read_uri`. config는 이 필드에 자격증명을 쓰지 말라고
/// 못박지만 실제로 그렇게 쓴 파일이 존재하고([`RedactedUri`] doc), 폼으로 그런 값이 들어올
/// 수 있다. **감사 로그는 append-only라 한 번 새면 지울 수 없으므로** 이 두 필드는
/// [`RedactedUri`]로 userinfo를 지운 표기만 남긴다(호스트는 남긴다 — 어디로 바뀌었는지가
/// 감사의 핵심 정보다).
///
/// 마지막으로 전체를 잡 러너의 레지스트리에 한 번 통과시킨다(2차 방어) — 운영자가 이미
/// config에 선언해 둔 시크릿 값이 어떤 필드에 붙여넣어졌더라도 그 값은 지워진다.
fn audit_args(change: &ProfileChange, registry: &crate::web::mask::SecretRegistry) -> Vec<String> {
    let mut args = vec![
        format!("op={}", change.op.label()),
        format!("syntax={}", change.syntax.token()),
    ];
    for (key, value) in change.set_rows() {
        let shown = if key == KEY_URI || key == KEY_READ_URI {
            RedactedUri::from_raw(&value).shown().to_string()
        } else {
            value
        };
        args.push(format!("set:{key}={shown}"));
    }
    for key in change.unset_rows() {
        args.push(format!("unset:{key}"));
    }
    if !change.broke_inheritance.is_empty() {
        args.push(format!(
            "broke-inheritance={}",
            change.broke_inheritance.join(",")
        ));
    }
    args.into_iter().map(|arg| registry.mask(&arg)).collect()
}

/// 변경을 저장 지점에 넘기고 결과 화면을 만든다.
///
/// ## 감사 게이트가 저장보다 **먼저**다
/// [`ConfigStore::apply`]는 [`AuditReceipt`]를 값으로 요구하고, 그 값은
/// [`AuditLog::gate`](crate::web::audit::AuditLog::gate)가 append에 성공했을 때만 만들어진다.
/// 그래서 이 함수의 순서는 선택이 아니라 **컴파일이 강제하는 순서**다: 게이트 → 저장 →
/// 완료 기록. 게이트가 실패하면(디스크 풀·권한 상실) 저장은 시작되지 않고 503으로 끊는다 —
/// "감사 append 실패 = 작업 거부"가 t8이 선언한 불변식이고, config 쓰기는 되돌릴 수 없는
/// 작업이라(특히 삭제) 그 불변식이 가장 필요한 자리다.
///
/// 무변경 제출(`is_noop`)도 게이트를 지난다. 예외를 하나 만들면 그 예외가 우회 경로가 되고,
/// "아무것도 안 바뀌었다"는 판정 자체를 감사 로그가 증언해 주는 편이 낫다.
///
/// ## 왜 여기서 `FileConfigStore`를 직접 만드는가(`store()`를 쓰지 않는 이유)
/// [`store`]는 no-op [`PendingStore`]를 돌려주는 **테스트용 기본값**으로 남겨 뒀다(t26이
/// 만든 검증 경로를 디스크 없이 계속 확인할 수 있도록). 실제 요청은 `ctx.config_path`가
/// 있어야만 무엇을 쓸지 알 수 있는데, `store()`는 인자를 받지 않는 `&'static` 함수라 그
/// 경로를 실을 자리가 없다. 그래서 프로덕션 경로는 요청마다
/// [`crate::web::config_write::FileConfigStore`]를 직접 만들어 쓴다.
///
/// ## 왜 `spawn_blocking`인가
/// [`ConfigStore::apply`]는 동기 트레이트 메서드다(`config_write` 모듈 헤더 참조 — dyn
/// 트레이트를 async로 바꾸려면 `async_trait` boxing이 필요해 파급이 크다). 그 안에서
/// doctor 자식을 기다리는 동안(수십 ms, 최악 20s) 이 함수를 그냥 `await` 없이 부르면 axum
/// 워커 스레드 하나가 그 시간만큼 막힌다. `tokio::task::spawn_blocking`으로 블로킹
/// 스레드풀에 넘기면 다른 요청을 처리하는 워커는 영향받지 않는다.
async fn apply_and_render(ctx: &ServeConfig, change: ProfileChange) -> Response {
    let Some(config_path) = ctx.config_path.clone() else {
        // `load_document`가 이미 `LoadError::NotWired`로 걸러 이 지점에 도달하지 않는다.
        // 그래도 패닉 대신 같은 오류 화면으로 접는다(방어적 — 호출 순서가 바뀌어도 안전).
        return load_problem(ctx, LoadError::NotWired);
    };
    let masked_args = audit_args(&change, ctx.jobs.secret_registry());
    let receipt = match ctx
        .audit
        .gate(
            AUDIT_ACTOR,
            change.op.audit_action(),
            change.name.as_str(),
            &masked_args,
        )
        .await
    {
        Ok(receipt) => receipt,
        Err(error) => {
            // 503 — 서버가 지금 이 작업을 수행할 수 없다는 뜻이다. 400/422(입력이 문제)도,
            // 500(예상 못 한 결함)도 사실과 다르다: 요청은 정상이고 원인은 감사 로그를 쓸
            // 수 없는 서버 상태이며, 운영자가 그것을 고치면 같은 요청이 성공한다.
            tracing::error!(
                action = change.op.audit_action(),
                target = change.name.as_str(),
                error = %error,
                "감사 로그에 기록하지 못해 config 변경을 거부했습니다"
            );
            return problem(
                ctx,
                StatusCode::SERVICE_UNAVAILABLE,
                ctx.lang.sel(
                    "The change was refused because it could not be written to the audit log.",
                    "감사 로그에 기록할 수 없어 변경을 거부했습니다 — config 파일은 손대지 않았습니다.",
                ),
                Some(&error.to_string()),
            );
        }
    };

    let store = crate::web::config_write::FileConfigStore::new(config_path);
    let change_for_task = change.clone();
    let apply_result =
        tokio::task::spawn_blocking(move || store.apply(&change_for_task, receipt)).await;

    let result = match apply_result {
        Ok(result) => result,
        Err(join_error) => {
            record_outcome(
                ctx,
                change.op.audit_action(),
                change.name.as_str(),
                &masked_args,
                AuditOutcome::Failure,
                None,
            )
            .await;
            return problem(
                ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                ctx.lang.sel(
                    "The save task panicked unexpectedly.",
                    "저장 작업이 예기치 않게 중단됐습니다.",
                ),
                Some(&join_error.to_string()),
            );
        }
    };

    match result {
        Ok(receipt) => {
            // config가 바뀌면 그 config의 **모든** 프로브를 버린다 — 프로파일이 추가·삭제·
            // 이름 변경될 수 있어 명부까지 낡는다(`crate::web::cache::invalidate_config`).
            if receipt.persisted() {
                crate::web::cache::invalidate_config(ctx);
            }
            let mut warnings = receipt.warnings().to_vec();
            // 완료 기록이 실패해도 되돌릴 것이 없다(파일은 이미 바뀌었다). 그래서 요청을
            // 실패로 만들지 않되, 그 사실을 **화면에** 남긴다 — 감사 로그에 시작만 있고
            // 결과가 없는 구간이 생겼다는 것은 운영자가 알아야 하는 정보다.
            if let Some(note) = record_outcome(
                ctx,
                change.op.audit_action(),
                change.name.as_str(),
                &masked_args,
                AuditOutcome::Success,
                Some(i32::from(crate::error::exit_codes::SUCCESS)),
            )
            .await
            {
                warnings.push(note);
            }
            let body = view::result_page(
                ctx.lang,
                change.name.as_str(),
                change.op.label(),
                &change.set_rows(),
                &change.unset_rows(),
                &change.broke_inheritance,
                view::SaveOutcome {
                    persisted: receipt.persisted(),
                    warnings: &warnings,
                },
            );
            layout::shell(ctx.lang, CONFIG_TITLE, body).into_response()
        }
        Err(error) => {
            record_outcome(
                ctx,
                change.op.audit_action(),
                change.name.as_str(),
                &masked_args,
                AuditOutcome::Failure,
                Some(i32::from(error.exit_code())),
            )
            .await;
            // doctor 검증이 저장을 막은 것(exit 2/3 → `Config`/`PrecheckFailed`)은 운영자가
            // 제출한 값이 원인이므로 422(Unprocessable Entity)다 — 400은 "요청 형태 자체가
            // 틀렸다"는 뜻이라 여기와 결이 다르고, 500은 "서버 잘못"이라 사실과 다르다.
            // 그 외(IO 오류·자식 spawn 실패·타임아웃 등)는 운영자가 고칠 수 있는 입력이
            // 아니므로 500이다.
            let status = match error.exit_code() {
                crate::error::exit_codes::PRECHECK | crate::error::exit_codes::USAGE => {
                    StatusCode::UNPROCESSABLE_ENTITY
                }
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            problem(ctx, status, &error.to_string(), None)
        }
    }
}

/// 작업이 끝난 뒤 결과(성공/실패)를 감사 로그에 남긴다.
///
/// 게이트([`AuditLog::gate`](crate::web::audit::AuditLog::gate))가 남긴 `requested` 줄과 짝을
/// 이룬다 — `routes::backup`이 잡 시작/종료에 `record()`를 두 번 부르는 것과 같은 형태다.
/// 인자는 게이트와 **같은 마스킹된 목록**을 재사용한다(두 줄이 같은 작업을 가리킨다는 것이
/// 눈으로 보여야 한다).
///
/// 반환값은 "기록에 실패했다면 화면에 띄울 경고 문장"이다. 이 시점의 실패는 되돌릴 수 없다
/// (파일은 이미 바뀌었다) — 요청을 실패로 만드는 대신 로그와 화면 양쪽에 남긴다.
async fn record_outcome(
    ctx: &ServeConfig,
    action: &'static str,
    target: &str,
    masked_args: &[String],
    outcome: AuditOutcome,
    exit_code: Option<i32>,
) -> Option<String> {
    let event = AuditEvent {
        actor: AUDIT_ACTOR,
        action,
        target,
        args_masked: masked_args,
        outcome,
        exit_code,
    };
    match ctx.audit.record(event).await {
        Ok(()) => None,
        Err(error) => {
            tracing::error!(
                action,
                target,
                error = %error,
                "config 변경의 완료 기록을 감사 로그에 남기지 못했습니다 — 작업 자체는 \
                 이미 끝났습니다(되돌릴 수 없음)"
            );
            Some(format!(
                "{} {error}",
                ctx.lang.sel(
                    "The change was applied but its completion could not be written to the audit log:",
                    "변경은 적용됐지만 완료 기록을 감사 로그에 남기지 못했습니다:",
                )
            ))
        }
    }
}

/// 화면 본문만 만드는 순수 렌더 진입점 — 테스트가 자식·파일시스템 없이 렌더를 검증한다
/// ([`crate::web::routes`] 헤더 규약 3).
pub fn render_list(lang: Lang, document: &ConfigDocument, probe: &dyn Fn(&str) -> bool) -> Markup {
    let profiles: Vec<ProfileView> = document
        .config
        .profiles
        .iter()
        .map(|(name, profile)| {
            build_profile_view(lang, name, profile, &document.origins_of(name), probe)
        })
        .collect();
    view::list_page(
        lang,
        document.syntax,
        document.config.default_profile.as_deref(),
        &profiles,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const V1_SAMPLE: &str = r#"
default_profile = "prod"

[profiles.prod.source]
uri_env          = "MONGO_URI_T26"
prefer_secondary = true

[profiles.prod.destination]
type = "s3"

[profiles.prod.destination.s3]
bucket          = "db-backups"
prefix          = "mongo/prod"
region          = "ap-northeast-2"
credentials_env = "S3_CREDS_T26"

[profiles.prod.features.compression]
algorithm = "zstd"
level     = 10

[profiles.prod.retention]
keep_full = 3
"#;

    const V2_SAMPLE: &str = r#"
default_profile = "prod"

[defaults]
compress = "zstd:3"
encrypt = "age:/keys/age.pub"

[base.s3prod]
dest = "s3:db-backups/mongo"
s3_region = "ap-northeast-2"
compress = "zstd:9"

[profile.prod]
extends = "s3prod"
uri_env = "MONGO_URI_T26"
keep_full = 4

[profile.dr]
uri = "mongodb://dr.internal:27117/app"
encrypt = false
"#;

    /// v1(`profiles.*`)과 v2(`profile.*`)가 한 파일에 섞인 표본 — 파서가 거부하는 형태다.
    const MIXED_SAMPLE: &str = r#"
default_profile = "prod"

[profiles.prod.source]
uri_env = "MONGO_URI_T28"

[profile.dr]
uri_env = "MONGO_URI_T28"
"#;

    /// **읽을 수 없는 config에 대한 쓰기는 409다** — 200이면 "저장했다"와 구분되지 않는다.
    ///
    /// 이 테스트가 잡는 회귀: `load_problem`(200)을 쓰기 경로에 그대로 쓰면, 아무것도
    /// 저장하지 않은 POST가 200을 낸다. 사람은 화면의 배너를 읽지만 `curl`·스크립트는
    /// 상태 코드만 본다(실측으로 확인된 실제 동작이었다).
    ///
    /// 읽기 경로는 200을 유지한다 — 화면이 정상적으로 렌더되고 이유를 설명하기 때문이다.
    #[tokio::test]
    async fn writing_to_an_unreadable_config_conflicts_while_reading_stays_ok() {
        use axum::body::Body;
        use axum::http::{header, Request, StatusCode};
        use axum::routing::{get, post};
        use axum::{middleware, Router};
        use tower::ServiceExt;

        const TOKEN: &str = "t28-mixed-config-token";
        let dir = std::env::temp_dir().join(format!("x-backup-t28-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("config.toml");
        std::fs::write(&config_path, MIXED_SAMPLE).unwrap();

        let ctx = Arc::new(ServeConfig {
            config_path: Some(config_path.clone()),
            ..ServeConfig::for_test()
        });
        let (auth, session) = crate::web::auth::AuthState::for_test_with_session(TOKEN);

        let app = Router::new()
            .route(CONFIG_PATH, get(list))
            .route(SAVE_PATH, post(save))
            .route(DELETE_PATH, post(delete_submit))
            .route_layer(middleware::from_fn_with_state(
                auth,
                crate::web::auth::require_auth,
            ))
            .with_state(ctx);

        let origin = "http://127.0.0.1:8787";
        let call = |method: &str, uri: &str, body: &str| {
            let mut builder = Request::builder().method(method).uri(uri).header(
                header::COOKIE,
                format!("{}={session}", crate::web::auth::SESSION_COOKIE_NAME),
            );
            if method == "POST" {
                builder = builder
                    .header(header::ORIGIN, origin)
                    .header(header::HOST, "127.0.0.1:8787")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
            }
            let request = builder.body(Body::from(body.to_string())).unwrap();
            let app = app.clone();
            async move {
                app.oneshot(request)
                    .await
                    .expect("라우터 호출 실패")
                    .status()
            }
        };

        // 읽기 — 화면은 정상 렌더되고 이유를 설명한다.
        assert_eq!(
            call("GET", CONFIG_PATH, "").await,
            StatusCode::OK,
            "읽기 화면이 200이 아니다 — 배너로 설명하는 것이 정상 동작이다"
        );

        // 쓰기 — 아무것도 하지 않았으므로 200이면 안 된다.
        for (uri, body) in [
            (SAVE_PATH, "name=newp&source_env=MONGO_URI_T28"),
            (DELETE_PATH, "name=prod"),
        ] {
            assert_eq!(
                call("POST", uri, body).await,
                StatusCode::CONFLICT,
                "{uri}가 아무것도 저장하지 않았는데 409가 아니다"
            );
        }

        // 파일은 손대지 않았다.
        let after = std::fs::read_to_string(&config_path).unwrap();
        assert_eq!(after, MIXED_SAMPLE, "거부했는데 파일이 바뀌었다");
    }

    fn document(text: &str) -> ConfigDocument {
        ConfigDocument::parse(text).expect("표본이 파싱되어야 함")
    }

    fn no_env(_: &str) -> bool {
        false
    }

    fn form(pairs: &[(&str, &str)]) -> FormBody {
        let body = pairs
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        FormBody::parse(&body)
    }

    // -- 문법 판별 --------------------------------------------------------

    /// v1 config는 v1으로 판정된다 — 저장이 형식을 보존할 근거다.
    #[test]
    fn v1_config_is_detected_as_v1() {
        let doc = document(V1_SAMPLE);
        assert_eq!(doc.syntax, ConfigSyntax::V1);
        assert!(doc.config.profiles.contains_key("prod"));
    }

    /// v2 config는 v2로 판정된다.
    #[test]
    fn v2_config_is_detected_as_v2() {
        assert_eq!(document(V2_SAMPLE).syntax, ConfigSyntax::V2);
    }

    /// 프로파일 섹션이 없는 config는 Empty — 없는 문법을 추측하지 않는다.
    #[test]
    fn config_without_profiles_is_empty_syntax() {
        assert_eq!(document("").syntax, ConfigSyntax::Empty);
        assert_eq!(
            document("[output]\nlanguage = \"ko\"\n").syntax,
            ConfigSyntax::Empty
        );
    }

    /// v1과 v2가 섞인 파일은 거부된다 — 판정을 로더에 맡기므로 메시지도 로더의 것이다.
    #[test]
    fn mixed_v1_and_v2_is_rejected() {
        let error =
            ConfigDocument::parse("[profiles.a.source]\nuri=\"x\"\n[profile.b]\nuri=\"y\"\n")
                .expect_err("혼합은 거부되어야 함");
        let LoadError::Parse(detail) = &error else {
            panic!("파싱 오류로 접혀야 함: {error:?}");
        };
        assert!(detail.contains("섞여 있"), "혼합 사유가 없다: {detail}");
    }

    /// `extends` 순환은 명확한 오류가 된다.
    #[test]
    fn extends_cycle_is_a_clear_error() {
        let error = ConfigDocument::parse(
            "[profile.a]\nextends = \"b\"\nuri=\"mongodb://h/a\"\n\
             [profile.b]\nextends = \"a\"\nuri=\"mongodb://h/b\"\n",
        )
        .expect_err("순환은 거부되어야 함");
        let LoadError::Parse(detail) = &error else {
            panic!("파싱 오류로 접혀야 함: {error:?}");
        };
        assert!(detail.contains("순환"), "순환 사유가 없다: {detail}");
        assert!(
            !error.explain(Lang::Ko).is_empty(),
            "설명 문장이 비어 있으면 화면이 아무것도 말하지 못한다"
        );
    }

    /// 존재하지 않는 base를 가리키면 명확한 오류가 된다.
    #[test]
    fn missing_extends_base_is_a_clear_error() {
        let error =
            ConfigDocument::parse("[profile.a]\nextends = \"nope\"\nuri=\"mongodb://h/a\"\n")
                .expect_err("없는 base는 거부되어야 함");
        let LoadError::Parse(detail) = &error else {
            panic!("파싱 오류로 접혀야 함: {error:?}");
        };
        assert!(
            detail.contains("nope"),
            "어느 base인지 지목하지 않았다: {detail}"
        );
        assert!(
            detail.contains("찾을 수 없습니다"),
            "사유가 명확하지 않다: {detail}"
        );
    }

    /// config가 연결되지 않은 서버는 그 사실을 오류로 구분한다(파일 문제와 다르다).
    #[test]
    fn missing_config_path_is_not_wired() {
        assert_eq!(ConfigDocument::load(None).unwrap_err(), LoadError::NotWired);
        let error = ConfigDocument::load(Some(FsPath::new("/nonexistent/x-backup-t26.toml")))
            .expect_err("없는 파일은 읽기 오류");
        assert!(matches!(error, LoadError::Read(_)), "{error:?}");
    }

    // -- 출처 추적 --------------------------------------------------------

    /// v2에서 값의 출처가 직접 지정 / extends 상속 / defaults로 구분된다.
    #[test]
    fn v2_origins_distinguish_direct_extends_and_defaults() {
        let doc = document(V2_SAMPLE);
        let prod = doc.origins_of("prod");

        // 프로파일에 직접 적힌 키.
        assert_eq!(prod.origin_of(&["uri_env"]), Origin::Direct);
        assert_eq!(prod.origin_of(&["keep_full"]), Origin::Direct);

        // base(s3prod)에서 상속 — 출처 이름까지 남는다.
        assert_eq!(
            prod.origin_of(&["dest"]),
            Origin::Inherited {
                from: "s3prod".to_string()
            }
        );
        assert_eq!(
            prod.origin_of(&["s3_region"]),
            Origin::Inherited {
                from: "s3prod".to_string()
            }
        );
        // compress는 defaults에도 있지만 base가 덮으므로 base가 출처다(로더의 우선순위와 동일).
        assert_eq!(
            prod.origin_of(&["compress_level", "compress"]),
            Origin::Inherited {
                from: "s3prod".to_string()
            }
        );

        // [defaults]에서만 온 키.
        assert_eq!(prod.origin_of(&["encrypt"]), Origin::Defaults);

        // config에 아예 없는 키는 내장 기본값.
        assert_eq!(prod.origin_of(&["engine"]), Origin::Builtin);
        assert_eq!(prod.origin_of(&["keep_days"]), Origin::Builtin);

        // 다른 프로파일은 자기 키가 직접, defaults는 그대로 defaults.
        let dr = doc.origins_of("dr");
        assert_eq!(dr.origin_of(&["uri"]), Origin::Direct);
        assert_eq!(
            dr.origin_of(&["encrypt"]),
            Origin::Direct,
            "override는 직접"
        );
        assert_eq!(
            dr.origin_of(&["compress_level", "compress"]),
            Origin::Defaults
        );
    }

    /// v1에서는 적힌 키가 전부 직접 지정으로 기록된다(v1에는 상속 표면이 없다).
    #[test]
    fn v1_origins_are_all_direct() {
        let origins = document(V1_SAMPLE).origins_of("prod");
        for key in [
            "uri_env",
            "prefer_secondary",
            "dest",
            "s3_region",
            "s3_creds",
            "compress_algorithm",
            "compress_level",
            "keep_full",
        ] {
            assert_eq!(
                origins.origin_of(&[key]),
                Origin::Direct,
                "'{key}'가 직접 지정으로 기록되지 않았다"
            );
        }
        // 적히지 않은 키는 내장 기본값이다 — v1이라고 전부 Direct가 되면 안 된다.
        assert_eq!(origins.origin_of(&["engine"]), Origin::Builtin);
        assert_eq!(origins.origin_of(&["keep_days"]), Origin::Builtin);
        assert!(origins.direct_keys().contains(&"dest"));
    }

    /// 출처 계산이 값 해석과 같은 우선순위를 쓴다 — 화면이 말하는 출처의 값이 실제
    /// 실효값과 일치해야 한다.
    #[test]
    fn origin_and_effective_value_agree() {
        let doc = document(V2_SAMPLE);
        let profile = doc.config.profile("prod").unwrap();
        // base가 defaults를 덮었으므로 실효 level은 9(base), 3(defaults)이 아니다.
        assert_eq!(profile.features.compression.level, 9);
        assert_eq!(
            doc.origins_of("prod")
                .origin_of(&["compress_level", "compress"]),
            Origin::Inherited {
                from: "s3prod".to_string()
            }
        );
    }

    // -- 시크릿 부재 ------------------------------------------------------

    /// 시크릿 값이 화면 어디에도 없다 — env 이름만 있고 값은 없다.
    ///
    /// 타입 수준 단정: [`SecretEnvRef`]에는 값 필드가 없고, [`RedactedUri`]는 생성자가
    /// 자격증명을 지운다. 여기서는 그 성질이 **렌더 결과에서도** 유지되는지 확인한다.
    #[test]
    fn secret_values_never_reach_the_markup() {
        const FAKE_URI_SECRET: &str = "NOT-A-REAL-SECRET-t26-uri-3f91a2";
        const FAKE_ENV_SECRET: &str = "NOT-A-REAL-SECRET-t26-env-77b0cd";
        let text = format!(
            "[profile.leaky]\n\
             uri = \"mongodb://admin:{FAKE_URI_SECRET}@db.internal:27017/app\"\n\
             dest = \"local:/srv/b\"\n\
             s3_creds = \"S3_CREDS_T26\"\n"
        );
        let doc = document(&text);
        // env가 실제로 설정돼 있어도 값이 화면에 들어가면 안 된다.
        let probe = |_: &str| true;
        let markup = render_list(Lang::En, &doc, &probe).into_string();
        assert!(
            !markup.contains(FAKE_URI_SECRET),
            "URI 비밀번호가 화면에 남았다: {markup}"
        );
        assert!(!markup.contains("admin"), "URI 사용자명이 남았다");
        assert!(
            !markup.contains(FAKE_ENV_SECRET),
            "env 값이 화면에 들어갔다(애초에 읽지 않아야 한다)"
        );
        // 이름과 호스트는 남아야 진단이 된다.
        assert!(markup.contains("S3_CREDS_T26"), "env 이름이 사라졌다");
        assert!(markup.contains("db.internal"), "호스트가 사라졌다");

        // 편집 폼에서도 같다 — 가린 표기가 입력 칸 value로 새지 않는다.
        let profile = doc.config.profile("leaky").unwrap();
        let profile_view =
            build_profile_view(Lang::En, "leaky", profile, &doc.origins_of("leaky"), &probe);
        let form_markup =
            view::form_page(Lang::En, doc.syntax, FormMode::Update, &profile_view).into_string();
        assert!(
            !form_markup.contains(FAKE_URI_SECRET),
            "폼에 시크릿이 남았다"
        );
        assert!(
            form_markup.contains("carries credentials"),
            "자격증명 경고가 없다: {form_markup}"
        );
        assert!(
            !form_markup.contains(r#"name="uri" value="#),
            "URI 입력 칸에 값이 미리 채워졌다 — 제출이 원본을 파괴한다: {form_markup}"
        );
    }

    /// 설정된 env와 미설정 env가 불리언으로 구분된다.
    #[test]
    fn env_presence_is_reported_per_name() {
        let doc = document(V1_SAMPLE);
        let profile = doc.config.profile("prod").unwrap();
        let origins = doc.origins_of("prod");

        let all_set = build_profile_view(Lang::En, "prod", profile, &origins, &|_| true);
        assert!(!all_set.secrets.is_empty(), "시크릿 참조를 못 모았다");
        assert!(all_set.secrets.iter().all(SecretEnvRef::is_present));
        assert!(!all_set.has_missing_secret());

        let none_set = build_profile_view(Lang::En, "prod", profile, &origins, &no_env);
        assert!(none_set.secrets.iter().all(|s| !s.is_present()));
        assert!(none_set.has_missing_secret());

        // 이름별로 갈린다 — "하나라도 없으면 전부 없음"이 아니다.
        let mixed = build_profile_view(Lang::En, "prod", profile, &origins, &|name| {
            name == "MONGO_URI_T26"
        });
        let by_name: BTreeMap<&str, bool> = mixed
            .secrets
            .iter()
            .map(|s| (s.name(), s.is_present()))
            .collect();
        assert_eq!(by_name.get("MONGO_URI_T26"), Some(&true));
        assert_eq!(by_name.get("S3_CREDS_T26"), Some(&false));
    }

    /// [`env_is_set`]은 빈 값을 미설정으로 본다(`config::merged`와 같은 판정).
    #[test]
    fn empty_env_counts_as_unset() {
        // 프로세스 env를 만지는 테스트는 auth 모듈의 가드로 직렬화한다.
        let _guard = crate::web::auth::ENV_GUARD
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        const NAME: &str = "XB_T26_PROBE_EMPTY";
        // SAFETY: 테스트 전용 — ENV_GUARD가 프로세스 env 접근을 직렬화한다.
        unsafe {
            std::env::set_var(NAME, "");
        }
        assert!(!env_is_set(NAME), "빈 env가 설정된 것으로 판정됐다");
        // SAFETY: 위와 같다.
        unsafe {
            std::env::set_var(NAME, "value");
        }
        assert!(env_is_set(NAME));
        // SAFETY: 위와 같다.
        unsafe {
            std::env::remove_var(NAME);
        }
        assert!(!env_is_set(NAME));
    }

    // -- 프로파일명 화이트리스트 ------------------------------------------

    /// 프로파일명 규칙 위반은 전부 거부된다 — 규칙 자체는 t10의 [`ProfileName`]을
    /// 재사용하므로(중복 구현 금지) 여기서는 **이 화면이 그 관문을 거치는지**를 본다.
    #[test]
    fn profile_name_whitelist_is_enforced() {
        let doc = document(V2_SAMPLE);
        let origins = doc.origins_of("prod");
        let hostile = [
            "",                 // 빈 문자열
            "..",               // 상위 디렉터리
            ".",                // 현재 디렉터리
            "../../etc/passwd", // 경로 탈출
            "a/b",              // 경로 구분자
            "a\\b",             // 윈도 구분자
            ".hidden",          // 숨김 파일
            "рrod",             // 키릴 р(호모글리프)
            "prod\u{200b}",     // 폭 없는 공백
            "한글프로파일",     // 비-ASCII
            " prod",            // 선행 공백
            "--force",          // 옵션처럼 보이는 이름
            "p\0.lock",         // 널 바이트
        ];
        for name in hostile {
            let error = ProfileName::parse(name, Lang::En)
                .err()
                .unwrap_or_else(|| panic!("'{name}'은 거부되어야 함"));
            assert_eq!(
                error.exit_code(),
                crate::error::exit_codes::USAGE,
                "'{name}': {error}"
            );
        }
        // 과도한 길이도 거부된다.
        let too_long = "a".repeat(MAX_PROFILE_NAME_LEN + 1);
        assert!(ProfileName::parse(&too_long, Lang::En).is_err());

        // 통과하는 이름은 변경 요청까지 만들어진다.
        let change = build_change(
            ChangeOp::Update,
            ProfileName::parse("prod", Lang::En).unwrap(),
            ConfigSyntax::V2,
            &origins,
            true,
            None,
            &form(&[]),
        )
        .expect("정상 이름은 통과해야 함");
        assert_eq!(change.name.as_str(), "prod");
    }

    /// config에 웹 규칙을 벗어난 이름이 있어도 화면은 깨지지 않고, 그 프로파일만 편집
    /// 대상에서 빠진다(CLI에서는 계속 동작한다).
    #[test]
    fn unsafe_names_in_config_are_shown_but_not_editable() {
        let doc = document("[profile.\"weird name\"]\nuri = \"mongodb://h/db\"\n");
        let profile = doc.config.profile("weird name").unwrap();
        let profile_view = build_profile_view(
            Lang::En,
            "weird name",
            profile,
            &doc.origins_of("weird name"),
            &no_env,
        );
        assert!(
            !profile_view.editable,
            "위험한 이름이 편집 가능으로 표시됐다"
        );
        assert!(profile_view.name_error.is_some(), "이유가 없다");
        let markup = render_list(Lang::En, &doc, &no_env).into_string();
        assert!(markup.contains("weird name"), "이름 자체는 보여야 한다");
        assert!(
            !markup.contains(&edit_href("weird name")),
            "편집 링크가 만들어졌다: {markup}"
        );
    }

    // -- 변경 조립: 상속 보존이 핵심 --------------------------------------

    /// **아무것도 입력하지 않은 제출은 아무것도 바꾸지 않는다.**
    ///
    /// 이 테스트가 함정 2를 고정한다 — 상속받은 값이 입력 칸에 채워져 있었다면 이 제출이
    /// 그 값 전부를 프로파일에 박아 넣었을 것이다.
    #[test]
    fn empty_submission_preserves_inheritance() {
        let doc = document(V2_SAMPLE);
        let origins = doc.origins_of("prod");
        let change = build_change(
            ChangeOp::Update,
            ProfileName::parse("prod", Lang::En).unwrap(),
            doc.syntax,
            &origins,
            true,
            // 폼이 상속 필드를 빈 값으로 제출한 상황을 그대로 재현한다.
            None,
            &form(&[
                ("compress_level", ""),
                ("encrypt", ""),
                ("dest", ""),
                ("s3_region", ""),
                ("engine", ""),
            ]),
        )
        .expect("빈 제출은 통과해야 함");
        assert!(change.set.is_empty(), "상속 값이 박혔다: {:?}", change.set);
        assert!(
            change.unset.is_empty(),
            "상속 값이 지워졌다(프로파일에 없던 키다): {:?}",
            change.unset
        );
        assert!(change.broke_inheritance.is_empty());
        assert!(change.is_noop());
        // 원본 문법이 변경 요청에 실려 있다 — 저장이 추측하지 않는다.
        assert_eq!(change.syntax, ConfigSyntax::V2);
    }

    /// 상속 필드에 값을 적으면 override가 되고, 그 사실이 기록된다.
    #[test]
    fn typing_into_an_inherited_field_records_the_override() {
        let doc = document(V2_SAMPLE);
        let change = build_change(
            ChangeOp::Update,
            ProfileName::parse("prod", Lang::En).unwrap(),
            doc.syntax,
            &doc.origins_of("prod"),
            true,
            None,
            &form(&[("compress_level", "12"), ("encrypt", "false")]),
        )
        .unwrap();
        assert_eq!(change.set.get("compress_level"), Some(&Value::Integer(12)));
        assert_eq!(change.set.get("encrypt"), Some(&Value::Boolean(false)));
        assert!(
            change
                .broke_inheritance
                .contains(&"compress_level".to_string()),
            "extends 상속을 끊었다는 사실이 기록되지 않았다: {:?}",
            change.broke_inheritance
        );
        assert!(
            change.broke_inheritance.contains(&"encrypt".to_string()),
            "defaults 상속을 끊었다는 사실이 기록되지 않았다"
        );
        // 상속 키는 프로파일에 없으므로 지울 것도 없다.
        assert!(change.unset.is_empty(), "{:?}", change.unset);
    }

    /// 직접 적힌 값을 비우면 그 키가 지워진다.
    #[test]
    fn clearing_a_direct_field_removes_the_key() {
        let doc = document(V2_SAMPLE);
        let change = build_change(
            ChangeOp::Update,
            ProfileName::parse("prod", Lang::En).unwrap(),
            doc.syntax,
            &doc.origins_of("prod"),
            true,
            None,
            &form(&[("keep_full", ""), ("uri_env", "")]),
        )
        .unwrap();
        assert!(change.unset.contains("keep_full"));
        assert!(change.unset.contains("uri_env"));
        assert!(change.set.is_empty());
        assert!(change.broke_inheritance.is_empty());
    }

    /// 명시 키를 새로 쓰면 같은 값을 대표하던 compact 키가 지워진다 — 한 프로파일에
    /// `compress`와 `compress_level`이 공존하지 않게 한다.
    #[test]
    fn writing_an_explicit_key_removes_the_compact_one() {
        // compact `compress`가 프로파일에 **직접** 적힌 경우.
        let doc = document("[profile.p]\nuri_env=\"U_T26\"\ncompress = \"zstd:6\"\n");
        let origins = doc.origins_of("p");
        assert_eq!(
            origins.origin_of(&["compress_level", "compress"]),
            Origin::Direct
        );
        let change = build_change(
            ChangeOp::Update,
            ProfileName::parse("p", Lang::En).unwrap(),
            doc.syntax,
            &origins,
            true,
            None,
            &form(&[("compress_level", "7"), ("compress_algorithm", "zstd")]),
        )
        .unwrap();
        assert_eq!(change.set.get("compress_level"), Some(&Value::Integer(7)));
        assert!(
            change.unset.contains("compress"),
            "compact 키가 남아 충돌한다: {:?}",
            change.unset
        );
    }

    /// `set`과 `unset`이 같은 키를 다투면 `set`이 이긴다 — compact `encrypt`를 편집하는
    /// 흔한 경로가 여기 걸린다.
    #[test]
    fn set_wins_over_unset_for_the_same_key() {
        let doc = document("[profile.p]\nuri_env=\"U_T26\"\nencrypt = \"age:/keys/age.pub\"\n");
        let change = build_change(
            ChangeOp::Update,
            ProfileName::parse("p", Lang::En).unwrap(),
            doc.syntax,
            &doc.origins_of("p"),
            true,
            None,
            &form(&[
                ("encrypt", "true"),
                ("encrypt_algorithm", "age"),
                ("recipient_file", "%2Fkeys%2Fnew.pub"),
            ]),
        )
        .unwrap();
        assert_eq!(change.set.get("encrypt"), Some(&Value::Boolean(true)));
        assert_eq!(
            change.set.get("recipient_file"),
            Some(&Value::String("/keys/new.pub".to_string())),
            "퍼센트 인코딩이 디코드되지 않았다"
        );
        assert!(
            !change.unset.contains("encrypt"),
            "쓰면서 동시에 지우는 요청이 나갔다: {:?}",
            change.unset
        );
    }

    /// URI 칸은 빈 값이 "변경 없음"이고, 지우려면 체크박스가 필요하다.
    #[test]
    fn empty_uri_means_no_change_and_clearing_needs_the_checkbox() {
        let doc =
            document("[profile.p]\nuri = \"mongodb://h:27017/db\"\ndest = \"local:/srv/b\"\n");
        let origins = doc.origins_of("p");
        assert_eq!(origins.origin_of(&["uri"]), Origin::Direct);

        // 빈 제출 — uri는 그대로 남는다(직접 적힌 값인데도 unset되지 않는다).
        let untouched = build_change(
            ChangeOp::Update,
            ProfileName::parse("p", Lang::En).unwrap(),
            doc.syntax,
            &origins,
            true,
            None,
            &form(&[("uri", "")]),
        )
        .unwrap();
        assert!(
            !untouched.unset.contains("uri"),
            "빈 URI 제출이 접속 정보를 지웠다"
        );

        // 체크박스가 있으면 지운다.
        let cleared = build_change(
            ChangeOp::Update,
            ProfileName::parse("p", Lang::En).unwrap(),
            doc.syntax,
            &origins,
            true,
            None,
            &form(&[("uri", ""), ("uri_clear", "1")]),
        )
        .unwrap();
        assert!(cleared.unset.contains("uri"), "명시적 삭제가 무시됐다");
    }

    /// 자격증명이 섞인 URI 입력은 거부된다 — 웹이 config에 시크릿을 써 넣는 경로를 막는다.
    #[test]
    fn plain_uri_with_credentials_is_rejected() {
        let doc = document(V2_SAMPLE);
        let error = build_change(
            ChangeOp::Update,
            ProfileName::parse("dr", Lang::En).unwrap(),
            doc.syntax,
            &doc.origins_of("dr"),
            true,
            None,
            &form(&[("uri", "mongodb%3A%2F%2Fu%3Ap%40h%2Fdb")]),
        )
        .expect_err("자격증명이 섞인 URI는 거부되어야 함");
        assert!(
            error.to_string().contains("uri_env"),
            "대안 안내 누락: {error}"
        );
        assert_eq!(error.exit_code(), crate::error::exit_codes::USAGE);
    }

    /// 잘못된 값은 전부 사용법 오류(exit 2)로 거부된다 — 라우트가 400으로 접을 수 있게
    /// 종류를 하나로 고정한다.
    #[test]
    fn invalid_values_are_usage_errors() {
        let doc = document(V2_SAMPLE);
        let origins = doc.origins_of("prod");
        let bad: &[(&str, &str)] = &[
            ("engine", "rsync"),              // 목록 밖
            ("compress_algorithm", "gzip"),   // 목록 밖
            ("encrypt", "yes"),               // 불리언 아님
            ("keep_full", "-1"),              // 범위 밖(u32)
            ("keep_full", "abc"),             // 정수 아님
            ("uri_env", "HAS%2DDASH"),        // env 이름에 `-`
            ("uri_env", "1START"),            // 숫자 시작
            ("dest", "ftp%3A%2F%2Fhost%2Fp"), // 알 수 없는 스킴
            ("dest", "local%3A"),             // 빈 경로
            ("dest", "nocolon"),              // 스킴 없음
            ("uri", "no-scheme"),             // 스킴 없음
            ("recipient_file", "a%0Ab"),      // 개행(제어문자)
        ];
        for (key, value) in bad {
            let error = build_change(
                ChangeOp::Update,
                ProfileName::parse("prod", Lang::En).unwrap(),
                doc.syntax,
                &origins,
                true,
                None,
                &form(&[(key, value)]),
            )
            .err()
            .unwrap_or_else(|| panic!("{key}={value}는 거부되어야 함"));
            assert_eq!(
                error.exit_code(),
                crate::error::exit_codes::USAGE,
                "{key}={value}: {error}"
            );
        }
        // 지나치게 긴 값도 거부된다.
        let huge = "a".repeat(MAX_FIELD_VALUE_LEN + 1);
        assert!(build_change(
            ChangeOp::Update,
            ProfileName::parse("prod", Lang::En).unwrap(),
            doc.syntax,
            &origins,
            true,
            None,
            &form(&[("recipient_file", &huge)]),
        )
        .is_err());
    }

    /// POSIX 환경변수 이름 문법을 만족하면 통과한다 — 대문자 관행을 **규칙으로 승격시키지
    /// 않는다.** 소문자 env 이름을 웹에서만 거부하면 "CLI로는 되는데 콘솔로는 안 되는
    /// config"가 생긴다([`crate::web`] 헤더의 불변식).
    #[test]
    fn conventional_and_lowercase_env_names_are_accepted() {
        for name in ["MONGO_URI", "_HIDDEN", "lower_case", "Mixed_9"] {
            assert_eq!(
                validate_env_name("uri_env", name).unwrap(),
                name,
                "'{name}'은 통과해야 함"
            );
        }
    }

    /// **제출에 없는 필드는 건드리지 않는다.**
    ///
    /// "빈 값으로 제출됨"(= 지워라)과 "아예 제출되지 않음"(= 손대지 마라)이 갈리지 않으면
    /// 부분 제출 하나가 프로파일 절반을 지운다. 이 화면에서 값이 사라지는 가장 큰 사고
    /// 경로이므로 성질을 테스트로 고정한다.
    #[test]
    fn fields_absent_from_the_submission_are_untouched() {
        // 직접 적힌 키가 여럿 있는 v1 프로파일 — 폼은 그중 하나만 제출한다.
        let doc = document(V1_SAMPLE);
        let origins = doc.origins_of("prod");
        assert_eq!(origins.origin_of(&["uri_env"]), Origin::Direct);
        assert_eq!(origins.origin_of(&["keep_full"]), Origin::Direct);

        let change = build_change(
            ChangeOp::Update,
            ProfileName::parse("prod", Lang::En).unwrap(),
            doc.syntax,
            &origins,
            true,
            None,
            &form(&[("keep_days", "7")]),
        )
        .unwrap();
        assert_eq!(change.set.get("keep_days"), Some(&Value::Integer(7)));
        assert!(
            change.unset.is_empty(),
            "제출되지 않은 직접 지정 키가 지워졌다: {:?}",
            change.unset
        );

        // 같은 키를 **빈 값으로** 제출하면 그때는 지운다 — 두 뜻이 실제로 갈린다.
        let cleared = build_change(
            ChangeOp::Update,
            ProfileName::parse("prod", Lang::En).unwrap(),
            doc.syntax,
            &origins,
            true,
            None,
            &form(&[("uri_env", "")]),
        )
        .unwrap();
        assert!(cleared.unset.contains("uri_env"));
    }

    /// **폼을 그대로 다시 제출하면 아무것도 바뀌지 않는다.**
    ///
    /// 브라우저는 직접 적힌 필드를 미리 채운 값으로 전부 다시 제출한다. 그것이 전부
    /// 변경으로 세어지면 결과 화면이 "키 12개를 씁니다"라고 말하고, 실제로 고친 한 줄이
    /// 그 소음에 묻힌다.
    #[test]
    fn resubmitting_the_rendered_form_unchanged_is_a_noop() {
        let doc = document(V1_SAMPLE);
        let profile = doc.config.profile("prod").unwrap();
        let origins = doc.origins_of("prod");
        let current = build_profile_view(Lang::En, "prod", profile, &origins, &no_env);

        // 브라우저가 보낼 본문을 화면 모델에서 그대로 만든다 — 미리 채운 값은 그 값,
        // 상속·기본값 칸은 빈 값.
        let pairs: Vec<(String, String)> = current
            .fields
            .iter()
            .map(|field| {
                (
                    field.key.to_string(),
                    field.prefill().unwrap_or("").to_string(),
                )
            })
            .collect();
        let body = pairs
            .iter()
            .map(|(k, v)| format!("{k}={}", v.replace('/', "%2F")))
            .collect::<Vec<_>>()
            .join("&");

        let change = build_change(
            ChangeOp::Update,
            ProfileName::parse("prod", Lang::En).unwrap(),
            doc.syntax,
            &origins,
            current.destination_editable,
            Some(&current),
            &FormBody::parse(&body),
        )
        .expect("렌더된 폼을 그대로 제출하면 통과해야 함");
        assert!(
            change.is_noop(),
            "무변경 재제출이 변경으로 세어졌다: set={:?} unset={:?}",
            change.set_rows(),
            change.unset_rows()
        );

        // 한 칸만 고치면 그 한 칸만 변경으로 나온다.
        let edited = body.replace("keep_full=3", "keep_full=5");
        let change = build_change(
            ChangeOp::Update,
            ProfileName::parse("prod", Lang::En).unwrap(),
            doc.syntax,
            &origins,
            current.destination_editable,
            Some(&current),
            &FormBody::parse(&edited),
        )
        .unwrap();
        assert_eq!(
            change.set_rows(),
            vec![("keep_full".to_string(), "5".to_string())],
            "고친 한 줄만 남아야 한다"
        );
        assert!(change.unset_rows().is_empty());
    }

    /// 새 프로파일에는 소스가 필요하다 — 아무것도 할 수 없는 프로파일을 만들지 않는다.
    #[test]
    fn create_requires_a_source() {
        let doc = document(V2_SAMPLE);
        let error = build_change(
            ChangeOp::Create,
            ProfileName::parse("fresh", Lang::En).unwrap(),
            doc.syntax,
            &ProfileOrigins::default(),
            true,
            None,
            &form(&[("keep_full", "3")]),
        )
        .expect_err("소스 없는 생성은 거부되어야 함");
        assert!(error.to_string().contains("uri_env"), "{error}");

        let ok = build_change(
            ChangeOp::Create,
            ProfileName::parse("fresh", Lang::En).unwrap(),
            doc.syntax,
            &ProfileOrigins::default(),
            true,
            None,
            &form(&[("uri_env", "FRESH_URI"), ("dest", "local%3A%2Fsrv%2Fb")]),
        )
        .expect("소스가 있으면 통과");
        assert_eq!(
            ok.set.get("uri_env"),
            Some(&Value::String("FRESH_URI".to_string()))
        );
        assert_eq!(
            ok.set.get("dest"),
            Some(&Value::String("local:/srv/b".to_string()))
        );
    }

    /// destination을 편집하지 않는 프로파일에서는 destination 필드 제출이 무시된다 —
    /// 폼 밖에서 만든 요청이 사본을 지우지 못한다.
    #[test]
    fn destination_fields_are_ignored_when_not_editable() {
        let text = "[profile.p]\nuri_env = \"U_T26\"\n\
                    [[profile.p.dest]]\nname=\"primary\"\ndest=\"s3:b/p\"\n\
                    [[profile.p.dest]]\nname=\"offsite\"\ndest=\"local:/mnt/off\"\n";
        let doc = document(text);
        let profile = doc.config.profile("p").unwrap();
        let (editable, note) = destination_editability(Lang::En, profile);
        assert!(!editable, "다중 destination이 편집 가능으로 판정됐다");
        assert!(note.is_some(), "이유가 없다");

        let change = build_change(
            ChangeOp::Update,
            ProfileName::parse("p", Lang::En).unwrap(),
            doc.syntax,
            &doc.origins_of("p"),
            editable,
            None,
            &form(&[("dest", "local%3A%2Fevil"), ("s3_creds", "")]),
        )
        .unwrap();
        assert!(
            !change.set.contains_key("dest"),
            "무시되지 않았다: {:?}",
            change.set
        );
        assert!(!change.unset.contains("s3_creds"));
        assert!(change.is_noop());
    }

    /// endpoint 전용 프로파일에는 destination을 새로 붙일 수 있다(사본을 잃을 게 없다).
    #[test]
    fn endpoint_only_profile_can_gain_a_destination() {
        let doc = document("[profile.dr]\nuri = \"mongodb://dr:27117/app\"\n");
        let profile = doc.config.profile("dr").unwrap();
        assert!(profile.is_endpoint_only());
        let (editable, note) = destination_editability(Lang::En, profile);
        assert!(editable);
        assert!(note.is_none());
    }

    // -- 폼 파싱 ----------------------------------------------------------

    /// `+`와 `%XX`가 디코드되고, 같은 이름이 여러 번 오면 첫 값이 이긴다.
    #[test]
    fn form_body_decodes_and_takes_the_first_value() {
        let body = FormBody::parse("a=x%2By+z&a=second&flag=1&empty=&noeq");
        assert_eq!(body.get("a"), Some("x+y z"));
        assert_eq!(body.get("a").unwrap(), "x+y z", "뒤 값이 앞 값을 덮었다");
        assert!(body.flag("flag"));
        assert!(!body.flag("empty"));
        assert!(!body.flag("missing"));
        assert_eq!(body.get("noeq"), Some(""));
        assert_eq!(body.get("nope"), None);
    }

    /// 값은 앞뒤 공백을 자르지만 프로파일 이름은 자르지 않는다.
    #[test]
    fn values_are_trimmed_but_the_profile_name_is_not() {
        let body = FormBody::parse("recipient_file=+%2Fk.pub+&profile=+prod+");
        assert_eq!(body.get("recipient_file"), Some("/k.pub"));
        assert_eq!(body.get_raw(FIELD_PROFILE), Some(" prod "));
        assert!(
            ProfileName::parse(body.get_raw(FIELD_PROFILE).unwrap(), Lang::En).is_err(),
            "공백이 섞인 이름이 통과했다"
        );
    }

    /// 잘못 인코딩된 본문에도 패닉하지 않는다(브라우저가 아닌 무엇이든 보낼 수 있다).
    #[test]
    fn malformed_bodies_do_not_panic() {
        for body in [
            "",
            "&&&",
            "=value",
            "key%",
            "key%2",
            "key%ZZ=v",
            "a=%F0%9F", // 잘린 멀티바이트
            "\u{0000}=\u{0000}",
        ] {
            let parsed = FormBody::parse(body);
            let _ = parsed.get("key");
            let _ = parsed.flag("a");
        }
    }

    // -- 저장 이음매 ------------------------------------------------------

    /// 기본 저장 지점은 파일을 쓰지 않고, 그 사실을 값으로 알린다.
    #[test]
    fn default_store_does_not_persist() {
        let doc = document(V2_SAMPLE);
        let change = build_change(
            ChangeOp::Update,
            ProfileName::parse("prod", Lang::En).unwrap(),
            doc.syntax,
            &doc.origins_of("prod"),
            true,
            None,
            &form(&[("keep_days", "14")]),
        )
        .unwrap();
        let receipt = store()
            .apply(&change, test_receipt())
            .expect("검증 경로는 동작해야 함");
        assert_eq!(receipt, ChangeReceipt::NotPersisted);
        assert!(!receipt.persisted());
    }

    /// [`ConfigStore::apply`]에 넘길 감사 영수증을 만든다.
    ///
    /// [`AuditReceipt`]는 [`crate::web::audit::AuditLog::gate`]가 append에 성공했을 때만
    /// 만들어지고 그 모듈 밖에서는 생성할 방법이 없다 — 그게 이 시그니처의 요점이다
    /// ([`ConfigStore`] doc). 그래서 테스트도 우회하지 않고 실제로 게이트를 통과한다.
    fn test_receipt() -> AuditReceipt {
        let dir = tempfile::tempdir().expect("임시 디렉터리 생성 실패");
        let log = crate::web::audit::AuditLog::open(dir.path()).expect("감사 로그 열기 실패");
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("런타임 생성 실패")
            .block_on(log.gate("test", "config.save", "p", &[]))
            .expect("정상 경로에서 게이트는 통과해야 함")
    }

    /// 변경 요청이 저장 계층에 필요한 것을 전부 들고 있다(t28의 상속·형식 보존 근거).
    #[test]
    fn change_carries_everything_the_store_needs() {
        let doc = document(V2_SAMPLE);
        let change = build_change(
            ChangeOp::Update,
            ProfileName::parse("prod", Lang::En).unwrap(),
            doc.syntax,
            &doc.origins_of("prod"),
            true,
            None,
            &form(&[("keep_days", "14")]),
        )
        .unwrap();
        // 1) 원본 문법.
        assert_eq!(change.syntax, ConfigSyntax::V2);
        // 2) 프로파일 테이블에만 손대면 된다는 사실.
        assert_eq!(
            change.set_rows(),
            vec![("keep_days".to_string(), "14".to_string())]
        );
        assert!(change.unset_rows().is_empty());
        // 3) 편집 전 출처 스냅샷 — 상속 구조를 재구성할 수 있다.
        let origins: BTreeMap<&str, String> = change
            .origins
            .entries()
            .map(|(k, o)| (k, o.label()))
            .collect();
        assert_eq!(origins.get("uri_env").map(String::as_str), Some("direct"));
        assert_eq!(
            origins.get("dest").map(String::as_str),
            Some("extends: s3prod")
        );
        assert_eq!(origins.get("encrypt").map(String::as_str), Some("defaults"));
        // 4) 프로파일에 직접 적힌 키만 골라낼 수 있다.
        let direct = change.origins.direct_keys();
        assert!(direct.contains(&"uri_env"));
        assert!(direct.contains(&"keep_full"));
        assert!(!direct.contains(&"dest"), "상속 키가 직접으로 새어 나왔다");
    }

    // -- 라운드트립: 변경 모델이 저장을 구현할 만큼 충분한가 --------------

    /// **테스트 전용** — 변경을 raw TOML 표면에 적용해 텍스트로 되돌린다.
    ///
    /// 원자적 저장(temp+fsync+rename)·직전본 보존·doctor 검증은 건너뛰지만, **테이블을 고치는
    /// 부분은 저장이 실제로 쓰는 함수를 그대로 부른다**
    /// ([`crate::web::config_write::apply_change_to_table`]). t26 시점에는 그 구현이 없어서 이
    /// 함수가 자기 사본을 들고 있었는데, 사본과 실물이 갈라지면(예: v1 compact `dest` 압축
    /// 해제) 테스트는 통과하는데 저장은 실패한다 — 그 위험을 없애기 위해 사본을 걷어냈다.
    ///
    /// 이 함수가 증명하는 것은 [`ProfileChange`]가 **저장을 구현할 수 있을 만큼의 정보를 담고
    /// 있는지**다 — 문법(v1/v2) 보존과 상속 보존이 이 값만으로 가능한가.
    fn apply_change_to_text(text: &str, change: &ProfileChange) -> String {
        let mut root = Value::Table(toml::from_str(text).unwrap_or_default());
        crate::web::config_write::apply_change_to_table(root.as_table_mut().unwrap(), change)
            .unwrap_or_else(|e| panic!("변경 적용 실패: {e}"));
        toml::to_string(&root).unwrap()
    }

    /// **v2 생성 → 수정 → 삭제 라운드트립.**
    ///
    /// 각 단계 뒤 텍스트를 다시 읽어 (1) 문법이 v2로 남는지, (2) 손대지 않은 키의 출처가
    /// 그대로인지(= `[defaults]`·`[base.*]`가 살아 있는지)를 확인한다. 두 번째가 이
    /// 태스크의 핵심 성질이다 — 폼이 상속을 프로파일에 박아 넣지 않는다는 것.
    #[test]
    fn v2_create_update_delete_roundtrip_preserves_syntax_and_inheritance() {
        // 1) 생성.
        let doc = document(V2_SAMPLE);
        let create = build_change(
            ChangeOp::Create,
            ProfileName::parse("fresh", Lang::En).unwrap(),
            doc.syntax,
            &ProfileOrigins::default(),
            true,
            None,
            &form(&[
                ("uri_env", "FRESH_URI"),
                ("dest", "local%3A%2Fsrv%2Ffresh"),
                ("keep_full", "2"),
            ]),
        )
        .unwrap();
        let text = apply_change_to_text(V2_SAMPLE, &create);
        let doc = document(&text);
        assert_eq!(
            doc.syntax,
            ConfigSyntax::V2,
            "생성이 문법을 바꿨다:\n{text}"
        );
        let fresh = doc.config.profile("fresh").expect("생성된 프로파일이 없다");
        assert_eq!(fresh.source.uri_env.as_deref(), Some("FRESH_URI"));
        assert_eq!(fresh.destination.path.as_deref(), Some("/srv/fresh"));
        assert_eq!(fresh.retention.keep_full, Some(2));
        // 새 프로파일도 [defaults]를 상속받는다 — 폼이 그 값을 복제하지 않았기 때문이다.
        assert!(fresh.features.encryption.enabled, "defaults 상속이 끊겼다");
        assert_eq!(
            fresh.features.encryption.recipient_file.as_deref(),
            Some("/keys/age.pub")
        );
        assert_eq!(
            doc.origins_of("fresh").origin_of(&["encrypt"]),
            Origin::Defaults
        );
        // 기존 프로파일의 상속도 그대로다.
        assert_eq!(
            doc.origins_of("prod").origin_of(&["dest"]),
            Origin::Inherited {
                from: "s3prod".to_string()
            }
        );

        // 2) 수정 — 상속 필드는 비워 두고 직접 적힌 키 하나만 고친다.
        let current = profile_view_for_test(&doc, "fresh");
        let update = build_change(
            ChangeOp::Update,
            ProfileName::parse("fresh", Lang::En).unwrap(),
            doc.syntax,
            &doc.origins_of("fresh"),
            current.destination_editable,
            Some(&current),
            &form(&[
                ("keep_full", "5"),
                ("encrypt", ""),          // 상속 유지
                ("recipient_file", ""),   // 상속 유지
                ("compress_level", ""),   // 상속 유지
                ("uri_env", "FRESH_URI"), // 그대로 재제출
            ]),
        )
        .unwrap();
        assert_eq!(
            update.set_rows(),
            vec![("keep_full".to_string(), "5".to_string())],
            "고친 한 줄만 나가야 한다"
        );
        let text = apply_change_to_text(&text, &update);
        let doc = document(&text);
        assert_eq!(doc.syntax, ConfigSyntax::V2);
        let fresh = doc.config.profile("fresh").unwrap();
        assert_eq!(fresh.retention.keep_full, Some(5));
        assert!(
            fresh.features.encryption.enabled,
            "수정이 defaults 상속을 끊었다:\n{text}"
        );
        assert_eq!(
            doc.origins_of("fresh").origin_of(&["encrypt"]),
            Origin::Defaults,
            "encrypt가 프로파일에 박혔다:\n{text}"
        );
        assert!(
            text.contains("[defaults]") && text.contains("[base.s3prod]"),
            "공용 섹션이 사라졌다:\n{text}"
        );

        // 3) 삭제.
        let delete = ProfileChange {
            op: ChangeOp::Delete,
            name: ProfileName::parse("fresh", Lang::En).unwrap(),
            syntax: doc.syntax,
            set: BTreeMap::new(),
            unset: BTreeSet::new(),
            broke_inheritance: Vec::new(),
            origins: doc.origins_of("fresh"),
        };
        let text = apply_change_to_text(&text, &delete);
        let doc = document(&text);
        assert_eq!(doc.syntax, ConfigSyntax::V2);
        assert!(
            doc.config.profile("fresh").is_err(),
            "삭제되지 않았다:\n{text}"
        );
        assert!(
            doc.config.profile("prod").is_ok(),
            "남의 프로파일이 사라졌다"
        );
        assert_eq!(
            doc.origins_of("prod").origin_of(&["encrypt"]),
            Origin::Defaults,
            "삭제가 상속 구조를 건드렸다:\n{text}"
        );
    }

    /// **v1 수정 라운드트립 — 파일이 v1로 남는다.**
    ///
    /// 폼이 v2로 직렬화하면 사용자는 값 하나만 고쳤는데 파일 전체 문법이 바뀐다. 변경
    /// 요청이 [`ProfileChange::syntax`]를 들고 있으므로 저장이 그것을 보고 v1 트리에
    /// 쓸 수 있다 — 그 성질을 여기서 확인한다.
    #[test]
    fn v1_update_roundtrip_stays_v1() {
        let doc = document(V1_SAMPLE);
        let current = profile_view_for_test(&doc, "prod");
        let change = build_change(
            ChangeOp::Update,
            ProfileName::parse("prod", Lang::En).unwrap(),
            doc.syntax,
            &doc.origins_of("prod"),
            current.destination_editable,
            Some(&current),
            &form(&[("keep_days", "21"), ("compress_level", "6")]),
        )
        .unwrap();
        assert_eq!(change.syntax, ConfigSyntax::V1);

        let text = apply_change_to_text(V1_SAMPLE, &change);
        let doc = document(&text);
        assert_eq!(
            doc.syntax,
            ConfigSyntax::V1,
            "v1이 v2로 다시 써졌다:\n{text}"
        );
        assert!(
            text.contains("[profiles.prod")
                && !text.contains("[profile.prod")
                && !text.contains("[defaults]"),
            "v1 중첩 문법이 유지되지 않았다:\n{text}"
        );
        let prod = doc.config.profile("prod").unwrap();
        assert_eq!(prod.retention.keep_days, Some(21));
        assert_eq!(prod.features.compression.level, 6);
        // 손대지 않은 값은 그대로다.
        assert_eq!(prod.source.uri_env.as_deref(), Some("MONGO_URI_T26"));
        assert_eq!(prod.retention.keep_full, Some(3));
        assert_eq!(
            prod.destination
                .s3
                .as_ref()
                .unwrap()
                .credentials_env
                .as_deref(),
            Some("S3_CREDS_T26")
        );
    }

    /// 화면 모델이 그린 폼을 브라우저가 그대로 다시 제출한 본문을 만든다.
    ///
    /// 미리 채운 값은 그 값, 상속·기본값 칸은 빈 값 — 실제 브라우저가 보내는 모양이다.
    fn resubmit_body(view: &ProfileView) -> String {
        view.fields
            .iter()
            .map(|field| {
                format!(
                    "{}={}",
                    field.key,
                    field.prefill().unwrap_or("").replace('/', "%2F")
                )
            })
            .collect::<Vec<_>>()
            .join("&")
    }

    /// **v1 compact `dest` 변경이 이제 저장되고, 왕복이 정확히 일치한다.**
    ///
    /// t27은 이 변경을 422로 거부했다(조용히 버리지 않기 위해). 이 테스트는 두 가지를 함께
    /// 고정한다:
    ///
    /// 1. compact 한 줄이 v1 nested 경로에 정확히 쓰인다(그리고 옛 스킴의 위치 키는 사라진다).
    /// 2. **저장 결과를 다시 읽어 그린 폼의 재제출이 무변경이다.** 이것이 진짜 왕복 증명이다 —
    ///    쓰는 문자열과 읽는 문자열이 한 글자라도 다르면, 화면을 열 때마다 destination이 바뀐
    ///    것처럼 보이고 아무것도 고치지 않은 제출이 파일을 계속 흔든다.
    #[test]
    fn v1_dest_change_saves_and_the_roundtrip_is_exact() {
        let doc = document(V1_SAMPLE);
        let current = profile_view_for_test(&doc, "prod");
        assert!(
            current.destination_editable,
            "단일 destination은 편집 가능해야 한다"
        );
        assert_eq!(
            current.field("dest").unwrap().effective.as_deref(),
            Some("s3:db-backups/mongo/prod"),
            "읽기가 compact로 접히지 않았다"
        );

        let change = build_change(
            ChangeOp::Update,
            ProfileName::parse("prod", Lang::En).unwrap(),
            doc.syntax,
            &doc.origins_of("prod"),
            current.destination_editable,
            Some(&current),
            &form(&[("dest", "local%3A%2Fsrv%2Fnew")]),
        )
        .expect("v1 dest 변경은 검증을 통과해야 함");
        assert_eq!(
            change.set.get("dest"),
            Some(&Value::String("local:/srv/new".to_string()))
        );

        let text = apply_change_to_text(V1_SAMPLE, &change);
        let saved = document(&text);
        assert_eq!(
            saved.syntax,
            ConfigSyntax::V1,
            "v1이 v2로 새어 나갔다:\n{text}"
        );
        let prod = saved.config.profile("prod").unwrap();
        assert_eq!(prod.destination.r#type.as_deref(), Some("local"));
        assert_eq!(prod.destination.path.as_deref(), Some("/srv/new"));
        assert!(
            prod.destination
                .s3
                .as_ref()
                .is_none_or(|s3| s3.bucket.is_none() && s3.prefix.is_none()),
            "옛 s3 위치가 남았다:\n{text}"
        );
        // 지우라고 하지 않은 부속 키는 그대로다.
        assert_eq!(
            prod.destination
                .s3
                .as_ref()
                .and_then(|s3| s3.credentials_env.as_deref()),
            Some("S3_CREDS_T26")
        );

        // (2) 왕복 — 저장된 파일을 다시 읽어 그린 폼의 재제출이 무변경이다.
        let after = profile_view_for_test(&saved, "prod");
        assert_eq!(
            after.field("dest").unwrap().effective.as_deref(),
            Some("local:/srv/new"),
            "쓴 값과 읽는 값이 다르다"
        );
        assert_eq!(
            saved.origins_of("prod").origin_of(&["dest"]),
            Origin::Direct
        );
        let again = build_change(
            ChangeOp::Update,
            ProfileName::parse("prod", Lang::En).unwrap(),
            saved.syntax,
            &saved.origins_of("prod"),
            after.destination_editable,
            Some(&after),
            &FormBody::parse(&resubmit_body(&after)),
        )
        .unwrap();
        assert!(
            again.is_noop(),
            "저장 직후의 재제출이 또 변경으로 세어졌다: set={:?} unset={:?}",
            again.set_rows(),
            again.unset_rows()
        );
    }

    /// v1에서 `dest` 칸을 비우면 destination이 **전부** 사라지고(4경로), 로더는 그 프로파일을
    /// endpoint 전용으로 읽는다.
    #[test]
    fn clearing_dest_in_v1_removes_the_whole_destination() {
        let doc = document(V1_SAMPLE);
        let current = profile_view_for_test(&doc, "prod");
        let change = build_change(
            ChangeOp::Update,
            ProfileName::parse("prod", Lang::En).unwrap(),
            doc.syntax,
            &doc.origins_of("prod"),
            true,
            Some(&current),
            &form(&[("dest", "")]),
        )
        .unwrap();
        assert!(change.unset.contains("dest"), "{:?}", change.unset);

        let text = apply_change_to_text(V1_SAMPLE, &change);
        let saved = document(&text);
        let prod = saved.config.profile("prod").unwrap();
        assert!(
            prod.is_endpoint_only(),
            "destination을 지웠는데 백업 잡으로 남았다:\n{text}"
        );
        assert_eq!(
            saved.origins_of("prod").origin_of(&["dest"]),
            Origin::Builtin,
            "지운 키의 출처가 남았다:\n{text}"
        );
    }

    /// 폼에 있는 모든 필드가 v1에 쓸 자리를 갖는다.
    ///
    /// 하나라도 빠지면 그 필드를 v1 파일에서 고칠 때 저장이 "v1 문법에서 쓸 자리가 없습니다"로
    /// 끊긴다 — 즉 v2에서는 되는데 v1에서는 안 되는 필드가 생긴다.
    #[test]
    fn every_form_field_has_a_v1_home() {
        let flat_keys: BTreeSet<&str> = V1_PATHS.iter().map(|(_, flat)| *flat).collect();
        for spec in FIELDS {
            assert!(
                flat_keys.contains(spec.key),
                "'{}'에 대응하는 v1 nested 경로가 V1_PATHS에 없다",
                spec.key
            );
        }
    }

    /// 테스트용 화면 모델(env는 전부 미설정으로 본다).
    fn profile_view_for_test(doc: &ConfigDocument, name: &str) -> ProfileView {
        build_profile_view(
            Lang::En,
            name,
            doc.config.profile(name).unwrap(),
            &doc.origins_of(name),
            &no_env,
        )
    }

    // -- 라우팅·인증 ------------------------------------------------------

    /// 인증 없이는 401, 세션 쿠키가 있으면 200.
    ///
    /// 실제 배선(`src/web/server.rs`의 `app_routes`)은 리더가 한다. 이 테스트는 **이 화면이
    /// 그 관문 뒤에서 동작하는지**를 같은 방식(`route_layer`)으로 조립해 확인한다 — 배선을
    /// 기다리지 않고 성질을 고정하기 위함이다.
    #[tokio::test]
    async fn screen_is_closed_without_a_session_cookie() {
        use axum::body::Body;
        use axum::http::{header, Request};
        use axum::routing::{get, post};
        use axum::{middleware, Router};
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        const TOKEN: &str = "t26-test-token";
        let dir = std::env::temp_dir().join(format!("x-backup-t26-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("config.toml");
        std::fs::write(&config_path, V2_SAMPLE).unwrap();

        let ctx = Arc::new(ServeConfig {
            config_path: Some(config_path),
            ..ServeConfig::for_test()
        });
        // 세션은 발급한 인스턴스에만 있으므로 미들웨어와 쿠키가 같은 상태를 봐야 한다
        // (`AuthState::for_test_with_session` doc).
        let (auth, session) = crate::web::auth::AuthState::for_test_with_session(TOKEN);

        let app = Router::new()
            .route(CONFIG_PATH, get(list))
            .route(NEW_PATH, get(new_form))
            .route(EDIT_ROUTE, get(edit_form))
            .route(DELETE_CONFIRM_ROUTE, get(delete_form))
            .route(SAVE_PATH, post(save))
            .route(DELETE_PATH, post(delete_submit))
            .route_layer(middleware::from_fn_with_state(
                auth,
                crate::web::auth::require_auth,
            ))
            .with_state(ctx);

        let call = |uri: &str, cookie: Option<&str>| {
            let mut builder = Request::builder().uri(uri);
            if let Some(session_id) = cookie {
                builder = builder.header(
                    header::COOKIE,
                    format!("{}={session_id}", crate::web::auth::SESSION_COOKIE_NAME),
                );
            }
            let request = builder.body(Body::empty()).unwrap();
            let app = app.clone();
            async move {
                let response = app.oneshot(request).await.expect("라우터 호출 실패");
                let status = response.status();
                let bytes = response.into_body().collect().await.unwrap().to_bytes();
                (status, String::from_utf8_lossy(&bytes).into_owned())
            }
        };

        for uri in [
            CONFIG_PATH,
            NEW_PATH,
            &edit_href("prod"),
            &delete_href("prod"),
        ] {
            let (status, body) = call(uri, None).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}는 닫혀 있어야 함");
            assert!(
                !body.contains("MONGO_URI_T26"),
                "401 응답에 config 내용이 새어 나왔다: {body}"
            );

            let (status, body) = call(uri, Some(session.as_str())).await;
            assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        }

        // 목록 화면에 프로파일이 실제로 그려진다.
        let (_, body) = call(CONFIG_PATH, Some(session.as_str())).await;
        assert!(body.contains("prod"), "프로파일이 렌더되지 않았다");
        assert!(body.contains("MONGO_URI_T26"), "env 이름이 없다");
        assert!(body.contains(r#"data-syntax="v2""#), "문법 배지가 없다");
    }

    /// 없는 프로파일은 404, 규칙을 벗어난 이름은 400.
    #[tokio::test]
    async fn unknown_and_unsafe_names_get_distinct_statuses() {
        let dir = std::env::temp_dir().join(format!("x-backup-t26-st-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("config.toml");
        std::fs::write(&config_path, V2_SAMPLE).unwrap();
        let ctx = Arc::new(ServeConfig {
            config_path: Some(config_path.clone()),
            ..ServeConfig::for_test()
        });

        let missing = edit_form(State(ctx.clone()), Path("nope".to_string())).await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);

        let unsafe_name = edit_form(State(ctx.clone()), Path("../etc".to_string())).await;
        assert_eq!(unsafe_name.status(), StatusCode::BAD_REQUEST);

        // 삭제 확인 없이 제출하면 아무것도 지우지 않고 400.
        let no_confirm = delete_submit(
            State(ctx.clone()),
            same_origin(),
            "profile=prod".to_string(),
        )
        .await;
        assert_eq!(no_confirm.status(), StatusCode::BAD_REQUEST);

        // 확인이 맞으면 삭제 경로를 탄다. 이 테스트는 실제 `x-backup` 바이너리 없이 도는
        // 라이브러리 단위 테스트라 `current_exe()`(t27의 저장 전 doctor 검증이 자기 자신을
        // 다시 실행하는 기준 — `config_write` 모듈 헤더 참조)가 테스트 하니스 자신을
        // 가리킨다. doctor를 doctor로서 띄울 수 없으므로 저장은 fail-closed로
        // 막힌다(500) — "prod를 잘못 지웠다"가 아니라 "검증할 수 없어 막았다"는 뜻이고,
        // 그 상태에서도 파일이 조금도 바뀌지 않는다는 것이 이 테스트가 확인하는 성질이다.
        // 실제 바이너리로 검증되는 exit 0/3/4 각 경로는 `config_write::tests::real_doctor_*`가
        // 맡는다.
        let before = std::fs::read(&config_path).unwrap();
        let confirmed = delete_submit(
            State(ctx),
            same_origin(),
            "profile=prod&confirm=prod".to_string(),
        )
        .await;
        assert_eq!(confirmed.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            std::fs::read(&config_path).unwrap(),
            before,
            "검증이 막혔는데 파일이 바뀌었다"
        );
    }

    /// config가 연결되지 않은 서버에서도 화면이 깨지지 않고 이유를 설명한다.
    #[tokio::test]
    async fn missing_config_renders_an_explanation() {
        let ctx = Arc::new(ServeConfig::for_test());
        let response = list(State(ctx)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(
            body.contains("without a config file"),
            "미연결 설명이 없다: {body}"
        );
    }

    // -- 감사 기록 (C1) ---------------------------------------------------

    /// 감사 로그 파일의 줄들을 JSON으로 읽는다.
    fn audit_lines(ctx: &ServeConfig) -> Vec<serde_json::Value> {
        let text = std::fs::read_to_string(ctx.audit.path()).expect("감사 로그를 읽을 수 없다");
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("감사 로그 줄이 JSON이 아니다"))
            .collect()
    }

    /// 콘솔 자기 화면에서 온 것으로 보이는 헤더(출처 검사 통과용).
    ///
    /// `Host`와 `Origin`의 authority가 같아야 통과한다 — 브라우저가 폼을 제출할 때의 모양이다
    /// (`crate::web::auth::verify_same_origin` doc 참조).
    fn same_origin() -> axum::http::HeaderMap {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::HOST,
            "console.example:8787".parse().unwrap(),
        );
        headers.insert(
            axum::http::header::ORIGIN,
            "https://console.example:8787".parse().unwrap(),
        );
        headers
    }

    /// **같은 호스트의 다른 포트**에서 온 것으로 보이는 헤더 — M9이 지적한 그 경로다.
    fn other_port_origin() -> axum::http::HeaderMap {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::HOST,
            "console.example:8787".parse().unwrap(),
        );
        headers.insert(
            axum::http::header::ORIGIN,
            "https://console.example:3000".parse().unwrap(),
        );
        headers
    }

    /// config 경로가 연결된 테스트 컨텍스트를 만든다(v2 표본).
    fn wired_ctx(tag: &str) -> (Arc<ServeConfig>, std::path::PathBuf) {
        wired_ctx_with(tag, V2_SAMPLE)
    }

    /// 주어진 config 텍스트로 컨텍스트를 만든다.
    fn wired_ctx_with(tag: &str, text: &str) -> (Arc<ServeConfig>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "x-backup-audit-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("config.toml");
        std::fs::write(&config_path, text).unwrap();
        let ctx = Arc::new(ServeConfig {
            config_path: Some(config_path.clone()),
            ..ServeConfig::for_test()
        });
        (ctx, config_path)
    }

    /// 응답 본문을 문자열로.
    async fn body_text(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("본문을 읽을 수 없다");
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// 미리보기 화면에서 확인 폼의 지문을 뽑는다.
    ///
    /// 지문을 테스트가 직접 계산하지 않는 것이 요점이다 — **화면이 실제로 그 값을 실어야**
    /// 전환을 실행할 수 있다는 성질까지 함께 확인한다.
    fn digest_from_preview(body: &str) -> String {
        let needle = format!(r#"name="{FIELD_FROM_DIGEST}" value=""#);
        let start = body
            .find(&needle)
            .unwrap_or_else(|| panic!("미리보기에 지문 필드가 없다:\n{body}"))
            + needle.len();
        let rest = &body[start..];
        rest[..rest.find('"').expect("지문 값이 닫히지 않았다")].to_string()
    }

    /// **config 쓰기는 감사 로그에 시작과 끝을 남긴다** — C1의 회귀 테스트.
    ///
    /// 이 파일에는 감사 호출이 한 줄도 없었다(`grep -c audit` → 0). 그런데 이 경로는
    /// 디스크의 `config.toml`을 실제로 덮어쓴다 — destination을 남의 S3로 바꾸거나 암호화를
    /// 끄는 변경이 흔적 없이 지나갈 수 있었다.
    ///
    /// 삭제 경로로 확인한다(가장 되돌릴 수 없는 작업). 이 테스트는 실제 `x-backup` 바이너리
    /// 없이 도는 단위 테스트라 저장 전 doctor 검증이 실패하므로 결과는 `failure`인데, **그
    /// 사실 자체가 요점이다**: 실행이 막혀도 "요청됐다"는 기록은 남고, 결과도 남는다.
    #[tokio::test]
    async fn config_delete_records_request_and_outcome() {
        let (ctx, config_path) = wired_ctx("delete");
        let before = std::fs::read(&config_path).unwrap();

        let response = delete_submit(
            State(Arc::clone(&ctx)),
            same_origin(),
            "profile=prod&confirm=prod".to_string(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            std::fs::read(&config_path).unwrap(),
            before,
            "검증이 막혔는데 파일이 바뀌었다"
        );

        let lines = audit_lines(&ctx);
        assert_eq!(lines.len(), 2, "시작·끝 두 줄이어야 한다: {lines:?}");
        for line in &lines {
            assert_eq!(line["actor"], AUDIT_ACTOR);
            assert_eq!(line["action"], "config.delete");
            assert_eq!(line["target"], "prod");
        }
        assert_eq!(lines[0]["outcome"], "requested");
        assert!(lines[0]["exit_code"].is_null(), "게이트는 결과를 모른다");
        assert_eq!(lines[1]["outcome"], "failure");
        assert_eq!(
            lines[1]["args_masked"], lines[0]["args_masked"],
            "두 줄이 같은 작업을 가리켜야 한다"
        );
    }

    /// **감사 로그에 쓸 수 없으면 config를 쓰지 않는다** — t8의 불변식이 이 경로에도 적용됨.
    ///
    /// 감사 로그 파일을 읽기 전용으로 만들어 append를 실패시킨다(`audit.rs`의
    /// `record_fails_when_log_file_becomes_read_only`와 같은 방법). 그 상태에서 저장 요청은
    /// 503으로 거부되고 config 파일은 한 바이트도 바뀌지 않아야 한다.
    #[cfg(unix)]
    #[tokio::test]
    async fn audit_gate_failure_refuses_the_config_write() {
        use std::os::unix::fs::PermissionsExt;

        let (ctx, config_path) = wired_ctx("gate");
        let before = std::fs::read(&config_path).unwrap();
        std::fs::set_permissions(ctx.audit.path(), std::fs::Permissions::from_mode(0o400)).unwrap();

        let response = save(
            State(Arc::clone(&ctx)),
            same_origin(),
            "op=update&profile=prod&keep_days=21".to_string(),
        )
        .await;

        // 원상복구를 먼저 한다(단정이 실패해도 임시 파일이 남지 않게).
        std::fs::set_permissions(ctx.audit.path(), std::fs::Permissions::from_mode(0o600)).unwrap();

        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "감사 기록 실패가 저장을 막지 않았다"
        );
        assert_eq!(
            std::fs::read(&config_path).unwrap(),
            before,
            "감사 기록 없이 config가 바뀌었다 — 이 경로의 존재 이유가 무너진다"
        );
        assert!(
            audit_lines(&ctx).is_empty(),
            "쓸 수 없다고 판정한 로그에 줄이 들어갔다"
        );
    }

    /// 감사 인자에 **평문 URI의 자격증명이 들어가지 않는다** — 감사 로그는 append-only라
    /// 한 번 새면 지울 수 없다.
    #[test]
    fn audit_args_redact_plaintext_uri_credentials_but_keep_env_names() {
        const PASSWORD: &str = "NOT-A-REAL-SECRET-hunter2hunter2";
        let doc = document(V2_SAMPLE);
        let mut change = ProfileChange {
            op: ChangeOp::Update,
            name: ProfileName::parse("prod", Lang::En).unwrap(),
            syntax: doc.syntax,
            set: BTreeMap::new(),
            unset: BTreeSet::new(),
            broke_inheritance: vec!["compress".to_string()],
            origins: doc.origins_of("prod"),
        };
        change.set.insert(
            KEY_URI.to_string(),
            Value::String(format!(
                "mongodb://backupuser:{PASSWORD}@db1.internal:27017/prod"
            )),
        );
        change
            .set
            .insert("uri_env".to_string(), Value::String("PROD_URI".to_string()));
        change.unset.insert("keep_days".to_string());

        let args = audit_args(&change, &crate::web::mask::SecretRegistry::new());
        let joined = args.join(" | ");
        assert!(
            !joined.contains(PASSWORD),
            "감사 로그에 평문 자격증명이 들어갔다: {joined}"
        );
        // 호스트는 남는다 — "어디로 바뀌었나"가 감사의 핵심 정보다.
        assert!(joined.contains("db1.internal"), "{joined}");
        // env 이름은 시크릿이 아니라 식별자다 — 가리면 감사가 쓸모없어진다.
        assert!(joined.contains("set:uri_env=PROD_URI"), "{joined}");
        assert!(joined.contains("unset:keep_days"), "{joined}");
        assert!(joined.contains("op=update"), "{joined}");
        assert!(joined.contains("syntax=v2"), "{joined}");
        assert!(joined.contains("broke-inheritance=compress"), "{joined}");
    }

    /// 이미 알려진 시크릿(잡 러너 레지스트리)이 어느 필드에 붙여넣어졌더라도 지워진다(2차 방어).
    #[test]
    fn audit_args_run_through_the_secret_registry() {
        const FAKE: &str = "NOT-A-REAL-SECRET-registry-9f2b7c41";
        let doc = document(V2_SAMPLE);
        let mut change = ProfileChange {
            op: ChangeOp::Create,
            name: ProfileName::parse("fresh", Lang::En).unwrap(),
            syntax: doc.syntax,
            set: BTreeMap::new(),
            unset: BTreeSet::new(),
            broke_inheritance: Vec::new(),
            origins: ProfileOrigins::default(),
        };
        // URI 필드가 아닌 곳(destination 경로)에 시크릿이 섞인 경우 — RedactedUri로는 안 잡힌다.
        change.set.insert(
            "dest".to_string(),
            Value::String(format!("local:/srv/{FAKE}")),
        );

        let mut registry = crate::web::mask::SecretRegistry::new();
        assert!(registry.register(FAKE));
        let args = audit_args(&change, &registry);
        let joined = args.join(" | ");
        assert!(
            !joined.contains(FAKE),
            "레지스트리 마스킹이 걸리지 않았다: {joined}"
        );
        assert!(joined.contains(crate::web::mask::REDACTED_PLACEHOLDER));
    }

    // -- 출처 검사 (M9) ---------------------------------------------------

    /// **같은 호스트의 다른 포트에서 온 변경 요청은 거부된다** — M9의 회귀 테스트.
    ///
    /// 브라우저 쿠키는 포트로 격리되지 않으므로 `:3000`의 서비스가 이 콘솔로 보내는 POST에는
    /// 세션 쿠키가 실려 온다. `SameSite=Strict`는 그것을 same-site로 보아 막지 못한다 —
    /// 그래서 출처를 포트까지 비교한다(`auth::verify_same_origin`).
    ///
    /// 함께 확인하는 것: 거부가 **감사 게이트보다 먼저**라 위조된 요청이 append-only 로그에
    /// 줄을 남기지 못한다(로그를 부풀리는 것 자체가 공격이 되지 않게).
    #[tokio::test]
    async fn state_changing_request_from_another_port_is_refused() {
        let (ctx, config_path) = wired_ctx("origin");
        let before = std::fs::read(&config_path).unwrap();

        let response = save(
            State(Arc::clone(&ctx)),
            other_port_origin(),
            "op=update&profile=prod&keep_days=21".to_string(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        let deleted = delete_submit(
            State(Arc::clone(&ctx)),
            other_port_origin(),
            "profile=prod&confirm=prod".to_string(),
        )
        .await;
        assert_eq!(deleted.status(), StatusCode::FORBIDDEN);

        assert_eq!(
            std::fs::read(&config_path).unwrap(),
            before,
            "거부된 요청이 config를 바꿨다"
        );
        assert!(
            audit_lines(&ctx).is_empty(),
            "위조 요청이 감사 로그에 줄을 남겼다 — append-only 로그를 부풀리는 경로가 된다"
        );
    }

    /// 출처 헤더가 아예 없는 변경 요청도 거부된다(fail-closed).
    #[tokio::test]
    async fn state_changing_request_without_origin_is_refused() {
        let (ctx, config_path) = wired_ctx("no-origin");
        let before = std::fs::read(&config_path).unwrap();
        let response = save(
            State(Arc::clone(&ctx)),
            axum::http::HeaderMap::new(),
            "op=update&profile=prod&keep_days=21".to_string(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(std::fs::read(&config_path).unwrap(), before);
    }

    /// 잘못된 `op` 값은 400으로 거부된다.
    #[tokio::test]
    async fn unknown_operation_is_rejected() {
        let dir = std::env::temp_dir().join(format!("x-backup-t26-op-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("config.toml");
        std::fs::write(&config_path, V2_SAMPLE).unwrap();
        let ctx = Arc::new(ServeConfig {
            config_path: Some(config_path),
            ..ServeConfig::for_test()
        });
        let response = save(
            State(ctx),
            same_origin(),
            "op=drop&profile=prod".to_string(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    // -- v1 → v2 전환 -----------------------------------------------------

    /// 미리보기 화면이 **무엇이 어떻게 바뀌는지** 말하고, 확인에 필요한 지문을 싣는다.
    /// 그리고 "전환은 정책 재구성이 아니다"를 화면이 먼저 말한다.
    #[tokio::test]
    async fn convert_preview_shows_the_diff_and_its_limits() {
        let (ctx, config_path) = wired_ctx_with("convert-preview", V1_SAMPLE);
        let before = std::fs::read(&config_path).unwrap();

        let response = convert_form(State(Arc::clone(&ctx))).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_text(response).await;

        // 어디서 어디로 옮겨지는지가 표에 있다.
        assert!(
            body.contains("destination.s3.bucket"),
            "v1 경로가 보이지 않는다: {body}"
        );
        assert!(body.contains("s3:db-backups/mongo/prod"), "{body}");
        assert!(body.contains("[profile.prod]"), "결과 텍스트가 없다");
        // 상속이 생기지 않는다는 사실을 말한다.
        assert!(
            body.contains("No [defaults], [base.&lt;name&gt;] or extends is created"),
            "정책 재구성이 아니라는 설명이 없다: {body}"
        );
        // 역방향이 없는 이유도 적혀 있다.
        assert!(body.contains("No conversion back") || body.contains("no conversion back"));
        // 확인 폼에 지문이 실린다.
        assert!(!digest_from_preview(&body).is_empty());

        // 미리보기는 아무것도 바꾸지 않는다.
        assert_eq!(std::fs::read(&config_path).unwrap(), before);
        assert!(
            audit_lines(&ctx).is_empty(),
            "읽기 전용 화면이 감사 로그에 줄을 남겼다"
        );
    }

    /// 미리보기 텍스트에 시크릿이 실리지 않는다 — 이 화면은 config 파일 본문을 그대로
    /// 보여주는 유일한 자리라, 평문 `uri`의 자격증명을 가리지 않으면 그대로 새어 나간다.
    #[tokio::test]
    async fn convert_preview_redacts_credentials_in_the_file_body() {
        const FAKE: &str = "NOT-A-REAL-SECRET-t28-preview-9c41";
        let text = format!(
            "[profiles.leaky.source]\n\
             uri = \"mongodb://admin:{FAKE}@db.internal:27017/app\"\n\
             [profiles.leaky.destination]\ntype = \"local\"\npath = \"/srv/b\"\n"
        );
        let (ctx, _) = wired_ctx_with("convert-redact", &text);
        let body = body_text(convert_form(State(ctx)).await).await;
        assert!(
            !body.contains(FAKE),
            "미리보기에 평문 자격증명이 실렸다: {body}"
        );
        assert!(!body.contains("admin"), "사용자명이 남았다: {body}");
        // 호스트는 남아야 운영자가 무엇을 보고 있는지 안다.
        assert!(body.contains("db.internal"), "{body}");
    }

    /// **미리보기의 지문 없이는 전환이 실행되지 않는다.**
    ///
    /// 이것이 "diff를 보여준 뒤에만 실행한다"의 회귀 테스트다. 지문은 미리보기 화면만 알 수
    /// 있으므로, 그 값이 없는 제출은 미리보기를 열지 않은 요청이다.
    #[tokio::test]
    async fn convert_without_the_preview_digest_changes_nothing() {
        let (ctx, config_path) = wired_ctx_with("convert-nodigest", V1_SAMPLE);
        let before = std::fs::read(&config_path).unwrap();

        for body in ["", "from_digest=", "from_digest=deadbeef"] {
            let response =
                convert_submit(State(Arc::clone(&ctx)), same_origin(), body.to_string()).await;
            assert_eq!(
                response.status(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "본문 '{body}'가 통과했다"
            );
            assert_eq!(
                std::fs::read(&config_path).unwrap(),
                before,
                "거부된 요청이 파일을 바꿨다"
            );
        }
        assert!(
            audit_lines(&ctx).is_empty(),
            "실행되지 않은 요청이 append-only 로그를 부풀렸다"
        );
    }

    /// 다른 포트에서 온 전환 요청은 **감사 게이트보다 먼저** 거부된다([`save`]와 같은 순서).
    #[tokio::test]
    async fn convert_from_another_port_is_refused() {
        let (ctx, config_path) = wired_ctx_with("convert-origin", V1_SAMPLE);
        let before = std::fs::read(&config_path).unwrap();
        let response = convert_submit(
            State(Arc::clone(&ctx)),
            other_port_origin(),
            "from_digest=whatever".to_string(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(std::fs::read(&config_path).unwrap(), before);
        assert!(audit_lines(&ctx).is_empty());
    }

    /// **전환은 감사 게이트를 지난다** — 시작과 끝이 로그에 남는다.
    ///
    /// 지문은 실제 미리보기 화면에서 뽑는다. 이 단위 테스트에는 실제 `x-backup` 바이너리가
    /// 없어 저장 전 doctor 검증이 fail-closed로 막히므로(t27의 같은 이유 — `current_exe()`가
    /// 테스트 하니스다) 결과는 `failure`인데, **그 사실 자체가 요점이다**: 실행이 막혀도
    /// "요청됐다"는 기록이 남고, 파일은 한 바이트도 바뀌지 않는다.
    #[tokio::test]
    async fn convert_passes_the_audit_gate_and_leaves_the_file_alone_when_blocked() {
        let (ctx, config_path) = wired_ctx_with("convert-gate", V1_SAMPLE);
        let preview = body_text(convert_form(State(Arc::clone(&ctx))).await).await;
        let digest = digest_from_preview(&preview);
        let before = std::fs::read(&config_path).unwrap();

        let response = convert_submit(
            State(Arc::clone(&ctx)),
            same_origin(),
            format!("{FIELD_FROM_DIGEST}={digest}"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            std::fs::read(&config_path).unwrap(),
            before,
            "검증이 막혔는데 파일이 바뀌었다"
        );

        let lines = audit_lines(&ctx);
        assert_eq!(lines.len(), 2, "시작·끝 두 줄이어야 한다: {lines:?}");
        for line in &lines {
            assert_eq!(line["actor"], AUDIT_ACTOR);
            assert_eq!(line["action"], AUDIT_ACTION_CONVERT);
            assert_eq!(
                line["target"], AUDIT_TARGET_WHOLE_FILE,
                "전환의 대상은 프로파일 하나가 아니라 파일 전체다"
            );
        }
        assert_eq!(lines[0]["outcome"], "requested");
        assert_eq!(lines[1]["outcome"], "failure");
        let args = lines[0]["args_masked"].to_string();
        assert!(args.contains("op=convert"), "{args}");
        assert!(args.contains("syntax=v1->v2"), "{args}");
    }

    /// v2 파일에서는 전환 화면이 열리지 않고, **왜 역방향이 없는지**를 말한다.
    #[tokio::test]
    async fn convert_is_refused_for_a_v2_file_with_the_reason() {
        let (ctx, _) = wired_ctx("convert-v2");
        let response = convert_form(State(Arc::clone(&ctx))).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_text(response).await;
        assert!(body.contains("not written in the v1"), "{body}");
        assert!(
            body.contains("no conversion from v2 back to v1"),
            "역방향이 없는 이유가 없다: {body}"
        );

        let submitted =
            convert_submit(State(ctx), same_origin(), "from_digest=x".to_string()).await;
        assert_eq!(submitted.status(), StatusCode::BAD_REQUEST);
    }

    /// 전환할 수 없는 config는 **이유를 말하고 422로 멈춘다**(조용히 일부만 옮기지 않는다).
    #[tokio::test]
    async fn unconvertible_config_explains_itself() {
        // v2 compact 표기로 접히지 않는 destination.
        let (ctx, config_path) = wired_ctx_with(
            "convert-bad",
            "[profiles.p.source]\nuri_env = \"U\"\n\
             [profiles.p.destination]\ntype = \"gcs\"\npath = \"/x\"\n",
        );
        let before = std::fs::read(&config_path).unwrap();
        let response = convert_form(State(Arc::clone(&ctx))).await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = body_text(response).await;
        assert!(
            body.contains("gcs"),
            "무엇이 문제인지 말하지 않았다: {body}"
        );
        assert_eq!(std::fs::read(&config_path).unwrap(), before);
    }

    /// 깨진 config에서는 전환 화면도 로더의 오류를 그대로 보여준다(`extends` 순환·혼합 문법).
    #[tokio::test]
    async fn convert_surfaces_loader_errors() {
        let (cyclic, _) = wired_ctx_with(
            "convert-cycle",
            "[profile.a]\nextends = \"b\"\nuri=\"mongodb://h/a\"\n\
             [profile.b]\nextends = \"a\"\nuri=\"mongodb://h/b\"\n",
        );
        let body = body_text(convert_form(State(cyclic)).await).await;
        assert!(body.contains("순환") || body.contains("cycle"), "{body}");

        let (mixed, _) = wired_ctx_with(
            "convert-mixed",
            "[profiles.a.source]\nuri=\"mongodb://h/a\"\n[profile.b]\nuri=\"mongodb://h/b\"\n",
        );
        let body = body_text(convert_form(State(mixed)).await).await;
        assert!(body.contains("섞여 있"), "{body}");
    }
}
