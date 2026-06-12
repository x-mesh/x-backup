//! 설정 레이어 병합 — 최종 [`ResolvedConfig`]를 만든다.
//!
//! 병합 순서(PRD §FR-10 우선순위 CLI > ENV > file > default):
//! 1. **default**: serde `#[serde(default)]`가 미지정 필드를 채운다(우선순위 최하단).
//! 2. **file**: config.toml을 toml::Value 트리로 로드(없으면 빈 테이블).
//! 3. **ENV**: `XB_` 오버라이드를 선택된 프로파일 테이블에 적용(file 위에 덮어씀).
//! 4. **CLI**: 상위 호출자가 [`ResolvedConfig`]의 필드를 직접 덮어쓴다(최우선).
//!
//! 시크릿 *값*은 여기서 트리에 넣지 않는다 — `uri_env`로 env에서 읽어 [`Secret`]에 담는다.

use crate::config::env::{apply_overrides, resolve_secret_with};
use crate::config::file::Profile;
use crate::config::secret::Secret;
use crate::error::{Result, XBackupError};

/// 모든 레이어를 병합해 해석한 단일 프로파일 설정.
#[derive(Debug)]
pub struct ResolvedConfig {
    /// 해석에 사용한 프로파일 이름.
    pub profile_name: String,
    /// 병합된 프로파일 값(default + file + ENV).
    pub profile: Profile,
    /// `uri_env`로 해석된 MongoDB URI 시크릿(있을 때만).
    pub resolved_uri: Option<Secret>,
}

/// 병합 입력. CLI 레이어는 호출자가 결과에 적용하므로 여기서는 file/ENV만 받는다.
pub struct MergeInput<'a> {
    /// config.toml 원문(없으면 `None` — ENV만으로 구성 가능).
    pub config_toml: Option<&'a str>,
    /// 사용할 프로파일 이름(CLI `--profile` 또는 default_profile).
    pub profile_name: &'a str,
    /// 적용할 `XB_` 오버라이드(점 경로, 값).
    pub overrides: &'a [(String, String)],
}

impl ResolvedConfig {
    /// 프로세스 환경에서 시크릿을 읽어 설정을 병합한다.
    pub fn build(input: MergeInput<'_>) -> Result<Self> {
        Self::build_with(input, |name| std::env::var(name).ok())
    }

