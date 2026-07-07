//! 네이티브 oplog 재생 — 드라이버 `applyOps`로 증분 슬라이스를 직접 적용(외부 도구 0).
//!
//! 종전 PITR은 슬라이스를 임시 `oplog.bson`으로 쓴 뒤 `mongorestore --oplogReplay`를
//! 스폰했다 — 런타임 외부 의존의 마지막 하드 지점이었다(로드맵 P0-1). 이 모듈은 그 재생을
//! **드라이버 `applyOps` 커맨드**로 대체한다. PG(`pg_pitr`)·MySQL(`mysql_pitr`)이 이미
//! 드라이버로 직접 재생하는 것과 같은 구조다. 부수 효과로 임시 파일 배치가 사라져
//! 재생 단계도 전 구간 스트리밍이 된다(PRD §7 원칙의 예외 제거).
//!
//! ## 입력 포맷
//! 증분 슬라이스(복호화·해제 후)는 **raw oplog 엔트리 BSON 문서의 연결 스트림**이다
//! ([`super::oplog`] 캡처 산출물 — mongodump의 `oplog.bson`과 바이트 동형). natural
//! order(= ts 오름차순)로 기록돼 있으므로 한 번의 순회로 순서대로 적용한다.
//!
//! ## 적용 규칙 — `mongorestore --oplogReplay` 의미론과의 대응
//! `applyOps`는 서버가 oplog 엔트리를 그대로 적용하는 커맨드로, mongorestore의
//! `--oplogReplay`도 내부적으로 이 커맨드를 쓴다. 따라서 `$v:2` update delta 등
//! 엔트리 의미론은 **서버가** 해석한다 — 클라이언트에서 재구현하지 않는다.
//! - **noop 스킵:** `op == "n"`은 적용 대상이 아니다.
//! - **제외 네임스페이스:** `local.*`(oplog 자신)·`config.*`(세션/트랜잭션 부기)·
//!   `admin.system.version`(FCV 문서)은 사용자 데이터가 아니므로 스킵한다.
//! - **`ui` 스트립:** 엔트리의 컬렉션 UUID(`ui`)는 **source** 컬렉션의 것이다. base
//!   복원이 target에 컬렉션을 새 UUID로 만들었으므로 그대로 두면 UUID 불일치로
//!   실패한다. 최상위·중첩(applyOps 내부) 엔트리 모두에서 제거한다
//!   (mongorestore가 `--preserveUUID` 미지정 시 하는 것과 동일).
//! - **세션/재시도 메타 스트립:** `lsid`/`txnNumber`/`stmtId`/`prevOpTime`/
//!   `preImageOpTime`/`postImageOpTime`/`needsRetryImage`는 재생 문맥(세션 없음)에서
//!   무의미하거나 거부 사유가 되므로 제거한다.
//! - **트랜잭션 재조립:** multi-doc 트랜잭션은 oplog에 `applyOps`(+`partialTxn`,
//!   prepared면 `prepare`) 체인으로 기록된다. 체인을 (lsid,txnNumber) 키로 버퍼링해
//!   커밋 지점에서 내부 op들을 펼쳐 일반 배치로 적용하고, abort면 버린다 —
//!   mongorestore의 txn buffer와 같은 전략(커밋 시점에 효과가 보이는 서버 의미론 보존).
//! - **배치:** CRUD 엔트리는 상한(개수/바이트)까지 모아 `applyOps` 한 번으로 보내고,
//!   커맨드(`c`) 엔트리는 순서 보존을 위해 배치를 먼저 비운 뒤 단독 적용한다.
//! - **limit(미만 의미):** `ts >= limit`인 첫 엔트리에서 중단한다. PITR의 "이하(<=)"
//!   매핑은 상위([`crate::pipeline::pitr`])가 limit을 `{target+1, 0}`으로 보정해 전달한다.
//!
//! ## 권한
//! `applyOps`는 강한 권한(`anyAction on anyResource` 수준)을 요구한다 — 이는
//! `mongorestore --oplogReplay`가 요구하던 것과 동일하므로 종전 대비 추가 요구가 아니다.
//! 권한 부족 시 에러 메시지에 사유를 담아 전파한다.

