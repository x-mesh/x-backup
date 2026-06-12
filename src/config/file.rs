//! config.toml의 serde 구조와 파싱(PRD §FR-10 예시 스키마 준수).
//!
//! 선택: **수동 레이어링(serde + toml)** 을 채택한다 — 리서치 권고대로 figment의
//! 마지막 릴리스가 1년 이상 정체되어 관리 리스크가 있고, 우선순위 규칙
//! (CLI > ENV > file > default)과 시크릿 env 참조를 명시적으로 제어하기 위함이다.
//!
//! 시크릿은 평문으로 담지 않는다 — `uri_env`/`credentials_env`로 env 변수명만 보관한다.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Result, XBackupError};

/// config.toml 최상위 구조.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// `--profile` 미지정 시 사용할 기본 프로파일 이름.
    #[serde(default)]
    pub default_profile: Option<String>,
    /// 이름별 프로파일 맵(`[profiles.<name>]`).
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

/// 단일 프로파일 — mode/source/destination/features (PRD §FR-10).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Profile {
    #[serde(default)]
    pub mode: ModeConfig,
    #[serde(default)]
    pub source: SourceConfig,
    #[serde(default)]
    pub destination: DestinationConfig,
    #[serde(default)]
    pub features: FeaturesConfig,
}

/// 동작 모드 — `[profiles.<name>.mode]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModeConfig {
    /// 기본 백업 유형(full | incr).
    #[serde(default = "default_backup_type")]
    pub backup_type: String,
    /// 기본 출력 모드(progress | quiet).
    #[serde(default = "default_output")]
    pub output: String,
    /// 백업 전 status 자동 선행 여부.
    #[serde(default = "default_true")]
    pub precheck: bool,
}

impl Default for ModeConfig {
    fn default() -> Self {
        Self {
            backup_type: default_backup_type(),
            output: default_output(),
            precheck: default_true(),
        }
    }
}

/// 접속 대상 — `[profiles.<name>.source]`. 시크릿은 env 참조만 보관한다.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourceConfig {
    /// MongoDB URI가 담긴 환경변수 이름(시크릿 평문 저장 금지).
    #[serde(default)]
    pub uri_env: Option<String>,
    /// 가능하면 secondary에서 백업할지 여부.
    #[serde(default)]
    pub prefer_secondary: bool,
}

/// 백업 위치 — `[profiles.<name>.destination]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DestinationConfig {
    /// 백엔드 유형(local | s3).
    #[serde(default)]
    pub r#type: Option<String>,
    /// 로컬 백엔드 경로(type = "local").
    #[serde(default)]
    pub path: Option<String>,
    /// S3 호환 백엔드 설정(type = "s3").
    #[serde(default)]
    pub s3: Option<S3Config>,
}

/// S3 호환 스토리지 설정 — `[profiles.<name>.destination.s3]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct S3Config {
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub bucket: Option<String>,
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    /// S3 자격증명이 담긴 환경변수 이름(시크릿 평문 저장 금지).
    #[serde(default)]
    pub credentials_env: Option<String>,
}

/// 기능 정의 — `[profiles.<name>.features]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FeaturesConfig {
    #[serde(default)]
    pub compression: CompressionConfig,
    #[serde(default)]
    pub encryption: EncryptionConfig,
    #[serde(default)]
    pub incremental: IncrementalConfig,
}

/// 압축 설정.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompressionConfig {
    #[serde(default = "default_compression_algorithm")]
    pub algorithm: String,
    #[serde(default = "default_compression_level")]
    pub level: i32,
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            algorithm: default_compression_algorithm(),
            level: default_compression_level(),
        }
    }
}

/// 암호화 설정. 암호화는 기본 활성(PRD §FR-5).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptionConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_encryption_algorithm")]
    pub algorithm: String,
    /// age 공개키(recipient) 파일 경로. 복호화 키는 별도 격리(§8.1).
    #[serde(default)]
    pub recipient_file: Option<String>,
}

impl Default for EncryptionConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            algorithm: default_encryption_algorithm(),
            recipient_file: None,
        }
    }
}

/// 증분 설정.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncrementalConfig {
    /// gap 경고 기준 간격(스케줄링용 아님, oplog 윈도우 대비 위험 판단용).
    #[serde(default = "default_incremental_interval")]
    pub interval: String,
    /// gap 감지 시 동작(promote_full 등).
    #[serde(default = "default_on_gap")]
    pub on_gap: String,
}

