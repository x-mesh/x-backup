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

/// 환경변수 이름으로 유효한지 — 첫 글자는 알파벳/`_`, 이후는 영숫자/`_`(POSIX 관례).
///
/// 사용자가 변수명 칸에 URI 값(`mongodb://...`)을 붙여 넣는 흔한 실수를 잡는다 —
/// URI에는 `:`/`/`/`@`/`.` 등 변수명에 못 쓰는 문자가 있어 자연히 걸러진다.
fn is_valid_env_name(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// 접속 정보를 묻는다 — 반환은 `(uri, uri_env)`(둘 중 하나만 Some).
///
/// 두 가지 입력을 모두 정식으로 받는다:
/// - **환경변수 이름**(권장, 기본 `MONGO_URI`) → `uri_env`로 보관(시크릿 평문 미저장, FR-10).
/// - **URI 직접 입력**(`mongodb://...`/`mongodb+srv://...`) → `source.uri`로 저장. 자격증명이
///   포함되면 config에 평문으로 남으므로 로컬/개발용이다 — 그 자리에서 한 줄 경고만 띄우고
///   추가 확인 없이 받는다(URI를 친 건 의도이므로). 양끝 따옴표/공백은 자동 정리한다.
///
/// 둘 다 아닌 입력(오타 등)은 다시 묻는다 — 변수명 칸에 잘못 친 값이 그대로 *변수 이름*으로
/// 저장돼 백업/status가 "환경변수가 설정되지 않았습니다"로 실패하던 함정을 막는다.
fn ask_connection(prompt: &mut dyn Prompt) -> Result<(Option<String>, Option<String>)> {
    loop {
        let answer = ask_default(
            prompt,
            "DB 접속 — URI가 담긴 환경변수 이름(권장, 시크릿 미저장) 또는 URI 직접 입력 \
             (mongodb:// · postgres:// · mysql:// — 스킴으로 엔진 자동 판별)",
            "DB_URI",
        )?;
        // 붙여넣기 실수 대비 양끝 따옴표/공백 정리.
        let cleaned = answer.trim().trim_matches(['"', '\'']).trim().to_string();

        // URI 직접 입력("://" 포함) — source.uri로 평문 저장(로컬/개발 전용).
        if cleaned.contains("://") {
            eprintln!(
                "  ! URI를 config에 평문으로 저장합니다(자격증명 포함 시 로컬/개발 전용). \
                 운영에선 환경변수 이름을 쓰세요."
            );
            return Ok((Some(cleaned), None));
        }
        // 환경변수 이름 — uri_env로 보관(시크릿 평문 미저장).
        if is_valid_env_name(&cleaned) {
            return Ok((None, Some(cleaned)));
        }
        // 둘 다 아님(오타 등) — 안내 후 다시 묻는다.
        prompt.read_line(
            "  ! 환경변수 이름(예: DB_URI) 또는 URI(mongodb:// · postgres:// · mysql://)를 입력하세요. Enter로 다시 입력",
        )?;
    }
}

/// y/n 질문. 빈 응답이면 `default`를 쓴다.
pub(crate) fn ask_yes_no(prompt: &mut dyn Prompt, question: &str, default: bool) -> Result<bool> {
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

    // 2) 접속 — 기본은 *env 변수명*(시크릿 평문 금지). 변수명 칸에 URI를 붙여 넣는
    //    실수는 ask_connection이 잡아 다시 묻거나 직접 저장(opt-in)으로 처리한다.
    let (uri, uri_env) = ask_connection(prompt)?;
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
                name: None,
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
                name: None,
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
            engine: "native".to_string(),
        },
        source: SourceConfig {
            uri,
            uri_env,
            prefer_secondary,
            connect_timeout_secs: None,
            ..Default::default()
        },
        destination,
        destinations: Vec::new(),
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
        retention: crate::config::file::RetentionConfig::default(),
        hooks: crate::config::file::HooksConfig::default(),
    };

    let mut config = Config {
        default_profile: Some(profile_name.clone()),
        ..Config::default()
    };
    config.profiles.insert(profile_name, profile);
    Ok(config)
}

