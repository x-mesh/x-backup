//! MongoDB 어댑터 — mongodump 오케스트레이션 + 드라이버 메타 질의.
//!
//! - [`dump`]: `mongodump --archive=- [--oplog]` 스폰, stderr 독립 drain,
//!   exit code 판정, kill+wait(pitfall 6).
//! - [`meta`]: 드라이버로 토폴로지(`hello`)·서버 버전(`buildInfo`)·oplog 최신 ts 질의.
//! - [`uri_config`]: 0600 임시 URI config 파일(argv 시크릿 노출 방지, PRD §11).
//!
//! dump 자체는 서브프로세스가 수행하고 드라이버는 메타데이터만 다룬다(태스크 지침 1).
//! 증분 캡처(드라이버 oplog 슬라이스)·복구(mongorestore)는 후속 태스크(t8/t5) 소유.

pub mod dump;
pub mod meta;
pub mod uri_config;

pub use dump::{DumpProcess, DumpSpec};
pub use meta::{MongoMeta, ServerMeta};
pub use uri_config::UriConfigFile;
