//! 환경변수(ENV) 처리 — 두 용도(PRD §FR-10).
//!
//! 1. **설정 오버라이드(12-factor):** `XB_` 접두사 + `__` 구분자로 중첩 키를 평탄화한다.
//!    예) `XB_DESTINATION__S3__BUCKET=db-backups-staging` → `destination.s3.bucket`.
//!    프로파일 컨텍스트 안의 값을 덮어쓰므로 config 파일 없이 ENV만으로도 구성 가능하다.
//! 2. **시크릿 참조:** config는 시크릿 *값*이 아니라 *env 변수명*(`uri_env`,
//!    `credentials_env`)만 담는다. 실행 시 해당 env에서 시크릿을 읽어 [`Secret`]에 담는다.
//!
//! 오버라이드는 toml::Value 트리를 직접 변형하는 방식으로 적용한다 — 수동 레이어링에서
//! 우선순위(ENV > file)를 가장 명확하게 표현하기 위함이다.

use crate::config::secret::Secret;
use crate::error::{Result, XBackupError};

/// 오버라이드 ENV의 접두사.
pub const ENV_PREFIX: &str = "XB_";
/// 중첩 키 구분자(`destination__s3__bucket`).
pub const ENV_SEPARATOR: &str = "__";

/// `(key, value)` 쌍 목록에서 `XB_` 오버라이드를 추출해 점 경로로 변환한다.
///
/// 반환: `("destination.s3.bucket", "db-backups-staging")` 형태의 목록.
/// 추출만 하고 적용은 [`apply_overrides`]가 담당한다(테스트 용이성).
pub fn collect_overrides<I, K, V>(vars: I) -> Vec<(String, String)>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    vars.into_iter()
        .filter_map(|(k, v)| {
            let key = k.as_ref();
            let rest = key.strip_prefix(ENV_PREFIX)?;
            // 빈 키(`XB_`만)는 무시.
            if rest.is_empty() {
                return None;
            }
            // `__`를 점으로, 키는 소문자 경로로 정규화.
            let path = rest
                .split(ENV_SEPARATOR)
                .map(|seg| seg.to_ascii_lowercase())
                .collect::<Vec<_>>()
                .join(".");
            Some((path, v.as_ref().to_string()))
        })
        .collect()
}

/// 현재 프로세스 환경에서 `XB_` 오버라이드를 수집한다.
pub fn collect_overrides_from_process() -> Vec<(String, String)> {
    collect_overrides(std::env::vars())
}

/// toml::Value(테이블)에 점 경로 오버라이드를 적용한다.
///
/// 중간 경로가 없으면 테이블을 생성한다(config 파일 없이 ENV만으로 구성 가능).
/// 값은 문자열로 삽입한다 — 타입 강제는 이후 serde 역직렬화 단계가 수행한다.
pub fn apply_overrides(root: &mut toml::Value, overrides: &[(String, String)]) -> Result<()> {
    for (path, value) in overrides {
        let segments: Vec<&str> = path.split('.').collect();
        insert_at_path(root, &segments, value)?;
    }
    Ok(())
}

/// 점 경로 세그먼트를 따라 내려가며 마지막 위치에 값을 삽입한다.
fn insert_at_path(node: &mut toml::Value, segments: &[&str], value: &str) -> Result<()> {
    let (head, tail) = match segments.split_first() {
        Some(parts) => parts,
        None => return Ok(()),
    };

    // 현재 노드가 테이블이 아니면 오버라이드를 안전히 적용할 수 없다.
    let table = node.as_table_mut().ok_or_else(|| {
        XBackupError::Config(crate::tr!(
            "ENV override path conflict: '{head}' is not a table there",
            "ENV 오버라이드 경로 충돌: '{head}' 위치가 테이블이 아닙니다"
        ))
    })?;

    if tail.is_empty() {
        table.insert(head.to_string(), parse_scalar(value));
    } else {
        let child = table
            .entry(head.to_string())
            .or_insert_with(|| toml::Value::Table(toml::value::Table::new()));
        insert_at_path(child, tail, value)?;
    }
    Ok(())
}

