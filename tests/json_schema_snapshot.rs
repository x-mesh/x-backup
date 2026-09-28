//! `--json` 출력 스키마 스냅샷 — 기계 판독 출력의 **키 구조**를 고정한다.
//!
//! ## 왜 필요한가
//! x-backup의 `--json` 요약은 핸들러 안에서 `serde_json::json!({...})` 리터럴로 만들어진다.
//! 리터럴은 타입이 아니므로 컴파일러가 계약을 지켜주지 않는다 — 필드 이름을 하나 바꿔도
//! 빌드는 통과하고, 그 출력을 파싱하는 소비자(웹 콘솔)만 조용히 깨진다.
//!
//! 이 파일은 그 침묵을 없앤다. 실제 바이너리를 스폰해 나온 JSON에서 **값이 아니라 키 경로**만
//! 뽑아 기대 목록과 비교한다. 값(경로·시각·크기)은 환경마다 다르지만 키 구조는 계약이다.
//!
//! ## 왜 값이 아니라 키 경로인가
//! 값까지 고정하면 임시 디렉터리 경로·타임스탬프 때문에 매 실행 실패한다. 반대로 키만 보면
//! 이름 변경·필드 삭제·중첩 구조 변경은 전부 잡으면서 환경 차이에는 흔들리지 않는다.
//!
//! ## 커버 범위 (DB·Docker 비의존 원칙)
//! DB 없이 결정론적으로 재현 가능한 명령만 다룬다:
//! - `doctor --json` — 오프라인 config 정적 점검(연결 없음)
//! - `list --json` — 로컬 store 카탈로그(빈 store)
//! - `prune --dry-run --json` — 보존 계획(무변경)
//! - `verify --json`(+`--chain`) — 손으로 심은 백업 산출물(연결 없음)
//!
//! 살아있는 DB가 필요한 `backup`/`restore`/`status`/`peek`/`migrate`의 요약은 여기서 다루지
//! 않는다. 대신 각 핸들러 파일의 단위 테스트가 **JSON 값을 만드는 순수 함수**를 직접 불러
//! "최상위 문서에 `schema` 키가 있다"를 고정한다(`build_json`/`build_*_json` 계열).
//! `prune`의 배열 원소 키 구조도 빈 store에서 관측되지 않으므로 같은 방식으로
//! `src/cli/handlers/prune.rs`의 단위 테스트가 별도로 고정한다.
//!
//! ## `schema` 필드의 의미
//! 모든 최상위 문서의 첫 필드는 `schema`(현재 1)다. 소비자는 이 값으로 파서를 고르고,
//! 모르는 버전을 만나면 조용히 오파싱하는 대신 명확히 실패한다. 중첩 객체·배열 원소는
//! 독립 문서가 아니므로 버전을 갖지 않는다.

use std::io::Write;

use assert_cmd::Command;
use serde_json::Value;
use x_backup::manifest::schema::{
    BackupManifest, BackupStatus, BackupType, OplogRange, OplogTimestamp, Topology, FORMAT_VERSION,
};
use x_backup::manifest::store::data_path;
use x_backup::manifest::ManifestStore;
use x_backup::storage::{BoxAsyncRead, LocalFs, Storage};

/// `x-backup` 바이너리 커맨드. tracing 노이즈를 끈다(판정은 stdout JSON으로만).
fn xbackup() -> Command {
    let mut cmd = Command::cargo_bin("x-backup").expect("x-backup 바이너리 빌드 필요");
    cmd.env("RUST_LOG", "off");
    cmd
}

