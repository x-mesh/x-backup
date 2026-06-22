//! 출력 모드 결정 — progress / quiet / json (PRD §FR-9, R15/R16).
//!
//! 우선순위(높음→낮음, PRD §FR-9/§FR-10):
//! **CLI(`--json` > `--quiet` > `--progress`) > config `mode.output` > TTY 자동 감지.**
//!
//! ## 핵심 규약(pitfall 9-1)
//! - **progress·로그는 stderr**, **결과·`--json`은 stdout**으로 분리한다. 따라서 TTY
//!   자동 감지는 *stderr*가 터미널인지로 판단한다(stdout이 파이프로 리다이렉트돼도
//!   사람이 보는 stderr가 터미널이면 진행 표시가 유효하기 때문).
//! - 비-TTY(cron/CI/파이프)에서는 자동으로 quiet 동작 — 진행 바가 로그를 오염시키지
//!   않도록. `--progress`로 강제할 수 있다.

use std::io::IsTerminal;

use crate::cli::table::{
    pad, paint, use_color, use_color_stderr, BOLD, CYAN, DIM, GREEN, RED, YELLOW,
};

/// 사용자에게 보여줄 출력 모드.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    /// 단계별 진행 표시(TTY 기본).
    Progress,
    /// 진행 억제, 요약·경고·에러만(cron/CI, 비-TTY 자동).
    Quiet,
    /// 기계 판독 JSON.
    Json,
}

/// 사람용 출력의 의미 색상.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// 성공적으로 끝난 작업 제목.
    Success,
    /// dry-run/계획/정보성 제목.
    Plan,
    /// 경고·주의.
    Warning,
    /// 위험하거나 실패한 상태.
    Danger,
    /// 필드 라벨·강조 기호.
    Label,
    /// 중요한 값.
    Value,
    /// 보조 설명·컨텍스트.
    Muted,
}

fn tone_codes(tone: Tone) -> &'static [&'static str] {
    match tone {
        Tone::Success => &[BOLD, GREEN],
        Tone::Plan => &[BOLD, CYAN],
        Tone::Warning => &[BOLD, YELLOW],
        Tone::Danger => &[BOLD, RED],
        Tone::Label => &[BOLD, CYAN],
        Tone::Value => &[BOLD],
        Tone::Muted => &[DIM],
    }
}

/// stdout 사람용 텍스트에 의미 색상을 적용한다. 비-TTY/NO_COLOR에서는 원문 그대로.
pub fn style(s: &str, tone: Tone) -> String {
    paint(s, tone_codes(tone), use_color())
}

/// stderr 사람용 텍스트에 의미 색상을 적용한다. 비-TTY/NO_COLOR에서는 원문 그대로.
pub fn style_stderr(s: &str, tone: Tone) -> String {
    paint(s, tone_codes(tone), use_color_stderr())
}

/// 정렬된 `label: value` 한 줄을 만든다. `width`는 `label:` 포함 표시 폭이다.
pub fn field_line(label: &str, value: impl AsRef<str>, width: usize) -> String {
    let key = pad(&format!("{label}:"), width);
    format!("  {}{}", style(&key, Tone::Label), value.as_ref())
}

/// 값의 상태에 따라 색을 다르게 입힌 `label: value` 한 줄.
pub fn field_line_toned(label: &str, value: impl AsRef<str>, width: usize, tone: Tone) -> String {
    let key = pad(&format!("{label}:"), width);
    format!(
        "  {}{}",
        style(&key, Tone::Label),
        style(value.as_ref(), tone)
    )
}

/// OK/WARN/FAIL 같은 짧은 상태 토큰.
pub fn status_token(text: &str, tone: Tone) -> String {
    style(text, tone)
}

/// 출력 모드 결정에 쓰는 CLI 플래그 묶음.
///
/// backup/restore가 공유하는 `--json`/`--quiet`/`--progress`를 한곳에 모은다(clap에서
/// 이미 `--quiet`↔`--progress` 상호 배타가 보장되므로 둘이 동시에 true일 일은 없다).
#[derive(Debug, Clone, Copy, Default)]
pub struct OutputFlags {
    /// `--json` — 진행/결과를 기계 판독 JSON으로.
    pub json: bool,
    /// `--quiet` — 진행 억제.
    pub quiet: bool,
    /// `--progress` — 비-TTY에서도 진행 강제.
    pub progress: bool,
}

