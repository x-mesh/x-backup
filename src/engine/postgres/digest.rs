//! PostgreSQL 테이블 내용 다이제스트(`verify --against --content`).
//!
//! 해시 입력은 `COPY ... TO STDOUT (FORMAT text)`가 내는 행 하나의 바이트다(줄바꿈 제외). 값 안의
//! 줄바꿈은 COPY가 `\n` 두 글자로 이스케이프하므로 날 줄바꿈은 항상 행 경계다. 백업은 덤프 스트림
//! 에서, 검증은 대상에서 같은 COPY를 같은 세션 설정(`DUMP_SESSION_SETTINGS`)으로 읽어 계산한다.
//!
//! PG 12 이전 서버는 `extra_float_digits = 3`에서 float를 더 길게 출력해 같은 값도 텍스트가 달라
//! 다이제스트가 어긋난다. 그래서 대상이 PG 12 미만이면 대조를 거부한다(원본은 항상 12 이상이다).

use futures::TryStreamExt;
use tokio_postgres::Client;

use super::backup::DUMP_SESSION_SETTINGS;
use super::compare::quote_ident;
use crate::engine::digest::RowDigest;
use crate::error::{Result, XBackupError};

/// manifest `table_digests.algorithm`에 기록하는 식별자. 해시 입력은 COPY text 행 바이트다.
pub const ALGORITHM: &str = "sha256-sum128-copytext-v1";

/// 대조를 지원하는 서버의 최소 `server_version_num`(PG 12).
const MIN_SERVER_VERSION_NUM: i32 = 120_000;

/// COPY text 청크 스트림을 행 단위로 잘라 [`RowDigest`]에 더한다. 청크 경계에 걸린 행은 보관한다.
#[derive(Debug, Default)]
pub struct CopyRowDigest {
    digest: RowDigest,
    partial: Vec<u8>,
    rows: u64,
}

impl CopyRowDigest {
    pub fn push(&mut self, chunk: &[u8]) {
        let mut rest = chunk;
        while let Some(end) = rest.iter().position(|&b| b == b'\n') {
            let (line, tail) = rest.split_at(end);
            if self.partial.is_empty() {
                self.digest.add(line);
            } else {
                self.partial.extend_from_slice(line);
                self.digest.add(&self.partial);
                self.partial.clear();
            }
            self.rows += 1;
            rest = &tail[1..];
        }
        self.partial.extend_from_slice(rest);
    }

    /// `(행 수, 다이제스트)`. 줄바꿈 없이 끝난 나머지가 있으면 마지막 행으로 더한다.
    pub fn finish(mut self) -> (u64, String) {
        if !self.partial.is_empty() {
            self.digest.add(&self.partial);
            self.rows += 1;
        }
        (self.rows, self.digest.finish())
    }
}

/// 대상 연결을 백업과 같은 세션(고정 설정 + 일관 스냅샷)으로 맞춘다. PG 12 미만이면 거부한다.
pub async fn prepare_scan_session(client: &Client) -> Result<()> {
    let version_num: String = client
        .query_one("SHOW server_version_num", &[])
        .await
        .map(|r| r.get(0))
        .map_err(|e| {
            XBackupError::Failure(crate::tr!(
                "failed to query server_version_num: {e}",
                "server_version_num 조회 실패: {e}"
            ))
        })?;
    if version_num.parse::<i32>().unwrap_or(0) < MIN_SERVER_VERSION_NUM {
        return Err(XBackupError::Usage(crate::tr!(
            "--content needs a PostgreSQL 12 or newer target (server_version_num {version_num}): older servers print floats differently",
            "--content는 PostgreSQL 12 이상 대상에서만 쓸 수 있습니다(server_version_num {version_num}): 이전 버전은 float 출력 형식이 달라 같은 값도 다르게 보입니다"
        )));
    }
    for setting in DUMP_SESSION_SETTINGS {
        client.batch_execute(setting).await.map_err(|e| {
            XBackupError::Failure(crate::tr!(
                "failed to set up the scan session ({setting}): {e}",
                "스캔 세션 설정 실패({setting}): {e}"
            ))
        })?;
    }
    client
        .batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .await
        .map_err(|e| {
            XBackupError::Failure(crate::tr!(
                "failed to start the snapshot transaction: {e}",
                "스냅샷 트랜잭션 시작 실패: {e}"
            ))
        })
}

