//! 증분 oplog 캡처 — 드라이버로 `local.oplog.rs`를 직접 질의(PRD §6.3, FR-2).
//!
//! 풀 백업이 `mongodump --archive`로 dump 시점 일관성을 담는 것과 달리, 증분은
//! `mongodump`로 임의 `ts` 범위 슬라이스를 뽑을 수 없으므로(스파이크 §5) **드라이버
//! 직접 질의**로 `ts > last_backup_ts`인 엔트리를 natural order로 읽어 raw BSON 바이트
//! 스트림으로 노출한다. 이 스트림은 풀 백업과 동일한 [`StageStack`](crate::pipeline::stage)
//! (압축·암호화)을 통과해 저장된다.
//!
//! ## 캡처 경계와 안전 규칙(리서치 pitfalls §2)
//! - **상한 고정(2-1):** 캡처 시작 시점의 최신 ts를 상한으로 고정하고
//!   `{ts: {$gt: last, $lte: upper}}`로 질의한다 — 캡처 중 들어오는 쓰기가 경계를
//!   움직이지 않게 한다.
//! - **partialTxn 경계(2-3):** 상한 엔트리가 분할 트랜잭션(`applyOps` + `partialTxn`)의
//!   중간이면 체인이 끊긴 채 저장될 수 있다. 상한을 체인의 *마지막*(비-partialTxn)
//!   엔트리까지 확장해 트랜잭션을 온전히 담는다([`extend_upper_past_partial_txn`]).
//! - **natural order(2-1):** 정렬은 `$natural:1`(삽입 순서) — oplog replay 순서와 일치.
//! - **late gap(2-4):** 캡처 도중 윈도우가 롤오버되면 드라이버가 `CursorNotFound`(code 43)를
//!   던진다. 이 경우 부분 산출물을 버리고 풀 백업으로 승격해야 하므로,
//!   스트림이 그 사실을 EOF가 아닌 **에러**로 전파한다([`CaptureError::LateGap`]).
//! - **local 권한(2-7):** `local.oplog.rs` 읽기 권한이 없으면 질의가 실패한다 —
//!   에러를 그대로 전파해 상위(핸들러)가 사유와 함께 보고한다.

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use bson::{doc, Bson, RawDocument, RawDocumentBuf, Timestamp};
use mongodb::error::ErrorKind;
use mongodb::options::{ClientOptions, FindOptions};
use mongodb::Client;
use tokio::io::{AsyncRead, AsyncWriteExt, DuplexStream, ReadBuf};
use tokio::task::JoinHandle;

use crate::config::secret::Secret;
use crate::error::{Result, XBackupError};
use crate::manifest::schema::OplogTimestamp;

/// `local.oplog.rs` 컬렉션 네임스페이스.
const OPLOG_DB: &str = "local";
const OPLOG_COLL: &str = "oplog.rs";

/// CursorNotFound MongoDB 에러 코드(항상 resumable; oplog 윈도우 롤오버 신호) — 리서치 §2-4.
const CODE_CURSOR_NOT_FOUND: i32 = 43;

/// 캡처 중 cursor→스트림 파이프 버퍼 크기(바이트). 백프레셔를 위한 경계 버퍼다.
const PIPE_BUFFER_BYTES: usize = 256 * 1024;

/// gap 판정 결과 — 캡처 진행 가능 여부.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GapCheck {
    /// 직전 기준점이 oplog 윈도우 안에 존재 — 증분 캡처 진행 가능.
    ///
    /// `boundary_risk`는 `last_backup_ts == min_ts`(경계에 걸침)일 때 true다 — 다음
    /// 폴 사이에 롤오버될 위험이 있어 경고 로그 대상이다(리서치 §3-1 `$gte` 경계).
    Ok { boundary_risk: bool },
    /// oplog가 비어 있거나 oplog 부재(standalone) — 진행 불가, 상위가 사유 판단.
    OplogEmpty,
    /// 직전 기준점이 윈도우에서 밀려남(롤오버) — 증분 거부, 풀 승격 대상(§6.2).
    Gap {
        /// 직전 백업 기준점.
        last: OplogTimestamp,
        /// 현재 oplog 최소 ts.
        min: OplogTimestamp,
    },
}

