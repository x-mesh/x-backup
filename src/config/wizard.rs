//! 대화형 마법사(`init`) — 질문에 답하면 [`Config`]를 만든다(PRD §FR-10, R18).
//!
//! ## 시크릿 비저장 원칙(필수)
//! 마법사는 **시크릿 값을 절대 입력받거나 저장하지 않는다**. URI·S3 자격증명은 *env
//! 변수명*만 받아 `uri_env`/`credentials_env`로 보관한다(config 유출돼도 시크릿 안전).
//! 암호화 키도 평문 대신 age recipient *파일 경로*만 받는다(공개키, 복호화 키는 별도 격리).
//!
//! ## 입력 추상화(테스트 용이성)
//! 실제 stdin 대신 [`Prompt`] trait로 입력을 주입한다 — 마법사 로직을 DB·터미널 없이
//! 입력 시퀀스만으로 단위 테스트할 수 있다([`VecPrompt`]). 실사용은 [`StdinPrompt`].
//! 대화형 크레이트(dialoguer)를 쓰지 않은 이유: trait 한 개로 주입이 더 단순하고,
//! 질문 흐름이 단순 라인 입력이라 추가 의존성 가치가 낮다.

use crate::config::file::{
    CompressionConfig, Config, DestinationConfig, EncryptionConfig, FeaturesConfig,
    IncrementalConfig, ModeConfig, Profile, S3Config, SourceConfig,
};
use crate::error::{Result, XBackupError};

/// 마법사가 사용하는 입력 소스 추상화. 한 줄 입력(프롬프트 텍스트 → 응답)을 제공한다.
///
/// `read_line`은 사용자가 입력한 한 줄(개행 제외)을 반환한다. EOF(입력 종료)면
/// [`XBackupError::Usage`]로 보고한다 — 비대화형에서 마법사를 돌리면 안 되기 때문.
pub trait Prompt {
    /// 질문을 표시하고 한 줄 응답을 읽는다.
    fn read_line(&mut self, question: &str) -> Result<String>;
}

/// 실제 stdin에서 읽는 프롬프트(stderr로 질문 출력 — stdout은 결과 전용).
pub struct StdinPrompt;

impl Prompt for StdinPrompt {
    fn read_line(&mut self, question: &str) -> Result<String> {
        use std::io::Write;
        // 질문은 stderr로(progress/로그와 같은 채널; stdout 오염 금지).
        eprint!("{question}");
        let _ = std::io::stderr().flush();

        let mut buf = String::new();
        let n = std::io::stdin()
            .read_line(&mut buf)
            .map_err(|e| XBackupError::Usage(format!("입력 읽기 실패: {e}")))?;
        if n == 0 {
            // EOF — 비대화형 입력. 마법사는 대화형 전용이다.
            return Err(XBackupError::Usage(
                "입력이 종료되었습니다(EOF) — init 마법사는 대화형 전용입니다".into(),
            ));
        }
        Ok(buf.trim_end_matches(['\n', '\r']).to_string())
    }
}

/// 미리 정한 응답 시퀀스를 순서대로 돌려주는 테스트용 프롬프트.
pub struct VecPrompt {
    answers: std::collections::VecDeque<String>,
}

impl VecPrompt {
    /// 응답 시퀀스로 생성한다(질문 순서대로 소비).
    pub fn new<I, S>(answers: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            answers: answers.into_iter().map(Into::into).collect(),
        }
    }
}

impl Prompt for VecPrompt {
    fn read_line(&mut self, _question: &str) -> Result<String> {
        self.answers.pop_front().ok_or_else(|| {
            XBackupError::Usage("테스트 입력 시퀀스가 부족합니다(질문이 더 많음)".into())
        })
    }
}

/// 기본값 안내가 붙은 질문 한 줄을 읽고, 빈 응답이면 기본값을 쓴다.
fn ask_default(prompt: &mut dyn Prompt, question: &str, default: &str) -> Result<String> {
    let answer = prompt.read_line(&format!("{question} [{default}]: "))?;
    if answer.trim().is_empty() {
        Ok(default.to_string())
    } else {
        Ok(answer.trim().to_string())
    }
}

/// 필수 입력(빈 값 불가) 질문. 빈 응답이면 설정 오류로 보고한다.
fn ask_required(prompt: &mut dyn Prompt, question: &str) -> Result<String> {
    let answer = prompt.read_line(&format!("{question}: "))?;
    let trimmed = answer.trim();
    if trimmed.is_empty() {
        return Err(XBackupError::Config(format!(
            "필수 항목이 비어 있습니다: {question}"
        )));
    }
    Ok(trimmed.to_string())
}

