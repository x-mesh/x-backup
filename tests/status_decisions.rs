//! 단위 테스트 — status 판정 로직(드라이버 무관 순수 함수)을 공개 API로 검증한다.
//!
//! 실제 MongoDB 없이 가짜 응답·합성 입력으로 신호등 합산·종료 코드·샤딩 거부·권한 누락·
//! oplog 윈도우 임계·버전 호환을 확인한다. 기본 CI에서 항상 실행된다(feature 미게이트).

use std::collections::BTreeSet;

use bson::doc;
use x_backup::engine::mongo::status::{
    aggregate, is_sharded, missing_actions, oplog_window_status, overall_exit_code,
    parse_interval_secs, parse_mongodump_version, replset_status, summarize_replset,
    version_compat_status, CheckItem, CheckStatus, ReplSetSummary, StatusReport,
};

/// 샤딩(mongos, msg=="isdbgrid") 감지 → fail 항목 → 전체 fail → exit 3(PRD §4/§6.5).
#[test]
fn sharded_hello_is_rejected_with_exit_3() {
    let mongos = doc! { "msg": "isdbgrid", "ok": 1.0 };
    assert!(is_sharded(&mongos), "mongos 감지 실패");

    // 샤딩이면 토폴로지 항목이 fail이 된다 — 보고서로 합산하면 exit 3.
    let report = StatusReport::new(
        "prod",
        vec![
            CheckItem::ok("connection", "연결·인증", "ok"),
            CheckItem::fail("topology", "토폴로지", "샤딩 클러스터 감지 — 스코프 외"),
        ],
    );
    assert_eq!(report.overall, CheckStatus::Fail);
    assert_eq!(report.exit_code(), 3);
}

/// replica set/standalone hello는 샤딩이 아니다.
#[test]
fn non_sharded_topologies_are_not_rejected() {
    assert!(!is_sharded(
        &doc! { "setName": "rs0", "isWritablePrimary": true }
    ));
    assert!(!is_sharded(&doc! { "isWritablePrimary": true }));
}

/// 권한 부족 시 누락 액션을 구체적으로 보고한다(PRD §FR-8 2).
#[test]
fn missing_privileges_are_reported_specifically() {
    let mut granted = BTreeSet::new();
    granted.insert("find".to_string());
    let missing = missing_actions(&granted);
    assert_eq!(missing, vec!["listCollections".to_string()]);

    granted.insert("listCollections".to_string());
    assert!(missing_actions(&granted).is_empty());
}

/// 신호등 합산 → 종료 코드(ok=0, warn=4, fail=3).
#[test]
fn aggregate_maps_to_exit_codes() {
    let ok = vec![CheckItem::ok("a", "A", "")];
    assert_eq!(overall_exit_code(aggregate(&ok)), 0);

    let warn = vec![CheckItem::ok("a", "A", ""), CheckItem::warn("b", "B", "")];
    assert_eq!(overall_exit_code(aggregate(&warn)), 4);

    let fail = vec![CheckItem::warn("a", "A", ""), CheckItem::fail("b", "B", "")];
    assert_eq!(overall_exit_code(aggregate(&fail)), 3);
}

/// oplog 윈도우가 증분 주기의 2배 미만이면 경고(PRD §6.2).
#[test]
fn oplog_window_threshold() {
    let interval = parse_interval_secs("15m").unwrap(); // 900s
    assert_eq!(oplog_window_status(1000, interval), CheckStatus::Warn);
    assert_eq!(oplog_window_status(1800, interval), CheckStatus::Ok);
}

/// mongodump 부재는 실패, 정상 쌍은 ok, 미래 서버 메이저는 경고(PRD §FR-8 3).
#[test]
fn version_compat_signals() {
    assert_eq!(version_compat_status("7.0.35", None), CheckStatus::Fail);
    assert_eq!(
        version_compat_status("7.0.35", Some("100.16.1")),
        CheckStatus::Ok
    );
    assert_eq!(
        version_compat_status("9.0.0", Some("100.16.1")),
        CheckStatus::Warn
    );
}

/// mongodump --version 출력 파싱.
#[test]
fn mongodump_version_parsing() {
    assert_eq!(
        parse_mongodump_version("mongodump version: 100.16.1\ngit version: deadbeef"),
        Some("100.16.1".to_string())
    );
    assert_eq!(parse_mongodump_version("garbage\n"), None);
}

/// replSetGetStatus 요약 → PRIMARY 부재 fail / 높은 lag warn / 정상 ok.
#[test]
fn replset_member_state_decisions() {
    let no_primary = ReplSetSummary {
        has_primary: false,
        secondary_count: 1,
        max_secondary_lag_secs: Some(0),
    };
    assert_eq!(replset_status(&no_primary), CheckStatus::Fail);

    let high_lag = ReplSetSummary {
        has_primary: true,
        secondary_count: 1,
        max_secondary_lag_secs: Some(600),
    };
    assert_eq!(replset_status(&high_lag), CheckStatus::Warn);

    let healthy = ReplSetSummary {
        has_primary: true,
        secondary_count: 2,
        max_secondary_lag_secs: Some(3),
    };
    assert_eq!(replset_status(&healthy), CheckStatus::Ok);
}

/// replSetGetStatus 응답 파싱: PRIMARY/SECONDARY 수와 lag 산정.
#[test]
fn summarize_replset_from_document() {
    let primary_dt = bson::DateTime::from_millis(2_000_000);
    let sec_dt = bson::DateTime::from_millis(1_985_000); // 15s 뒤처짐.
    let status = doc! {
        "set": "rs0",
        "members": [
            { "stateStr": "PRIMARY", "optimeDate": primary_dt },
            { "stateStr": "SECONDARY", "optimeDate": sec_dt },
        ]
    };
    let summary = summarize_replset(&status);
    assert!(summary.has_primary);
    assert_eq!(summary.secondary_count, 1);
    assert_eq!(summary.max_secondary_lag_secs, Some(15));
}