impl OutputMode {
    /// CLI 플래그·config 기본값·TTY 여부로 출력 모드를 결정한다(전체 규칙).
    ///
    /// 우선순위: `--json` > `--quiet` > `--progress` > config(`mode.output`) > TTY 자동.
    /// `config_output`은 config `mode.output`("progress"|"quiet"); 알 수 없는 값은
    /// 무시하고 TTY 자동 감지로 폴백한다(설정 결함이 출력에 치명적이지 않도록).
    pub fn resolve(flags: OutputFlags, config_output: Option<&str>, is_tty: bool) -> Self {
        if flags.json {
            return Self::Json;
        }
        if flags.quiet {
            return Self::Quiet;
        }
        if flags.progress {
            // --progress는 비-TTY에서도 진행을 강제한다.
            return Self::Progress;
        }
        // CLI에서 출력 모드를 지정하지 않았으면 config 기본값을 본다.
        match config_output {
            Some("quiet") => Self::Quiet,
            Some("progress") => {
                // config가 progress를 원해도, 비-TTY면 진행 바가 로그를 오염시키므로
                // 자동 quiet로 낮춘다(명시적 --progress만 비-TTY 강제).
                if is_tty {
                    Self::Progress
                } else {
                    Self::Quiet
                }
            }
            // config 미지정/알 수 없는 값 → TTY 자동 감지.
            _ => {
                if is_tty {
                    Self::Progress
                } else {
                    Self::Quiet
                }
            }
        }
    }

    /// 현재 프로세스의 **stderr** TTY 여부로 출력 모드를 결정한다(실사용 진입점).
    ///
    /// progress·로그는 stderr로 나가므로(pitfall 9-1), 진행 표시 가능 여부는 stderr가
    /// 터미널인지로 판단한다 — stdout이 파이프로 묶여도 사람이 보는 stderr 기준.
    pub fn resolve_from_env(flags: OutputFlags, config_output: Option<&str>) -> Self {
        Self::resolve(flags, config_output, std::io::stderr().is_terminal())
    }

    /// 진행 표시(progress bar/spinner)를 그려야 하는 모드인지.
    ///
    /// `Progress`만 true다. `Quiet`/`Json`은 진행 바를 그리지 않는다(Json은 결과만,
    /// 진행 *이벤트*는 별도로 stderr JSON 라인으로 선택 출력 — [`Self::emits_json`] 참조).
    pub fn shows_progress_bar(self) -> bool {
        matches!(self, Self::Progress)
    }

    /// 사람용 요약(stdout 텍스트)을 출력하는 모드인지. `Json`/`Quiet`은 false.
    pub fn shows_human_summary(self) -> bool {
        matches!(self, Self::Progress)
    }

    /// 결과를 JSON으로 출력하는 모드인지(`--json`).
    pub fn emits_json(self) -> bool {
        matches!(self, Self::Json)
    }
}

/// `--json`만 받는 핸들러(list/status/peek/prune/pitr)에서 컨텍스트 표시 모드를 만든다 —
/// json이면 Json(컨텍스트 생략), 아니면 stderr TTY 여부로 Progress/Quiet.
pub fn context_mode(json: bool) -> OutputMode {
    OutputMode::resolve_from_env(
        OutputFlags {
            json,
            quiet: false,
            progress: false,
        },
        None,
    )
}