/// 최소 config.toml(v1 중첩 문법)을 임시 파일로 만든다. destination은 local(tempdir).
fn write_config(dest_path: &str) -> tempfile::NamedTempFile {
    let mut f = tempfile::Builder::new()
        .prefix("x-backup-schema-")
        .suffix(".toml")
        .tempfile()
        .unwrap();
    write!(
        f,
        r#"
default_profile = "p"

[profiles.p.mode]
backup_type = "full"
output      = "quiet"
precheck    = true

[profiles.p.source]
uri_env = "XB_SCHEMA_URI"

[profiles.p.destination]
type = "local"
path = "{dest_path}"

[profiles.p.features.compression]
algorithm = "zstd"
level     = 3

[profiles.p.features.encryption]
enabled = false
"#
    )
    .unwrap();
    f.flush().unwrap();
    f
}

/// JSON에서 키 경로만 뽑아 정렬해 돌려준다(값은 무시).
///
/// - 객체: `a.b` 형태로 내려간다.
/// - 배열: 첫 원소만 `a[]` 접두로 따라간다 — 동종 배열을 전제하며, 빈 배열은 원소 경로를
///   만들지 않는다(그 구조는 관측되지 않았으므로 고정하지 않는 게 정직하다).
/// - 스칼라: 경로 자체만 남긴다.
fn key_paths(v: &Value) -> Vec<String> {
    let mut out = Vec::new();
    walk(v, String::new(), &mut out);
    out.sort();
    out
}

fn walk(v: &Value, prefix: String, out: &mut Vec<String>) {
    match v {
        Value::Object(map) => {
            for (k, val) in map {
                let path = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                out.push(path.clone());
                walk(val, path, out);
            }
        }
        Value::Array(items) => {
            if let Some(first) = items.first() {
                walk(first, format!("{prefix}[]"), out);
            }
        }
        _ => {}
    }
}

/// stdout을 단일 JSON 문서로 파싱한다 — 두 덩어리로 나오면 여기서 실패한다.
fn parse_single_json(stdout: &[u8]) -> Value {
    let s = String::from_utf8(stdout.to_vec()).expect("stdout은 UTF-8이어야 한다");
    serde_json::from_str(&s)
        .unwrap_or_else(|e| panic!("stdout이 단일 JSON 문서가 아니다: {e}\n--- stdout ---\n{s}"))
}

/// 기대 키 경로와 실제를 비교하고, 차이를 사람이 읽을 수 있게 알린다.
fn assert_key_paths(actual: &[String], expected: &[&str], what: &str) {
    let expected: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
    let missing: Vec<&String> = expected.iter().filter(|e| !actual.contains(e)).collect();
    let extra: Vec<&String> = actual.iter().filter(|a| !expected.contains(a)).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "{what}의 JSON 키 구조가 바뀌었다 — 소비자(웹 콘솔) 파서가 깨진다.\n \
         사라진 키: {missing:?}\n 새로 생긴 키: {extra:?}\n \
         의도한 변경이면 이 테스트의 기대 목록과 스키마 버전을 함께 올려라."
    );
}

/// `doctor --json` — 오프라인 정적 점검. 프로파일·항목 배열이 채워지므로 원소 키까지 고정된다.
#[test]
fn doctor_json_keys_are_stable() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_config(dir.path().to_str().unwrap());

    let out = xbackup()
        .args(["--config", cfg.path().to_str().unwrap()])
        .args(["doctor", "--json"])
        .env("XB_SCHEMA_URI", "mongodb://u:p@127.0.0.1:1/?replicaSet=rs0")
        .output()
        .unwrap();

    let v = parse_single_json(&out.stdout);
    assert_key_paths(
        &key_paths(&v),
        &[
            "schema",
            "overall",
            "profiles",
            "profiles[].profile",
            "profiles[].db",
            "profiles[].items",
            "profiles[].items[].label",
            "profiles[].items[].status",
            "profiles[].items[].message",
        ],
        "doctor --json",
    );
    assert_eq!(
        v["schema"], 1,
        "스키마 버전은 소비자가 파서를 고르는 근거다"
    );
}

