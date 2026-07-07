//! MongoDB 어댑터 — mongodump 오케스트레이션 + 드라이버 메타 질의.
//!
//! - [`dump`]: `mongodump --archive=- [--oplog]` 스폰, stderr 독립 drain,
//!   exit code 판정, kill+wait(pitfall 6).
//! - [`restore`]: `mongorestore --archive=- [--nsInclude] [--drop]` 스폰(stdin 싱크),
//!   dump와 동일한 안전 규칙(stderr drain·exit code 판정·kill+wait)(t5).
//! - [`meta`]: 드라이버로 토폴로지(`hello`)·서버 버전(`buildInfo`)·oplog 최신 ts 질의 +
//!   복구 사전 점검용 기존 네임스페이스 조회(t5).
//! - [`status`]: 백업 가능 상태 점검(FR-8) — 연결·권한·버전·토폴로지(샤딩 거부)·oplog
//!   윈도우·저장 엔진·예상 크기·secondary 가용성. 읽기 전용·무부작용(t11).
//! - [`uri_config`]: 0600 임시 URI config 파일(argv 시크릿 노출 방지, PRD §11).
//!
//! - [`oplog`]: 증분 캡처 — 드라이버로 `local.oplog.rs`를 직접 질의해 raw BSON 슬라이스를
//!   스트림으로 노출하고, gap 감지·partialTxn 경계 확장을 수행한다(t8, PRD §6.3/FR-2).
//!
//! dump/restore 자체는 서브프로세스가 수행하고 드라이버는 메타데이터만 다룬다(태스크 지침 1).

pub mod apply;
pub mod conn;
pub mod dump;
pub mod meta;
pub mod oplog;
pub mod restore;
pub mod status;
pub mod uri_config;

pub use apply::{ApplyStats, OplogApplier};
pub use conn::{client_options, DEFAULT_TIMEOUT_SECS};
pub use dump::{DumpProcess, DumpSpec};
pub use meta::{MongoMeta, ServerMeta};
pub use oplog::{CaptureError, CaptureHandle, GapCheck, OplogCaptureStream, OplogReader};
pub use restore::{RestoreProcess, RestoreSpec};
pub use status::{CheckItem, CheckStatus, StatusChecker, StatusReport};
pub use uri_config::UriConfigFile;
