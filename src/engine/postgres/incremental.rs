//! PG 증분 — logical decoding(pgoutput) slot에서 변경을 캡처해 아카이브하고, 복구 시 DML로 적용.
//!
//! Mongo oplog 증분과 동형이되 PG WAL을 logical decoding으로 읽는다:
//! - 풀 백업 시 [`recreate_slot_and_publication`]으로 슬롯을 (재)생성해 base에 정렬하고,
//!   증분 시 [`slot_health`]로 gap(슬롯 유실/invalidated)을 감지해 풀 승격(FR-2).
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

/// publication(FOR ALL TABLES)을 보장한다(없으면 생성). slot은 다루지 않는다.
async fn ensure_publication(client: &Client, publication: &str) -> Result<()> {
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
    Ok(())
}

/// 풀 백업용으로 슬롯을 **새로 만든다**(있으면 drop 후 재생성). publication은 재사용(FOR ALL
/// TABLES). 반환 = 새 슬롯의 consistent LSN.
///
/// ## 왜 매 풀 백업마다 재생성하나(C2 — 스타 모델 base 바인딩)
/// PG 증분은 모두 "가장 최신 풀백업"에 체인된다(스타 모델, [`select_pg_full_base`]). 슬롯을
/// **재사용**하면, 2번째 풀백업 이후에도 슬롯은 옛 base 위치에서 계속 전진하므로 새 증분이
/// **시작된 적 없는 base에 묶이는** 조용한 체인 붕괴가 생긴다(FR-2 위반). 매 풀백업마다 슬롯을
/// 재생성하면 살아있는 슬롯이 항상 최신 풀백업(=선택되는 base)에 정렬된다.
///
/// 옛 슬롯을 drop하면 이전 base의 미캡처 증분 WAL은 버려지지만, 새 풀백업이 그 base를
/// 대체하므로(이후 증분은 새 base에 체인) 안전하다.
///
/// 이름은 sanitize된 식별자라 인젝션 안전(파라미터화 불가한 DDL이므로 직접 보간하지 않고
/// 함수 인자로 넘긴다).
///
/// > **잔여(정확한 H2 정렬):** 슬롯 consistent point(이 LSN)와 덤프 스냅샷 사이의 좁은
/// > 구간에 커밋된 변경은 base와 첫 증분에 **둘 다** 들어갈 수 있다(double-apply). 복구
/// > 적용이 idempotent(I=upsert, U/D=키 기반)라 일반적으로 무해하다. 구간을 0으로 만드는
/// > 정확한 정렬은 replication 프로토콜의 exported snapshot이 필요하며 후속 과제다.
pub async fn recreate_slot_and_publication(
    client: &Client,
    slot: &str,
    publication: &str,
) -> Result<String> {
    ensure_publication(client, publication).await?;

    if slot_exists(client, slot).await? {
        client
            .execute("SELECT pg_drop_replication_slot($1)", &[&slot])
            .await
            .map_err(|e| XBackupError::Failure(format!("기존 slot drop 실패: {e}")))?;
    }
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

/// 증분 슬롯 건강도 — gap 판정(FR-2)에 쓴다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotHealth {
    /// 정상 — 캡처 가능.
    Active,
    /// 슬롯이 invalidated(WAL이 max_slot_wal_keep_size 초과로 제거됨, wal_status='lost')거나
    /// restart_lsn이 NULL — 체인이 끊겼다. 풀 백업으로 승격해야 한다.
    Lost,
    /// 슬롯이 아예 없음 — base/슬롯 설정이 사라졌다. 풀 백업으로 (재)생성해야 한다.
    Missing,
}

