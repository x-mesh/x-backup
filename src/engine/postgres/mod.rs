//! PostgreSQL 엔진(2차) — 외부 `pg_dump`/`pg_restore` 없이 드라이버 프로토콜만으로
//! 백업·복구·증분·PITR을 수행한다(프로젝트 정체성: 외부 의존 없는 단일 정적 바이너리).
//!
//! - [`conn`]: tokio-postgres 연결(rustls TLS — sslmode `prefer`면 평문 폴백).
//! - [`archive`]: `xb-pg-v1` 풀 백업 포맷(스키마 DDL + COPY 데이터).
//! - [`backup`]: pg_catalog introspection + `COPY ... TO STDOUT` → 아카이브.
//! - [`restore`]: 아카이브 → 스키마 재생성 + `COPY ... FROM STDIN`.
//! - [`meta`]: 테이블/행 수·DB 크기 등 메타 질의(status·peek·migrate 공용).
//! - [`status`]: 연결·버전·DB 크기·테이블/행 수.
//! - [`pgoutput`]: logical decoding(pgoutput) 바이너리 디코더(증분 토대).
//! - [`incremental`]: logical decoding 기반 증분 캡처/적용 + PITR(`xb-pg-incr-v1`).
//!
//! **스키마 충실도:** 테이블·제약·인덱스·시퀀스·행에 더해 확장, 타입(enum/domain/composite),
//! 함수, 트리거, 뷰/머티리얼라이즈드 뷰, 파티셔닝, IDENTITY·생성(STORED) 컬럼까지 덤프/복구한다.
//! 미지원(경고만): 집계/윈도우 함수(`pg_get_functiondef` 한계), 사용자 정의 range/base 타입.
//!
//! **증분/PITR:** `features.incremental.pg_logical=true`로 opt-in. 풀 백업이 replication
//! slot+publication을 만들고, `backup --type incr`가 변경을 캡처, `restore --at`이 시점까지
//! DML로 재생한다(상세는 [`incremental`]).

pub mod archive;
pub mod backup;
pub mod conn;
pub mod incremental;
pub mod meta;
pub mod pgoutput;
pub mod restore;
pub mod status;