impl Default for IncrementalConfig {
    fn default() -> Self {
        Self {
            interval: default_incremental_interval(),
            on_gap: default_on_gap(),
        }
    }
}

// ── 내장 기본값(우선순위 최하단) ──
fn default_true() -> bool {
    true
}
fn default_backup_type() -> String {
    "full".to_string()
}
fn default_output() -> String {
    "progress".to_string()
}
fn default_compression_algorithm() -> String {
    "zstd".to_string()
}
/// 기본 압축 레벨. PRD §FR-10 예시(prod=10)를 따른 합리적 기본값.
fn default_compression_level() -> i32 {
    10
}
fn default_encryption_algorithm() -> String {
    "age".to_string()
}
fn default_incremental_interval() -> String {
    "15m".to_string()
}
fn default_on_gap() -> String {
    "promote_full".to_string()
}

impl Config {
    /// TOML 문자열에서 설정을 파싱한다.
    pub fn from_toml_str(s: &str) -> Result<Self> {
        toml::from_str(s).map_err(|e| XBackupError::Config(format!("config.toml 파싱 실패: {e}")))
    }

    /// 파일 경로에서 설정을 읽어 파싱한다.
    pub fn from_path(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path).map_err(|e| {
            XBackupError::Config(format!("config 파일 읽기 실패({}): {e}", path.display()))
        })?;
        Self::from_toml_str(&raw)
    }

    /// 이름으로 프로파일을 조회한다. 없으면 [`XBackupError::Config`](exit 2).
    pub fn profile(&self, name: &str) -> Result<&Profile> {
        self.profiles
            .get(name)
            .ok_or_else(|| XBackupError::Config(format!("알 수 없는 프로파일: '{name}'")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
default_profile = "prod"

[profiles.prod.mode]
backup_type = "full"
output      = "progress"
precheck    = true

[profiles.prod.source]
uri_env          = "MONGO_URI"
prefer_secondary = true

[profiles.prod.destination]
type = "s3"

[profiles.prod.destination.s3]
endpoint        = "https://s3.example.com"
bucket          = "db-backups"
prefix          = "mongo/prod"
region          = "ap-northeast-2"
credentials_env = "S3_CREDS"

[profiles.prod.features.compression]
algorithm = "zstd"
level     = 10

[profiles.prod.features.encryption]
enabled        = true
algorithm      = "age"
recipient_file = "/etc/x-backup/age.pub"

[profiles.prod.features.incremental]
interval = "15m"
on_gap   = "promote_full"
"#;

    #[test]
    fn parses_prd_example_schema() {
        let cfg = Config::from_toml_str(SAMPLE).expect("파싱 성공해야 함");
        assert_eq!(cfg.default_profile.as_deref(), Some("prod"));
        let prod = cfg.profile("prod").expect("prod 프로파일 존재");
        assert_eq!(prod.source.uri_env.as_deref(), Some("MONGO_URI"));
        assert!(prod.source.prefer_secondary);
        assert_eq!(prod.destination.r#type.as_deref(), Some("s3"));
        let s3 = prod.destination.s3.as_ref().expect("s3 블록 존재");
        assert_eq!(s3.bucket.as_deref(), Some("db-backups"));
        assert_eq!(s3.credentials_env.as_deref(), Some("S3_CREDS"));
        assert_eq!(prod.features.compression.level, 10);
        assert!(prod.features.encryption.enabled);
        assert_eq!(prod.features.incremental.on_gap, "promote_full");
    }

    #[test]
    fn unknown_profile_is_config_error() {
        let cfg = Config::from_toml_str(SAMPLE).unwrap();
        let err = cfg.profile("nope").unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn defaults_apply_to_minimal_profile() {
        // 거의 빈 프로파일 — 기본값이 채워져야 한다(우선순위 최하단).
        let cfg = Config::from_toml_str("[profiles.p.source]\nuri_env = \"M\"\n").unwrap();
        let p = cfg.profile("p").unwrap();
        assert_eq!(p.mode.backup_type, "full");
        assert_eq!(p.mode.output, "progress");
        assert!(p.mode.precheck);
        assert!(p.features.encryption.enabled);
        assert_eq!(p.features.compression.algorithm, "zstd");
    }
}