/// 증분 캡처 중 발생할 수 있는 에러.
#[derive(Debug)]
pub enum CaptureError {
    /// 캡처 도중 oplog 윈도우 롤오버(`CursorNotFound`) — late gap. 부분 산출물 폐기 후
    /// 풀 승격해야 한다(§6.2).
    LateGap(String),
    /// 그 외 드라이버/IO 오류.
    Other(XBackupError),
}

impl From<CaptureError> for XBackupError {
    fn from(e: CaptureError) -> Self {
        match e {
            // late gap도 결국 풀 승격을 유도하는 경고 흐름이므로 상위에서 해석한다.
            // 단 타입을 잃지 않도록 핸들러는 CaptureError를 직접 매칭한다.
            CaptureError::LateGap(m) => XBackupError::Warning(format!("late gap: {m}")),
            CaptureError::Other(e) => e,
        }
    }
}

/// 드라이버로 oplog를 질의하는 캡처 리더.
pub struct OplogReader {
    client: Client,
}

impl OplogReader {
    /// 기존 드라이버 클라이언트로 캡처 리더를 만든다(메타 질의와 동일 연결 재사용).
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    /// URI 시크릿으로 드라이버 클라이언트를 연결해 캡처 리더를 만든다.
    ///
    /// [`MongoMeta::connect`](super::meta::MongoMeta::connect)와 동일한 연결 절차다 —
    /// 증분 핸들러가 메타 질의와 별개로 oplog 전용 리더를 만들 때 쓴다.
    pub async fn connect(uri: &Secret) -> Result<Self> {
        let options = ClientOptions::parse(uri.expose())
            .await
            .map_err(|e| XBackupError::Failure(format!("MongoDB URI 파싱/연결 실패: {e}")))?;
        let client = Client::with_options(options)
            .map_err(|e| XBackupError::Failure(format!("MongoDB 클라이언트 생성 실패: {e}")))?;
        Ok(Self::new(client))
    }

    /// oplog 컬렉션 핸들 — raw 바이트 스트리밍을 위해 [`RawDocumentBuf`](sized) 타입으로
    /// 연다(커서 `current()`는 `&RawDocument`를 반환하므로 바이트 접근은 동일하다).
    fn oplog(&self) -> mongodb::Collection<RawDocumentBuf> {
        self.client
            .database(OPLOG_DB)
            .collection::<RawDocumentBuf>(OPLOG_COLL)
    }

    /// oplog의 **최소** ts를 조회한다(`$natural:1` 1건). 비었으면 `None`.
    pub async fn min_oplog_ts(&self) -> Result<Option<Timestamp>> {
        self.first_ts(doc! {}, 1).await
    }

    /// oplog의 **최신** ts를 조회한다(`$natural:-1` 1건). 비었으면 `None`.
    pub async fn latest_oplog_ts(&self) -> Result<Option<Timestamp>> {
        self.first_ts(doc! {}, -1).await
    }

    /// 주어진 필터·정렬방향으로 1건을 읽어 그 `ts`를 반환한다(내부 헬퍼).
    async fn first_ts(&self, filter: bson::Document, natural: i32) -> Result<Option<Timestamp>> {
        let opts = FindOptions::builder()
            .sort(doc! { "$natural": natural })
            .limit(1)
            .build();
        let mut cursor = self
            .oplog()
            .find(filter)
            .with_options(opts)
            .await
            .map_err(map_find_err)?;
        if cursor.advance().await.map_err(map_find_err)? {
            Ok(ts_of(cursor.current()))
        } else {
            Ok(None)
        }
    }

