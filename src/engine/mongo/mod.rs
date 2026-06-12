//! MongoDB 어댑터 — mongodump 오케스트레이션 + 드라이버 메타 질의.
//!
//! - [`dump`]: `mongodump --archive=- [--oplog]` 스폰, stderr 독립 drain,
//!   exit code 판정, kill+wait(pitfall 6).
//! - [`restore`]: `mongorestore --archive=- [--nsInclude] [--drop]` 스폰(stdin 싱크),
//!   dump와 동일한 안전 규칙(stderr drain·exit code 판정·kill+wait)(t5).
//! - [`meta`]: 드라이버로 토폴로지(`hello`)·서버 버전(`buildInfo`)·oplog 최신 ts 질의 +
//!   복구 사전 점검용 기존 네임스페이스 조회(t5).
//! - [`uri_config`]: 0600 임시 URI config 파일(argv 시크릿 노출 방지, PRD §11).
//!
//! dump/restore 자체는 서브프로세스가 수행하고 드라이버는 메타데이터만 다룬다(태스크 지침 1).
//! 증분 캡처(드라이버 oplog 슬라이스)는 후속 태스크(t8) 소유.

pub mod dump;
pub mod meta;
pub mod restore;
pub mod uri_config;

pub use dump::{DumpProcess, DumpSpec};
pub use meta::{MongoMeta, ServerMeta};
pub use restore::{RestoreProcess, RestoreSpec};
pub use uri_config::UriConfigFile;
