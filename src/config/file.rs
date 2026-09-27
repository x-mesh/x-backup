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
    /// 출력 표시 설정 — `[output]`. 현재는 설명 텍스트 언어(language)만 둔다.
    #[serde(default)]
    pub output: Option<OutputSection>,
}

/// `[output]` 섹션 — 출력 표시 설정.
///
/// 라벨·기술용어(checksum/id/size/…)는 언어와 무관하게 항상 영문이며, 여기서 고르는
/// `language`는 **설명·안내 문구**의 ko/en 선택에만 영향을 준다.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OutputSection {
    /// 설명/안내 텍스트 언어("en" | "ko"). 미지정 시 기본 en.
    #[serde(default)]
    pub language: Option<String>,
}

/// 단일 프로파일 — mode/source/destination/features (PRD §FR-10).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Profile {
    #[serde(default)]
    pub mode: ModeConfig,
    #[serde(default)]
    pub source: SourceConfig,
    /// 단일 백업 위치(하위호환). `destinations`(복수)가 비어 있을 때만 쓰인다.
    #[serde(default)]
    pub destination: DestinationConfig,
    /// 복수 백업 위치 — `[[profiles.<name>.destinations]]`. 비어 있지 않으면
    /// 이 목록이 우선하고 `destination`(단일)은 무시된다. 첫 항목이 primary다.
    #[serde(default)]
    pub destinations: Vec<DestinationConfig>,
    #[serde(default)]
    pub features: FeaturesConfig,
    /// 보존 정책 — `[profiles.<name>.retention]`. prune의 기본값으로 쓰인다(CLI 플래그가
    /// 없을 때). 비어 있으면 prune은 명시적 CLI 기준이 필요하다.
    #[serde(default)]
    pub retention: RetentionConfig,
    /// 생명주기 훅 — `[profiles.<name>.hooks]`. 백업/복구/prune 전후에 사용자 셸 명령을
    /// 실행한다(PRD-04). 모두 선택적이며 미지정이면 훅 없음.
    #[serde(default)]
    pub hooks: HooksConfig,
}

/// 생명주기 훅 설정 — `[profiles.<name>.hooks]`. 각 지점의 명령(셸 문자열)은 선택적이다.
///
/// `pre_*`는 게이트(비-0 종료면 작업 중단), `post_*`/`on_error`는 관측(실패해도 경고만).
/// 실행·환경변수·마스킹은 [`crate::hooks`]가 담당한다.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HooksConfig {
    /// 백업 시작 전(게이트).
    #[serde(default)]
    pub pre_backup: Option<String>,
    /// 백업 성공 후(관측).
    #[serde(default)]
    pub post_backup: Option<String>,
    /// 복구 시작 전(게이트).
    #[serde(default)]
    pub pre_restore: Option<String>,
    /// 복구 성공 후(관측).
    #[serde(default)]
    pub post_restore: Option<String>,
    /// prune 시작 전(게이트).
    #[serde(default)]
    pub pre_prune: Option<String>,
    /// prune 성공 후(관측).
    #[serde(default)]
    pub post_prune: Option<String>,
    /// 어느 단계든 실패 시(관측).
    #[serde(default)]
    pub on_error: Option<String>,
    /// 각 훅의 타임아웃(초). 미지정/0이면 기본값(60초).
    #[serde(default)]
    pub hook_timeout_secs: Option<u64>,
}

/// 보존 정책 설정 — `[profiles.<name>.retention]`. prune이 CLI 플래그가 없을 때 기본값으로
/// 사용한다. 모두 선택적이며, 지정된 규칙들의 **합집합**으로 보존한다(하나라도 보존 대상이면
/// 유지). 백업 세트(체인) 단위로 적용되어 살아있는 증분의 base는 절대 단독 삭제되지 않는다.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct RetentionConfig {
    /// 최신 풀백업 체인 N개 보존.
    #[serde(default)]
    pub keep_full: Option<u32>,
    /// 최근 D일 이내 체인 보존.
    #[serde(default)]
    pub keep_days: Option<u32>,
    /// 최신 백업 N개 보존(체인 단위로 누적 — 예: 100이면 최신 체인부터 누적 100벌까지 유지).
    #[serde(default)]
    pub keep_last: Option<u32>,
    /// 복구 보장 윈도우(일) — 지난 N일 임의 시점 복구를 보장한다(PRD-02). 윈도우 내부 체인
    /// 전부 + 윈도우 경계를 커버하는 가장 최근의 경계 base 1개를 보존한다(keep_days와 달리
    /// "생성 시각"이 아니라 "복구 가능"을 보장).
    #[serde(default)]
    pub recovery_window_days: Option<u32>,
    /// 최소 이중화 — 어떤 규칙이든 최소 M개의 완결 풀 체인은 남긴다(단일 손상 대비).
    #[serde(default)]
    pub min_redundancy: Option<u32>,
}

