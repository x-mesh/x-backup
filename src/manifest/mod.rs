//! Manifest 계층 — 백업 메타·체크섬·oplog ts·체인 기록/조회(PRD §FR-7).
//!
//! - [`schema`]: [`BackupManifest`] 스키마와 [`OplogTimestamp`] 등 값 타입.
//! - [`store`]: 저장 레이아웃(`<id>/{data.bin,manifest.json,manifest.json.sha256}`)과
//!   기록 순서(data→manifest→사이드카, pitfall 7-1).
//!
//! manifest 자체 무결성은 사이드카 체크섬으로 보호한다(FR-7). 체인 검증(verify
//! --chain)은 [`chain`] 모듈이 이 스키마 위에 순수 함수로 구현한다(R12).

pub mod chain;
pub mod schema;
pub mod store;

pub use chain::{verify_chain, ChainBreak, ChainNode, ChainReport, ChainWarning};
pub use schema::{
    BackupManifest, BackupStatus, BackupType, CompressionMeta, EncryptionMeta, OplogRange,
    OplogTimestamp, Topology, ToolVersions, FORMAT_VERSION,
};
pub use store::ManifestStore;