/// 증분 슬롯의 건강도를 판정한다(gap 감지, FR-2). 캡처 전에 호출해 Lost/Missing이면
/// 호출자가 풀 백업으로 승격한다(exit 4).
pub async fn slot_health(client: &Client, slot: &str) -> Result<SlotHealth> {
    let row = client
        .query_opt(
            "SELECT wal_status, (restart_lsn IS NULL) AS restart_null \
             FROM pg_replication_slots WHERE slot_name=$1",
            &[&slot],
        )
        .await
        .map_err(|e| XBackupError::Failure(format!("slot 건강도 조회 실패: {e}")))?;
    match row {
        None => Ok(SlotHealth::Missing),
        Some(r) => {
            // wal_status: 'reserved'|'extended'|'unreserved'|'lost'(PG13+). NULL일 수도 있다.
            let wal_status: Option<String> = r.get("wal_status");
            let restart_null: bool = r.get("restart_null");
            if restart_null || wal_status.as_deref() == Some("lost") {
                Ok(SlotHealth::Lost)
            } else {
                Ok(SlotHealth::Active)
            }
        }
    }
}

/// 한 번의 capture가 peek할 최대 변경 수(`upto_nchanges`). 메모리 상한을 위해 둔다(H4).
///
/// pgoutput은 트랜잭션 경계에서만 멈추므로 실제 반환은 이 값을 약간 넘길 수 있고, 트랜잭션이
/// 쪼개지지 않는다. 한도를 넘는 backlog는 다음 증분 실행이 이어서 캡처한다(슬롯은 캡처한
/// 만큼만 전진). 정상 주기 증분은 이 한도에 한참 못 미친다.
const MAX_CHANGES_PER_CAPTURE: i32 = 50_000;

/// 캡처 결과 — 아카이브 바이트, 마지막 LSN(없으면 변경 0), 변경 수, backlog 잔여 여부.
pub struct CaptureOutcome {
    pub archive: Vec<u8>,
    pub last_lsn: Option<String>,
    pub count: u64,
    /// `upto_nchanges` 한도에 걸려 더 남은 backlog가 있을 수 있는지(H4 — 다음 실행이 이어감).
    pub more_pending: bool,
}

/// 슬롯에서 변경을 peek(비소비)해 디코드 → `xb-pg-incr-v1` 아카이브 바이트로 만든다.
///
/// peek라 슬롯을 전진시키지 않는다 — 호출자가 **저장 성공 후** [`advance_slot`]로 전진한다
/// (저장 실패 시 다음에 재캡처 — 적용이 idempotent라 안전).
///
/// **메모리(H4):** `upto_nchanges`([`MAX_CHANGES_PER_CAPTURE`])로 한 번에 가져오는 변경
/// 수를 제한해 backlog가 커도 상주 메모리를 상수 상한으로 묶는다. 한도 초과분은 다음 실행이
/// 이어서 캡처한다.
pub async fn capture(client: &Client, slot: &str, publication: &str) -> Result<CaptureOutcome> {
    if !slot_exists(client, slot).await? {
        return Err(XBackupError::PrecheckFailed(format!(
            "logical replication slot '{slot}'이 없습니다 — 먼저 풀 백업으로 슬롯을 만드세요(gap)"
        )));
    }
    let rows = client
        .query(
            "SELECT lsn::text, data FROM pg_logical_slot_peek_binary_changes(\
                $1, NULL, $3::int, 'proto_version','1','publication_names',$2)",
            &[&slot, &publication, &MAX_CHANGES_PER_CAPTURE],
        )
        .await
        .map_err(|e| XBackupError::Failure(format!("logical 변경 peek 실패: {e}")))?;

    // upto_nchanges는 B/C/R 등 모든 디코드 메시지를 세므로, 반환 행 수가 한도에 근접하면
    // backlog가 더 남았을 수 있다(다음 실행이 이어감).
    let more_pending = rows.len() as i32 >= MAX_CHANGES_PER_CAPTURE;

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
    if more_pending {
        tracing::warn!(
            "PG 증분 capture가 {MAX_CHANGES_PER_CAPTURE} 변경 한도에 도달 — 남은 backlog는 \
             다음 증분 실행이 이어서 캡처합니다(메모리 보호)"
        );
    }
    Ok(CaptureOutcome {
        archive,
        last_lsn,
        count,
        more_pending,
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

    let mut meta_cache: HashMap<(String, String), HashMap<String, ColMeta>> = HashMap::new();
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
                if !meta_cache.contains_key(&key) {
                    let m = column_meta_map(client, &c.schema, &c.table).await?;
                    meta_cache.insert(key.clone(), m);
                }
                let map = &meta_cache[&key];
                // 스트림 컬럼명 순서대로 대상 카탈로그의 타입/identity 메타를 정렬한다 —
                // pgoutput은 STORED generated 컬럼을 제외하므로 위치가 아닌 **이름**으로 맞춘다.
                let mut cols = Vec::with_capacity(c.colnames.len());
                for name in &c.colnames {
                    match map.get(name) {
                        Some(meta) => cols.push(meta.clone()),
                        None => {
                            return Err(XBackupError::Failure(format!(
                                "{}.{} 증분 적용: 대상에 컬럼 '{name}'이 없습니다(스키마 불일치)",
                                c.schema, c.table
                            )))
                        }
                    }
                }
                match build_dml(&c, &cols) {
                    Some((sql, params)) => {
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
                    // U/D인데 키가 없으면(PK/REPLICA IDENTITY 부재) 행을 식별할 수 없어 건너뛴다.
                    // 무성 데이터 손실이 되지 않게 경고로 남긴다(테이블에 PK 또는 REPLICA
                    // IDENTITY FULL이 필요).
                    None if !matches!(c.op, Op::Insert) => {
                        tracing::warn!(
                            "{}.{} {:?} 변경을 건너뜀 — 키 없음(PK/REPLICA IDENTITY 필요)",
                            c.schema,
                            c.table,
                            c.op
                        );
                    }
                    None => {}
                }
            }
            IncrFrame::End => break,
        }
    }

    // 변경을 적용한 테이블의 identity/serial 시퀀스를 max(컬럼)으로 재동기화한다.
    // 증분은 OVERRIDING SYSTEM VALUE로 명시 id를 넣어 시퀀스를 전진시키지 않으므로,
    // 이대로 두면 복구 후 새 insert가 기존 행과 PK 충돌한다.
    for (schema, table) in meta_cache.keys() {
        if let Err(e) = resync_sequences(client, schema, table).await {
            tracing::warn!("{schema}.{table} 시퀀스 재동기화 실패(무시): {e}");
        }
    }
    Ok(applied)
}

