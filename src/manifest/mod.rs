//! Manifest 계층 — 백업 메타·체크섬·oplog ts·체인 기록/조회(PRD §FR-7).
//!
//! BackupManifest: format_version·id·created_at·backup_type·base_id·topology·
//! server_version·checksum_sha256·oplog_range·status 등.
//! manifest 자체 무결성도 사이드카 체크섬으로 보호한다.
//!
//! TODO(후속 태스크 R11/R12/R22): manifest 스키마, store, chain 검증 구현.