/// `list --json` — 빈 store. store 위치와 backups 배열이 계약이다.
#[test]
fn list_json_keys_are_stable() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_config(dir.path().to_str().unwrap());

    let out = xbackup()
        .args(["--config", cfg.path().to_str().unwrap()])
        .args(["list", "--profile", "p", "--json"])
        .env("XB_SCHEMA_URI", "mongodb://u:p@127.0.0.1:1/?replicaSet=rs0")
        .output()
        .unwrap();

    let v = parse_single_json(&out.stdout);
    assert_key_paths(
        &key_paths(&v),
        &["schema", "store", "backups"],
        "list --json",
    );
    assert_eq!(
        v["schema"], 1,
        "스키마 버전은 소비자가 파서를 고르는 근거다"
    );
    assert!(
        v["backups"].is_array(),
        "backups는 항상 배열이어야 한다(없으면 빈 배열)"
    );
}

/// `prune --dry-run --json` — 무변경. 계획·정책·결과 필드가 계약이다.
#[test]
fn prune_json_keys_are_stable() {
    let dir = tempfile::tempdir().unwrap();
    let lock_dir = tempfile::tempdir().unwrap();
    let cfg = write_config(dir.path().to_str().unwrap());

    let out = xbackup()
        .args(["--config", cfg.path().to_str().unwrap()])
        .args(["prune", "--profile", "p", "--keep-last", "3"])
        .args(["--dry-run", "--json"])
        .env("XB_SCHEMA_URI", "mongodb://u:p@127.0.0.1:1/?replicaSet=rs0")
        .env("XDG_RUNTIME_DIR", lock_dir.path())
        .output()
        .unwrap();

    let v = parse_single_json(&out.stdout);
    assert_key_paths(
        &key_paths(&v),
        &[
            "schema",
            "profile",
            "store",
            "dry_run",
            "policy",
            "policy.keep_full",
            "policy.keep_days",
            "policy.keep_last",
            "policy.recovery_window_days",
            "policy.min_redundancy",
            "targets",
            "retained_base_ids",
            "outcome",
        ],
        "prune --dry-run --json",
    );
    assert_eq!(
        v["schema"], 1,
        "스키마 버전은 소비자가 파서를 고르는 근거다"
    );
    assert!(v["outcome"].is_null(), "dry-run은 아무것도 바꾸지 않는다");
}

/// 키 경로 추출기 자체의 계약 — 이 도구가 틀리면 위 세 테스트가 조용히 무력해진다.
#[test]
fn key_paths_captures_nesting_and_arrays() {
    let v = serde_json::json!({
        "a": 1,
        "b": { "c": "x", "d": null },
        "e": [ { "f": true } ],
        "empty": []
    });
    assert_eq!(
        key_paths(&v),
        vec!["a", "b", "b.c", "b.d", "e", "e[].f", "empty"]
    );
}

/// 필드 이름이 바뀌면 비교가 실패한다 — 스냅샷이 실제로 방어하고 있음을 고정한다.
#[test]
fn renaming_a_field_is_detected() {
    let before = key_paths(&serde_json::json!({ "deleted_chains": 1 }));
    let after = key_paths(&serde_json::json!({ "deletedChains": 1 }));
    assert_ne!(before, after);

    let result = std::panic::catch_unwind(|| {
        assert_key_paths(&after, &["deleted_chains"], "테스트용");
    });
    assert!(result.is_err(), "이름이 바뀌면 반드시 실패해야 한다");
}

