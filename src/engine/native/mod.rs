//! 네이티브 백업 엔진 — `mongodump`/`mongorestore` 없이 드라이버로 직접 백업/복구.
//!
//! 1차 범위: **데이터 + 인덱스 + 컬렉션 옵션**의 풀 백업/복구. 산출물은 자체 포맷
//! [`archive`]`xb-native-v1`이며, 네이티브 엔진으로만 복구된다(manifest.archive_format).
//!
//! ## mongodump 엔진과의 차이
//! - **장점:** 외부 도구 불필요(단일 바이너리만으로 풀 백업/복구).
//! - **일관성:** 컬렉션별로 읽으므로 컬렉션 *간* 단일 시점 일관성은 보장하지 않는다
//!   (mongodump `--oplog` 없는 것과 동일 프로파일). 시점 일관성/PITR은 oplog 증분으로 얻는다.
//! - **미커버(로드맵):** users·roles, view·timeseries, 샤딩 메타. (1차는 일반 컬렉션 데이터·
//!   인덱스·옵션만 — 그 외는 건너뛰며 경고 로그.)
//!
//! 증분 oplog 캡처는 엔진과 무관하게 이미 드라이버 네이티브다([`super::oplog`]).

pub mod archive;
pub mod backup;
pub mod restore;

pub use backup::{NativeDumpStream, NativeDumper};
pub use restore::native_restore;