    /// 시크릿 lookup을 주입할 수 있는 병합 진입점(테스트용).
    pub fn build_with<F>(input: MergeInput<'_>, secret_lookup: F) -> Result<Self>
    where
        F: Fn(&str) -> Option<String>,
    {
        // 1) file 레이어를 toml::Value로 로드(없으면 빈 테이블).
        let mut root: toml::Value = match input.config_toml {
            Some(raw) => toml::from_str(raw)
                .map_err(|e| XBackupError::Config(format!("config.toml 파싱 실패: {e}")))?,
            None => toml::Value::Table(toml::value::Table::new()),
        };

        // 2) 선택된 프로파일 하위 테이블을 확보한다(없으면 생성 — ENV만 구성 허용).
        let profile_value = profile_subtree(&mut root, input.profile_name)?;

        // 3) ENV 오버라이드를 프로파일 테이블에 적용(file 위에 덮어씀).
        apply_overrides(profile_value, input.overrides)?;

        // 4) serde로 역직렬화 — 미지정 필드는 default가 채운다(우선순위 최하단).
        let profile: Profile = profile_value.clone().try_into().map_err(|e| {
            XBackupError::Config(format!(
                "프로파일 '{}' 역직렬화 실패: {e}",
                input.profile_name
            ))
        })?;

        // 5) uri_env가 있으면 시크릿을 해석한다.
        let resolved_uri = match profile.source.uri_env.as_deref() {
            Some(env_name) => Some(resolve_secret_with(env_name, &secret_lookup)?),
            None => None,
        };

        Ok(Self {
            profile_name: input.profile_name.to_string(),
            profile,
            resolved_uri,
        })
    }
}

/// `root` 안에서 `[profiles.<name>]` 하위 테이블의 가변 참조를 얻는다(없으면 생성).
fn profile_subtree<'a>(root: &'a mut toml::Value, name: &str) -> Result<&'a mut toml::Value> {
    let table = root.as_table_mut().ok_or_else(|| {
        XBackupError::Config("config.toml 최상위가 테이블이 아닙니다".to_string())
    })?;

    let profiles = table
        .entry("profiles".to_string())
        .or_insert_with(|| toml::Value::Table(toml::value::Table::new()));

    let profiles_table = profiles
        .as_table_mut()
        .ok_or_else(|| XBackupError::Config("'profiles'가 테이블이 아닙니다".to_string()))?;

    Ok(profiles_table
        .entry(name.to_string())
        .or_insert_with(|| toml::Value::Table(toml::value::Table::new())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::env::collect_overrides;

    fn lookup(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    }

    const CONFIG: &str = r#"
default_profile = "prod"
[profiles.prod.source]
uri_env = "MONGO_URI"
[profiles.prod.destination.s3]
bucket = "db-backups"
"#;

    #[test]
    fn merges_file_and_resolves_uri_secret() {
        let cfg = ResolvedConfig::build_with(
            MergeInput {
                config_toml: Some(CONFIG),
                profile_name: "prod",
                overrides: &[],
            },
            lookup(&[("MONGO_URI", "mongodb://host/db")]),
        )
        .unwrap();

        assert_eq!(cfg.profile.source.uri_env.as_deref(), Some("MONGO_URI"));
        assert_eq!(
            cfg.profile
                .destination
                .s3
                .as_ref()
                .unwrap()
                .bucket
                .as_deref(),
            Some("db-backups")
        );
        assert_eq!(cfg.resolved_uri.unwrap().expose(), "mongodb://host/db");
    }

    #[test]
    fn env_override_beats_file() {
        // file은 bucket=db-backups, ENV가 staging으로 덮어써야 한다(ENV > file).
        let overrides = collect_overrides([("XB_DESTINATION__S3__BUCKET", "db-backups-staging")]);
        let cfg = ResolvedConfig::build_with(
            MergeInput {
                config_toml: Some(CONFIG),
                profile_name: "prod",
                overrides: &overrides,
            },
            lookup(&[("MONGO_URI", "mongodb://host/db")]),
        )
        .unwrap();
        assert_eq!(
            cfg.profile.destination.s3.unwrap().bucket.as_deref(),
            Some("db-backups-staging")
        );
    }

    #[test]
    fn config_file_absent_env_only_minimal() {
        // config.toml 없이 ENV만으로 최소 구성이 가능해야 한다(PRD §FR-10).
        let overrides = collect_overrides([
            ("XB_SOURCE__URI_ENV", "MONGO_URI"),
            ("XB_DESTINATION__TYPE", "local"),
            ("XB_DESTINATION__PATH", "/var/backups/mongo"),
        ]);
        let cfg = ResolvedConfig::build_with(
            MergeInput {
                config_toml: None,
                profile_name: "default",
                overrides: &overrides,
            },
            lookup(&[("MONGO_URI", "mongodb://host/db")]),
        )
        .unwrap();
        assert_eq!(cfg.profile.source.uri_env.as_deref(), Some("MONGO_URI"));
        assert_eq!(cfg.profile.destination.r#type.as_deref(), Some("local"));
        assert_eq!(cfg.resolved_uri.unwrap().expose(), "mongodb://host/db");
        // 기본값이 채워졌는지(우선순위 최하단).
        assert!(cfg.profile.features.encryption.enabled);
        assert_eq!(cfg.profile.mode.backup_type, "full");
    }

    #[test]
    fn missing_secret_env_is_config_error() {
        let err = ResolvedConfig::build_with(
            MergeInput {
                config_toml: Some(CONFIG),
                profile_name: "prod",
                overrides: &[],
            },
            lookup(&[]), // MONGO_URI 미설정
        )
        .unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }
}