    /// gap을 감지한다(캡처 *전*). `last_backup_ts`가 oplog 윈도우 안에 아직 있는지 본다.
    ///
    /// 규칙(§6.2, 리서치 §3-1):
    /// - oplog 비었음 → [`GapCheck::OplogEmpty`].
    /// - `last < min` → 롤오버 → [`GapCheck::Gap`](증분 거부, 풀 승격).
    /// - `last == min` → 경계 위험 동반 진행([`GapCheck::Ok`] `boundary_risk=true`).
    /// - `last > min` → 안전하게 진행.
    pub async fn detect_gap(&self, last_backup_ts: Timestamp) -> Result<GapCheck> {
        let min = match self.min_oplog_ts().await? {
            Some(ts) => ts,
            None => return Ok(GapCheck::OplogEmpty),
        };
        let last_ord: OplogTimestamp = last_backup_ts.into();
        let min_ord: OplogTimestamp = min.into();
        Ok(classify_gap(last_ord, min_ord))
    }

    /// 캡처 상한 ts를 산정한다 — 최신 ts를 고정한 뒤 partialTxn 경계를 확장한다.
    ///
    /// 반환값이 `None`이면 oplog가 비어 캡처할 상한이 없다(빈 슬라이스 경로).
    pub async fn resolve_upper_bound(&self) -> Result<Option<Timestamp>> {
        let latest = match self.latest_oplog_ts().await? {
            Some(ts) => ts,
            None => return Ok(None),
        };
        let extended = self.extend_upper_past_partial_txn(latest).await?;
        Ok(Some(extended))
    }

    /// 상한 엔트리가 분할 트랜잭션의 중간이면, 체인의 마지막(비-partialTxn) 엔트리까지
    /// 상한을 확장한다(리서치 §2-3).
    ///
    /// 분할 트랜잭션은 `applyOps` 엔트리가 여러 개로 쪼개져 `partialTxn:true` 플래그로
    /// 이어지고, 마지막 조각만 `partialTxn`이 없다(또는 `count` 필드로 마감). 상한
    /// 엔트리부터 `$natural:1`로 전진하며 partialTxn 플래그가 없는 첫 엔트리까지 ts를
    /// 끌어올린다. 상한 자체가 partialTxn이 아니면 그대로 반환한다.
    ///
    /// 경계 산정 로직은 [`extend_boundary`] 순수 함수에 위임한다(드라이버 없이 단위
    /// 테스트 가능). 이 메서드는 upper 엔트리(포함)부터 `$natural:1`로 읽어 `(ts,
    /// is_partial_txn)` 시퀀스를 만들어 넘긴다.
    async fn extend_upper_past_partial_txn(&self, upper: Timestamp) -> Result<Timestamp> {
        // upper 엔트리(포함)부터 자연 순서로 읽는다.
        let opts = FindOptions::builder().sort(doc! { "$natural": 1 }).build();
        let mut cursor = self
            .oplog()
            .find(doc! { "ts": { "$gte": Bson::Timestamp(upper) } })
            .with_options(opts)
            .await
            .map_err(map_find_err)?;

        let mut seq: Vec<(OplogTimestamp, bool)> = Vec::new();
        while cursor.advance().await.map_err(map_find_err)? {
            let raw = cursor.current();
            let partial = is_partial_txn(raw);
            // 첫 엔트리(=upper)가 partialTxn이 아니면 확장 불필요 — 즉시 반환.
            if seq.is_empty() && !partial {
                return Ok(upper);
            }
            if let Some(ts) = ts_of(raw) {
                seq.push((ts.into(), partial));
            }
            // partialTxn 플래그가 사라지는 첫 엔트리에서 트랜잭션이 닫힌다 — 더 읽지 않는다.
            if !partial {
                break;
            }
        }
        Ok(extend_boundary(upper.into(), &seq).into())
    }