impl Profile {
    /// 실효 백업 위치 목록 — `destinations`(복수)가 있으면 그것, 없으면 단일
    /// `destination`을 1개짜리 목록으로. 항상 첫 항목이 primary(필수)다(FR: 멀티 dest).
    pub fn effective_destinations(&self) -> Vec<&DestinationConfig> {
        if self.destinations.is_empty() {
            vec![&self.destination]
        } else {
            self.destinations.iter().collect()
        }
    }

    /// 백업 저장소(destination)가 설정되지 않은 **endpoint 전용** 프로파일인지.
    ///
    /// `restore`/`migrate`의 `--target-profile` 대상으로만 쓰는 프로파일은 destination을 두지
    /// 않는 게 정상이다(백업 잡이 아님). `status`·`doctor`가 이런 프로파일에 destination/
    /// last-backup/encryption 점검을 적용해 오탐 경고를 내지 않도록 판별에 쓴다.
    pub fn is_endpoint_only(&self) -> bool {
        self.destinations.is_empty() && self.destination.r#type.is_none()
    }
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
    /// 백업/복구 엔진(native | mongodump). 기본 `native`(외부 도구 불필요).
    ///
    /// - `native`: 드라이버로 직접 백업/복구(데이터+인덱스+옵션, 자체 아카이브 포맷).
    /// - `mongodump`: 외부 `mongodump`/`mongorestore` 오케스트레이션(시점 일관 `--oplog` 지원).
    ///
    /// 복구는 백업이 어떤 엔진으로 만들어졌는지(manifest.archive_format)를 따라가므로,
    /// 이 값은 *새 백업*과 *증분 PITR replay 경로* 선택에만 영향을 준다.
    #[serde(default = "default_engine")]
    pub engine: String,
}

impl Default for ModeConfig {
    fn default() -> Self {
        Self {
            backup_type: default_backup_type(),
            output: default_output(),
            precheck: default_true(),
            engine: default_engine(),
        }
    }
}

/// 접속 대상 — `[profiles.<name>.source]`. 시크릿은 env 참조만 보관한다.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourceConfig {
    /// 백업 대상(prod) MongoDB URI를 **직접** 지정한다.
    ///
    /// 자격증명이 없는 연결(로컬/개발)에 편리하다. **비밀번호가 포함된 URI는
    /// 여기 쓰지 말 것** — config 파일이 유출되면 시크릿도 함께 샌다. 그런 경우
    /// [`uri_env`](Self::uri_env)로 env 참조를 쓴다(FR-10·§11). `XB_SOURCE__URI`
    /// 환경변수로도 오버라이드된다.
    #[serde(default)]
    pub uri: Option<String>,
    /// 백업 대상 MongoDB URI가 담긴 **환경변수 이름**(시크릿 평문 저장 금지).
    ///
    /// `uri`(직접)보다 우선한다 — env가 설정돼 있으면 그 값을, 비어 있으면
    /// `uri` 리터럴로 폴백한다.
    #[serde(default)]
    pub uri_env: Option<String>,
    /// 가능하면 secondary에서 백업할지 여부.
    #[serde(default)]
    pub prefer_secondary: bool,
    /// **백업 읽기 전용** 소스 URI(복제본) — primary 부하 분리(PRD-05). 지정하면 backup은
    /// 이 URI에서 읽고, `uri`/`uri_env`는 제어/메타용으로 남는다. 미지정이면 backup도
    /// `uri`/`uri_env`를 쓴다. 비밀번호가 있으면 [`read_uri_env`](Self::read_uri_env)를 쓴다.
    #[serde(default)]
    pub read_uri: Option<String>,
    /// 백업 읽기 전용 소스 URI가 담긴 **환경변수 이름**(시크릿 평문 저장 금지). `read_uri`보다
    /// 우선한다(env가 설정돼 있으면 그 값, 비었으면 `read_uri` 리터럴로 폴백).
    #[serde(default)]
    pub read_uri_env: Option<String>,
    /// MongoDB 접속(server-selection/connect) 타임아웃(초). 미지정 시 기본 5초.
    ///
    /// 이 프로파일로 실행하는 모든 명령의 MongoDB 연결(source·`--target` 모두)에 적용된다.
    /// URI에 `serverSelectionTimeoutMS`가 있으면 **URI가 우선**하며, 이 값과 다르면
    /// 경고를 출력한다(설정한 경우에만). `None`이면 기본 5초.
    #[serde(default)]
    pub connect_timeout_secs: Option<u64>,
}

