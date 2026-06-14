//! PG 증분 — logical decoding(pgoutput) slot에서 변경을 캡처해 아카이브하고, 복구 시 DML로 적용.
//!
//! Mongo oplog 증분과 동형이되 PG WAL을 logical decoding으로 읽는다:
//! - 풀 백업 시 [`ensure_slot_and_publication`]으로 슬롯·publication을 만들고 base LSN을 기록.
//! - [`capture`]가 `pg_logical_slot_peek_binary_changes`로 pgoutput을 읽어 디코드 →
//!   `xb-pg-incr-v1` 아카이브(변경 레코드 나열). 저장 성공 후 [`advance_slot`]로 슬롯 전진.
//! - [`apply`]가 복구 대상에 변경을 DML(I=upsert / U·D=키 기반)로 적용. `--at`은 commit ts 필터.
//!
//! 적용은 `session_replication_role=replica`로 FK/트리거를 우회하고, 값은 텍스트로 받아
//! 컬럼 타입으로 캐스트(`$n::타입`)한다 — 타입은 복구 대상 카탈로그에서 조회(캐시).

use std::collections::HashMap;

use bson::{Bson, Document};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_postgres::types::{PgLsn, ToSql};
use tokio_postgres::Client;

use super::pgoutput::{Change, Decoder, Op};
use crate::error::{Result, XBackupError};

/// 증분 아카이브 포맷 식별자(manifest.archive_format).
pub const INCR_FORMAT_ID: &str = "xb-pg-incr-v1";

const TAG_HEADER: u8 = b'H';
const TAG_CHANGE: u8 = b'C';
const TAG_END: u8 = b'E';

/// 프로파일 이름을 PG 식별자 안전 형태로([a-z0-9_], 소문자). 슬롯/publication 이름 구성용.
fn sanitize(profile: &str) -> String {
    let s: String = profile
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    s.chars().take(40).collect()
}

/// 프로파일의 logical replication slot 이름.
pub fn slot_name(profile: &str) -> String {
    format!("xb_{}", sanitize(profile))
}

/// 프로파일의 publication 이름.
pub fn publication_name(profile: &str) -> String {
    format!("xb_{}_pub", sanitize(profile))
}

/// publication(FOR ALL TABLES) + pgoutput slot을 보장하고 현재(또는 기존) LSN을 돌려준다.
///
/// 풀 백업 직전에 호출 — 슬롯이 이 시점부터 WAL을 잡아 증분의 시작점이 된다. 이름은 sanitize된
/// 식별자라 인젝션 안전(파라미터화 불가한 DDL이므로 직접 보간).
pub async fn ensure_slot_and_publication(
    client: &Client,
    slot: &str,
    publication: &str,
) -> Result<String> {
    let pub_exists: bool = client
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM pg_publication WHERE pubname=$1)",
            &[&publication],
        )
        .await
        .map(|r| r.get(0))
        .map_err(|e| XBackupError::Failure(format!("publication 확인 실패: {e}")))?;
    if !pub_exists {
        client
            .batch_execute(&format!("CREATE PUBLICATION {publication} FOR ALL TABLES"))
            .await
            .map_err(|e| XBackupError::Failure(format!("publication 생성 실패: {e}")))?;
    }

    let slot_lsn: Option<String> = client
        .query_opt(
            "SELECT confirmed_flush_lsn::text FROM pg_replication_slots WHERE slot_name=$1",
            &[&slot],
        )
        .await
        .map_err(|e| XBackupError::Failure(format!("slot 확인 실패: {e}")))?
        .map(|r| r.get(0));

    match slot_lsn {
        Some(lsn) => Ok(lsn),
        None => {
            let lsn: String = client
                .query_one(
                    "SELECT lsn::text FROM pg_create_logical_replication_slot($1,'pgoutput')",
                    &[&slot],
                )
                .await
                .map(|r| r.get(0))
                .map_err(|e| XBackupError::Failure(format!("slot 생성 실패: {e}")))?;
            Ok(lsn)
        }
    }
}

/// 캡처 결과 — 아카이브 바이트, 마지막 LSN(없으면 변경 0), 변경 수.
pub struct CaptureOutcome {
    pub archive: Vec<u8>,
    pub last_lsn: Option<String>,
    pub count: u64,
}

