//! 통합 테스트 — fixture replica set 대상 `status` 전 점검 항목 출력(FR-8).
//!
//! 환경에 `mongodump` + replica set(`tests/fixtures/replica-set.sh`)이 필요하므로
//! `integration-tests` feature로 격리한다. 기본 CI 단위 테스트에는 포함되지 않는다.
//!
//! 실행:
//! ```bash
//! tests/fixtures/replica-set.sh up
//! XB_TEST_MONGO_URI="$(tests/fixtures/replica-set.sh uri)" \
//!   cargo test --features integration-tests --test status_integration -- --ignored --nocapture
//! tests/fixtures/replica-set.sh down
//! ```
//!
//! `XB_TEST_MONGO_URI`가 없으면 기본 fixture URI를 사용한다.

#![cfg(feature = "integration-tests")]

use std::collections::BTreeSet;

use x_backup::config::secret::Secret;
use x_backup::engine::mongo::status::{CheckStatus, StatusChecker};

fn test_uri() -> String {
    std::env::var("XB_TEST_MONGO_URI").unwrap_or_else(|_| {
        "mongodb://localhost:27017/?replicaSet=rs0&directConnection=true".to_string()
    })
}

/// replica set 대상 전체 status가 모든 점검 항목을 출력하고, 핵심 항목이 ok이며,
/// 전체 신호등이 fail이 아닌지(정상/경고) 확인한다.
#[tokio::test]
#[ignore = "requires mongodump + replica set; run with --features integration-tests"]
async fn full_status_emits_all_check_items() {
    let checker = StatusChecker::connect(&Secret::new(test_uri()), None)
        .await
        .expect("status 연결");

    let report = checker
        .full_report(
            "test",
            Some("mongodump"),
            "15m",
            false,
            x_backup::i18n::Lang::En,
        )
        .await;

    // 점검 항목 키 집합 — FR-8 항목들이 모두 보고되어야 한다.
    let keys: BTreeSet<&str> = report.items.iter().map(|i| i.key).collect();
    for expected in [
        "connection",
        "topology",
        "privileges",
        "version",
        "storage_engine",
        "oplog_window",
        "estimated_size",
    ] {
        assert!(
            keys.contains(expected),
            "점검 항목 누락: {expected} (보고: {keys:?})"
        );
    }

    // 연결·토폴로지는 replica set fixture에서 정상이어야 한다.
    let by_key = |k: &str| report.items.iter().find(|i| i.key == k).unwrap();
    assert_eq!(
        by_key("connection").status,
        CheckStatus::Ok,
        "연결 실패: {:?}",
        by_key("connection").message
    );
    assert_eq!(
        by_key("topology").status,
        CheckStatus::Ok,
        "토폴로지 비정상: {:?}",
        by_key("topology").message
    );

    // 전체 신호등은 fail이 아니어야 한다(정상 또는 경고).
    assert_ne!(
        report.overall,
        CheckStatus::Fail,
        "전체 점검 실패: {:?}",
        report.items
    );
    // 종료 코드는 0 또는 4(fail=3 아님).
    assert!(
        matches!(report.exit_code(), 0 | 4),
        "exit code: {}",
        report.exit_code()
    );
}

/// `--json` 직렬화가 항목 배열 + overall 구조를 담는지 확인한다.
#[tokio::test]
#[ignore = "requires mongodump + replica set; run with --features integration-tests"]
async fn full_status_serializes_json() {
    let checker = StatusChecker::connect(&Secret::new(test_uri()), None)
        .await
        .expect("status 연결");
    let report = checker
        .full_report(
            "test",
            Some("mongodump"),
            "15m",
            false,
            x_backup::i18n::Lang::En,
        )
        .await;

    let json = serde_json::to_value(&report).expect("JSON 직렬화");
    assert!(json["items"].is_array(), "items 배열 아님");
    assert!(json["overall"].is_string(), "overall 누락");
    assert_eq!(json["profile"], "test");
    // 각 항목은 key/status/message를 가진다.
    let first = &json["items"][0];
    assert!(first["key"].is_string());
    assert!(first["status"].is_string());
}

/// 핵심 사전 점검 서브셋(backup 자동 선행용)이 replica set에서 fail이 아닌지 확인한다.
#[tokio::test]
#[ignore = "requires mongodump + replica set; run with --features integration-tests"]
async fn precheck_subset_passes_on_healthy_replica_set() {
    let checker = StatusChecker::connect(&Secret::new(test_uri()), None)
        .await
        .expect("status 연결");
    let report = checker.precheck_subset("test", Some("mongodump")).await;

    // 서브셋은 연결·토폴로지·권한·도구 존재만 본다.
    let keys: BTreeSet<&str> = report.items.iter().map(|i| i.key).collect();
    assert!(keys.contains("connection"));
    assert!(keys.contains("topology"));
    assert!(keys.contains("tool"));

    assert_ne!(
        report.overall,
        CheckStatus::Fail,
        "사전 점검 실패: {:?}",
        report.items
    );
}