/// 백업 위치 — `[profiles.<name>.destination]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DestinationConfig {
    /// 식별용 이름(여러 destination일 때 보고·`restore --from` 선택에 사용). 선택.
    #[serde(default)]
    pub name: Option<String>,
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

impl DestinationConfig {
    /// 보고·선택용 표시 이름 — `name`이 있으면 그것, 없으면 `type#idx`로 합성한다.
    pub fn label(&self, idx: usize) -> String {
        match &self.name {
            Some(n) => n.clone(),
            None => format!("{}#{idx}", self.r#type.as_deref().unwrap_or("dest")),
        }
    }
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
    /// PostgreSQL 증분(logical decoding/pgoutput) 사용 여부(기본 false).
    ///
    /// true면 PG 풀 백업이 replication slot + publication(FOR ALL TABLES)을 만들어 그 시점부터
    /// WAL을 잡고, `backup --type incr`로 변경을 캡처한다. **명시적 opt-in**인 이유: 미사용
    /// slot은 WAL을 무한 보존해 디스크를 채울 수 있고, `wal_level=logical`(재시작 필요) 전제가
    /// 있기 때문이다. Mongo 증분(oplog)에는 영향이 없다.
    #[serde(default)]
    pub pg_logical: bool,
    /// MySQL/MariaDB 증분(binlog ROW 디코드) 사용 여부(기본 false).
    ///
    /// true면 `backup --type incr`가 풀 백업이 기록한 binlog 좌표 이후의 ROW 변경을 캡처한다.
    /// **명시적 opt-in**인 이유: 서버에 `log_bin=ON`·`binlog_format=ROW`·`binlog_row_image=FULL`
    /// 전제와 REPLICATION SLAVE/CLIENT 권한이 필요하기 때문이다. Mongo/PG 증분에는 영향이 없다.
    #[serde(default)]
    pub mysql_binlog: bool,
}

impl Default for IncrementalConfig {
    fn default() -> Self {
        Self {
            interval: default_incremental_interval(),
            on_gap: default_on_gap(),
            pg_logical: false,
            mysql_binlog: false,
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
fn default_engine() -> String {
    "native".to_string()
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
    ///
    /// v2(flat/상속) 표면이면 [`normalize_v2`](crate::config::v2::normalize_v2)로 v1 nested
    /// 트리로 정규화한 뒤 역직렬화한다 — 모든 raw-TOML→Config 진입점이 이 함수를 거치도록
    /// 통일해 v2가 일부 명령에서만 동작하는 split-brain을 방지한다.
    pub fn from_toml_str(s: &str) -> Result<Self> {
        let value: toml::Value = toml::from_str(s).map_err(|e| {
            XBackupError::Config(crate::tr!(
                "failed to parse config.toml: {e}",
                "config.toml 파싱 실패: {e}"
            ))
        })?;
        let value = crate::config::v2::normalize_v2(value)?;
        value.try_into().map_err(|e| {
            XBackupError::Config(crate::tr!(
                "failed to parse config.toml: {e}",
                "config.toml 파싱 실패: {e}"
            ))
        })
    }

    /// 파일 경로에서 설정을 읽어 파싱한다.
    pub fn from_path(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path).map_err(|e| {
            XBackupError::Config(crate::tr!(
                "failed to read the config file ({}): {e}",
                "config 파일 읽기 실패({}): {e}",
                path.display()
            ))
        })?;
        Self::from_toml_str(&raw)
    }

    /// 이름으로 프로파일을 조회한다. 없으면 [`XBackupError::Config`](exit 2).
    pub fn profile(&self, name: &str) -> Result<&Profile> {
        self.profiles.get(name).ok_or_else(|| {
            XBackupError::Config(crate::tr!(
                "unknown profile: '{name}'",
                "알 수 없는 프로파일: '{name}'"
            ))
        })
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

    /// effective_destinations: destinations(복수)가 없으면 단일 destination 1개로.
    #[test]
    fn effective_destinations_falls_back_to_single() {
        let cfg = Config::from_toml_str(
            "[profiles.p.destination]\ntype = \"local\"\npath = \"/var/b\"\n",
        )
        .unwrap();
        let dests = cfg.profile("p").unwrap().effective_destinations();
        assert_eq!(dests.len(), 1);
        assert_eq!(dests[0].path.as_deref(), Some("/var/b"));
    }

    /// effective_destinations: destinations(복수)가 있으면 그 목록이 우선, 첫 항목이 primary.
    #[test]
    fn effective_destinations_prefers_array() {
        let toml = "\
[[profiles.p.destinations]]
name = \"primary\"
type = \"local\"
path = \"/var/b1\"

[[profiles.p.destinations]]
name = \"offsite\"
type = \"local\"
path = \"/var/b2\"
";
        let cfg = Config::from_toml_str(toml).unwrap();
        let dests = cfg.profile("p").unwrap().effective_destinations();
        assert_eq!(dests.len(), 2);
        assert_eq!(dests[0].name.as_deref(), Some("primary"));
        assert_eq!(dests[1].name.as_deref(), Some("offsite"));
        // 이름 없는 경우 label은 type#idx로 합성.
        let unnamed = DestinationConfig {
            name: None,
            r#type: Some("s3".to_string()),
            path: None,
            s3: None,
        };
        assert_eq!(unnamed.label(1), "s3#1");
    }

    /// is_endpoint_only: destination이 없으면(source만) endpoint 전용으로 판별한다.
    #[test]
    fn is_endpoint_only_detects_source_only_profile() {
        // source만 있는 프로파일 → endpoint 전용.
        let cfg =
            Config::from_toml_str("[profiles.dr.source]\nuri = \"mongodb://localhost:27117/db\"\n")
                .unwrap();
        assert!(cfg.profile("dr").unwrap().is_endpoint_only());

        // destination이 있으면 endpoint 전용이 아니다(backup 잡).
        let cfg2 = Config::from_toml_str(
            "[profiles.p.source]\nuri = \"mongodb://h/db\"\n\
             [profiles.p.destination]\ntype = \"local\"\npath = \"/var/b\"\n",
        )
        .unwrap();
        assert!(!cfg2.profile("p").unwrap().is_endpoint_only());

        // destinations(복수)가 있어도 backup 잡이다.
        let cfg3 = Config::from_toml_str(
            "[[profiles.p.destinations]]\ntype = \"local\"\npath = \"/var/b\"\n",
        )
        .unwrap();
        assert!(!cfg3.profile("p").unwrap().is_endpoint_only());
    }

    /// v1 nested `[profiles.<name>.hooks]`가 파싱되어 Profile.hooks로 들어간다(PRD-04).
    #[test]
    fn parses_hooks_v1_nested() {
        let toml = "[profiles.p.source]\nuri = \"mongodb://h/db\"\n\
                    [profiles.p.hooks]\npre_backup = \"echo hi\"\non_error = \"pager.sh\"\n\
                    hook_timeout_secs = 30\n";
        let cfg = Config::from_toml_str(toml).unwrap();
        let p = cfg.profile("p").unwrap();
        assert_eq!(p.hooks.pre_backup.as_deref(), Some("echo hi"));
        assert_eq!(p.hooks.on_error.as_deref(), Some("pager.sh"));
        assert_eq!(p.hooks.hook_timeout_secs, Some(30));
        // 미지정 훅은 None.
        assert!(p.hooks.post_backup.is_none());
    }
}