/// 슬롯에서 변경을 peek(비소비)해 디코드 → `xb-pg-incr-v1` 아카이브 바이트로 만든다.
///
/// peek라 슬롯을 전진시키지 않는다 — 호출자가 **저장 성공 후** [`advance_slot`]로 전진한다
/// (저장 실패 시 다음에 재캡처 — 적용이 idempotent라 안전).
pub async fn capture(client: &Client, slot: &str, publication: &str) -> Result<CaptureOutcome> {
    if !slot_exists(client, slot).await? {
        return Err(XBackupError::PrecheckFailed(format!(
            "logical replication slot '{slot}'이 없습니다 — 먼저 풀 백업으로 슬롯을 만드세요(gap)"
        )));
    }
    let rows = client
        .query(
            "SELECT lsn::text, data FROM pg_logical_slot_peek_binary_changes(\
                $1, NULL, NULL, 'proto_version','1','publication_names',$2)",
            &[&slot, &publication],
        )
        .await
        .map_err(|e| XBackupError::Failure(format!("logical 변경 peek 실패: {e}")))?;

    let mut archive = Vec::new();
    write_header(&mut archive, &chrono::Utc::now().to_rfc3339()).await?;
    let mut decoder = Decoder::new();
    let mut count = 0u64;
    let mut last_lsn = None;
    for r in &rows {
        last_lsn = Some(r.get::<_, String>(0));
        let data: Vec<u8> = r.get(1);
        if let Some(change) = decoder.feed(&data)? {
            write_change(&mut archive, &change).await?;
            count += 1;
        }
    }
    write_end(&mut archive).await?;
    Ok(CaptureOutcome {
        archive,
        last_lsn,
        count,
    })
}

/// 슬롯을 주어진 LSN까지 전진시킨다(저장 성공 후 호출 — 이후 그 변경은 다시 안 읽힘).
pub async fn advance_slot(client: &Client, slot: &str, lsn: &str) -> Result<()> {
    let lsn: PgLsn = lsn
        .parse()
        .map_err(|_| XBackupError::Failure(format!("LSN 파싱 실패: {lsn}")))?;
    client
        .execute("SELECT pg_replication_slot_advance($1, $2)", &[&slot, &lsn])
        .await
        .map_err(|e| XBackupError::Failure(format!("slot advance 실패: {e}")))?;
    Ok(())
}

/// 슬롯 존재 여부.
pub async fn slot_exists(client: &Client, slot: &str) -> Result<bool> {
    client
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM pg_replication_slots WHERE slot_name=$1)",
            &[&slot],
        )
        .await
        .map(|r| r.get(0))
        .map_err(|e| XBackupError::Failure(format!("slot 확인 실패: {e}")))
}

/// 증분 아카이브를 복구 대상에 적용한다. `max_commit_micros`가 `Some`이면 그 시각 이하 변경만
/// (PITR `--at`). 반환은 적용한 변경 수.
pub async fn apply<R: AsyncRead + Unpin>(
    reader: &mut R,
    client: &Client,
    max_commit_micros: Option<i64>,
) -> Result<u64> {
    // 헤더 확인.
    match read_frame(reader).await? {
        IncrFrame::Header(h) => {
            let fmt = h.get_str("format").unwrap_or("");
            if fmt != INCR_FORMAT_ID {
                return Err(XBackupError::Failure(format!(
                    "PG 증분 포맷 불일치: '{fmt}'"
                )));
            }
        }
        other => {
            return Err(XBackupError::Failure(format!(
                "PG 증분 헤더 누락: {other:?}"
            )))
        }
    }
    // FK/트리거 우회(적용 순서 무관). 세션 한정.
    let _ = client
        .batch_execute("SET session_replication_role = replica")
        .await;

    let mut casttype_cache: HashMap<(String, String), Vec<String>> = HashMap::new();
    let mut applied = 0u64;
    loop {
        match read_frame(reader).await? {
            IncrFrame::Header(_) => return Err(XBackupError::Failure("PG 증분 헤더 중복".into())),
            IncrFrame::Change(c) => {
                if let Some(max) = max_commit_micros {
                    if c.commit_unix_micros > max {
                        continue; // PITR 목표 이후 — 건너뜀.
                    }
                }
                let key = (c.schema.clone(), c.table.clone());
                if !casttype_cache.contains_key(&key) {
                    let types = column_casttypes(client, &c.schema, &c.table).await?;
                    casttype_cache.insert(key.clone(), types);
                }
                let casttypes = &casttype_cache[&key];
                if let Some((sql, params)) = build_dml(&c, casttypes) {
                    let refs: Vec<&(dyn ToSql + Sync)> =
                        params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
                    client.execute(&sql, &refs).await.map_err(|e| {
                        XBackupError::Failure(format!(
                            "{}.{} 증분 적용 실패: {e}\n  SQL: {sql}",
                            c.schema, c.table
                        ))
                    })?;
                    applied += 1;
                }
            }
            IncrFrame::End => break,
        }
    }
    Ok(applied)
}