use std::collections::HashMap;

use bson::{doc, Bson, Document};
use mongodb::Client;
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::config::secret::Secret;
use crate::error::{Result, XBackupError};
use crate::manifest::schema::OplogTimestamp;

/// 한 `applyOps` 배치의 엔트리 수 상한.
const MAX_BATCH_OPS: usize = 1000;
/// 한 `applyOps` 배치의 스트림 바이트 근사 상한(커맨드 문서 16MiB 한계의 여유분).
const MAX_BATCH_BYTES: usize = 12 * 1024 * 1024;
/// 스트림에서 읽는 BSON 문서의 안전 상한(oplog 엔트리 최대 16MiB + 여유).
const MAX_ENTRY_BYTES: u32 = 32 * 1024 * 1024;

/// 재생 통계(보고용).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ApplyStats {
    /// 서버에 적용한 op 수(트랜잭션 내부 op 포함).
    pub applied: u64,
    /// 스킵한 엔트리 수(noop·제외 ns·abort된 트랜잭션).
    pub skipped: u64,
}

/// 드라이버 `applyOps` 기반 oplog 적용기.
pub struct OplogApplier {
    client: Client,
}

/// 엔트리 분류 결과 — 스트림 순회 루프가 취할 행동.
enum EntryAction {
    /// 적용 대상 아님(noop·제외 ns).
    Skip,
    /// 일반 엔트리(CRUD 또는 커맨드) — 배치/단독 적용.
    Apply(Document),
    /// 트랜잭션 체인 조각 — (키, 내부 op들)을 버퍼에 누적.
    TxnBuffer(String, Vec<Document>),
    /// 미준비(unprepared) 트랜잭션의 마지막 조각 — 내부 op들을 버퍼에 더한 뒤 즉시 커밋.
    TxnBufferThenCommit(String, Vec<Document>),
    /// 트랜잭션 커밋(prepared) — 키의 버퍼를 펼쳐 적용.
    TxnCommit(String),
    /// 트랜잭션 abort — 키의 버퍼 폐기.
    TxnAbort(String),
}

impl OplogApplier {
    /// 기존 드라이버 클라이언트로 적용기를 만든다.
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    /// URI 시크릿으로 연결해 적용기를 만든다([`super::oplog::OplogReader::connect`]와
    /// 동일한 연결 절차).
    pub async fn connect(uri: &Secret, timeout_secs: Option<u64>) -> Result<Self> {
        let options = super::conn::client_options(uri, timeout_secs).await?;
        let client = Client::with_options(options)
            .map_err(|e| XBackupError::Failure(format!("MongoDB 클라이언트 생성 실패: {e}")))?;
        Ok(Self::new(client))
    }

