//! MySQL/MariaDB 엔진(3차) — 외부 `mysqldump`/`mysql`/`mysqlbinlog` 없이 드라이버 프로토콜만으로
//! 백업·복구·증분·PITR을 수행한다(프로젝트 정체성: 외부 의존 없는 단일 정적 바이너리).
//!
//! - [`conn`]: `mysql_async` 연결(rustls TLS — URI `ssl-mode`로 협상, 기본 평문 폴백 없음).
//!
//! 다음 계층은 PostgreSQL 엔진([`crate::engine::postgres`])을 1:1 미러링한다:
//! `archive`(`xb-mysql-v1`/`xb-mysql-incr-v1`), `backup`(`SHOW CREATE` + `SELECT` 스트림),
//! `restore`(DDL 재생성 + 다행 `INSERT`), `meta`/`status`, `binlog`(ROW 이벤트 디코더),
//! `incremental`(binlog 캡처/적용 + PITR).
//!
//! **스키마 충실도:** 테이블·뷰·트리거·루틴·이벤트 DDL을 `SHOW CREATE *`로 그대로 보존하고,
//! generated(STORED/VIRTUAL)·invisible 컬럼을 정확히 처리한다. InnoDB 일관 스냅샷
//! (`START TRANSACTION WITH CONSISTENT SNAPSHOT`)을 전제로 하며 non-transactional 엔진은
//! 일관성을 보장하지 않는다(문서화).
//!
//! **증분/PITR:** `features.incremental.mysql_binlog=true`로 opt-in. 풀 백업이 스냅샷 시점의
//! binlog 좌표(file:pos + GTID)를 기록하고, `backup --type incr`가 그 이후 ROW 이벤트를
//! 캡처, `restore --at`이 시점까지 멱등 DML로 재생한다.

pub mod archive;
pub mod backup;
pub mod conn;
pub mod incremental;
pub mod meta;
pub mod restore;
pub mod status;
pub mod util;
pub mod value;