/// 테이블의 identity/serial 시퀀스를 현재 max(컬럼)으로 맞춘다(복구 후 새 insert 충돌 방지).
async fn resync_sequences(client: &Client, schema: &str, table: &str) -> Result<()> {
    let q = format!("{}.{}", quote_ident(schema), quote_ident(table));
    // identity 또는 serial(소유 시퀀스가 있는) 컬럼과 그 시퀀스 이름을 찾는다.
    // 정규화된 따옴표 식별자(q)를 단일 text 파라미터로 넘긴다($1::text::regclass로 타입 고정).
    let rows = client
        .query(
            "SELECT a.attname, pg_get_serial_sequence($1, a.attname) \
             FROM pg_attribute a \
             WHERE a.attrelid = $1::text::regclass \
             AND a.attnum > 0 AND NOT a.attisdropped \
             AND pg_get_serial_sequence($1, a.attname) IS NOT NULL",
            &[&q],
        )
        .await
        .map_err(|e| XBackupError::Failure(format!("시퀀스 조회 실패: {e}")))?;

    for r in &rows {
        let col: String = r.get(0);
        let seq: String = r.get(1);
        // 현재 최대값(없으면 NULL). int2/4/8 모두 i64로 받는다.
        let max: Option<i64> = client
            .query_one(
                &format!("SELECT max({})::bigint FROM {q}", quote_ident(&col)),
                &[],
            )
            .await
            .map_err(|e| XBackupError::Failure(format!("max 조회 실패: {e}")))?
            .get(0);
        // setval(seq, value, is_called): max가 있으면 (max,true)→다음=max+1, 없으면 (1,false)→다음=1.
        let value = max.unwrap_or(1);
        let is_called = max.is_some();
        client
            .execute(
                "SELECT setval($1::text::regclass, $2, $3)",
                &[&seq, &value, &is_called],
            )
            .await
            .map_err(|e| XBackupError::Failure(format!("setval 실패({seq}): {e}")))?;
    }
    Ok(())
}

