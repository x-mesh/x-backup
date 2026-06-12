//! Manifest 스키마 — 백업 메타·체크섬·oplog ts·체인(PRD §FR-7, 리서치 §7).
//!
//! [`BackupManifest`]는 각 백업 산출물의 사이드카 메타데이터다. `format_version`으로
//! 포맷 진화에 대비하고, 본문 sha256은 [별도 사이드카](super::store)로 보호한다.
//!
//! ## 시크릿 비저장
//! manifest는 평문 JSON으로 저장된다(키 없이 구조 검증 가능, PRD §8.5). 따라서
//! 시크릿(URI·자격증명·암호화 *키 자체*)은 절대 담지 않는다 — 암호화 메타는
//! 알고리즘·키 *식별자*만 기록한다(PRD §8.3).

use serde::{Deserialize, Serialize};

/// 현재 manifest 포맷 버전. 비호환 변경 시 증가시킨다(리서치 §7: =1).
pub const FORMAT_VERSION: u32 = 1;

/// oplog 타임스탬프 — BSON Timestamp{t,i}의 정밀도 보존 표현(리서치 §7).
///
/// MongoDB `ts`는 DateTime이 아니라 (초, 증분) 쌍이다(스파이크 §3.3, pitfall 2-2).
/// 두 필드를 그대로 보존해 `(t, i)` **사전순** 비교로 oplog 순서를 정확히 표현한다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OplogTimestamp {
    /// Unix epoch 이후 초(BSON Timestamp.time).
    pub t: u32,
    /// 동일 초 내 순서를 위한 증분(BSON Timestamp.increment).
    pub i: u32,
}

impl OplogTimestamp {
    /// `(t, i)` 쌍으로 생성한다.
    pub fn new(t: u32, i: u32) -> Self {
        Self { t, i }
    }
}

impl From<bson::Timestamp> for OplogTimestamp {
    fn from(ts: bson::Timestamp) -> Self {
        Self {
            t: ts.time,
            i: ts.increment,
        }
    }
}

impl From<OplogTimestamp> for bson::Timestamp {
    fn from(ts: OplogTimestamp) -> Self {
        bson::Timestamp {
            time: ts.t,
            increment: ts.i,
        }
    }
}

/// `(t, i)` 사전순 정렬 — oplog ts의 자연 순서.
impl PartialOrd for OplogTimestamp {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OplogTimestamp {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // t를 1차 키, i를 2차 키로 비교(사전순).
        self.t.cmp(&other.t).then(self.i.cmp(&other.i))
    }
}

/// dump가 본 oplog 구간 — dump 시작/끝 시점의 최신 oplog ts(FR-7).
///
/// archive `--oplog`는 dump 시점 일관성을 위해 구간 내 변경을 archive에 함께 담는다
/// (스파이크 §3.2). 이 구간은 PITR 체인 연결·gap 판단의 기준점이 된다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OplogRange {
    /// dump 시작 직전 최신 oplog ts.
    pub start_ts: OplogTimestamp,
    /// dump 완료 직후 최신 oplog ts.
    pub end_ts: OplogTimestamp,
}

/// 백업 유형(PRD §FR-1/§FR-2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackupType {
    /// 풀 백업.
    Full,
    /// 증분 백업(oplog 기반, t8).
    Incremental,
}

/// 백업 토폴로지(PRD §4 지원 매트릭스).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Topology {
    /// replica set — 증분의 전제(`--oplog` 가능).
    ReplicaSet,
    /// standalone — oplog 부재(증분 불가).
    Standalone,
}

/// 압축 메타데이터(t6이 실제 값을 채움; t4는 None = 평문).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompressionMeta {
    /// 알고리즘 식별자(예: `"zstd"`).
    pub algorithm: String,
    /// 압축 레벨.
    pub level: i32,
}

/// 암호화 메타데이터(t6이 실제 값을 채움; t4는 None = 평문).
///
/// 키 *자체*는 담지 않는다 — 알고리즘과 키 식별자(recipient 지문 등)만 기록한다(PRD §8.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncryptionMeta {
    /// 알고리즘 식별자(예: `"age"`, `"aes-256-gcm"`).
    pub algorithm: String,
    /// 키/수신자 식별자(공개키 지문 등). 키 자체가 아니다.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_id: Option<String>,
}

/// 백업 완료 상태(리서치 §4: 부분 성공은 Incomplete로 표기).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackupStatus {
    /// 전 단계 성공(data + manifest + 사이드카 기록 완료).
    Complete,
    /// 부분 성공 — list/verify가 경고, 증분 base로 사용 금지.
    Incomplete,
}

/// mongodump/mongorestore 등 외부 도구 버전(버전 호환 추적, pitfall 1-5).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ToolVersions {
    /// mongodump 버전 문자열(예: `"100.16.1"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mongodump: Option<String>,
    /// archive 포맷 버전(서버 7.0 기준 `"0.1"`, 스파이크 §2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_format: Option<String>,
}