/// 실행 컨텍스트 한 줄(어떤 프로파일·어떤 DB)을 **stderr**에 표시한다.
///
/// x-backup은 MongoDB·PostgreSQL을 한 바이너리로 다루는 다중 DB 툴이라, 모든 명령이 "지금
/// 어느 프로파일·어느 DB를 건드리는지"를 항상 보여주는 게 안전하다. 결과(stdout)·`--json`을
/// 오염시키지 않도록 stderr로 내보내고, 사람용(Progress) 모드에서만 표시한다(quiet/json 생략).
pub fn print_run_context(profile: &str, db: Option<crate::engine::DbKind>, mode: OutputMode) {
    // json만 제외(기계 판독 오염 방지). progress·quiet 모두 stderr에 한 줄 — 다중 DB 툴에서
    // "지금 무엇을 건드리는지"는 quiet에서도 보이는 게 안전하다.
    if mode.emits_json() {
        return;
    }
    let line = match db {
        Some(d) => format!("▸ profile {profile} · DB {}", d.label()),
        None => format!("▸ profile {profile}"),
    };
    eprintln!("{}", style_stderr(&line, Tone::Muted));
}

/// 복구가 백업을 **읽어올** 저장소 위치를 stderr에 한 줄로 표시한다.
///
/// restore에서 백업을 고르기 전에 "어느 store의 어떤 백업을 보고 있는지"를 분명히 한다
/// (`list`의 `store:` 표기와 의미가 같다). json은 기계 출력 오염 방지로 생략한다.
pub fn print_backup_store(location: &str, mode: OutputMode) {
    if mode.emits_json() {
        return;
    }
    eprintln!(
        "{}",
        style_stderr(&format!("▸ store {location}"), Tone::Muted)
    );
}

/// 접속 URI에서 자격증명(userinfo)·민감 쿼리 값을 가린 표시용 문자열을 만든다.
///
/// "어디로 연결/복구되는지"(scheme·host:port·DB)는 사람이 확인할 수 있어야 하지만, 비밀번호·
/// 토큰은 절대 노출하지 않는다(PRD §11). `user:pass@`는 `***@`로, 민감 쿼리 파라미터의 값은
/// `***`로 치환하고 나머지(replicaSet 등)는 그대로 둔다. [`Secret`](crate::config::secret::Secret)이
/// 통째로 `[REDACTED]`만 내는 것과 달리, 이 함수는 host를 보존해 대상 식별을 돕는다.
pub fn redact_uri(uri: &str) -> String {
    // scheme://rest 분리(스킴이 없으면 통째로 authority/rest로 취급).
    let (scheme, rest) = match uri.split_once("://") {
        Some((s, r)) => (Some(s), r),
        None => (None, uri),
    };
    // authority(host[:port])와 path/query 분리 — authority는 첫 '/' 또는 '?' 전까지.
    let auth_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(auth_end);
    // userinfo(user:pass@)가 있으면 흔적(***@)만 남기고 호스트는 보존한다.
    let host = match authority.rsplit_once('@') {
        Some((_, h)) => format!("***@{h}"),
        None => authority.to_string(),
    };
    let tail = redact_query(tail);
    match scheme {
        Some(s) => format!("{s}://{host}{tail}"),
        None => format!("{host}{tail}"),
    }
}