/// 복구 대상 컬럼 메타 — 캐스트 타입(format_type)과 GENERATED ALWAYS identity 여부.
#[derive(Debug, Clone)]
struct ColMeta {
    /// `$n::<casttype>` 캐스트에 쓸 타입(예: `bigint`, `numeric(12,2)`).
    casttype: String,
    /// GENERATED ALWAYS AS IDENTITY 컬럼(attidentity='a'). INSERT엔 OVERRIDING SYSTEM
    /// VALUE가 필요하고, UPDATE SET에는 넣을 수 없다("can only be updated to DEFAULT").
    generated_always: bool,
}

/// 대상 테이블의 컬럼명→메타 맵(타입·identity). 이름으로 조회해 스트림 컬럼 순서에 맞춘다.
async fn column_meta_map(
    client: &Client,
    schema: &str,
    table: &str,
) -> Result<HashMap<String, ColMeta>> {
    let rows = client
        .query(
            "SELECT a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod), \
             (a.attidentity = 'a') AS gen_always \
             FROM pg_attribute a \
             WHERE a.attrelid = (quote_ident($1)||'.'||quote_ident($2))::regclass \
             AND a.attnum > 0 AND NOT a.attisdropped",
            &[&schema, &table],
        )
        .await
        .map_err(|e| XBackupError::Failure(format!("{schema}.{table} 컬럼 메타 조회 실패: {e}")))?;
    Ok(rows
        .iter()
        .map(|r| {
            (
                r.get::<_, String>(0),
                ColMeta {
                    casttype: r.get::<_, String>(1),
                    generated_always: r.get::<_, bool>(2),
                },
            )
        })
        .collect())
}