/// 단일 백업의 사이드카 manifest(PRD §FR-7).
///
/// `data.bin`과 짝을 이뤄 `<backup-id>/manifest.json`으로 저장되고, 자체 무결성은
/// `<backup-id>/manifest.json.sha256` 사이드카로 보호된다([store](super::store)).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupManifest {
    /// 포맷 버전(포맷 진화 대비). 항상 [`FORMAT_VERSION`]으로 기록한다.
    pub format_version: u32,
    /// 백업 ID(UUID v7 — 시간 정렬 가능, 저장 디렉터리명이기도 함).
    pub id: String,
    /// 생성 시각(UTC RFC3339).
    pub created_at: String,
    /// 백업 유형(full/incremental).
    pub backup_type: BackupType,
    /// 증분의 base 풀백업 ID(full이면 None).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_id: Option<String>,
    /// 백업 시점의 서버 토폴로지.
    pub topology: Topology,
    /// 백업 원본 MongoDB 서버 버전(복구 호환 점검용, PRD §FR-3).
    pub server_version: String,
    /// 외부 도구 버전(pitfall 1-5).
    #[serde(default)]
    pub tool_versions: ToolVersions,
    /// 선택적 백업 여부(`--db`/`--collection`). true면 증분 base 부적격(FR-1).
    pub selective: bool,
    /// 원본(파이프라인 입력) 바이트 수 — dump stdout 총량.
    pub original_size_bytes: u64,
    /// 저장(파이프라인 출력) 바이트 수 — Storage에 쓰인 총량.
    pub stored_size_bytes: u64,
    /// 압축 메타(t4 평문은 None).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compression: Option<CompressionMeta>,
    /// 암호화 메타(t4 평문은 None).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encryption: Option<EncryptionMeta>,
    /// 저장 바이트(data.bin)의 sha256 hex — 무결성 primary(PRD §8.5).
    pub checksum_sha256: String,
    /// dump가 본 oplog 구간(replica set + --oplog일 때만; standalone은 None).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oplog_range: Option<OplogRange>,
    /// 완료 상태.
    pub status: BackupStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> BackupManifest {
        BackupManifest {
            format_version: FORMAT_VERSION,
            id: "0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b".to_string(),
            created_at: "2026-06-12T13:00:00Z".to_string(),
            backup_type: BackupType::Full,
            base_id: None,
            topology: Topology::ReplicaSet,
            server_version: "7.0.35".to_string(),
            tool_versions: ToolVersions {
                mongodump: Some("100.16.1".to_string()),
                archive_format: Some("0.1".to_string()),
            },
            selective: false,
            original_size_bytes: 1024,
            stored_size_bytes: 1024,
            compression: None,
            encryption: None,
            checksum_sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
                .to_string(),
            oplog_range: Some(OplogRange {
                start_ts: OplogTimestamp::new(1_781_272_000, 1),
                end_ts: OplogTimestamp::new(1_781_272_133, 5),
            }),
            status: BackupStatus::Complete,
        }
    }

    /// JSON serde round-trip이 손실 없이 동일 값을 복원해야 한다.
    #[test]
    fn manifest_serde_round_trip() {
        let m = sample();
        let json = serde_json::to_string_pretty(&m).unwrap();
        let back: BackupManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
        assert_eq!(back.format_version, 1);
    }

    /// 평문(t4) manifest는 compression/encryption 필드를 생략한다(skip_serializing_if).
    #[test]
    fn plaintext_manifest_omits_crypto_fields() {
        let m = sample();
        let json = serde_json::to_string(&m).unwrap();
        assert!(!json.contains("\"compression\""));
        assert!(!json.contains("\"encryption\""));
    }

    /// OplogTimestamp는 (t, i) 사전순으로 비교된다.
    #[test]
    fn oplog_timestamp_lexicographic_order() {
        let a = OplogTimestamp::new(100, 5);
        let b = OplogTimestamp::new(100, 6);
        let c = OplogTimestamp::new(101, 1);
        // 같은 t에서는 i가 결정.
        assert!(a < b);
        // t가 크면 i와 무관하게 큼.
        assert!(b < c);
        assert!(a < c);
        // 동일 값.
        assert_eq!(a, OplogTimestamp::new(100, 5));
    }

    /// bson::Timestamp ↔ OplogTimestamp 상호 변환이 값을 보존한다.
    #[test]
    fn bson_timestamp_conversion_round_trip() {
        let bts = bson::Timestamp {
            time: 1_781_272_133,
            increment: 5,
        };
        let ours: OplogTimestamp = bts.into();
        assert_eq!(ours, OplogTimestamp::new(1_781_272_133, 5));
        let back: bson::Timestamp = ours.into();
        assert_eq!(back.time, 1_781_272_133);
        assert_eq!(back.increment, 5);
    }

    /// backup_type/status는 소문자로 직렬화된다(카탈로그 가독성).
    #[test]
    fn enums_serialize_lowercase() {
        assert_eq!(
            serde_json::to_string(&BackupType::Incremental).unwrap(),
            "\"incremental\""
        );
        assert_eq!(
            serde_json::to_string(&BackupStatus::Complete).unwrap(),
            "\"complete\""
        );
        assert_eq!(
            serde_json::to_string(&Topology::ReplicaSet).unwrap(),
            "\"replica_set\""
        );
    }
}