/// 복구 대상 테이블의 컬럼별 캐스트 타입(format_type)을 컬럼 순서대로 조회한다.
async fn column_casttypes(client: &Client, schema: &str, table: &str) -> Result<Vec<String>> {
    let rows = client
        .query(
            "SELECT a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod) \
             FROM pg_attribute a \
             WHERE a.attrelid = (quote_ident($1)||'.'||quote_ident($2))::regclass \
             AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum",
            &[&schema, &table],
        )
        .await
        .map_err(|e| XBackupError::Failure(format!("{schema}.{table} 컬럼 타입 조회 실패: {e}")))?;
    Ok(rows.iter().map(|r| r.get::<_, String>(1)).collect())
}

/// 컬럼 식별자를 표준 quote(쌍따옴표·내부 두 배).
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// 한 변경을 DML과 바인드 파라미터로 만든다(순수 — 단위 테스트 가능).
///
/// 값은 텍스트로 바인드하고 SQL에서 `$n::타입`으로 캐스트한다(타입은 컬럼 순서대로 `casttypes`).
/// Insert는 키 충돌 시 upsert(키 없으면 plain), Update/Delete는 키 컬럼으로 식별. 키가 없으면
/// 식별 불가라 `None`(호출자가 건너뜀).
fn build_dml(c: &Change, casttypes: &[String]) -> Option<(String, Vec<Option<String>>)> {
    if c.colnames.len() != casttypes.len() {
        return None; // 스키마 불일치(컬럼 수) — 안전하게 건너뜀.
    }
    let q = format!("{}.{}", quote_ident(&c.schema), quote_ident(&c.table));
    let ty = |i: usize| casttypes[i].clone();
    let key_positions: Vec<usize> = c
        .keycols
        .iter()
        .enumerate()
        .filter(|(_, k)| **k)
        .map(|(i, _)| i)
        .collect();

    match c.op {
        Op::Insert => {
            let n = c.colnames.len();
            let collist = c
                .colnames
                .iter()
                .map(|s| quote_ident(s))
                .collect::<Vec<_>>()
                .join(", ");
            let vals = (0..n)
                .map(|i| format!("${}::{}", i + 1, ty(i)))
                .collect::<Vec<_>>()
                .join(", ");
            let mut sql = format!("INSERT INTO {q} ({collist}) VALUES ({vals})");
            if !key_positions.is_empty() {
                let keylist = key_positions
                    .iter()
                    .map(|&i| quote_ident(&c.colnames[i]))
                    .collect::<Vec<_>>()
                    .join(", ");
                let setlist = (0..n)
                    .filter(|i| !key_positions.contains(i))
                    .map(|i| {
                        let col = quote_ident(&c.colnames[i]);
                        format!("{col} = EXCLUDED.{col}")
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                if setlist.is_empty() {
                    sql.push_str(&format!(" ON CONFLICT ({keylist}) DO NOTHING"));
                } else {
                    sql.push_str(&format!(" ON CONFLICT ({keylist}) DO UPDATE SET {setlist}"));
                }
            }
            Some((sql, c.new_vals.clone()))
        }
        Op::Update => {
            if key_positions.is_empty() {
                return None; // 행 식별 불가.
            }
            // 키 값 출처: old/key 튜플이 있으면 그것, 없으면(키 미변경) 신규 튜플.
            let keysrc = if c.key_vals.len() == c.colnames.len() {
                &c.key_vals
            } else {
                &c.new_vals
            };
            let mut params: Vec<Option<String>> = Vec::new();
            let mut idx = 1;
            let set = (0..c.colnames.len())
                .map(|i| {
                    params.push(c.new_vals.get(i).cloned().flatten());
                    let s = format!("{} = ${}::{}", quote_ident(&c.colnames[i]), idx, ty(i));
                    idx += 1;
                    s
                })
                .collect::<Vec<_>>()
                .join(", ");
            let whr = key_positions
                .iter()
                .map(|&i| {
                    params.push(keysrc.get(i).cloned().flatten());
                    let s = format!("{} = ${}::{}", quote_ident(&c.colnames[i]), idx, ty(i));
                    idx += 1;
                    s
                })
                .collect::<Vec<_>>()
                .join(" AND ");
            Some((format!("UPDATE {q} SET {set} WHERE {whr}"), params))
        }
        Op::Delete => {
            if key_positions.is_empty() {
                return None;
            }
            let mut params: Vec<Option<String>> = Vec::new();
            let mut idx = 1;
            let whr = key_positions
                .iter()
                .map(|&i| {
                    params.push(c.key_vals.get(i).cloned().flatten());
                    let s = format!("{} = ${}::{}", quote_ident(&c.colnames[i]), idx, ty(i));
                    idx += 1;
                    s
                })
                .collect::<Vec<_>>()
                .join(" AND ");
            Some((format!("DELETE FROM {q} WHERE {whr}"), params))
        }
    }
}

// ───────────────────────── 증분 아카이브 프레이밍(BSON, 자기 길이) ─────────────────────────

enum IncrFrame {
    Header(Document),
    Change(Change),
    End,
}

impl std::fmt::Debug for IncrFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IncrFrame::Header(_) => write!(f, "Header"),
            IncrFrame::Change(_) => write!(f, "Change"),
            IncrFrame::End => write!(f, "End"),
        }
    }
}