/// 손으로 만든 풀백업 하나를 local store에 심는다(DB·외부 도구 불필요).
///
/// 체크섬은 **저장 바이트 기준**이라 평문 payload의 sha256을 그대로 쓰면 verify가 통과한다
/// (`verify_integration.rs`와 같은 규칙).
async fn seed_full_backup(root: &std::path::Path, id: &str, payload: &[u8]) {
    use sha2::{Digest, Sha256};

    let fs = LocalFs::new(root).unwrap();
    let reader: BoxAsyncRead = Box::pin(std::io::Cursor::new(payload.to_vec()));
    fs.put_stream(&data_path(id), reader, Some(payload.len() as u64))
        .await
        .unwrap();
    let m = BackupManifest {
        format_version: FORMAT_VERSION,
        id: id.to_string(),
        created_at: "2026-06-12T00:00:00Z".into(),
        backup_type: BackupType::Full,
        base_id: None,
        topology: Topology::ReplicaSet,
        server_version: "7.0.35".into(),
        tool_versions: Default::default(),
        selective: false,
        original_size_bytes: payload.len() as u64,
        stored_size_bytes: payload.len() as u64,
        compression: None,
        encryption: None,
        checksum_sha256: hex::encode(Sha256::digest(payload)),
        oplog_range: Some(OplogRange {
            start_ts: OplogTimestamp::new(100, 1),
            end_ts: OplogTimestamp::new(100, 1),
        }),
        oplog_count: None,
        promoted_from_gap: false,
        mysql_binlog: None,
        status: BackupStatus::Complete,
    };
    ManifestStore::new(&fs).write(&m).await.unwrap();
}

/// `verify --json` — 심어 둔 풀백업의 구조 검증. `--chain` 없으면 `chain`은 null이다.
#[tokio::test]
async fn verify_json_keys_are_stable() {
    let dir = tempfile::tempdir().unwrap();
    seed_full_backup(dir.path(), "bk-schema", b"archive payload").await;
    let cfg = write_config(dir.path().to_str().unwrap());

    let out = xbackup()
        .args(["--config", cfg.path().to_str().unwrap()])
        .args(["verify", "--id", "bk-schema", "--json"])
        .env("XB_SCHEMA_URI", "mongodb://u:p@127.0.0.1:1/?replicaSet=rs0")
        .output()
        .unwrap();

    let v = parse_single_json(&out.stdout);
    assert_key_paths(
        &key_paths(&v),
        &[
            "schema",
            "backup_id",
            "manifest_sidecar_ok",
            "data_checksum_ok",
            "deep_decode_ok",
            "empty_slice",
            "warnings",
            "ok",
            "chain",
        ],
        "verify --json",
    );
    assert_eq!(
        v["schema"], 1,
        "스키마 버전은 소비자가 파서를 고르는 근거다"
    );
    assert_eq!(v["ok"], true, "온전한 백업은 검증을 통과해야 한다");
    assert!(v["chain"].is_null(), "--chain 없으면 chain은 null");
}

/// `verify --chain --json` — `chain` 하위 객체의 키 구조까지 계약이다.
///
/// 중첩 객체이므로 `schema`를 갖지 않는다는 점도 함께 고정한다.
#[tokio::test]
async fn verify_chain_json_keys_are_stable() {
    let dir = tempfile::tempdir().unwrap();
    seed_full_backup(dir.path(), "bk-chain", b"archive payload").await;
    let cfg = write_config(dir.path().to_str().unwrap());

    let out = xbackup()
        .args(["--config", cfg.path().to_str().unwrap()])
        .args(["verify", "--id", "bk-chain", "--chain", "--json"])
        .env("XB_SCHEMA_URI", "mongodb://u:p@127.0.0.1:1/?replicaSet=rs0")
        .output()
        .unwrap();

    let v = parse_single_json(&out.stdout);
    assert_key_paths(
        &key_paths(&v),
        &[
            "schema",
            "backup_id",
            "manifest_sidecar_ok",
            "data_checksum_ok",
            "deep_decode_ok",
            "empty_slice",
            "warnings",
            "ok",
            "chain",
            "chain.base_id",
            "chain.incremental_ids",
            "chain.continuous",
            "chain.breaks",
            "chain.warnings",
        ],
        "verify --chain --json",
    );
    assert_eq!(v["schema"], 1);
    assert!(
        v["chain"].get("schema").is_none(),
        "중첩 객체는 독립 문서가 아니므로 버전을 갖지 않는다"
    );
}
