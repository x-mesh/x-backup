//! 설정 레이어 병합 — 최종 [`ResolvedConfig`]를 만든다.
//!
//! 병합 순서(PRD §FR-10 우선순위 CLI > ENV > file > default):
//! 1. **default**: serde `#[serde(default)]`가 미지정 필드를 채운다(우선순위 최하단).
//! 2. **file**: config.toml을 toml::Value 트리로 로드(없으면 빈 테이블).
//! 3. **ENV**: `XB_` 오버라이드를 선택된 프로파일 테이블에 적용(file 위에 덮어씀).
//! 4. **CLI**: 상위 호출자가 [`ResolvedConfig`]의 필드를 직접 덮어쓴다(최우선).
//!
//! 시크릿 *값*은 여기서 트리에 넣지 않는다 — `uri_env`로 env에서 읽어 [`Secret`]에 담는다.

use crate::config::env::apply_overrides;
use crate::config::file::{Profile, SourceConfig};
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
    /// 백업 읽기 전용 소스(복제본) URI 시크릿 — `read_uri_env`/`read_uri`로 해석(PRD-05).
    /// 미지정이면 `None`(backup은 `resolved_uri`로 폴백).
    pub resolved_read_uri: Option<Secret>,
}

impl ResolvedConfig {
    /// backup이 읽을 실효 소스 — 복제본(`read_uri`)이 있으면 그것, 없으면 주 소스(`uri`).
    /// restore/status/migrate 등 제어 경로는 `resolved_uri`(주 소스)를 계속 쓴다.
    pub fn effective_read_uri(&self) -> Option<&Secret> {
        self.resolved_read_uri
            .as_ref()
            .or(self.resolved_uri.as_ref())
    }
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
        // 1) file 레이어를 toml::Value로 로드(없으면 빈 테이블). v2 표면이면 v1 nested로 정규화한다
        //    — ENV 오버라이드(아래 3단계)가 정규화된 nested 트리에 정확히 적용되도록 이 시점에서 한다.
        let mut root: toml::Value = match input.config_toml {
            Some(raw) => {
                let value: toml::Value = toml::from_str(raw)
                    .map_err(|e| XBackupError::Config(format!("config.toml 파싱 실패: {e}")))?;
                crate::config::v2::normalize_v2(value)?
            }
            None => toml::Value::Table(toml::value::Table::new()),
        };

        // 1.5) 프로파일 이름 확정 — 빈 이름(예: XB_PROFILE="" 잔재)이면 config의
        //      default_profile로 폴백한다. 그래야 다중 프로파일 워크스페이스에서 plain 명령이
        //      기본 프로파일로 동작하고, "프로파일 ''" 같은 혼란스러운 에러를 막는다
        //      (list/verify의 default_profile 폴백과 일관). 둘 다 없으면 명확히 거부한다.
        let effective_name: String = if input.profile_name.is_empty() {
            root.get("default_profile")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .ok_or_else(|| {
                    XBackupError::Config(
                        "사용할 프로파일이 없습니다 — --profile/XB_PROFILE 또는 config의 \
                         default_profile을 지정하세요"
                            .to_string(),
                    )
                })?
        } else {
            input.profile_name.to_string()
        };

        // 2) 선택된 프로파일 하위 테이블을 확보한다(없으면 생성 — ENV만 구성 허용).
        let profile_value = profile_subtree(&mut root, &effective_name)?;

        // 3) ENV 오버라이드를 프로파일 테이블에 적용(file 위에 덮어씀).
        apply_overrides(profile_value, input.overrides)?;

        // 4) serde로 역직렬화 — 미지정 필드는 default가 채운다(우선순위 최하단).
        let profile: Profile = profile_value.clone().try_into().map_err(|e| {
            XBackupError::Config(format!("프로파일 '{effective_name}' 역직렬화 실패: {e}"))
        })?;

        // 5) source URI 해석 — 우선순위: uri_env(env 값) > uri(직접 리터럴).
        //    uri_env가 가리키는 env가 설정돼 있으면 그 값을, 비었거나 없으면 직접 uri로
        //    폴백한다. 둘 다 없으면 None(URI가 필요한 핸들러가 이후 명확히 거부).
        let resolved_uri = resolve_source_uri(&profile.source, &secret_lookup)?;
        // 백업 읽기 전용(복제본) URI — read_uri_env > read_uri. 둘 다 없으면 None(폴백).
        //
        // read 소스는 **backup 전용·옵셔널**이므로 해석 실패를 하드 에러로 전파하지 않는다.
        // 예: read_uri_env만 있고 그 env가 이 환경에 없으면, read 소스를 쓰지도 않는
        // status/list/restore/prune까지 동반 실패한다. 실패는 None으로 강등하고 경고만 남긴다
        // (backup은 effective_read_uri로 주 소스에 폴백; 운영자는 경고로 인지).
        let resolved_read_uri = match resolve_uri_pair(
            profile.source.read_uri_env.as_deref(),
            profile.source.read_uri.as_deref(),
            &secret_lookup,
        ) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("read 소스 URI 해석 실패(backup 시에만 필요 — 주 소스로 폴백): {e}");
                None
            }
        };

        Ok(Self {
            profile_name: effective_name,
            profile,
            resolved_uri,
            resolved_read_uri,
        })
    }
}

