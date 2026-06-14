//! PostgreSQL 엔진(2차) — 외부 `pg_dump`/`pg_restore` 없이 드라이버 COPY 프로토콜로
//! 테이블 데이터를 스트리밍 백업/복구한다.
//!
//! - [`conn`]: tokio-postgres 연결(NoTls, 1차).
//! - [`archive`]: `xb-pg-v1` 스트리밍 포맷(스키마 DDL + COPY 바이너리 바이트).
//! - [`backup`]: 스키마 introspection + `COPY ... TO STDOUT` → 아카이브.
//! - [`restore`]: 아카이브 → 스키마 재생성 + `COPY ... FROM STDIN`.
//! - [`status`]: 연결·버전·DB 크기·테이블/행 수.
//!
//! 1차 스코프는 **데이터 중심**(테이블·제약·인덱스·시퀀스·행). 뷰·함수·트리거·확장·권한 등
//! pg_dump 풀 충실도는 후속이다([`backup`] 모듈 문서 참조).

pub mod archive;
pub mod backup;
pub mod conn;
pub mod meta;
pub mod pgoutput;
pub mod restore;
pub mod status;