/// y/n 질문. 빈 응답이면 `default`를 쓴다.
fn ask_yes_no(prompt: &mut dyn Prompt, question: &str, default: bool) -> Result<bool> {
    let hint = if default { "Y/n" } else { "y/N" };
    let answer = prompt.read_line(&format!("{question} [{hint}]: "))?;
    let t = answer.trim().to_ascii_lowercase();
    if t.is_empty() {
        return Ok(default);
    }
    Ok(matches!(t.as_str(), "y" | "yes"))
}

/// 마법사를 실행해 단일 프로파일 [`Config`]를 만든다(시크릿 평문 0 — env 참조만).
///
/// 질문 순서(DoD·FR-10): 프로파일명 → 접속(uri_env) → secondary → destination(local|s3)
/// → 압축 레벨 → 암호화(enabled/algorithm/recipient_file) → 증분 interval → 출력 기본값.
pub fn run_wizard(prompt: &mut dyn Prompt) -> Result<Config> {
    // 1) 프로파일 이름.
    let profile_name = ask_default(prompt, "프로파일 이름", "prod")?;

    // 2) 접속 — URI는 시크릿이므로 *env 변수명*만 받는다(값 직접 입력 금지).
    let uri_env = ask_default(
        prompt,
        "MongoDB URI가 담긴 환경변수 이름(시크릿 평문 금지 — 값이 아니라 변수명)",
        "MONGO_URI",
    )?;
    let prefer_secondary = ask_yes_no(prompt, "가능하면 secondary에서 백업하시겠습니까?", false)?;

    // 3) destination — local 경로 또는 s3 설정.
    let dest_type = loop {
        let t = ask_default(prompt, "백업 위치 유형 (local | s3)", "local")?;
        match t.as_str() {
            "local" | "s3" => break t,
            other => {
                // 잘못된 값은 다시 묻는다(대화형이므로 즉시 재질문).
                let _ = other;
                prompt.read_line("  ! 'local' 또는 's3'만 가능합니다. Enter로 다시 입력")?;
            }
        }
    };

    let destination = match dest_type.as_str() {
        "local" => {
            let path = ask_default(prompt, "로컬 백업 경로", "/var/backups/mongo")?;
            DestinationConfig {
                r#type: Some("local".to_string()),
                path: Some(path),
                s3: None,
            }
        }
        "s3" => {
            let endpoint = ask_required(prompt, "S3 endpoint(예: https://s3.example.com)")?;
            let bucket = ask_required(prompt, "S3 bucket")?;
            let prefix = ask_default(prompt, "S3 prefix", "mongo")?;
            let region = ask_default(prompt, "S3 region", "us-east-1")?;
            // 자격증명도 시크릿 — env 변수명만 받는다(평문 금지).
            let credentials_env = ask_default(
                prompt,
                "S3 자격증명이 담긴 환경변수 이름(값이 아니라 변수명)",
                "S3_CREDS",
            )?;
            DestinationConfig {
                r#type: Some("s3".to_string()),
                path: None,
                s3: Some(S3Config {
                    endpoint: Some(endpoint),
                    bucket: Some(bucket),
                    prefix: Some(prefix),
                    region: Some(region),
                    credentials_env: Some(credentials_env),
                }),
            }
        }
        // 위 루프가 local|s3만 통과시키므로 도달 불가.
        _ => unreachable!("dest_type은 local|s3로 검증됨"),
    };

    // 4) 압축 레벨(zstd 단일).
    let level_str = ask_default(prompt, "압축 레벨(zstd, 1~22)", "10")?;
    let level: i32 = level_str
        .parse()
        .map_err(|_| XBackupError::Config(format!("압축 레벨이 숫자가 아닙니다: '{level_str}'")))?;

    // 5) 암호화 — 기본 ON. 시크릿 키는 받지 않고 recipient 파일 경로(공개키)만 받는다.
    let enc_enabled = ask_yes_no(prompt, "백업을 암호화하시겠습니까?", true)?;
    let encryption = if enc_enabled {
        let algorithm = loop {
            let a = ask_default(prompt, "암호화 알고리즘 (age | aes-256-gcm)", "age")?;
            match a.as_str() {
                "age" | "aes-256-gcm" => break a,
                _ => {
                    prompt.read_line(
                        "  ! 'age' 또는 'aes-256-gcm'만 가능합니다. Enter로 다시 입력",
                    )?;
                }
            }
        };
        let recipient_file = if algorithm == "age" {
            // age는 공개키(recipient) 파일 경로 — 시크릿 아님(복호화 키는 별도 격리).
            Some(ask_required(
                prompt,
                "age recipient(공개키) 파일 경로(예: /etc/x-backup/age.pub)",
            )?)
        } else {
            // aes-256-gcm 키는 env(XB_AES_KEY_HEX)로 주입 — config에 담지 않는다(안내만).
            None
        };
        EncryptionConfig {
            enabled: true,
            algorithm,
            recipient_file,
        }
    } else {
        EncryptionConfig {
            enabled: false,
            ..EncryptionConfig::default()
        }
    };

    // 6) 증분 interval(gap 위험 경고 기준).
    let interval = ask_default(prompt, "증분 간격(gap 경고 기준, 예: 15m/1h)", "15m")?;

    // 7) 출력 기본값.
    let output = loop {
        let o = ask_default(prompt, "기본 출력 모드 (progress | quiet)", "progress")?;
        match o.as_str() {
            "progress" | "quiet" => break o,
            _ => {
                prompt.read_line("  ! 'progress' 또는 'quiet'만 가능합니다. Enter로 다시 입력")?;
            }
        }
    };

    // 조립. compression/incremental의 나머지 필드는 기본값을 따른다.
    let profile = Profile {
        mode: ModeConfig {
            backup_type: "full".to_string(),
            output,
            precheck: true,
        },
        source: SourceConfig {
            uri: None,
            uri_env: Some(uri_env),
            prefer_secondary,
        },
        destination,
        features: FeaturesConfig {
            compression: CompressionConfig {
                level,
                ..CompressionConfig::default()
            },
            encryption,
            incremental: IncrementalConfig {
                interval,
                ..IncrementalConfig::default()
            },
        },
    };

    let mut config = Config {
        default_profile: Some(profile_name.clone()),
        ..Config::default()
    };
    config.profiles.insert(profile_name, profile);
    Ok(config)
}