/// [`Config`]를 **v2 표면 TOML 문자열**로 직렬화한다(파일 기록용; init이 v2를 쓴다).
/// 시크릿이 없으므로 그대로 안전하다. v2 직렬화는
/// [`crate::config::v2::to_v2_string`](normalize_v2의 역연산)에 위임한다.
pub fn config_to_toml(config: &Config) -> Result<String> {
    crate::config::v2::to_v2_string(config)
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
        assert_eq!(p.source.uri_env.as_deref(), Some("DB_URI"));
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

    /// is_valid_env_name: 변수명만 통과하고 URI 값/따옴표/빈 값은 거른다.
    #[test]
    fn env_name_validation() {
        assert!(is_valid_env_name("MONGO_URI"));
        assert!(is_valid_env_name("_x9"));
        assert!(!is_valid_env_name(""));
        assert!(!is_valid_env_name("9ABC")); // 숫자로 시작
        assert!(!is_valid_env_name("mongodb://localhost:27017"));
        assert!(!is_valid_env_name("\"mongodb://admin:pw@host:27017")); // 사용자가 친 그 입력
    }

    /// URI를 직접 입력하면(따옴표 포함) 추가 확인 없이 source.uri로 저장된다
    /// (따옴표·공백 제거, 끝의 `/`는 보존). uri_env는 비어 있다.
    #[test]
    fn direct_uri_accepted_as_source_uri() {
        let answers = vec![
            "prod",                                                  // 프로파일명
            "\"mongodb://admin:adminpassword@100.100.202.71:27017/", // 변수명 칸에 붙인 URI
            "n",                                                     // prefer_secondary
            "local",                                                 // dest
            "/data",
            "10",
            "n", // 암호화 off
            "15m",
            "progress",
        ];
        let mut prompt = VecPrompt::new(answers);
        let cfg = run_wizard(&mut prompt).unwrap();
        let p = cfg.profile("prod").unwrap();
        assert_eq!(
            p.source.uri.as_deref(),
            Some("mongodb://admin:adminpassword@100.100.202.71:27017/"),
            "양끝 따옴표는 제거하되 URI(끝 `/` 포함)는 그대로 source.uri로 저장돼야 함"
        );
        assert!(
            p.source.uri_env.is_none(),
            "직접 저장이면 uri_env는 없어야 함"
        );
    }

    /// mongodb+srv URI도 직접 입력으로 인식해 source.uri에 저장한다.
    #[test]
    fn direct_srv_uri_accepted() {
        let answers = vec![
            "prod",
            "mongodb+srv://u:p@cluster.example.net/?retryWrites=true",
            "n",
            "local",
            "/data",
            "10",
            "n",
            "15m",
            "progress",
        ];
        let mut prompt = VecPrompt::new(answers);
        let cfg = run_wizard(&mut prompt).unwrap();
        let p = cfg.profile("prod").unwrap();
        assert_eq!(
            p.source.uri.as_deref(),
            Some("mongodb+srv://u:p@cluster.example.net/?retryWrites=true")
        );
        assert!(p.source.uri_env.is_none());
    }

    /// 직접 입력한 URI는 config.toml(v2)에 source.uri로 직렬화되고 다시 파싱된다
    /// (평문 저장 경로가 실제로 config에 살아남는지 — 사용자 요구의 핵심).
    #[test]
    fn direct_uri_serializes_and_reparses() {
        let answers = vec![
            "prod",
            "mongodb://admin:adminpassword@100.100.202.71:27017/",
            "n",
            "local",
            "/data",
            "10",
            "n",
            "15m",
            "progress",
        ];
        let cfg = run_wizard(&mut VecPrompt::new(answers)).unwrap();
        let toml = config_to_toml(&cfg).unwrap();
        assert!(
            toml.contains("uri = \"mongodb://admin:adminpassword@100.100.202.71:27017/\""),
            "직접 URI가 source.uri로 기록돼야 함:\n{toml}"
        );
        // 재파싱해도 동일 URI가 보존된다.
        let reparsed = Config::from_toml_str(&toml).unwrap();
        assert_eq!(
            reparsed.profile("prod").unwrap().source.uri.as_deref(),
            Some("mongodb://admin:adminpassword@100.100.202.71:27017/")
        );
    }

    /// 변수명도 URI도 아닌 입력(오타)은 다시 묻고, 이후 올바른 변수명을 uri_env로 받는다.
    #[test]
    fn neither_env_name_nor_uri_reasks() {
        let answers = vec![
            "prod",           // 프로파일명
            "mongo-uri",      // 변수명 형식 아님(대시), URI도 아님 → 재질문
            "",               // 재질문 Enter
            "PROD_MONGO_URI", // 올바른 변수명
            "n",              // prefer_secondary
            "local",
            "/data",
            "10",
            "n",
            "15m",
            "progress",
        ];
        let mut prompt = VecPrompt::new(answers);
        let cfg = run_wizard(&mut prompt).unwrap();
        let p = cfg.profile("prod").unwrap();
        assert_eq!(p.source.uri_env.as_deref(), Some("PROD_MONGO_URI"));
        assert!(p.source.uri.is_none());
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