/// 테이블 하나를 백업과 같은 컬럼 목록·COPY로 읽어 다이제스트를 구한다.
/// 호출 전에 [`prepare_scan_session`]을 실행해야 한다.
pub async fn scan_table_digest(client: &Client, schema: &str, table: &str) -> Result<String> {
    // 백업(`introspect_table`)과 같은 규칙: attnum 순서, STORED generated 제외.
    let cols: Vec<String> = client
        .query(
            "SELECT quote_ident(a.attname) FROM pg_attribute a \
             JOIN pg_class c ON c.oid = a.attrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = $2 AND a.attnum > 0 \
               AND NOT a.attisdropped AND a.attgenerated <> 's' \
             ORDER BY a.attnum",
            &[&schema, &table],
        )
        .await
        .map_err(|e| {
            XBackupError::Failure(crate::tr!(
                "{schema}.{table}: failed to query columns: {e}",
                "{schema}.{table} 컬럼 조회 실패: {e}"
            ))
        })?
        .iter()
        .map(|r| r.get(0))
        .collect();
    let mut digest = CopyRowDigest::default();
    if !cols.is_empty() {
        let copy_sql = format!(
            "COPY {}.{} ({}) TO STDOUT (FORMAT text)",
            quote_ident(schema),
            quote_ident(table),
            cols.join(", ")
        );
        let stream = client.copy_out(copy_sql.as_str()).await.map_err(|e| {
            XBackupError::Failure(crate::tr!(
                "{schema}.{table}: COPY OUT failed: {e}",
                "{schema}.{table} COPY OUT 실패: {e}"
            ))
        })?;
        futures::pin_mut!(stream);
        while let Some(chunk) = stream.try_next().await.map_err(|e| {
            XBackupError::Failure(crate::tr!(
                "{schema}.{table}: failed to read a COPY chunk: {e}",
                "{schema}.{table} COPY 청크 읽기 실패: {e}"
            ))
        })? {
            digest.push(&chunk);
        }
    }
    Ok(digest.finish().1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest_of(chunks: &[&[u8]]) -> (u64, String) {
        let mut d = CopyRowDigest::default();
        for chunk in chunks {
            d.push(chunk);
        }
        d.finish()
    }

    #[test]
    fn chunk_boundaries_do_not_change_the_digest() {
        let whole = digest_of(&[b"1\talice\n2\tbob\n3\tcarol\n"]);
        let split = digest_of(&[b"1\tali", b"ce\n2\tbob", b"\n3\tcarol\n"]);
        let bytewise: Vec<&[u8]> = b"1\talice\n2\tbob\n3\tcarol\n".chunks(1).collect();
        assert_eq!(whole, split);
        assert_eq!(whole, digest_of(&bytewise));
        assert_eq!(whole.0, 3);
    }

    #[test]
    fn escaped_newline_inside_a_value_is_not_a_row_boundary() {
        let (rows, _) = digest_of(&[b"1\tline1\\nline2\n"]);
        assert_eq!(rows, 1);
    }

    #[test]
    fn row_order_does_not_matter_and_a_changed_value_does() {
        let a = digest_of(&[b"1\tx\n2\ty\n"]).1;
        assert_eq!(a, digest_of(&[b"2\ty\n1\tx\n"]).1);
        assert_ne!(a, digest_of(&[b"1\tx\n2\tz\n"]).1);
    }

    #[test]
    fn empty_stream_is_zero_rows_and_the_zero_digest() {
        assert_eq!(digest_of(&[]), (0, "0".repeat(32)));
    }
}