/// source URI를 해석한다 — `uri_env`(env 값) > `uri`(직접 리터럴) 우선순위.
///
/// - `uri_env`가 가리키는 env가 설정·비어있지 않으면 그 값.
/// - 그렇지 않고 직접 `uri`가 있으면 그 값(env 미설정 시 폴백).
/// - `uri_env`만 있고 env도 `uri`도 없으면 명확한 설정 오류.
/// - 둘 다 없으면 `None`(URI가 필요한 핸들러가 이후 거부).
fn resolve_source_uri<F>(source: &SourceConfig, lookup: &F) -> Result<Option<Secret>>
where
    F: Fn(&str) -> Option<String>,
{
    resolve_uri_pair(source.uri_env.as_deref(), source.uri.as_deref(), lookup)
}

/// `env_name`(env 값) > `literal`(직접) 우선순위로 URI 시크릿을 해석한다.
///
/// - `env_name`이 가리키는 env가 설정·비어있지 않으면 그 값.
/// - 그렇지 않고 `literal`이 있으면 그 값(env 미설정 폴백).
/// - `env_name`만 있고 env도 `literal`도 없으면 명확한 설정 오류.
/// - 둘 다 없으면 `None`. source `uri`/`uri_env`와 read 소스 `read_uri`/`read_uri_env`가
///   공유한다.
fn resolve_uri_pair<F>(
    env_name: Option<&str>,
    literal: Option<&str>,
    lookup: &F,
) -> Result<Option<Secret>>
where
    F: Fn(&str) -> Option<String>,
{
    if let Some(env_name) = env_name {
        match lookup(env_name) {
            Some(v) if !v.is_empty() => return Ok(Some(Secret::new(v))),
            _ => {
                if let Some(uri) = literal.filter(|s| !s.is_empty()) {
                    return Ok(Some(Secret::new(uri.to_string())));
                }
                return Err(XBackupError::Config(format!(
                    "시크릿 환경변수 '{env_name}'가 설정되지 않았습니다(또는 직접 URI로 지정)"
                )));
            }
        }
    }
    Ok(literal
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .map(Secret::new))
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

    /// source.uri를 직접 쓰면 env 없이도 해석된다(개발/무자격증명용).
    #[test]
    fn direct_uri_resolves_without_env() {
        let toml = "[profiles.p.source]\nuri = \"mongodb://localhost:27017/?replicaSet=rs0\"\n";
        let cfg = ResolvedConfig::build_with(
            MergeInput {
                config_toml: Some(toml),
                profile_name: "p",
                overrides: &[],
            },
            lookup(&[]),
        )
        .unwrap();
        assert_eq!(
            cfg.resolved_uri.unwrap().expose(),
            "mongodb://localhost:27017/?replicaSet=rs0"
        );
    }

    /// 빈 프로파일 이름(예: XB_PROFILE="" 잔재)은 config의 default_profile로 폴백한다.
    #[test]
    fn empty_profile_falls_back_to_default_profile() {
        let toml = "default_profile = \"mongo\"\n\
                    [profiles.mongo.source]\nuri = \"mongodb://localhost:27017/db\"\n";
        let cfg = ResolvedConfig::build_with(
            MergeInput {
                config_toml: Some(toml),
                profile_name: "",
                overrides: &[],
            },
            lookup(&[]),
        )
        .unwrap();
        assert_eq!(cfg.profile_name, "mongo");
        assert_eq!(
            cfg.resolved_uri.unwrap().expose(),
            "mongodb://localhost:27017/db"
        );
    }

    /// 빈 프로파일 + default_profile도 없으면 명확한 설정 오류(빈 테이블 생성으로 폴백하지 않음).
    #[test]
    fn empty_profile_without_default_is_error() {
        let toml = "[profiles.mongo.source]\nuri = \"mongodb://localhost/db\"\n";
        let err = ResolvedConfig::build_with(
            MergeInput {
                config_toml: Some(toml),
                profile_name: "",
                overrides: &[],
            },
            lookup(&[]),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("프로파일이 없습니다"),
            "기대한 폴백 에러가 아님: {err}"
        );
    }

    /// uri_env가 가리키는 env가 설정돼 있으면 직접 uri보다 우선한다(ENV > 리터럴).
    #[test]
    fn uri_env_beats_direct_uri_when_set() {
        let toml = "[profiles.p.source]\nuri = \"mongodb://literal/db\"\nuri_env = \"MONGO_URI\"\n";
        let cfg = ResolvedConfig::build_with(
            MergeInput {
                config_toml: Some(toml),
                profile_name: "p",
                overrides: &[],
            },
            lookup(&[("MONGO_URI", "mongodb://from-env/db")]),
        )
        .unwrap();
        assert_eq!(cfg.resolved_uri.unwrap().expose(), "mongodb://from-env/db");
    }

    /// uri_env가 있으나 env 미설정이면 직접 uri로 폴백한다.
    #[test]
    fn uri_env_unset_falls_back_to_direct_uri() {
        let toml = "[profiles.p.source]\nuri = \"mongodb://literal/db\"\nuri_env = \"MONGO_URI\"\n";
        let cfg = ResolvedConfig::build_with(
            MergeInput {
                config_toml: Some(toml),
                profile_name: "p",
                overrides: &[],
            },
            lookup(&[]), // MONGO_URI 미설정 → uri 리터럴로 폴백
        )
        .unwrap();
        assert_eq!(cfg.resolved_uri.unwrap().expose(), "mongodb://literal/db");
    }

    /// read_uri(복제본)가 해석되어 effective_read_uri가 그것을, 주 소스는 primary를 유지한다.
    #[test]
    fn read_uri_resolves_and_is_effective() {
        let toml = "[profiles.p.source]\nuri = \"mongodb://primary/db\"\n\
                    read_uri = \"mongodb://replica/db\"\n";
        let cfg = ResolvedConfig::build_with(
            MergeInput {
                config_toml: Some(toml),
                profile_name: "p",
                overrides: &[],
            },
            lookup(&[]),
        )
        .unwrap();
        assert_eq!(
            cfg.resolved_read_uri.as_ref().unwrap().expose(),
            "mongodb://replica/db"
        );
        assert_eq!(
            cfg.effective_read_uri().unwrap().expose(),
            "mongodb://replica/db"
        );
        // 제어 경로가 쓰는 주 소스는 그대로 primary.
        assert_eq!(cfg.resolved_uri.unwrap().expose(), "mongodb://primary/db");
    }

    /// read_uri_env(env 값)가 read_uri 리터럴보다 우선한다.
    #[test]
    fn read_uri_env_beats_literal() {
        let toml = "[profiles.p.source]\nuri = \"mongodb://primary/db\"\n\
                    read_uri = \"mongodb://lit/db\"\nread_uri_env = \"READ_URI\"\n";
        let cfg = ResolvedConfig::build_with(
            MergeInput {
                config_toml: Some(toml),
                profile_name: "p",
                overrides: &[],
            },
            lookup(&[("READ_URI", "mongodb://from-env/db")]),
        )
        .unwrap();
        assert_eq!(
            cfg.resolved_read_uri.unwrap().expose(),
            "mongodb://from-env/db"
        );
    }

    /// read_uri 미지정이면 effective_read_uri는 주 소스로 폴백한다(복제본 없음).
    #[test]
    fn no_read_uri_falls_back_to_source() {
        let toml = "[profiles.p.source]\nuri = \"mongodb://primary/db\"\n";
        let cfg = ResolvedConfig::build_with(
            MergeInput {
                config_toml: Some(toml),
                profile_name: "p",
                overrides: &[],
            },
            lookup(&[]),
        )
        .unwrap();
        assert!(cfg.resolved_read_uri.is_none());
        assert_eq!(
            cfg.effective_read_uri().unwrap().expose(),
            "mongodb://primary/db"
        );
    }

    /// uri도 uri_env도 없으면 resolved_uri는 None(이후 핸들러가 거부).
    #[test]
    fn no_source_uri_yields_none() {
        let toml = "[profiles.p.destination]\ntype = \"local\"\npath = \"/tmp/x\"\n";
        let cfg = ResolvedConfig::build_with(
            MergeInput {
                config_toml: Some(toml),
                profile_name: "p",
                overrides: &[],
            },
            lookup(&[]),
        )
        .unwrap();
        assert!(cfg.resolved_uri.is_none());
    }
}