    /// oplog 엔트리 스트림을 순서대로 적용한다. `limit`가 있으면 `ts >= limit`인 첫
    /// 엔트리에서 중단한다(미만 의미 — 상위가 +1 보정한 값을 전달).
    pub async fn apply_stream<R: AsyncRead + Unpin>(
        &self,
        reader: &mut R,
        limit: Option<OplogTimestamp>,
    ) -> Result<ApplyStats> {
        let mut stats = ApplyStats::default();
        // CRUD 배치(순서 보존 — 커맨드/트랜잭션 flush 전에 반드시 먼저 비운다).
        let mut batch: Vec<Document> = Vec::new();
        let mut batch_bytes = 0usize;
        // 진행 중 트랜잭션 버퍼: (lsid,txnNumber) 키 → 내부 op 누적.
        let mut txns: HashMap<String, Vec<Document>> = HashMap::new();

        while let Some((entry, entry_bytes)) = read_bson_doc(reader).await? {
            // limit 컷 — 스트림은 ts 오름차순이므로 첫 초과 지점에서 전체 중단.
            if let Some(lim) = limit {
                if entry_ts(&entry)? >= lim {
                    break;
                }
            }

            match classify_entry(entry)? {
                EntryAction::Skip => stats.skipped += 1,
                EntryAction::Apply(op) => {
                    let is_command = op.get_str("op") == Ok("c");
                    if is_command {
                        // 순서 보존: 선행 CRUD를 먼저 적용한 뒤 커맨드를 단독 적용.
                        stats.applied += self.flush(&mut batch, &mut batch_bytes).await?;
                        stats.applied += self.apply_ops(vec![op]).await?;
                    } else {
                        if batch.len() >= MAX_BATCH_OPS
                            || batch_bytes + entry_bytes > MAX_BATCH_BYTES
                        {
                            stats.applied += self.flush(&mut batch, &mut batch_bytes).await?;
                        }
                        batch.push(op);
                        batch_bytes += entry_bytes;
                    }
                }
                EntryAction::TxnBuffer(key, ops) => {
                    txns.entry(key).or_default().extend(ops);
                }
                EntryAction::TxnBufferThenCommit(key, ops) => {
                    txns.entry(key.clone()).or_default().extend(ops);
                    let ops = txns.remove(&key).unwrap_or_default();
                    // 커밋 시점 순서 보존: 선행 CRUD 배치 → 트랜잭션 op들.
                    stats.applied += self.flush(&mut batch, &mut batch_bytes).await?;
                    for chunk in ops.chunks(MAX_BATCH_OPS) {
                        stats.applied += self.apply_ops(chunk.to_vec()).await?;
                    }
                }
                EntryAction::TxnCommit(key) => {
                    let ops = txns.remove(&key).unwrap_or_default();
                    stats.applied += self.flush(&mut batch, &mut batch_bytes).await?;
                    for chunk in ops.chunks(MAX_BATCH_OPS) {
                        stats.applied += self.apply_ops(chunk.to_vec()).await?;
                    }
                }
                EntryAction::TxnAbort(key) => {
                    if let Some(dropped) = txns.remove(&key) {
                        stats.skipped += dropped.len() as u64;
                    }
                }
            }
        }

        // 남은 CRUD 배치 적용. 미커밋 트랜잭션 버퍼는 폐기 — 커밋이 limit 밖(미래)이면
        // 그 트랜잭션의 효과는 목표 시점에 존재하지 않았던 것이므로 버리는 게 옳다.
        stats.applied += self.flush(&mut batch, &mut batch_bytes).await?;
        for (_, dropped) in txns {
            stats.skipped += dropped.len() as u64;
        }
        Ok(stats)
    }

    /// CRUD 배치를 비우고 적용한 op 수를 반환한다.
    async fn flush(&self, batch: &mut Vec<Document>, batch_bytes: &mut usize) -> Result<u64> {
        *batch_bytes = 0;
        if batch.is_empty() {
            return Ok(0);
        }
        self.apply_ops(std::mem::take(batch)).await
    }

    /// `applyOps` 커맨드 1회 실행. 실패 시 사유(권한 포함)를 담아 전파한다.
    async fn apply_ops(&self, ops: Vec<Document>) -> Result<u64> {
        if ops.is_empty() {
            return Ok(0);
        }
        let count = ops.len() as u64;
        let cmd = doc! { "applyOps": ops.into_iter().map(Bson::Document).collect::<Vec<_>>() };
        self.client
            .database("admin")
            .run_command(cmd)
            .await
            .map_err(|e| {
                XBackupError::Failure(format!(
                    "PITR oplog 적용 실패(applyOps): {e} — applyOps는 mongorestore \
                     --oplogReplay와 동일한 강한 권한이 필요합니다(권한 오류라면 재생 계정의 \
                     역할을 확인하세요)"
                ))
            })?;
        Ok(count)
    }
}