    /// `last_backup_ts < ts <= upper` 범위의 oplog 엔트리를 raw BSON 바이트 스트림으로
    /// 캡처한다(natural order). 반환된 [`OplogCaptureStream`]을 StageStack에 흘린다.
    ///
    /// 커서를 별도 task로 구동해 [`DuplexStream`]에 raw document 바이트를 연결한다 —
    /// 경계 버퍼 + 백프레셔로 전 구간 스트리밍을 유지한다. 캡처 중 `CursorNotFound`는
    /// late gap으로 판정해 스트림 끝에서 에러로 전파한다(부분 산출물 폐기 신호).
    pub fn capture_stream(
        &self,
        last_backup_ts: Timestamp,
        upper: Timestamp,
    ) -> OplogCaptureStream {
        let (writer, reader) = tokio::io::duplex(PIPE_BUFFER_BYTES);
        let count = Arc::new(Mutex::new(0u64));
        let outcome: Arc<Mutex<Option<std::result::Result<(), CaptureError>>>> =
            Arc::new(Mutex::new(None));

        let oplog = self.oplog();
        let count_task = Arc::clone(&count);
        let outcome_task = Arc::clone(&outcome);

        let handle = tokio::spawn(async move {
            let res =
                drain_cursor_to_writer(oplog, last_backup_ts, upper, writer, &count_task).await;
            *outcome_task.lock().expect("capture outcome mutex poisoned") = Some(res);
        });

        OplogCaptureStream {
            reader,
            count,
            outcome,
            task: Arc::new(Mutex::new(Some(handle))),
        }
    }
}

/// 캡처 결과를 담은 raw BSON 바이트 스트림.
///
/// [`AsyncRead`]로 StageStack에 흘린다. 스트림이 `put_stream`에 move-out되더라도 결과를
/// 회수할 수 있도록, 엔트리 개수·late-gap 여부는 [`handle`](Self::handle)로 떠 둔
/// [`CaptureHandle`]에서 EOF 이후 읽는다(sha256 tee와 동형 패턴).
pub struct OplogCaptureStream {
    reader: DuplexStream,
    count: Arc<Mutex<u64>>,
    outcome: Arc<Mutex<Option<std::result::Result<(), CaptureError>>>>,
    task: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl OplogCaptureStream {
    /// 스트림 소비(EOF) 후 결과를 회수할 핸들을 떠 둔다.
    ///
    /// 스트림을 `put_stream`에 넘기기 *전에* 핸들을 떠 두면, 업로드가 끝난 뒤
    /// [`CaptureHandle::finish`]로 엔트리 개수·late-gap 여부를 읽을 수 있다.
    pub fn handle(&self) -> CaptureHandle {
        CaptureHandle {
            count: Arc::clone(&self.count),
            outcome: Arc::clone(&self.outcome),
            task: Arc::clone(&self.task),
        }
    }
}

/// [`OplogCaptureStream`]의 캡처 결과를 회수하는 핸들.
#[derive(Clone)]
pub struct CaptureHandle {
    count: Arc<Mutex<u64>>,
    outcome: Arc<Mutex<Option<std::result::Result<(), CaptureError>>>>,
    task: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl CaptureHandle {
    /// 스트림 소비(EOF) 후 캡처 결과를 회수한다.
    ///
    /// - `Ok(count)`: 정상 캡처(엔트리 개수).
    /// - `Err(CaptureError::LateGap)`: 캡처 도중 롤오버 — 부분 산출물 폐기 + 풀 승격.
    /// - `Err(CaptureError::Other)`: 그 외 드라이버/IO 오류.
    pub async fn finish(self) -> std::result::Result<u64, CaptureError> {
        // 구동 task가 outcome을 채울 때까지 join(스트림 EOF 후엔 곧 완료).
        let task = self
            .task
            .lock()
            .expect("capture task mutex poisoned")
            .take();
        if let Some(task) = task {
            let _ = task.await;
        }
        let outcome = self
            .outcome
            .lock()
            .expect("capture outcome mutex poisoned")
            .take();
        match outcome {
            Some(Ok(())) => Ok(self.count()),
            Some(Err(e)) => Err(e),
            // task가 결과를 못 남긴 경우(드뭄) — 보수적으로 Other 처리.
            None => Err(CaptureError::Other(XBackupError::Failure(
                "oplog 캡처 task가 결과를 남기지 않음".into(),
            ))),
        }
    }

    /// 현재까지 캡처한 엔트리 개수.
    pub fn count(&self) -> u64 {
        *self.count.lock().expect("capture count mutex poisoned")
    }
}

impl AsyncRead for OplogCaptureStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.reader).poll_read(cx, buf)
    }
}