/// [`Config`]를 TOML 문자열로 직렬화한다(파일 기록용). 시크릿이 없으므로 그대로 안전하다.
pub fn config_to_toml(config: &Config) -> Result<String> {
    toml::to_string_pretty(config)
        .map_err(|e| XBackupError::Failure(format!("config.toml 직렬화 실패: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 전형적인 local + age 암호화 흐름의 입력 시퀀스.
    fn local_age_answers() -> Vec<String> {
        vec![
            "prod".to_string(),          // 프로파일명
            "MONGO_URI".to_string(),     // uri_env
            "y".to_string(),             // prefer_secondary
            "local".to_string(),         // destination type
            "/data/backups".to_string(), // local path
            "12".to_string(),            // 압축 레벨
            "y".to_string(),             // 암호화 enabled
            "age".to_string(),           // 알고리즘
            "/etc/age.pub".to_string(),  // recipient file
            "30m".to_string(),           // 증분 interval
            "progress".to_string(),      // 출력 기본값
        ]
    }

    #[test]
    fn builds_local_age_config() {
        let mut prompt = VecPrompt::new(local_age_answers());
        let cfg = run_wizard(&mut prompt).unwrap();

        assert_eq!(cfg.default_profile.as_deref(), Some("prod"));
        let p = cfg.profile("prod").unwrap();
        assert_eq!(p.source.uri_env.as_deref(), Some("MONGO_URI"));
        assert!(p.source.prefer_secondary);
        assert_eq!(p.destination.r#type.as_deref(), Some("local"));
        assert_eq!(p.destination.path.as_deref(), Some("/data/backups"));
        assert_eq!(p.features.compression.level, 12);
        assert!(p.features.encryption.enabled);
        assert_eq!(p.features.encryption.algorithm, "age");
        assert_eq!(
            p.features.encryption.recipient_file.as_deref(),
            Some("/etc/age.pub")
        );
        assert_eq!(p.features.incremental.interval, "30m");
        assert_eq!(p.mode.output, "progress");
    }

    /// 핵심 DoD: 생성된 config TOML에 시크릿 *값*이 평문으로 들어가지 않는다.
    /// 사용자가 시크릿처럼 보이는 값을 입력해도 마법사는 env 변수명만 받으므로,
    /// 직렬화 결과에는 변수명만 남는다.
    #[test]
    fn config_has_no_plaintext_secrets() {
        let mut prompt = VecPrompt::new(local_age_answers());
        let cfg = run_wizard(&mut prompt).unwrap();
        let toml = config_to_toml(&cfg).unwrap();

        // env 변수명은 있어야 한다.
        assert!(toml.contains("uri_env"), "uri_env 참조가 있어야 함");
        assert!(toml.contains("MONGO_URI"));
        // 평문 URI/비밀번호 패턴은 없어야 한다.
        assert!(
            !toml.contains("mongodb://"),
            "평문 URI가 config에 들어가면 안 됨:\n{toml}"
        );
        assert!(
            !toml.to_lowercase().contains("password"),
            "비밀번호 평문이 들어가면 안 됨"
        );
    }

    /// s3 흐름: 자격증명은 env 변수명으로만 저장된다.
    #[test]
    fn builds_s3_config_with_credentials_env() {
        let answers = vec![
            "staging",             // 프로파일명
            "STG_MONGO_URI",       // uri_env
            "n",                   // prefer_secondary
            "s3",                  // destination type
            "https://minio.local", // endpoint
            "db-backups",          // bucket
            "mongo/staging",       // prefix
            "ap-northeast-2",      // region
            "MINIO_CREDS",         // credentials_env(변수명)
            "6",                   // 압축 레벨
            "n",                   // 암호화 disabled
            "1h",                  // 증분 interval
            "quiet",               // 출력 기본값
        ];
        let mut prompt = VecPrompt::new(answers);
        let cfg = run_wizard(&mut prompt).unwrap();

        let p = cfg.profile("staging").unwrap();
        let s3 = p.destination.s3.as_ref().unwrap();
        assert_eq!(s3.bucket.as_deref(), Some("db-backups"));
        assert_eq!(s3.credentials_env.as_deref(), Some("MINIO_CREDS"));
        assert!(!p.features.encryption.enabled);
        assert_eq!(p.mode.output, "quiet");

        // 직렬화에 자격증명 *값*은 없고 변수명만 있다.
        let toml = config_to_toml(&cfg).unwrap();
        assert!(toml.contains("MINIO_CREDS"));
        assert!(!toml.to_lowercase().contains("secretaccesskey"));
    }

    /// 기본값(빈 입력) 흐름: Enter만 누르면 합리적 기본값으로 채워진다.
    #[test]
    fn empty_inputs_use_defaults() {
        let answers = vec![
            "",                      // 프로파일명 → prod
            "",                      // uri_env → MONGO_URI
            "",                      // prefer_secondary → false
            "",                      // destination type → local
            "",                      // local path → 기본
            "",                      // 압축 레벨 → 10
            "",                      // 암호화 enabled → true(기본)
            "",                      // 알고리즘 → age
            "/etc/x-backup/age.pub", // recipient file(필수)
            "",                      // 증분 interval → 15m
            "",                      // 출력 → progress
        ];
        let mut prompt = VecPrompt::new(answers);
        let cfg = run_wizard(&mut prompt).unwrap();
        let p = cfg.profile("prod").unwrap();
        assert_eq!(p.source.uri_env.as_deref(), Some("MONGO_URI"));
        assert_eq!(p.destination.r#type.as_deref(), Some("local"));
        assert_eq!(p.features.compression.level, 10);
        assert!(p.features.encryption.enabled);
        assert_eq!(p.features.incremental.interval, "15m");
        assert_eq!(p.mode.output, "progress");
    }

    /// 입력 시퀀스가 부족하면(EOF 상당) 명확한 오류(exit 2).
    #[test]
    fn insufficient_input_errors() {
        let mut prompt = VecPrompt::new(vec!["prod"]);
        let err = run_wizard(&mut prompt).unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    /// 압축 레벨이 숫자가 아니면 설정 오류(exit 2).
    #[test]
    fn non_numeric_compress_level_errors() {
        let answers = vec![
            "prod",
            "MONGO_URI",
            "n",
            "local",
            "/data",
            "abc", // ← 잘못된 레벨
        ];
        let mut prompt = VecPrompt::new(answers);
        let err = run_wizard(&mut prompt).unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    /// 잘못된 destination 유형은 재질문 후 올바른 값으로 진행한다.
    #[test]
    fn invalid_dest_type_reprompts() {
        let answers = vec![
            "prod",
            "MONGO_URI",
            "n",
            "ftp",   // 잘못된 유형
            "",      // 재질문 Enter
            "local", // 올바른 유형
            "/data",
            "10",
            "n",
            "15m",
            "progress",
        ];
        let mut prompt = VecPrompt::new(answers);
        let cfg = run_wizard(&mut prompt).unwrap();
        assert_eq!(
            cfg.profile("prod").unwrap().destination.r#type.as_deref(),
            Some("local")
        );
    }
}