async fn write_header<W: AsyncWrite + Unpin>(w: &mut W, created_at: &str) -> Result<()> {
    write_doc(
        w,
        TAG_HEADER,
        &bson::doc! { "format": INCR_FORMAT_ID, "created_at": created_at },
    )
    .await
}

async fn write_change<W: AsyncWrite + Unpin>(w: &mut W, c: &Change) -> Result<()> {
    write_doc(w, TAG_CHANGE, &change_to_doc(c)).await
}

async fn write_end<W: AsyncWrite + Unpin>(w: &mut W) -> Result<()> {
    w.write_all(&[TAG_END])
        .await
        .map_err(|e| XBackupError::Failure(format!("증분 끝 태그 쓰기 실패: {e}")))
}

async fn write_doc<W: AsyncWrite + Unpin>(w: &mut W, tag: u8, doc: &Document) -> Result<()> {
    let mut buf = Vec::new();
    doc.to_writer(&mut buf)
        .map_err(|e| XBackupError::Failure(format!("증분 프레임 직렬화 실패: {e}")))?;
    w.write_all(&[tag])
        .await
        .map_err(|e| XBackupError::Failure(format!("증분 태그 쓰기 실패: {e}")))?;
    w.write_all(&buf)
        .await
        .map_err(|e| XBackupError::Failure(format!("증분 본문 쓰기 실패: {e}")))?;
    Ok(())
}

async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<IncrFrame> {
    let mut tag = [0u8; 1];
    match r.read_exact(&mut tag).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(IncrFrame::End),
        Err(e) => return Err(XBackupError::Failure(format!("증분 태그 읽기 실패: {e}"))),
    }
    match tag[0] {
        TAG_END => Ok(IncrFrame::End),
        TAG_HEADER => Ok(IncrFrame::Header(read_doc(r).await?)),
        TAG_CHANGE => Ok(IncrFrame::Change(doc_to_change(&read_doc(r).await?)?)),
        other => Err(XBackupError::Failure(format!(
            "증분 프레임 태그 손상: 0x{other:02x}"
        ))),
    }
}

async fn read_doc<R: AsyncRead + Unpin>(r: &mut R) -> Result<Document> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)
        .await
        .map_err(|e| XBackupError::Failure(format!("증분 길이 읽기 실패: {e}")))?;
    let len = u32::from_le_bytes(len_buf);
    if !(5..=64 * 1024 * 1024).contains(&len) {
        return Err(XBackupError::Failure(format!(
            "증분 프레임 길이 비정상: {len}"
        )));
    }
    let mut buf = vec![0u8; len as usize];
    buf[..4].copy_from_slice(&len_buf);
    r.read_exact(&mut buf[4..])
        .await
        .map_err(|e| XBackupError::Failure(format!("증분 본문 읽기 실패: {e}")))?;
    Document::from_reader(&buf[..])
        .map_err(|e| XBackupError::Failure(format!("증분 프레임 파싱 실패: {e}")))
}

fn change_to_doc(c: &Change) -> Document {
    let vals = |v: &[Option<String>]| {
        v.iter()
            .map(|x| match x {
                Some(s) => Bson::String(s.clone()),
                None => Bson::Null,
            })
            .collect::<Vec<_>>()
    };
    let op = match c.op {
        Op::Insert => "I",
        Op::Update => "U",
        Op::Delete => "D",
    };
    bson::doc! {
        "op": op,
        "schema": &c.schema,
        "table": &c.table,
        "cols": c.colnames.iter().map(|s| Bson::String(s.clone())).collect::<Vec<_>>(),
        "keys": c.keycols.iter().map(|b| Bson::Boolean(*b)).collect::<Vec<_>>(),
        "new": vals(&c.new_vals),
        "key": vals(&c.key_vals),
        "ts": c.commit_unix_micros,
    }
}