/// 커서를 끝까지 구동해 raw document 바이트를 writer로 흘린다(별도 task 본체).
///
/// `CursorNotFound`(code 43)는 late gap으로 분류한다(리서치 §2-4). writer는 함수가
/// 끝나면 drop되어 read 측에 EOF를 전한다.
async fn drain_cursor_to_writer(
    oplog: mongodb::Collection<RawDocumentBuf>,
    last: Timestamp,
    upper: Timestamp,
    mut writer: DuplexStream,
    count: &Arc<Mutex<u64>>,
) -> std::result::Result<(), CaptureError> {
    let opts = FindOptions::builder().sort(doc! { "$natural": 1 }).build();
    let filter = doc! {
        "ts": { "$gt": Bson::Timestamp(last), "$lte": Bson::Timestamp(upper) }
    };

    let mut cursor = match oplog.find(filter).with_options(opts).await {
        Ok(c) => c,
        Err(e) => return Err(classify_cursor_err(e)),
    };

    loop {
        match cursor.advance().await {
            Ok(true) => {
                let bytes = cursor.current().as_bytes().to_vec();
                if let Err(e) = writer.write_all(&bytes).await {
                    return Err(CaptureError::Other(XBackupError::Io(e)));
                }
                let mut c = count.lock().expect("capture count mutex poisoned");
                *c += 1;
            }
            Ok(false) => break,
            Err(e) => return Err(classify_cursor_err(e)),
        }
    }

    if let Err(e) = writer.flush().await {
        return Err(CaptureError::Other(XBackupError::Io(e)));
    }
    Ok(())
}

/// 드라이버 에러를 캡처 에러로 분류한다 — CursorNotFound(43) → late gap.
fn classify_cursor_err(e: mongodb::error::Error) -> CaptureError {
    if let ErrorKind::Command(cmd) = e.kind.as_ref() {
        if cmd.code == CODE_CURSOR_NOT_FOUND {
            return CaptureError::LateGap(format!(
                "캡처 중 oplog 윈도우 롤오버(CursorNotFound, code 43): {}",
                cmd.message
            ));
        }
    }
    CaptureError::Other(XBackupError::Failure(format!("oplog 캡처 실패: {e}")))
}

/// find/advance 에러를 일반 실패로 매핑한다(gap 사전 점검 경로).
fn map_find_err(e: mongodb::error::Error) -> XBackupError {
    XBackupError::Failure(format!("oplog 질의 실패: {e}"))
}

/// raw document에서 `ts`(BSON Timestamp)를 꺼낸다. 없거나 타입 불일치면 None.
fn ts_of(raw: &RawDocument) -> Option<Timestamp> {
    match raw.get("ts") {
        Ok(Some(val)) => val.as_timestamp(),
        _ => None,
    }
}

/// raw document가 분할 트랜잭션의 *중간* 조각인지 — `partialTxn:true` 플래그로 판정.
///
/// `applyOps` 엔트리가 분할되면 마지막 조각을 제외한 모든 조각이 `partialTxn:true`다.
/// 플래그가 없거나 false면 트랜잭션의 마지막(또는 단일) 엔트리로 본다(리서치 §2-3).
fn is_partial_txn(raw: &RawDocument) -> bool {
    matches!(raw.get_bool("partialTxn"), Ok(true))
}

