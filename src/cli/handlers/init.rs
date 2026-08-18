//! `init` 서브커맨드 핸들러 — 대화형 마법사로 config.toml 생성(PRD §FR-10, R18).
//!
//! 흐름: 비-TTY 가드 → 기존 파일 가드(--force) → 마법사 실행 → config.toml 기록 →
//! (선택) status 즉시 검증.
//!
//! ## 가드레일
//! - **대화형 전용:** stdin이 TTY가 아니면 거부([`XBackupError::Usage`], exit 2) —
//!   마법사는 질문/응답이 전제다(비대화형은 EOF로 끊겨 부분 config가 생길 수 있음).
//! - **기존 파일 보호:** 대상 config.toml이 이미 있으면 `--force` 없이는 거부(exit 2) —
//!   기존 설정을 실수로 덮어쓰지 않게(FR-10).
//! - **시크릿 비저장:** 마법사가 env 참조만 받으므로 기록물에 평문 시크릿이 없다.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use crate::cli::args::{InitArgs, StatusArgs};
use crate::cli::output::{style_stderr, Tone};
use crate::config::wizard::{ask_yes_no, config_to_toml, run_wizard, Prompt, StdinPrompt};
use crate::config::Config;
use crate::error::{Result, XBackupError};

/// init이 config 경로 미지정 시 사용할 기본 파일명(현재 디렉터리 기준).
const DEFAULT_CONFIG_FILENAME: &str = "config.toml";

/// `init` 핸들러 진입점.
pub async fn handle(
    config_path: Option<PathBuf>,
    lang_flag: Option<crate::i18n::Lang>,
    args: InitArgs,
) -> Result<()> {
    // init은 config.toml을 새로 만드는 흐름이라 읽어들일 기존 config_toml이 없다 → flag/기본만 본다.
    let lang = crate::i18n::resolve(lang_flag, None);

    // 1) 비-TTY 가드 — 마법사는 대화형 전용. 표준입력이 터미널이 아니면 거부한다.
    if !std::io::stdin().is_terminal() {
        return Err(XBackupError::Usage(
            "init 마법사는 대화형(TTY) 전용입니다 — 파이프/CI에서는 config.toml을 직접 \
             작성하거나 ENV(XB_*)로 구성하세요"
                .into(),
        ));
    }

    // 2) 대상 경로 결정 + 기존 파일 가드(--force).
    let target = config_path.unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_FILENAME));
    guard_existing(&target, args.force)?;

    // 3) 마법사 실행(실 stdin) → Config.
    let mut prompt = StdinPrompt;
    let config = run_wizard(&mut prompt)?;

    // 4) config.toml 기록.
    write_config(&target, &config)?;
    // 안내는 stderr로(결과 채널 분리). 경로를 stdout으로 한 줄 알린다(스크립트 활용).
    eprintln!(
        "{}",
        style_stderr(
            &format!(
                "{}: {}",
                lang.sel("Created config.toml", "config.toml을 생성했습니다"),
                target.display()
            ),
            Tone::Success,
        )
    );
    println!("{}", target.display());

    // 4.5) age 암호화인데 공개키 파일이 없으면 키쌍 생성을 제안한다(친절 초기화).
    //      마법사는 recipient 파일 *경로*만 받으므로, 키 자체는 여기서 만들어 준다.
    ensure_age_key(&mut prompt, &config, lang)?;

    // 5) (선택) status 즉시 검증 — 동의 시 status 핸들러를 호출한다(FR-10 "생성 후 status").
    if let Some(profile_name) = config.default_profile.clone() {
        if ask_run_status(&mut prompt, lang)? {
            // status 핸들러는 같은 config 경로를 읽어 연결·권한을 점검한다.
            // 경고/실패도 정상 종료 코드로 표현되므로 여기서 에러를 흡수하지 않고 전파한다.
            let status_args = StatusArgs {
                profile: Some(profile_name),
                all: false,
                json: false,
                watch: false,
                interval: 1.0,
                count: 0,
                ns_detail: false,
            };
            return crate::cli::handlers::status::handle(Some(target), lang_flag, status_args)
                .await;
        }
    }

    Ok(())
}