/// 컬럼 식별자를 표준 quote(쌍따옴표·내부 두 배).
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// 한 변경을 DML과 바인드 파라미터로 만든다(순수 — 단위 테스트 가능).
///
/// 값은 텍스트로 바인드하고 SQL에서 `$n::타입`으로 캐스트한다(`cols`는 스트림 컬럼 순서에
/// 맞춘 대상 컬럼 메타). 규칙:
/// - **Insert**: 모든 컬럼을 넣고 `OVERRIDING SYSTEM VALUE`(GENERATED ALWAYS identity에
///   명시값을 넣기 위함 — identity가 없어도 무해). 키 충돌 시 upsert(키 없으면 plain).
/// - **Update**: 키 컬럼으로 식별. SET에는 GENERATED ALWAYS identity를 제외한다(불가).
/// - **Delete**: 키 컬럼으로 식별.
///
/// 키가 없으면 식별 불가라 `None`(호출자가 건너뜀). `cols.len() != colnames.len()`(이론상
/// 호출자가 정렬 보장)면 안전하게 `None`.
fn build_dml(c: &Change, cols: &[ColMeta]) -> Option<(String, Vec<Option<String>>)> {
    if c.colnames.len() != cols.len() {
        return None;
    }
    let q = format!("{}.{}", quote_ident(&c.schema), quote_ident(&c.table));
    let ty = |i: usize| cols[i].casttype.clone();
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
                .map(|i| format!("${}::text::{}", i + 1, ty(i)))
                .collect::<Vec<_>>()
                .join(", ");
            // OVERRIDING SYSTEM VALUE: GENERATED ALWAYS identity에 캡처된 명시값을 그대로
            // 넣기 위함. identity 컬럼이 없으면 PG가 무시한다(안전).
            let mut sql =
                format!("INSERT INTO {q} ({collist}) OVERRIDING SYSTEM VALUE VALUES ({vals})");
            if !key_positions.is_empty() {
                let keylist = key_positions
                    .iter()
                    .map(|&i| quote_ident(&c.colnames[i]))
                    .collect::<Vec<_>>()
                    .join(", ");
                // upsert SET: 키도 GENERATED ALWAYS도 아닌 컬럼만(둘 다 갱신 불가/무의미).
                let setlist = (0..n)
                    .filter(|i| !key_positions.contains(i) && !cols[*i].generated_always)
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
            // SET: GENERATED ALWAYS identity 제외(명시값 SET 불가) + unchanged-TOAST('u') 제외
            // (H3: 값이 실리지 않은 컬럼을 NULL로 덮어쓰면 기존 값이 파손됨). 나머지 전부.
            let set = (0..c.colnames.len())
                .filter(|i| !cols[*i].generated_always)
                .filter(|i| !c.new_unchanged.get(*i).copied().unwrap_or(false))
                .map(|i| {
                    params.push(c.new_vals.get(i).cloned().flatten());
                    let s = format!(
                        "{} = ${}::text::{}",
                        quote_ident(&c.colnames[i]),
                        idx,
                        ty(i)
                    );
                    idx += 1;
                    s
                })
                .collect::<Vec<_>>()
                .join(", ");
            if set.is_empty() {
                return None; // SET할 컬럼이 없음(전부 키/identity) — no-op.
            }
            let whr = key_positions
                .iter()
                .map(|&i| {
                    params.push(keysrc.get(i).cloned().flatten());
                    let s = format!(
                        "{} = ${}::text::{}",
                        quote_ident(&c.colnames[i]),
                        idx,
                        ty(i)
                    );
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
                    let s = format!(
                        "{} = ${}::text::{}",
                        quote_ident(&c.colnames[i]),
                        idx,
                        ty(i)
                    );
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
        // unchanged-TOAST 마스크(H3) — new[i]가 'u'(미변경)라 적용 시 SET에서 제외해야 함.
        "unchanged": c.new_unchanged.iter().map(|b| Bson::Boolean(*b)).collect::<Vec<_>>(),
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
    let new_vals = opt_vals("new");
    // unchanged 마스크(H3) — 구 아카이브엔 없을 수 있으므로 없으면 모두 false(기존 동작).
    let new_unchanged = match d.get_array("unchanged") {
        Ok(a) => a.iter().map(|b| b.as_bool().unwrap_or(false)).collect(),
        Err(_) => vec![false; new_vals.len()],
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
        new_vals,
        new_unchanged,
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
            new_unchanged: vec![false, false],
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

    /// 테스트용 컬럼 메타(타입, generated_always).
    fn cm(ty: &str, gen: bool) -> ColMeta {
        ColMeta {
            casttype: ty.into(),
            generated_always: gen,
        }
    }

    #[test]
    fn dml_insert_upsert() {
        let cols = vec![cm("integer", false), cm("text", false)];
        let (sql, params) = build_dml(&ch(Op::Insert), &cols).unwrap();
        assert!(sql.contains("INSERT INTO \"public\".\"t\" (\"id\", \"name\")"));
        assert!(sql.contains("OVERRIDING SYSTEM VALUE VALUES"));
        assert!(sql.contains("$1::text::integer"));
        assert!(sql.contains("ON CONFLICT (\"id\") DO UPDATE SET \"name\" = EXCLUDED.\"name\""));
        assert_eq!(params, vec![Some("7".into()), Some("a".into())]);
    }

    /// GENERATED ALWAYS identity 키만 있고 비키 컬럼이 전부 generated면 upsert는 DO NOTHING.
    #[test]
    fn dml_insert_generated_always_key_do_nothing() {
        // id=키+generated_always, 비키 컬럼 없음.
        let mut c = ch(Op::Insert);
        c.colnames = vec!["id".into()];
        c.keycols = vec![true];
        c.new_vals = vec![Some("7".into())];
        let cols = vec![cm("bigint", true)];
        let (sql, _) = build_dml(&c, &cols).unwrap();
        assert!(sql.contains("OVERRIDING SYSTEM VALUE"));
        assert!(sql.contains("ON CONFLICT (\"id\") DO NOTHING"), "sql={sql}");
    }

    #[test]
    fn dml_update_by_key() {
        let cols = vec![cm("integer", false), cm("text", false)];
        let (sql, params) = build_dml(&ch(Op::Update), &cols).unwrap();
        assert!(sql.starts_with("UPDATE \"public\".\"t\" SET"));
        assert!(sql.contains("WHERE \"id\" = $3::text::integer"));
        // SET id,name (params 1,2) + WHERE id (param 3).
        assert_eq!(params.len(), 3);
    }

    /// UPDATE는 GENERATED ALWAYS identity 컬럼을 SET에서 제외한다(명시 SET 불가).
    #[test]
    fn dml_update_excludes_generated_always_from_set() {
        let cols = vec![cm("bigint", true), cm("text", false)];
        let (sql, params) = build_dml(&ch(Op::Update), &cols).unwrap();
        // SET에는 name만(id는 generated_always라 제외), WHERE에는 id(키).
        assert!(sql.contains("SET \"name\" = $1::text"), "sql={sql}");
        assert!(
            !sql.contains("SET \"id\""),
            "id가 SET에 들어가면 안 됨: {sql}"
        );
        assert!(sql.contains("WHERE \"id\" = $2::text::bigint"), "sql={sql}");
        // params: SET name(1) + WHERE id(2).
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn dml_delete_by_key() {
        let cols = vec![cm("integer", false), cm("text", false)];
        let (sql, params) = build_dml(&ch(Op::Delete), &cols).unwrap();
        assert_eq!(
            sql,
            "DELETE FROM \"public\".\"t\" WHERE \"id\" = $1::text::integer"
        );
        assert_eq!(params, vec![Some("7".into())]);
    }

    #[test]
    fn dml_update_no_key_skipped() {
        let mut c = ch(Op::Update);
        c.keycols = vec![false, false];
        assert!(build_dml(&c, &[cm("integer", false), cm("text", false)]).is_none());
    }

    /// H3 — UPDATE의 unchanged-TOAST 컬럼은 SET에서 제외되고 NULL로 덮어쓰지 않는다.
    #[test]
    fn dml_update_excludes_unchanged_toast_from_set() {
        // id(키), body(unchanged TOAST 'u'), note(변경됨). body는 SET에서 빠져야 한다.
        let mut c = ch(Op::Update);
        c.colnames = vec!["id".into(), "body".into(), "note".into()];
        c.keycols = vec![true, false, false];
        c.new_vals = vec![Some("7".into()), None, Some("hello".into())];
        c.new_unchanged = vec![false, true, false]; // body만 unchanged
        c.key_vals = vec![Some("7".into()), None, None];
        let cols = vec![cm("integer", false), cm("text", false), cm("text", false)];
        let (sql, params) = build_dml(&c, &cols).unwrap();
        // 핵심(H3): body(unchanged TOAST)는 SET·params에 절대 없어야 한다(기존 값 보존).
        assert!(
            !sql.contains("\"body\""),
            "unchanged body가 SET에 포함됨: {sql}"
        );
        // id·note는 SET에 그대로(기존 동작 — 키 컬럼도 SET에 포함). body만 빠진다.
        assert!(
            sql.contains("SET \"id\" = $1::text::integer, \"note\" = $2::text::text"),
            "sql={sql}"
        );
        assert!(
            sql.contains("WHERE \"id\" = $3::text::integer"),
            "sql={sql}"
        );
        // params: SET id(1), note(2) + WHERE id(3) — body는 바인드되지 않음.
        assert_eq!(
            params,
            vec![Some("7".into()), Some("hello".into()), Some("7".into())]
        );
    }

    /// H3 — unchanged 마스크가 증분 아카이브(BSON)를 round-trip한다.
    #[tokio::test]
    async fn unchanged_mask_round_trips_through_archive() {
        let mut c = ch(Op::Update);
        c.new_unchanged = vec![false, true];
        let mut buf = Vec::new();
        write_change(&mut buf, &c).await.unwrap();
        let mut r = std::io::Cursor::new(buf);
        match read_frame(&mut r).await.unwrap() {
            IncrFrame::Change(back) => assert_eq!(back.new_unchanged, vec![false, true]),
            f => panic!("change 기대, {f:?}"),
        }
    }
}