/// ENV 문자열을 적절한 스칼라로 해석한다(bool/integer는 토글·레벨 오버라이드 편의).
fn parse_scalar(value: &str) -> toml::Value {
    if let Ok(b) = value.parse::<bool>() {
        return toml::Value::Boolean(b);
    }
    if let Ok(i) = value.parse::<i64>() {
        return toml::Value::Integer(i);
    }
    toml::Value::String(value.to_string())
}

/// env 변수명으로 시크릿을 해석해 [`Secret`]에 담는다.
///
/// 변수가 없거나 비어 있으면 [`XBackupError::Config`](exit 2) — 시크릿 미설정은 설정 오류다.
pub fn resolve_secret(env_name: &str) -> Result<Secret> {
    resolve_secret_with(env_name, |name| std::env::var(name).ok())
}

/// [`resolve_secret`]의 주입 가능 버전 — 테스트에서 lookup 함수를 교체한다.
pub fn resolve_secret_with<F>(env_name: &str, lookup: F) -> Result<Secret>
where
    F: Fn(&str) -> Option<String>,
{
    match lookup(env_name) {
        Some(v) if !v.is_empty() => Ok(Secret::new(v)),
        Some(_) => Err(XBackupError::Config(crate::tr!(
            "secret environment variable '{env_name}' is empty",
            "시크릿 환경변수 '{env_name}'가 비어 있습니다"
        ))),
        None => Err(XBackupError::Config(crate::tr!(
            "secret environment variable '{env_name}' is not set",
            "시크릿 환경변수 '{env_name}'가 설정되지 않았습니다"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_flattens_prefix_and_separator() {
        let vars = [
            ("XB_DESTINATION__S3__BUCKET", "db-backups-staging"),
            ("XB_MODE__OUTPUT", "quiet"),
            ("PATH", "/usr/bin"), // 접두사 없음 → 무시
            ("XB_", "ignored"),   // 빈 키 → 무시
        ];
        let mut got = collect_overrides(vars);
        got.sort();
        assert_eq!(
            got,
            vec![
                (
                    "destination.s3.bucket".to_string(),
                    "db-backups-staging".to_string()
                ),
                ("mode.output".to_string(), "quiet".to_string()),
            ]
        );
    }

    #[test]
    fn apply_overrides_overwrites_existing_value() {
        let mut root: toml::Value =
            toml::from_str("[destination.s3]\nbucket = \"db-backups\"\n").unwrap();
        let overrides = collect_overrides([("XB_DESTINATION__S3__BUCKET", "staging")]);
        apply_overrides(&mut root, &overrides).unwrap();
        let bucket = root["destination"]["s3"]["bucket"].as_str().unwrap();
        assert_eq!(bucket, "staging");
    }

    #[test]
    fn apply_overrides_creates_missing_path() {
        // config 파일 없이 ENV만으로 구성 — 빈 테이블에서 시작.
        let mut root = toml::Value::Table(toml::value::Table::new());
        let overrides = collect_overrides([("XB_SOURCE__URI_ENV", "MONGO_URI")]);
        apply_overrides(&mut root, &overrides).unwrap();
        assert_eq!(root["source"]["uri_env"].as_str().unwrap(), "MONGO_URI");
    }

    #[test]
    fn override_parses_bool_and_int() {
        let mut root = toml::Value::Table(toml::value::Table::new());
        let overrides = collect_overrides([
            ("XB_MODE__PRECHECK", "false"),
            ("XB_FEATURES__COMPRESSION__LEVEL", "3"),
        ]);
        apply_overrides(&mut root, &overrides).unwrap();
        assert_eq!(root["mode"]["precheck"].as_bool(), Some(false));
        assert_eq!(
            root["features"]["compression"]["level"].as_integer(),
            Some(3)
        );
    }

    #[test]
    fn resolve_secret_reads_from_lookup() {
        let secret = resolve_secret_with("MONGO_URI", |name| {
            (name == "MONGO_URI").then(|| "mongodb://host/db".to_string())
        })
        .unwrap();
        assert_eq!(secret.expose(), "mongodb://host/db");
    }

    #[test]
    fn resolve_secret_missing_is_config_error() {
        let err = resolve_secret_with("ABSENT", |_| None).unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn resolve_secret_empty_is_config_error() {
        let err = resolve_secret_with("EMPTY", |_| Some(String::new())).unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }
}