fn doc_to_change(d: &Document) -> Result<Change> {
    let strs = |arr: &str| -> Vec<String> {
        d.get_array(arr)
            .map(|a| {
                a.iter()
                    .filter_map(|b| b.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    };
    let opt_vals = |arr: &str| -> Vec<Option<String>> {
        d.get_array(arr)
            .map(|a| a.iter().map(|b| b.as_str().map(String::from)).collect())
            .unwrap_or_default()
    };
    let op = match d.get_str("op").unwrap_or("") {
        "I" => Op::Insert,
        "U" => Op::Update,
        "D" => Op::Delete,
        other => return Err(XBackupError::Failure(format!("증분 op 손상: '{other}'"))),
    };
    Ok(Change {
        op,
        schema: d.get_str("schema").unwrap_or("").to_string(),
        table: d.get_str("table").unwrap_or("").to_string(),
        colnames: strs("cols"),
        keycols: d
            .get_array("keys")
            .map(|a| a.iter().map(|b| b.as_bool().unwrap_or(false)).collect())
            .unwrap_or_default(),
        new_vals: opt_vals("new"),
        key_vals: opt_vals("key"),
        commit_unix_micros: d.get_i64("ts").unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(op: Op) -> Change {
        Change {
            op,
            schema: "public".into(),
            table: "t".into(),
            colnames: vec!["id".into(), "name".into()],
            keycols: vec![true, false],
            new_vals: vec![Some("7".into()), Some("a".into())],
            key_vals: vec![Some("7".into()), None],
            commit_unix_micros: 123,
        }
    }

    #[tokio::test]
    async fn archive_round_trip() {
        let mut buf = Vec::new();
        write_header(&mut buf, "2026-06-14T00:00:00Z")
            .await
            .unwrap();
        write_change(&mut buf, &ch(Op::Insert)).await.unwrap();
        write_change(&mut buf, &ch(Op::Delete)).await.unwrap();
        write_end(&mut buf).await.unwrap();

        let mut r = std::io::Cursor::new(buf);
        assert!(matches!(
            read_frame(&mut r).await.unwrap(),
            IncrFrame::Header(_)
        ));
        match read_frame(&mut r).await.unwrap() {
            IncrFrame::Change(c) => {
                assert_eq!(c.op, Op::Insert);
                assert_eq!(c.colnames, vec!["id", "name"]);
                assert_eq!(c.new_vals, vec![Some("7".into()), Some("a".into())]);
            }
            f => panic!("change 기대, {f:?}"),
        }
        assert!(matches!(
            read_frame(&mut r).await.unwrap(),
            IncrFrame::Change(_)
        ));
        assert!(matches!(read_frame(&mut r).await.unwrap(), IncrFrame::End));
    }

    #[test]
    fn dml_insert_upsert() {
        let types = vec!["integer".to_string(), "text".to_string()];
        let (sql, params) = build_dml(&ch(Op::Insert), &types).unwrap();
        assert!(sql.contains("INSERT INTO \"public\".\"t\" (\"id\", \"name\")"));
        assert!(sql.contains("$1::integer"));
        assert!(sql.contains("ON CONFLICT (\"id\") DO UPDATE SET \"name\" = EXCLUDED.\"name\""));
        assert_eq!(params, vec![Some("7".into()), Some("a".into())]);
    }

    #[test]
    fn dml_update_by_key() {
        let types = vec!["integer".to_string(), "text".to_string()];
        let (sql, params) = build_dml(&ch(Op::Update), &types).unwrap();
        assert!(sql.starts_with("UPDATE \"public\".\"t\" SET"));
        assert!(sql.contains("WHERE \"id\" = $3::integer"));
        // SET id,name (params 1,2) + WHERE id (param 3).
        assert_eq!(params.len(), 3);
    }

    #[test]
    fn dml_delete_by_key() {
        let types = vec!["integer".to_string(), "text".to_string()];
        let (sql, params) = build_dml(&ch(Op::Delete), &types).unwrap();
        assert_eq!(
            sql,
            "DELETE FROM \"public\".\"t\" WHERE \"id\" = $1::integer"
        );
        assert_eq!(params, vec![Some("7".into())]);
    }

    #[test]
    fn dml_update_no_key_skipped() {
        let mut c = ch(Op::Update);
        c.keycols = vec![false, false];
        assert!(build_dml(&c, &["integer".into(), "text".into()]).is_none());
    }
}