/// age 암호화 공개키 파일이 없으면 그 자리에서 키쌍 생성을 제안한다.
///
/// 마법사는 시크릿 격리 원칙상 recipient(공개키) *파일 경로*만 받는다. 그 경로에 파일이
/// 없으면(처음 설정하는 경우가 대부분) 백업 시 "age recipient 파일 읽기 실패"로 막히므로,
/// 여기서 동의를 받아 키쌍을 만든다 — 개인키는 공개키 옆(`age.pub`→`age.key`)에 0600으로,
/// 공개키는 지정 경로에 쓴다. age가 아니거나 이미 파일이 있으면 아무것도 하지 않는다.
fn ensure_age_key(prompt: &mut dyn Prompt, config: &Config, lang: crate::i18n::Lang) -> Result<()> {
    let Some(profile_name) = config.default_profile.as_deref() else {
        return Ok(());
    };
    let Ok(profile) = config.profile(profile_name) else {
        return Ok(());
    };
    let enc = &profile.features.encryption;
    if !enc.enabled || enc.algorithm != crate::crypto::ALGORITHM_AGE {
        return Ok(());
    }
    let Some(pub_str) = enc.recipient_file.as_deref() else {
        return Ok(());
    };
    let pub_path = Path::new(pub_str);
    if pub_path.exists() {
        return Ok(()); // 이미 공개키가 있으면 그대로 쓴다.
    }

    let make = ask_yes_no(
        prompt,
        &format!(
            "age 공개키 파일이 없습니다({}) — age 키쌍을 지금 만들까요?",
            pub_path.display()
        ),
        true,
    )?;
    if !make {
        eprintln!(
            "{}",
            style_stderr(
                &format!(
                    "{} {}",
                    lang.sel(
                        "Skipped — create the recipient file before backup:",
                        "건너뜀 — 백업 전에 공개키 파일을 준비하세요:"
                    ),
                    pub_path.display()
                ),
                Tone::Plan,
            )
        );
        return Ok(());
    }

    // 개인키는 공개키 옆에 둔다(age.pub → age.key). 확장자 교체 결과가 공개키와 같아지면
    // (recipient를 .key로 줬을 때 등) 덮어쓰지 않도록 .key를 덧붙인다.
    let mut key_path = pub_path.with_extension("key");
    if key_path == pub_path {
        key_path = PathBuf::from(format!("{}.key", pub_path.display()));
    }

    let recipient = crate::crypto::generate_keypair_files(&key_path, pub_path)?;
    eprintln!(
        "{}",
        style_stderr(
            &format!(
                "{} {}\n  {} {} (0600)\n  recipient: {}",
                lang.sel("Generated age keypair →", "age 키쌍을 생성했습니다 →"),
                pub_path.display(),
                lang.sel("private key:", "개인키:"),
                key_path.display(),
                recipient,
            ),
            Tone::Success,
        )
    );
    eprintln!(
        "{}",
        style_stderr(
            lang.sel(
                "Keep the private key safe — restore needs it (XB_AGE_IDENTITY_FILE).",
                "개인키를 안전히 보관하세요 — 복구에 필요합니다(XB_AGE_IDENTITY_FILE로 지정).",
            ),
            Tone::Plan,
        )
    );
    Ok(())
}