/// path?query에서 민감 키의 값을 `***`로 가린다(키·나머지 파라미터는 보존). query가 없으면 그대로.
fn redact_query(tail: &str) -> String {
    let Some((path, query)) = tail.split_once('?') else {
        return tail.to_string();
    };
    let redacted = query
        .split('&')
        .map(|pair| {
            let key = pair.split('=').next().unwrap_or(pair);
            if is_sensitive_param(key) {
                format!("{key}=***")
            } else {
                pair.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("&");
    format!("{path}?{redacted}")
}

/// 값에 시크릿이 담길 수 있는 쿼리 파라미터 키인지 판별한다(대소문자 무시).
fn is_sensitive_param(key: &str) -> bool {
    const SENSITIVE: &[&str] = &[
        "password",
        "pwd",
        "sslpassword",
        "tlscertificatekeyfilepassword",
        "authmechanismproperties",
        "secret",
        "token",
        "accesskey",
        "secretkey",
        "awssessiontoken",
    ];
    let k = key.to_ascii_lowercase();
    SENSITIVE.contains(&k.as_str())
}

/// 복구 대상 URI가 어디서 왔는지 — 표시·감사용.
///
/// restore의 대상 결정 우선순위(`--target` > `--target-profile` > 프로파일 자신 source)를
/// 그대로 반영한다. 이름(`Profile`)은 소유한다 — 짧은 문자열이라 복제 비용이 무시할 만하고,
/// `args` 수명에 묶이지 않아 핸들러 간 이동이 자유롭다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreTargetOrigin {
    /// `--target <uri>`로 직접 지정.
    Flag,
    /// `--target-profile <name>` — 다른 프로파일의 source 접속.
    Profile(String),
    /// 미지정 — 프로파일 자신의 source로 되돌리는 in-place 복구(기본).
    InPlace,
}

impl RestoreTargetOrigin {
    /// 사람용 출처 라벨(괄호 안 표시).
    fn label(&self, lang: crate::i18n::Lang) -> String {
        match self {
            RestoreTargetOrigin::Flag => "--target".to_string(),
            RestoreTargetOrigin::Profile(name) => format!("--target-profile: {name}"),
            RestoreTargetOrigin::InPlace => {
                lang.sel("profile source", "프로파일 source").to_string()
            }
        }
    }
}

/// 복구 대상(어느 host로 복원하는지)을 **stderr**에 한 줄로 명확히 표시한다.
///
/// restore는 기본적으로 백업을 떠온 프로파일 `source`로 되돌린다(in-place). `--target`(URI)이나
/// `--target-profile`(다른 프로파일의 source)을 주면 다른 곳으로 보낸다 — 어느 쪽이든 자격증명을
/// 가린 대상 URI와 그 출처([`RestoreTargetOrigin`])를 보여줘, 운영 DB를 실수로 덮어쓰는 일을 막는다
/// ([`redact_uri`]로 시크릿 차단). json은 생략(기계 출력 오염 방지)하고 사람 모드(progress/quiet)에서만
/// 표시하며, 같은 정보는 json 출력에 `destination` 필드로 들어간다.
pub fn print_restore_target(
    target_uri: &crate::config::secret::Secret,
    origin: &RestoreTargetOrigin,
    mode: OutputMode,
    lang: crate::i18n::Lang,
) {
    if mode.emits_json() {
        return;
    }
    let line = format!(
        "→ {} {} ({})",
        lang.sel("restore target", "복구 대상"),
        redact_uri(target_uri.expose()),
        origin.label(lang),
    );
    eprintln!("{}", style_stderr(&line, Tone::Warning));
}

/// 참조 중인 config 파일 위치를 사람이 읽는 한 줄로 만든다(절대경로 우선).
///
/// config 경로는 `--config <PATH>` 또는 `XB_CONFIG`로만 결정된다(자동 탐색 없음). 그래서
/// `None`이면 "어디서도 읽지 않는다"를 명시해, 사용자가 어떤 파일을 보는지(특히 `xbenv`
/// 활성화로 `XB_CONFIG`가 자동 주입된 경우) 헷갈리지 않게 한다. 존재하는 경로는
/// canonicalize로 절대경로화하고, 실패하면 입력 경로를 그대로 보여준다.
pub fn config_source_label(path: Option<&std::path::Path>, lang: crate::i18n::Lang) -> String {
    match path {
        Some(p) => std::fs::canonicalize(p)
            .map(|abs| abs.display().to_string())
            .unwrap_or_else(|_| p.display().to_string()),
        None => lang
            .sel(
                "(unset — no XB_CONFIG/--config; env override only)",
                "(미지정 — XB_CONFIG/--config 없음, env override만)",
            )
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(json: bool, quiet: bool, progress: bool) -> OutputFlags {
        OutputFlags {
            json,
            quiet,
            progress,
        }
    }

    #[test]
    fn json_flag_wins_over_everything() {
        // json + quiet + progress + config quiet + tty → Json.
        assert_eq!(
            OutputMode::resolve(flags(true, true, true), Some("quiet"), true),
            OutputMode::Json
        );
    }

    #[test]
    fn quiet_flag_beats_config_and_tty() {
        assert_eq!(
            OutputMode::resolve(flags(false, true, false), Some("progress"), true),
            OutputMode::Quiet
        );
    }

    #[test]
    fn progress_flag_forces_on_non_tty() {
        assert_eq!(
            OutputMode::resolve(flags(false, false, true), Some("quiet"), false),
            OutputMode::Progress
        );
    }

    #[test]
    fn config_quiet_applies_when_no_cli_flag() {
        // CLI 미지정 + config quiet → Quiet(TTY여도).
        assert_eq!(
            OutputMode::resolve(flags(false, false, false), Some("quiet"), true),
            OutputMode::Quiet
        );
    }

    #[test]
    fn config_progress_on_tty_is_progress() {
        assert_eq!(
            OutputMode::resolve(flags(false, false, false), Some("progress"), true),
            OutputMode::Progress
        );
    }

    #[test]
    fn config_progress_on_non_tty_falls_back_to_quiet() {
        // config가 progress를 원해도 비-TTY면 자동 quiet(명시 --progress만 강제).
        assert_eq!(
            OutputMode::resolve(flags(false, false, false), Some("progress"), false),
            OutputMode::Quiet
        );
    }

    #[test]
    fn no_config_tty_defaults_to_progress() {
        assert_eq!(
            OutputMode::resolve(flags(false, false, false), None, true),
            OutputMode::Progress
        );
    }

    #[test]
    fn no_config_non_tty_defaults_to_quiet() {
        assert_eq!(
            OutputMode::resolve(flags(false, false, false), None, false),
            OutputMode::Quiet
        );
    }

    #[test]
    fn unknown_config_value_falls_back_to_tty_auto() {
        // 알 수 없는 config 값은 무시하고 TTY 자동 감지.
        assert_eq!(
            OutputMode::resolve(flags(false, false, false), Some("garbage"), true),
            OutputMode::Progress
        );
        assert_eq!(
            OutputMode::resolve(flags(false, false, false), Some("garbage"), false),
            OutputMode::Quiet
        );
    }

    #[test]
    fn mode_predicates() {
        assert!(OutputMode::Progress.shows_progress_bar());
        assert!(!OutputMode::Quiet.shows_progress_bar());
        assert!(!OutputMode::Json.shows_progress_bar());

        assert!(OutputMode::Progress.shows_human_summary());
        assert!(!OutputMode::Quiet.shows_human_summary());
        assert!(!OutputMode::Json.shows_human_summary());

        assert!(OutputMode::Json.emits_json());
        assert!(!OutputMode::Progress.emits_json());
    }

    #[test]
    fn redact_uri_strips_userinfo_keeps_host() {
        // 자격증명은 ***@로 가리고 host:port·DB·일반 쿼리는 보존한다.
        assert_eq!(
            redact_uri("mongodb://alice:s3cr3t@db.prod:27017/app?replicaSet=rs0"),
            "mongodb://***@db.prod:27017/app?replicaSet=rs0"
        );
        // userinfo가 없으면 그대로 둔다(로컬/개발 URI).
        assert_eq!(
            redact_uri("mongodb://localhost:27017/?replicaSet=rs0&directConnection=true"),
            "mongodb://localhost:27017/?replicaSet=rs0&directConnection=true"
        );
        assert_eq!(
            redact_uri("postgres://u:p@10.0.0.5:5432/maindb"),
            "postgres://***@10.0.0.5:5432/maindb"
        );
    }

    #[test]
    fn redact_uri_masks_sensitive_query_values() {
        // 쿼리에 담긴 시크릿(password 등)도 값만 가리고 키·나머지는 남긴다.
        assert_eq!(
            redact_uri("postgres://host:5432/db?sslmode=require&password=hunter2"),
            "postgres://host:5432/db?sslmode=require&password=***"
        );
    }

    #[test]
    fn redact_uri_never_leaks_secrets() {
        // 어떤 형태든 평문 시크릿이 남지 않아야 한다.
        let rendered = redact_uri("mongodb://admin:topsecret@h:27017/db?token=abc123");
        assert!(!rendered.contains("topsecret"), "userinfo 노출: {rendered}");
        assert!(!rendered.contains("abc123"), "쿼리 시크릿 노출: {rendered}");
    }
}