/// partialTxn 경계 확장 순수 함수(리서치 §2-3) — 드라이버 없이 단위 테스트 가능.
///
/// `upper`(포함)부터 `$natural:1`로 읽은 `(ts, is_partial_txn)` 시퀀스를 받아, 분할
/// 트랜잭션 체인을 온전히 담도록 확장된 상한 ts를 반환한다:
/// - 시퀀스가 비었거나 첫 엔트리(=upper)가 partialTxn이 아니면 `upper` 그대로.
/// - upper가 partialTxn이면 partialTxn이 사라지는 첫(=닫힘) 엔트리까지 ts를 끌어올린다.
/// - 체인이 윈도우 끝까지 닫히지 않으면(모두 partialTxn) 마지막으로 본 ts를 쓴다
///   (보수적 — 다음 폴에서 나머지가 캡처되거나 gap으로 잡힌다).
fn extend_boundary(upper: OplogTimestamp, seq: &[(OplogTimestamp, bool)]) -> OplogTimestamp {
    match seq.first() {
        // 첫 엔트리가 partialTxn이 아니면 단일/마지막 엔트리 — 확장 불필요.
        Some((_, false)) | None => upper,
        Some((_, true)) => {
            let mut result = upper;
            for (ts, partial) in seq {
                result = *ts;
                if !partial {
                    // partialTxn이 닫히는 엔트리 — 여기서 트랜잭션이 완결된다.
                    break;
                }
            }
            result
        }
    }
}