/// 스트림에서 naked BSON 문서 한 개를 읽는다. 깔끔한 EOF면 `None`.
///
/// BSON은 앞 4바이트(LE)가 전체 길이라 self-delimiting이다 — [`super::oplog`] 캡처가
/// 기록한 연결 스트림을 그대로 되읽는다. 반환 바이트 수는 배치 크기 계산에 쓴다.
async fn read_bson_doc<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<(Document, usize)>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => {
            return Err(XBackupError::Failure(format!(
                "oplog 엔트리 길이 읽기 실패: {e}"
            )))
        }
    }
    let len = u32::from_le_bytes(len_buf);
    if !(5..=MAX_ENTRY_BYTES).contains(&len) {
        return Err(XBackupError::Failure(format!(
            "oplog 엔트리 길이 비정상: {len}바이트 — 슬라이스 손상 의심(verify --deep 권장)"
        )));
    }
    let mut buf = vec![0u8; len as usize];
    buf[..4].copy_from_slice(&len_buf);
    r.read_exact(&mut buf[4..])
        .await
        .map_err(|e| XBackupError::Failure(format!("oplog 엔트리 본문 읽기 실패: {e}")))?;
    let doc = Document::from_reader(&buf[..])
        .map_err(|e| XBackupError::Failure(format!("oplog 엔트리 파싱 실패: {e}")))?;
    Ok(Some((doc, len as usize)))
}

/// 엔트리의 `ts`(BSON Timestamp)를 읽는다.
fn entry_ts(entry: &Document) -> Result<OplogTimestamp> {
    match entry.get("ts") {
        Some(Bson::Timestamp(ts)) => Ok(OplogTimestamp::from(*ts)),
        other => Err(XBackupError::Failure(format!(
            "oplog 엔트리에 ts(Timestamp)가 없습니다(발견: {other:?}) — 슬라이스 손상 의심"
        ))),
    }
}

/// 엔트리 하나를 분류하고 적용 가능한 형태로 정리한다(모듈 주석의 적용 규칙).
fn classify_entry(mut entry: Document) -> Result<EntryAction> {
    let op = entry.get_str("op").unwrap_or_default().to_string();
    if op == "n" {
        return Ok(EntryAction::Skip);
    }
    let ns = entry.get_str("ns").unwrap_or_default().to_string();
    if should_skip_ns(&ns) {
        return Ok(EntryAction::Skip);
    }

    // 트랜잭션 엔트리(applyOps 체인/commit/abort) 판별 — 세션 키가 있어야 한다.
    if op == "c" {
        if let Some(key) = txn_key(&entry) {
            let o = entry.get_document("o").cloned().unwrap_or_default();
            if o.contains_key("commitTransaction") {
                return Ok(EntryAction::TxnCommit(key));
            }
            if o.contains_key("abortTransaction") {
                return Ok(EntryAction::TxnAbort(key));
            }
            if o.contains_key("applyOps") {
                let inner = inner_txn_ops(&o);
                let is_final = !o.get_bool("partialTxn").unwrap_or(false)
                    && !o.get_bool("prepare").unwrap_or(false);
                return Ok(if is_final {
                    // 미준비(unprepared) 트랜잭션의 마지막 조각 — 이 엔트리가 곧 커밋.
                    EntryAction::TxnBufferThenCommit(key, inner)
                } else {
                    EntryAction::TxnBuffer(key, inner)
                });
            }
        }
    }

    sanitize_entry(&mut entry);
    Ok(EntryAction::Apply(entry))
}

/// 제외 네임스페이스 — 서버 부기용이지 사용자 데이터가 아니다(모듈 주석).
fn should_skip_ns(ns: &str) -> bool {
    let db = ns.split('.').next().unwrap_or("");
    db == "local" || db == "config" || ns == "admin.system.version"
}