/// 마지막 질문 — 지금 status로 연결·권한을 검증할지.
fn ask_run_status(prompt: &mut dyn Prompt, lang: crate::i18n::Lang) -> Result<bool> {
    let answer = prompt.read_line(&style_stderr(
        lang.sel(
            "Run status now to verify connection and permissions? [y/N]: ",
            "지금 status로 연결·권한을 검증하시겠습니까? [y/N]: ",
        ),
        Tone::Plan,
    ))?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// 기존 config 파일 가드 — 존재하는데 `--force`가 없으면 거부한다(exit 2).
fn guard_existing(target: &Path, force: bool) -> Result<()> {
    if target.exists() && !force {
        return Err(XBackupError::Usage(format!(
            "config 파일이 이미 존재합니다: {} — 덮어쓰려면 --force를 지정하세요",
            target.display()
        )));
    }
    Ok(())
}

/// Config를 TOML로 직렬화해 파일에 기록한다(상위 디렉터리는 필요 시 생성).
fn write_config(target: &Path, config: &Config) -> Result<()> {
    let toml = config_to_toml(config)?;
    if let Some(parent) = target.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| {
                XBackupError::Failure(format!(
                    "config 디렉터리 생성 실패({}): {e}",
                    parent.display()
                ))
            })?;
        }
    }
    std::fs::write(target, toml).map_err(|e| {
        XBackupError::Failure(format!("config.toml 기록 실패({}): {e}", target.display()))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::wizard::{run_wizard, VecPrompt};

    /// age + 미존재 recipient 경로로 config를 만드는 마법사 입력 시퀀스.
    fn age_answers(pub_path: &str) -> Vec<String> {
        vec![
            "prod".into(),
            "MONGO_URI".into(),
            "n".into(),
            "local".into(),
            "/data".into(),
            "10".into(),
            "y".into(), // 암호화 on
            "age".into(),
            pub_path.into(), // recipient(미존재)
            "15m".into(),
            "progress".into(),
        ]
    }

    /// 공개키가 없으면 동의(y) 시 키쌍을 만든다 — 공개키·개인키(0600) 생성.
    #[test]
    fn ensure_age_key_generates_when_missing() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let pub_path = dir.path().join("age.pub");
        let key_path = dir.path().join("age.key");

        let cfg = run_wizard(&mut VecPrompt::new(age_answers(pub_path.to_str().unwrap()))).unwrap();
        assert!(!pub_path.exists());

        ensure_age_key(&mut VecPrompt::new(vec!["y"]), &cfg, crate::i18n::Lang::En).unwrap();

        assert!(pub_path.exists(), "공개키가 생성돼야 함");
        assert!(key_path.exists(), "개인키가 생성돼야 함");
        let mode = std::fs::metadata(&key_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "개인키는 0600이어야 함");
        // 생성된 공개키 경로로 암호화 단계가 실제로 만들어진다(유효한 recipient).
        assert!(
            crate::crypto::AgeEncryptStage::from_recipient_file(pub_path.to_str().unwrap()).is_ok()
        );
    }

    /// 거부(n) 시 키를 만들지 않는다(공개키 파일 없음 유지).
    #[test]
    fn ensure_age_key_skips_on_decline() {
        let dir = tempfile::tempdir().unwrap();
        let pub_path = dir.path().join("age.pub");
        let cfg = run_wizard(&mut VecPrompt::new(age_answers(pub_path.to_str().unwrap()))).unwrap();

        ensure_age_key(&mut VecPrompt::new(vec!["n"]), &cfg, crate::i18n::Lang::En).unwrap();
        assert!(!pub_path.exists(), "거부했으면 생성하지 않아야 함");
    }

    /// 암호화 off면 프롬프트를 묻지 않고 통과한다(no-op).
    #[test]
    fn ensure_age_key_noop_when_encryption_disabled() {
        let answers = vec![
            "prod",
            "MONGO_URI",
            "n",
            "local",
            "/data",
            "10",
            "n",
            "15m",
            "progress",
        ];
        let cfg = run_wizard(&mut VecPrompt::new(answers)).unwrap();
        // 빈 프롬프트 — 무언가 물으면 입력 부족으로 에러가 났을 것.
        ensure_age_key(
            &mut VecPrompt::new(Vec::<String>::new()),
            &cfg,
            crate::i18n::Lang::En,
        )
        .unwrap();
    }

    /// 공개키가 이미 있으면 묻지 않고 그대로 둔다(no-op).
    #[test]
    fn ensure_age_key_noop_when_recipient_exists() {
        let dir = tempfile::tempdir().unwrap();
        let pub_path = dir.path().join("age.pub");
        std::fs::write(&pub_path, "age1existingkeyplaceholder\n").unwrap();
        let cfg = run_wizard(&mut VecPrompt::new(age_answers(pub_path.to_str().unwrap()))).unwrap();

        ensure_age_key(
            &mut VecPrompt::new(Vec::<String>::new()),
            &cfg,
            crate::i18n::Lang::En,
        )
        .unwrap();
        // 내용이 그대로 유지(덮어쓰지 않음).
        assert_eq!(
            std::fs::read_to_string(&pub_path).unwrap(),
            "age1existingkeyplaceholder\n"
        );
    }

    /// 기존 파일 + --force 없음 → 거부(exit 2).
    #[test]
    fn guard_rejects_existing_without_force() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "x").unwrap();
        let err = guard_existing(&path, false).unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    /// 기존 파일 + --force → 통과.
    #[test]
    fn guard_allows_existing_with_force() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "x").unwrap();
        assert!(guard_existing(&path, true).is_ok());
    }

    /// 없는 파일 → 항상 통과.
    #[test]
    fn guard_allows_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.toml");
        assert!(guard_existing(&path, false).is_ok());
    }

    /// write_config가 시크릿 평문 없는 파일을 만들고, 다시 파싱 가능하다.
    #[test]
    fn write_config_roundtrips_without_secrets() {
        use crate::config::wizard::{run_wizard, VecPrompt};
        let mut prompt = VecPrompt::new(vec![
            "prod",
            "MONGO_URI",
            "n",
            "local",
            "/data/backups",
            "10",
            "n",
            "15m",
            "progress",
        ]);
        let cfg = run_wizard(&mut prompt).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("config.toml");
        write_config(&path, &cfg).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("MONGO_URI"));
        assert!(!written.contains("mongodb://"));
        // init은 v2 표면을 쓴다: [profile.<name>](단수) + flat 키.
        assert!(
            written.contains("[profile.prod]"),
            "v2 형식이어야 함([profile.prod]):\n{written}"
        );
        assert!(
            !written.contains("[profiles."),
            "v1 형식(profiles 복수)이면 안 됨:\n{written}"
        );
        assert!(
            written.contains("dest = \"local:/data/backups\""),
            "v2 compact dest여야 함:\n{written}"
        );
        // 다시 파싱되어야 한다(v2 → v1 nested 정규화 왕복 안전).
        let reparsed = Config::from_toml_str(&written).unwrap();
        assert_eq!(reparsed.default_profile.as_deref(), Some("prod"));
        assert_eq!(
            reparsed
                .profile("prod")
                .unwrap()
                .destination
                .path
                .as_deref(),
            Some("/data/backups")
        );
    }
}