/// gap 분류 순수 함수 — 단위 테스트가 직접 검증한다(드라이버 불필요).
fn classify_gap(last: OplogTimestamp, min: OplogTimestamp) -> GapCheck {
    use std::cmp::Ordering;
    match last.cmp(&min) {
        // last < min: 직전 기준점이 윈도우에서 밀려남 → gap(풀 승격).
        Ordering::Less => GapCheck::Gap { last, min },
        // last == min: 경계에 걸침 — 진행하되 위험 경고.
        Ordering::Equal => GapCheck::Ok {
            boundary_risk: true,
        },
        // last > min: 안전 구간.
        Ordering::Greater => GapCheck::Ok {
            boundary_risk: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(t: u32, i: u32) -> OplogTimestamp {
        OplogTimestamp::new(t, i)
    }

    /// last > min: 안전 진행(경계 위험 없음).
    #[test]
    fn gap_classify_safe_when_last_after_min() {
        assert_eq!(
            classify_gap(ts(100, 5), ts(90, 1)),
            GapCheck::Ok {
                boundary_risk: false
            }
        );
    }

    /// last == min: 경계에 걸침 — 진행하되 위험 플래그.
    #[test]
    fn gap_classify_boundary_risk_when_equal() {
        assert_eq!(
            classify_gap(ts(100, 5), ts(100, 5)),
            GapCheck::Ok {
                boundary_risk: true
            }
        );
    }

    /// last < min: 롤오버 → gap(풀 승격 대상).
    #[test]
    fn gap_classify_gap_when_last_before_min() {
        match classify_gap(ts(80, 0), ts(90, 1)) {
            GapCheck::Gap { last, min } => {
                assert_eq!(last, ts(80, 0));
                assert_eq!(min, ts(90, 1));
            }
            other => panic!("gap을 기대했으나: {other:?}"),
        }
    }

    /// i(increment) 차원의 경계도 정확히 본다 — 같은 t에서 last.i < min.i면 gap.
    #[test]
    fn gap_classify_uses_increment_dimension() {
        match classify_gap(ts(100, 3), ts(100, 7)) {
            GapCheck::Gap { .. } => {}
            other => panic!("increment 차원 gap 미감지: {other:?}"),
        }
    }

    /// is_partial_txn: partialTxn:true 플래그를 정확히 읽는다.
    #[test]
    fn detects_partial_txn_flag() {
        let with = bson::doc! { "ts": Bson::Timestamp(Timestamp { time: 1, increment: 1 }), "partialTxn": true };
        let raw = bson::RawDocumentBuf::from_document(&with).unwrap();
        assert!(is_partial_txn(&raw));

        let without = bson::doc! { "ts": Bson::Timestamp(Timestamp { time: 1, increment: 2 }) };
        let raw2 = bson::RawDocumentBuf::from_document(&without).unwrap();
        assert!(!is_partial_txn(&raw2));

        let false_flag = bson::doc! { "partialTxn": false };
        let raw3 = bson::RawDocumentBuf::from_document(&false_flag).unwrap();
        assert!(!is_partial_txn(&raw3));
    }

    /// ts_of: raw document에서 Timestamp를 추출한다(타입 불일치/부재는 None).
    #[test]
    fn extracts_ts_from_raw_document() {
        let d = bson::doc! { "ts": Bson::Timestamp(Timestamp { time: 1781272133, increment: 5 }), "op": "i" };
        let raw = bson::RawDocumentBuf::from_document(&d).unwrap();
        assert_eq!(
            ts_of(&raw),
            Some(Timestamp {
                time: 1781272133,
                increment: 5
            })
        );

        let no_ts = bson::doc! { "op": "i" };
        let raw2 = bson::RawDocumentBuf::from_document(&no_ts).unwrap();
        assert_eq!(ts_of(&raw2), None);

        // ts가 Timestamp가 아닌 경우(이론상) None.
        let wrong = bson::doc! { "ts": 42i64 };
        let raw3 = bson::RawDocumentBuf::from_document(&wrong).unwrap();
        assert_eq!(ts_of(&raw3), None);
    }

    /// CaptureError::LateGap → exit 4(Warning) 매핑(풀 승격 경고 흐름).
    #[test]
    fn late_gap_maps_to_warning_exit_4() {
        let e: XBackupError = CaptureError::LateGap("rollover".into()).into();
        assert_eq!(e.exit_code(), 4);
    }

    // ── partialTxn 경계 확장 단위 테스트(가짜 엔트리 시퀀스) ──

    /// 상한이 단일(비-partialTxn) 엔트리면 그대로 둔다.
    #[test]
    fn boundary_single_entry_not_extended() {
        let upper = ts(100, 1);
        // upper 자신이 partialTxn 아님.
        let seq = [(ts(100, 1), false)];
        assert_eq!(extend_boundary(upper, &seq), ts(100, 1));
    }

    /// 빈 시퀀스(upper가 더 이상 없음)면 upper 그대로.
    #[test]
    fn boundary_empty_seq_returns_upper() {
        let upper = ts(100, 1);
        assert_eq!(extend_boundary(upper, &[]), upper);
    }

    /// 상한이 분할 트랜잭션 *중간*이면 닫힘 엔트리까지 확장한다.
    /// 시퀀스: upper(partial) → partial → 닫힘(non-partial) → 그 다음(무관).
    #[test]
    fn boundary_extends_through_partial_txn_chain() {
        let upper = ts(100, 1);
        let seq = [
            (ts(100, 1), true),  // upper — applyOps partialTxn 시작
            (ts(100, 2), true),  // 중간 조각
            (ts(100, 3), false), // 닫힘(commit) — 여기까지 확장
            (ts(100, 4), false), // 트랜잭션 밖 — 포함하지 않음
        ];
        assert_eq!(extend_boundary(upper, &seq), ts(100, 3));
    }

    /// 두 조각짜리 분할 트랜잭션(upper=시작, 다음=닫힘).
    #[test]
    fn boundary_two_part_txn() {
        let upper = ts(200, 7);
        let seq = [(ts(200, 7), true), (ts(200, 8), false)];
        assert_eq!(extend_boundary(upper, &seq), ts(200, 8));
    }

    /// 체인이 윈도우 끝까지 닫히지 않으면(모두 partialTxn) 마지막 본 ts로 보수적 확장.
    #[test]
    fn boundary_unclosed_chain_uses_last_seen() {
        let upper = ts(300, 1);
        let seq = [(ts(300, 1), true), (ts(300, 2), true), (ts(300, 3), true)];
        assert_eq!(extend_boundary(upper, &seq), ts(300, 3));
    }
}