/// 트랜잭션 버퍼 키 — lsid 문서 + txnNumber의 결정적 직렬화. 세션 키가 없으면 None
/// (일반 사용자 applyOps 커맨드 — 트랜잭션 아님).
fn txn_key(entry: &Document) -> Option<String> {
    let lsid = entry.get_document("lsid").ok()?;
    let txn = entry.get_i64("txnNumber").ok()?;
    Some(format!("{lsid:?}#{txn}"))
}

/// `o.applyOps` 내부 op들을 꺼내 정리(sanitize)한다.
fn inner_txn_ops(o: &Document) -> Vec<Document> {
    let Ok(arr) = o.get_array("applyOps") else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|b| match b {
            Bson::Document(d) => {
                let mut d = d.clone();
                sanitize_entry(&mut d);
                Some(d)
            }
            _ => None,
        })
        .collect()
}

/// 엔트리에서 UUID(`ui`)와 세션/재시도 메타를 제거한다(모듈 주석의 스트립 규칙).
/// 중첩 applyOps(트랜잭션 외 사용자 applyOps 커맨드)도 재귀적으로 정리한다.
fn sanitize_entry(entry: &mut Document) {
    const STRIP: &[&str] = &[
        "ui",
        "lsid",
        "txnNumber",
        "stmtId",
        "prevOpTime",
        "preImageOpTime",
        "postImageOpTime",
        "needsRetryImage",
    ];
    for key in STRIP {
        entry.remove(*key);
    }
    // 중첩 applyOps 내부 엔트리의 ui도 스트립(사용자 applyOps 커맨드 케이스).
    if let Ok(o) = entry.get_document_mut("o") {
        if let Ok(arr) = o.get_array_mut("applyOps") {
            for item in arr.iter_mut() {
                if let Bson::Document(d) = item {
                    d.remove("ui");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts_bson(t: u32, i: u32) -> Bson {
        Bson::Timestamp(bson::Timestamp {
            time: t,
            increment: i,
        })
    }

    fn insert_entry(ns: &str, id: i32) -> Document {
        doc! {
            "ts": ts_bson(100, id as u32),
            "op": "i",
            "ns": ns,
            "ui": bson::Binary { subtype: bson::spec::BinarySubtype::Uuid, bytes: vec![0; 16] },
            "o": { "_id": id },
            "wall": "x",
        }
    }

    // ── BSON 스트림 리더 ──

    #[tokio::test]
    async fn read_bson_doc_round_trips_concatenated_docs() {
        let d1 = doc! { "a": 1 };
        let d2 = doc! { "b": "two" };
        let mut buf = Vec::new();
        d1.to_writer(&mut buf).unwrap();
        d2.to_writer(&mut buf).unwrap();
        let mut r = std::io::Cursor::new(buf);
        let (got1, n1) = read_bson_doc(&mut r).await.unwrap().unwrap();
        assert_eq!(got1, d1);
        assert!(n1 > 4);
        let (got2, _) = read_bson_doc(&mut r).await.unwrap().unwrap();
        assert_eq!(got2, d2);
        assert!(read_bson_doc(&mut r).await.unwrap().is_none(), "깔끔한 EOF");
    }

    #[tokio::test]
    async fn read_bson_doc_rejects_bogus_length() {
        // 길이 4바이트가 5 미만 — 손상.
        let mut r = std::io::Cursor::new(vec![1, 0, 0, 0, 0]);
        assert!(read_bson_doc(&mut r).await.is_err());
    }

    // ── 분류 규칙 ──

    #[test]
    fn classify_skips_noop_and_excluded_namespaces() {
        let noop = doc! { "ts": ts_bson(1,1), "op": "n", "ns": "", "o": {} };
        assert!(matches!(classify_entry(noop).unwrap(), EntryAction::Skip));
        for ns in [
            "local.oplog.rs",
            "config.transactions",
            "admin.system.version",
        ] {
            let e = doc! { "ts": ts_bson(1,1), "op": "i", "ns": ns, "o": {"_id": 1} };
            assert!(
                matches!(classify_entry(e).unwrap(), EntryAction::Skip),
                "{ns}는 스킵 대상"
            );
        }
    }

    #[test]
    fn classify_strips_ui_and_session_meta_from_crud() {
        let mut e = insert_entry("shop.orders", 1);
        e.insert("lsid", doc! { "id": "s" });
        e.insert("txnNumber", 7i64);
        e.insert("stmtId", 0i32);
        match classify_entry(e).unwrap() {
            EntryAction::Apply(d) => {
                for k in ["ui", "lsid", "txnNumber", "stmtId"] {
                    assert!(!d.contains_key(k), "{k}는 스트립돼야 함: {d:?}");
                }
                // 적용 의미에 필요한 필드는 보존.
                assert_eq!(d.get_str("op").unwrap(), "i");
                assert_eq!(d.get_str("ns").unwrap(), "shop.orders");
            }
            _ => panic!("Apply 기대"),
        }
    }

    #[test]
    fn classify_buffers_partial_txn_and_commits_on_final() {
        let lsid = doc! { "id": "sess-1" };
        let partial = doc! {
            "ts": ts_bson(10, 1), "op": "c", "ns": "admin.$cmd",
            "lsid": lsid.clone(), "txnNumber": 3i64,
            "o": {
                "applyOps": [ { "op": "i", "ns": "d.c", "ui": "u", "o": {"_id": 1} } ],
                "partialTxn": true,
            },
        };
        let final_entry = doc! {
            "ts": ts_bson(10, 2), "op": "c", "ns": "admin.$cmd",
            "lsid": lsid, "txnNumber": 3i64,
            "o": { "applyOps": [ { "op": "i", "ns": "d.c", "o": {"_id": 2} } ] },
        };
        let key = match classify_entry(partial).unwrap() {
            EntryAction::TxnBuffer(key, ops) => {
                assert_eq!(ops.len(), 1);
                assert!(!ops[0].contains_key("ui"), "내부 op의 ui 스트립");
                key
            }
            _ => panic!("TxnBuffer 기대"),
        };
        match classify_entry(final_entry).unwrap() {
            EntryAction::TxnBufferThenCommit(k, ops) => {
                assert_eq!(k, key, "같은 (lsid,txnNumber)는 같은 키");
                assert_eq!(ops.len(), 1);
            }
            _ => panic!("TxnBufferThenCommit 기대"),
        }
    }

    #[test]
    fn classify_routes_commit_and_abort() {
        let lsid = doc! { "id": "sess-2" };
        let commit = doc! {
            "ts": ts_bson(11, 1), "op": "c", "ns": "admin.$cmd",
            "lsid": lsid.clone(), "txnNumber": 9i64,
            "o": { "commitTransaction": 1 },
        };
        let abort = doc! {
            "ts": ts_bson(11, 2), "op": "c", "ns": "admin.$cmd",
            "lsid": lsid, "txnNumber": 9i64,
            "o": { "abortTransaction": 1 },
        };
        assert!(matches!(
            classify_entry(commit).unwrap(),
            EntryAction::TxnCommit(_)
        ));
        assert!(matches!(
            classify_entry(abort).unwrap(),
            EntryAction::TxnAbort(_)
        ));
    }

    #[test]
    fn classify_treats_sessionless_command_as_plain_apply() {
        // 세션 키 없는 커맨드(create 등)는 트랜잭션이 아니라 일반 적용 대상.
        let create = doc! {
            "ts": ts_bson(12, 1), "op": "c", "ns": "shop.$cmd",
            "o": { "create": "orders" },
        };
        assert!(matches!(
            classify_entry(create).unwrap(),
            EntryAction::Apply(_)
        ));
    }

    // ── ts/limit ──

    #[test]
    fn entry_ts_reads_timestamp_and_rejects_absent() {
        let e = insert_entry("d.c", 1);
        assert_eq!(entry_ts(&e).unwrap(), OplogTimestamp::new(100, 1));
        assert!(entry_ts(&doc! {"op": "i"}).is_err());
    }
}
