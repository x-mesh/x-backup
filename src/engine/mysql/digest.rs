//! 테이블 내용 다이제스트 — 행 수가 같아도 값이 다른 손상을 잡는다(`verify --against --content`).
//!
//! 행마다 렌더링된 튜플 바이트의 SHA-256을 구해 앞 128비트를 덧셈(wrapping)으로 합친다. 덧셈은
//! 교환법칙이 성립하므로 행 순서와 무관하게 같은 값이 나온다. 백업은 덤프가 쓰는 튜플로,
//! 검증은 대상에서 같은 SELECT·렌더링으로 읽은 튜플로 계산하므로, 다이제스트는 "원본 비트"가
//! 아니라 "덤프가 말한 값을 복구가 재현했는지"를 비교한다.

use futures::StreamExt;
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Row};
use sha2::{Digest, Sha256};

use super::backup::{load_table_plan, render_row, SNAPSHOT_SESSION_SETUP};
use super::util::quote_qualified;
use crate::error::{Result, XBackupError};

/// manifest `table_digests.algorithm`에 기록하는 식별자. 계산 방식이 바뀌면 올린다.
pub const ALGORITHM: &str = "sha256-sum128-v1";

/// 행 다이제스트 누적기.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RowDigest(u128);

impl RowDigest {
    /// 한 행의 렌더링된 튜플 바이트를 더한다.
    pub fn add(&mut self, tuple: &[u8]) {
        let hash = Sha256::digest(tuple);
        let head: [u8; 16] = hash[..16].try_into().expect("SHA-256은 32바이트다");
        self.0 = self.0.wrapping_add(u128::from_le_bytes(head));
    }

    /// 32자리 소문자 hex.
    pub fn finish(self) -> String {
        format!("{:032x}", self.0)
    }
}

/// 대상 DB 연결을 백업과 같은 스냅샷 세션(UTF-8, UTC, REPEATABLE READ, 일관 스냅샷)으로 맞춘다.
///
/// 렌더링은 세션 문자셋·시간대에 의존하므로 백업과 같은 값이어야 다이제스트가 같다.
pub async fn prepare_scan_session(conn: &mut Conn) -> Result<()> {
    for stmt in SNAPSHOT_SESSION_SETUP {
        conn.query_drop(stmt).await.map_err(|e| {
            XBackupError::Failure(crate::tr!(
                "failed to set up the scan session ({stmt}): {e}",
                "스캔 세션 설정 실패({stmt}): {e}"
            ))
        })?;
    }
    conn.query_drop("START TRANSACTION WITH CONSISTENT SNAPSHOT")
        .await
        .map_err(|e| {
            XBackupError::Failure(crate::tr!(
                "failed to start the consistent snapshot: {e}",
                "일관 스냅샷 시작 실패: {e}"
            ))
        })
}

/// 연결된 DB의 테이블 하나를 읽어 `(행 수, 다이제스트)`를 구한다.
///
/// 백업과 같은 컬럼 목록·렌더링 계획으로 SELECT한다. 컬럼이 모두 generated라 읽을 컬럼이 없으면
/// 백업과 마찬가지로 0행으로 본다. 호출 전에 [`prepare_scan_session`]을 실행해야 한다.
pub async fn scan_table_digest(conn: &mut Conn, db: &str, table: &str) -> Result<(u64, String)> {
    let plan = load_table_plan(conn, db, table).await?;
    let mut digest = RowDigest::default();
    let mut rows = 0u64;
    if !plan.select_exprs.is_empty() {
        let quoted = quote_qualified(db, table);
        let select = format!("SELECT {} FROM {quoted}", plan.select_exprs.join(", "));
        let mut result = conn.query_iter(select).await.map_err(|e| {
            XBackupError::Failure(crate::tr!(
                "{db}.{table}: SELECT failed: {e}",
                "{db}.{table} SELECT 실패: {e}"
            ))
        })?;
        let stream = result.stream::<Row>().await.map_err(|e| {
            XBackupError::Failure(crate::tr!(
                "{db}.{table}: row stream failed: {e}",
                "{db}.{table} 행 스트림 실패: {e}"
            ))
        })?;
        if let Some(mut stream) = stream {
            while let Some(row) = stream.next().await {
                let row = row.map_err(|e| {
                    XBackupError::Failure(crate::tr!(
                        "{db}.{table}: failed to read a row: {e}",
                        "{db}.{table} 행 읽기 실패: {e}"
                    ))
                })?;
                digest.add(render_row(&row, &plan.render_plan).as_bytes());
                rows += 1;
            }
        }
    }
    Ok((rows, digest.finish()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest_of(rows: &[&str]) -> String {
        let mut d = RowDigest::default();
        for row in rows {
            d.add(row.as_bytes());
        }
        d.finish()
    }

    #[test]
    fn empty_table_has_the_zero_digest() {
        assert_eq!(digest_of(&[]), "0".repeat(32));
    }

    #[test]
    fn digest_does_not_depend_on_row_order() {
        let forward = digest_of(&["(1,'a')", "(2,'b')", "(3,'c')"]);
        let shuffled = digest_of(&["(3,'c')", "(1,'a')", "(2,'b')"]);
        assert_eq!(forward, shuffled);
    }

    #[test]
    fn changing_one_value_changes_the_digest() {
        assert_ne!(
            digest_of(&["(1,'a')", "(2,'b')"]),
            digest_of(&["(1,'a')", "(2,'B')"])
        );
    }

    #[test]
    fn duplicate_rows_are_counted_as_a_multiset() {
        let once = digest_of(&["(1,'a')"]);
        assert_ne!(once, digest_of(&["(1,'a')", "(1,'a')"]));
        assert_ne!(
            digest_of(&["(1,'a')", "(1,'a')"]),
            digest_of(&["(2,'b')", "(2,'b')"])
        );
    }

    #[test]
    fn row_boundaries_matter() {
        assert_ne!(digest_of(&["ab", "c"]), digest_of(&["a", "bc"]));
    }

    #[test]
    fn digest_is_32_lowercase_hex_chars() {
        let digest = digest_of(&["(1,'a')"]);
        assert_eq!(digest.len(), 32);
        assert!(digest
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }
}
