//! `status` 점검 로직 — 대상 MongoDB가 백업 가능한 상태인지 읽기 전용으로 점검한다(FR-8).
//!
//! 단순 ping이 아니라 백업 성공 여부를 좌우하는 항목을 본다(PRD §FR-8 1~8):
//! 1. **연결·인증**: 드라이버 connect + ping, 사용 인증 메커니즘.
//! 2. **권한**: `connectionStatus {showPrivileges:true}`로 현재 권한 조회 후 필요 집합 대비
//!    누락 권한을 구체적으로 보고.
//! 3. **버전 정합**: `buildInfo`(서버) + `mongodump --version`(도구). 메이저 차이 경고.
//! 4. **토폴로지**: `hello`로 standalone/replica set 판별, 샤딩(`isdbgrid`) 감지 시 거부(exit 3).
//!    replica set이면 `replSetGetStatus`로 PRIMARY 존재·SECONDARY lag 요약.
//! 5. **oplog 윈도우**: `local.oplog.rs` 최소~최신 ts 시간 폭. config interval 대비 여유.
//! 6. **저장 엔진**: `serverStatus`의 `storageEngine.name`.
//! 7. **예상 크기 + 데이터 형상**: 전 DB `dbStats` 한 번 합산 → dataSize/storageSize/indexSize와
//!    문서 수·컬렉션 수·인덱스 수(추가 쿼리 없이 같은 합산에서).
//! 8. **(선택) secondary 가용성**: prefer_secondary 구성 시 읽기 가능한 secondary 존재 여부.
//! 9. **FCV**: `getParameter featureCompatibilityVersion`(서버 버전과 별개 호환성 경계).
//! 10. **서버 시계**: `hello.localTime` vs 로컬 시각(clock skew — PITR/oplog 안전성).
//!
//! "마지막 백업"(destination 최신 manifest)·"destination 점검"(쓰기 가능·여유 공간)은 source가
//! 아닌 저장소 쪽 정보라 핸들러([`crate::cli::handlers::status`])에서 보고서에 덧붙인다.
//!
//! ## 읽기 전용·무부작용
//! 모든 명령은 조회 전용(`hello`/`buildInfo`/`connectionStatus`/`replSetGetStatus`/
//! `serverStatus`/`dbStats`/oplog find)이다. 쓰기·변경은 일절 하지 않는다(PRD §FR-8).
//!
//! ## 순수 판정 함수
//! 신호등 합산·권한 누락 집계·oplog 윈도우 임계·버전 호환·샤딩 감지는 드라이버와
//! 분리된 순수 함수로 두어 단위 테스트한다(태스크 지침 — 테스트 용이성).

use std::collections::BTreeSet;

use bson::{doc, Document, Timestamp};
use chrono::Utc;
use mongodb::options::FindOneOptions;
use mongodb::Client;
use serde::Serialize;

use crate::config::secret::Secret;
use crate::error::{Result, XBackupError};

/// 사용자 DB `dbStats` 합산 — 문서·컬렉션·인덱스 수와 데이터·저장·인덱스 크기(바이트).
#[derive(Debug, Default, Clone, Copy)]
struct DbTotals {
    objects: i64,
    collections: i64,
    indexes: i64,
    data_size: i64,
    storage_size: i64,
    index_size: i64,
}

/// 단일 점검 항목의 판정 결과(신호등 한 칸).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    /// 정상.
    Ok,
    /// 경고 — 백업은 가능하나 주의가 필요(예: oplog 윈도우 부족, 버전 메이저 차이).
    Warn,
    /// 실패 — 백업을 막는 결함(예: 연결 불가, 권한 누락, 샤딩, mongodump 부재).
    Fail,
}

impl CheckStatus {
    /// 신호등 합산용 심각도 서열(Ok < Warn < Fail).
    fn rank(self) -> u8 {
        match self {
            CheckStatus::Ok => 0,
            CheckStatus::Warn => 1,
            CheckStatus::Fail => 2,
        }
    }
}

/// 점검 항목 하나 — 키(머신용)·사람용 라벨·판정·메시지.
#[derive(Debug, Clone, Serialize)]
pub struct CheckItem {
    /// 머신 판독용 안정 키(예: `"connection"`, `"privileges"`).
    pub key: &'static str,
    /// 사람용 라벨(표 출력).
    pub label: &'static str,
    /// 판정(ok/warn/fail).
    pub status: CheckStatus,
    /// 상세 메시지(누락 권한·버전·윈도우 등 구체 정보).
    pub message: String,
    /// 프로파일 간 비교용 짧은 값(예: `"7.0.35"`, `"wiredTiger"`, `"none"`). `status --all`
    /// 비교 표에서 열 간 diff 판정·표시에 쓴다. 비교 의미가 없는 항목은 `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

impl CheckItem {
    /// ok 항목.
    pub fn ok(key: &'static str, label: &'static str, message: impl Into<String>) -> Self {
        Self {
            key,
            label,
            status: CheckStatus::Ok,
            message: message.into(),
            value: None,
        }
    }
    /// warn 항목.
    pub fn warn(key: &'static str, label: &'static str, message: impl Into<String>) -> Self {
        Self {
            key,
            label,
            status: CheckStatus::Warn,
            message: message.into(),
            value: None,
        }
    }
    /// fail 항목.
    pub fn fail(key: &'static str, label: &'static str, message: impl Into<String>) -> Self {
        Self {
            key,
            label,
            status: CheckStatus::Fail,
            message: message.into(),
            value: None,
        }
    }

    /// 비교용 값을 설정한다(빌더 — `CheckItem::ok(...).with_value("7.0.35")`).
    pub fn with_value(mut self, value: impl Into<String>) -> Self {
        self.value = Some(value.into());
        self
    }
}

/// 전체 점검 결과 — 항목 배열 + 신호등 합산.
#[derive(Debug, Clone, Serialize)]
pub struct StatusReport {
    /// 점검한 프로파일 이름.
    pub profile: String,
    /// 항목별 결과(점검 순서대로).
    pub items: Vec<CheckItem>,
    /// 전체 신호등(항목 중 최악).
    pub overall: CheckStatus,
}

impl StatusReport {
    /// 항목들로 보고서를 만들고 전체 신호등을 합산한다.
    pub fn new(profile: impl Into<String>, items: Vec<CheckItem>) -> Self {
        let overall = aggregate(&items);
        Self {
            profile: profile.into(),
            items,
            overall,
        }
    }

    /// 신호등 → 종료 코드(PRD §9). fail=3(사전 점검 실패), warn=4(경고 동반 성공), ok=0.
    pub fn exit_code(&self) -> u8 {
        overall_exit_code(self.overall)
    }
}

/// 항목 집합에서 가장 심각한 판정을 전체 신호등으로 합산한다(순수 함수).
pub fn aggregate(items: &[CheckItem]) -> CheckStatus {
    items
        .iter()
        .map(|i| i.status)
        .max_by_key(|s| s.rank())
        .unwrap_or(CheckStatus::Ok)
}

/// 전체 신호등을 PRD §9 종료 코드로 매핑한다(순수 함수).
///
/// - `Fail` → 3(사전 점검 실패 — 작업 미시작).
/// - `Warn` → 4(경고 동반 성공).
/// - `Ok`   → 0.
pub fn overall_exit_code(overall: CheckStatus) -> u8 {
    use crate::error::exit_codes::{PRECHECK, SUCCESS, WARNING};
    match overall {
        CheckStatus::Fail => PRECHECK,
        CheckStatus::Warn => WARNING,
        CheckStatus::Ok => SUCCESS,
    }
}

// ───────────────────────── 순수 판정 함수(드라이버 무관) ─────────────────────────

/// 백업 사용자에게 요구하는 액션 집합(권한 점검의 필요 집합, PRD §FR-8 2 / 미결정 §13.6).
///
/// 전 DB read(`find`) + oplog 읽기(`local.oplog.rs`의 `find`)가 백업의 최소 요구다.
/// 빌트인 역할로는 `backup`(+`read`/`readAnyDatabase` 상당)이 이를 만족한다. 여기서는
/// `connectionStatus`가 돌려주는 권한 액션 이름으로 직접 대조한다(역할명에 의존하지 않음).
pub const REQUIRED_ACTIONS: [&str; 2] = ["find", "listCollections"];

/// `connectionStatus {showPrivileges:true}`가 부여한 액션 집합 대비 필요 집합의 누락을 집계한다.
///
/// `granted_actions`는 사용자가 가진(어느 리소스에 대해서든) 액션 이름의 합집합이다.
/// 반환은 누락된 필요 액션의 정렬 목록 — 비어 있으면 권한 충분.
pub fn missing_actions(granted_actions: &BTreeSet<String>) -> Vec<String> {
    REQUIRED_ACTIONS
        .iter()
        .filter(|a| !granted_actions.contains(**a))
        .map(|a| a.to_string())
        .collect()
}

/// oplog 윈도우(초) 대비 증분 주기(초)의 여유를 판정한다(PRD §6.2 gap 위험, §FR-8 5).
///
/// 윈도우가 `interval × SAFETY_FACTOR`보다 짧으면 gap 위험으로 경고한다. 윈도우가 충분히
/// 길면 ok. 두 값 모두 초 단위.
pub fn oplog_window_status(window_secs: u64, interval_secs: u64) -> CheckStatus {
    /// 윈도우가 증분 주기의 최소 몇 배는 되어야 안전한가(§6.2 운영 권고).
    const SAFETY_FACTOR: u64 = 2;
    if interval_secs == 0 {
        // 주기를 모르면 임계 판단 불가 — 윈도우만 보고(경고 아님).
        return CheckStatus::Ok;
    }
    if window_secs < interval_secs.saturating_mul(SAFETY_FACTOR) {
        CheckStatus::Warn
    } else {
        CheckStatus::Ok
    }
}

/// `15m`/`2h`/`30s`/`1d` 형식의 간격 문자열을 초로 파싱한다(config incremental.interval).
///
/// 단위 미지정 순수 숫자는 초로 해석한다. 파싱 실패 시 `None`(임계 판단을 건너뜀).
pub fn parse_interval_secs(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num_part, unit): (&str, u64) = match s.chars().last() {
        Some('s') => (&s[..s.len() - 1], 1),
        Some('m') => (&s[..s.len() - 1], 60),
        Some('h') => (&s[..s.len() - 1], 3600),
        Some('d') => (&s[..s.len() - 1], 86_400),
        Some(c) if c.is_ascii_digit() => (s, 1),
        _ => return None,
    };
    num_part
        .trim()
        .parse::<u64>()
        .ok()
        .map(|n| n.saturating_mul(unit))
}

/// `hello` 응답이 샤딩(mongos)인지 판정한다(PRD §4/§6.5 — 스코프 외, 거부).
///
/// mongos는 `msg == "isdbgrid"`로 식별한다(스파이크·드라이버 관례).
pub fn is_sharded(hello: &Document) -> bool {
    hello
        .get_str("msg")
        .map(|m| m == "isdbgrid")
        .unwrap_or(false)
}

/// 서버 버전과 mongodump 버전의 호환을 판정한다(PRD §FR-8 3).
///
/// MongoDB Database Tools는 서버 메이저와 느슨하게 매핑되지 않는다(독립 버전 체계,
/// 100.x). 따라서 mongodump 부재만 **실패**로 다루고, 도구 버전을 알 수 있으면 표시만 한다.
/// 서버 메이저가 도구가 검증한 범위를 크게 벗어나면(서버 메이저가 미래로 점프) 경고한다.
///
/// - `mongodump_version`이 `None`이면 도구 부재 → `Fail`.
/// - 둘 다 있으면 기본 `Ok`(표시 위주). 단, 알려진 비호환 시그널이 있으면 `Warn`.
pub fn version_compat_status(server_version: &str, mongodump_version: Option<&str>) -> CheckStatus {
    match mongodump_version {
        None => CheckStatus::Fail,
        Some(tool) => {
            // 서버 메이저가 도구 major support 상한을 넘어서면 경고(보수적 휴리스틱).
            // Database Tools 100.x는 서버 ~8.x까지 지원한다(2024~). 서버 메이저가 9+이고
            // 도구가 100.x면 미검증 조합이므로 경고.
            let server_major = parse_major(server_version);
            let tool_major = parse_major(tool);
            match (server_major, tool_major) {
                (Some(sv), Some(100)) if sv >= 9 => CheckStatus::Warn,
                _ => CheckStatus::Ok,
            }
        }
    }
}

/// 점(`.`) 구분 버전 문자열의 첫 세그먼트(메이저)를 파싱한다.
fn parse_major(version: &str) -> Option<u32> {
    version.split('.').next()?.trim().parse::<u32>().ok()
}

/// replica set 멤버 상태 요약(replSetGetStatus 파싱 결과, 순수 표현).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReplSetSummary {
    /// PRIMARY 멤버가 존재하는가(stateStr == "PRIMARY").
    pub has_primary: bool,
    /// SECONDARY 멤버 수.
    pub secondary_count: usize,
    /// PRIMARY 대비 SECONDARY 중 최대 복제 지연(초). 산정 불가 시 None.
    pub max_secondary_lag_secs: Option<i64>,
}

/// PRIMARY 부재 또는 과도한 SECONDARY lag를 판정한다(PRD §FR-8 4, pitfall 2-5).
///
/// PRIMARY가 없으면 실패(쓰기 일관 시점을 잡을 수 없어 백업 신뢰성 위험). lag가 임계
/// (`LAG_WARN_SECS`)를 넘으면 경고. 그 외 ok.
pub fn replset_status(summary: &ReplSetSummary) -> CheckStatus {
    /// SECONDARY lag 경고 임계(초) — maxStalenessSeconds 권장 하한(90s) 근처(pitfall 2-5).
    const LAG_WARN_SECS: i64 = 120;
    if !summary.has_primary {
        return CheckStatus::Fail;
    }
    if let Some(lag) = summary.max_secondary_lag_secs {
        if lag > LAG_WARN_SECS {
            return CheckStatus::Warn;
        }
    }
    CheckStatus::Ok
}

/// `replSetGetStatus` 응답에서 멤버 상태를 요약한다(순수 파싱).
///
/// `optimeDate`(또는 `optime.ts`) 기반 PRIMARY-SECONDARY 시간차를 lag로 본다. 응답 구조는
/// 멤버 배열(`members`)이며 각 멤버는 `stateStr`·`optimeDate`를 가진다.
pub fn summarize_replset(status_doc: &Document) -> ReplSetSummary {
    let members = match status_doc.get_array("members") {
        Ok(m) => m,
        Err(_) => {
            return ReplSetSummary {
                has_primary: false,
                secondary_count: 0,
                max_secondary_lag_secs: None,
            }
        }
    };

    let mut has_primary = false;
    let mut secondary_count = 0usize;
    let mut primary_optime_ms: Option<i64> = None;
    let mut secondary_optimes_ms: Vec<i64> = Vec::new();

    for member in members {
        let Some(m) = member.as_document() else {
            continue;
        };
        let state = m.get_str("stateStr").unwrap_or("");
        // optimeDate(BSON DateTime) → epoch millis. 없으면 lag 산정에서 제외.
        let optime_ms = m
            .get_datetime("optimeDate")
            .ok()
            .map(|dt| dt.timestamp_millis());
        match state {
            "PRIMARY" => {
                has_primary = true;
                primary_optime_ms = optime_ms;
            }
            "SECONDARY" => {
                secondary_count += 1;
                if let Some(ms) = optime_ms {
                    secondary_optimes_ms.push(ms);
                }
            }
            _ => {}
        }
    }

    let max_secondary_lag_secs = match (primary_optime_ms, secondary_optimes_ms.iter().min()) {
        (Some(primary), Some(&oldest_secondary)) => {
            Some(((primary - oldest_secondary).max(0)) / 1000)
        }
        _ => None,
    };

    ReplSetSummary {
        has_primary,
        secondary_count,
        max_secondary_lag_secs,
    }
}

// ───────────────────────── 드라이버 기반 점검 실행 ─────────────────────────

/// 점검에 사용한 연결·인증 정보(메시지 구성용).
struct ConnectionInfo {
    /// 사용 인증 메커니즘 표시(없으면 "none(인증 없음)").
    mechanism: String,
    /// 인증 사용자(있으면).
    username: Option<String>,
}

/// 대상 서버 상태를 읽기 전용으로 점검하는 드라이버 래퍼.
pub struct StatusChecker {
    client: Client,
    connection: ConnectionInfo,
}

impl StatusChecker {
    /// URI 시크릿으로 연결하고 인증 메타를 캐싱한다(ping은 [`Self::full_report`] 첫 항목에서).
    pub async fn connect(uri: &Secret, timeout_secs: Option<u64>) -> Result<Self> {
        let options = super::conn::client_options(uri, timeout_secs).await?;

        // 인증 메커니즘·사용자 추출(시크릿 비노출 — 메커니즘/사용자명만).
        let connection = match &options.credential {
            Some(cred) => ConnectionInfo {
                mechanism: cred
                    .mechanism
                    .as_ref()
                    .map(|m| m.as_str().to_string())
                    .unwrap_or_else(|| "SCRAM (auto-negotiated)".to_string()),
                username: cred.username.clone(),
            },
            None => ConnectionInfo {
                mechanism: "none".to_string(),
                username: None,
            },
        };

        let client = Client::with_options(options)
            .map_err(|e| XBackupError::Failure(format!("MongoDB 클라이언트 생성 실패: {e}")))?;

        Ok(Self { client, connection })
    }

    /// 핵심 사전 점검 서브셋(backup 자동 선행용) — 연결·권한·토폴로지·도구 존재만 빠르게 본다.
    ///
    /// 전체 [`Self::full_report`]보다 가벼우며, 백업을 *막는* 결함만 검사한다. 항목 중
    /// 하나라도 `Fail`이면 [`StatusReport::overall`]이 `Fail`이 되어 호출자가 exit 3으로
    /// 백업을 미시작한다(PRD §FR-8). 경고(예: oplog 윈도우)는 여기서 보지 않는다 —
    /// 전체 status가 담당.
    /// `mongodump_program`이 `Some`이면 외부 도구 존재를 점검한다(mongodump 엔진). 네이티브
    /// 엔진은 외부 도구가 필요 없으므로 `None`을 넘겨 도구 점검을 생략한다.
    pub async fn precheck_subset(
        &self,
        profile: &str,
        mongodump_program: Option<&str>,
    ) -> StatusReport {
        // precheck 항목은 Fail 판정에만 쓰이고 사람 표시 메시지는 출력되지 않으므로 En 고정.
        let lang = crate::i18n::Lang::En;
        let mut items = Vec::new();
        items.push(self.check_connection(lang).await);

        // 연결 실패면 이후 점검은 의미 없음 — 조기 종료.
        if items[0].status == CheckStatus::Fail {
            return StatusReport::new(profile, items);
        }

        let hello = self.run_admin(doc! { "hello": 1 }).await;
        items.push(self.check_topology_core(&hello, lang));
        items.push(self.check_privileges(lang).await);
        // 네이티브 엔진은 외부 도구 불필요 — mongodump 점검을 건너뛴다.
        if let Some(program) = mongodump_program {
            items.push(check_tool_presence(program));
        }

        StatusReport::new(profile, items)
    }

    /// 전체 status 점검(FR-8 1~8) — 사람·`--json` 출력용 보고서를 만든다.
    pub async fn full_report(
        &self,
        profile: &str,
        mongodump_program: &str,
        interval: &str,
        prefer_secondary: bool,
        lang: crate::i18n::Lang,
    ) -> StatusReport {
        let mut items = Vec::new();

        // 1) 연결·인증.
        items.push(self.check_connection(lang).await);
        if items[0].status == CheckStatus::Fail {
            // 연결이 안 되면 나머지는 모두 의미 없음 — 연결 실패만 보고.
            return StatusReport::new(profile, items);
        }

        // 4) 토폴로지(샤딩 거부 포함) — 먼저 hello로 샤딩을 잡는다.
        let hello = self.run_admin(doc! { "hello": 1 }).await;
        let topology_item = self.check_topology_full(&hello, lang).await;
        let is_sharded_target = topology_item.status == CheckStatus::Fail
            && hello.as_ref().map(is_sharded).unwrap_or(false);
        items.push(topology_item);

        // 2) 권한.
        items.push(self.check_privileges(lang).await);
        // 3) 버전 정합.
        items.push(self.check_version(mongodump_program, lang).await);
        // 3.5) FCV(호환성 경계).
        items.push(self.check_fcv(lang).await);
        // 6) 저장 엔진.
        items.push(self.check_storage_engine(lang).await);
        // 3.6) 서버 시계(clock skew).
        items.push(self.check_clock(lang).await);

        // 샤딩이면 oplog/크기/secondary 점검은 스코프 외이므로 생략(거부가 우선).
        if !is_sharded_target {
            // 5) oplog 윈도우.
            items.push(self.check_oplog_window(&hello, interval, lang).await);
            // 7) 데이터 형상 + 예상 크기 — dbStats 한 번 합산으로 함께 만든다.
            match self.db_stats_totals().await {
                Ok(totals) => {
                    items.extend(Self::shape_items(&totals, lang));
                    items.push(Self::estimated_size_item(&totals));
                }
                Err(e) => {
                    items.push(
                        CheckItem::warn(
                            "estimated_size",
                            "est. size",
                            lang.sel(
                                &format!("dbStats aggregation failed (privileges may be insufficient): {e}"),
                                &format!("dbStats 합산 실패(권한 부족 가능): {e}"),
                            ),
                        )
                        .with_value("조회 실패"),
                    );
                }
            }
            // 8) (선택) secondary 가용성.
            if prefer_secondary {
                items.push(self.check_secondary_availability(&hello, lang));
            }
        }

        StatusReport::new(profile, items)
    }

    /// admin DB에 명령을 실행하고 결과 Document를 돌려준다(읽기 전용 점검 전용).
    async fn run_admin(&self, command: Document) -> Result<Document> {
        self.client
            .database("admin")
            .run_command(command)
            .await
            .map_err(|e| XBackupError::Failure(format!("명령 실행 실패: {e}")))
    }

    /// 1) 연결·인증 — admin.ping + 인증 메커니즘 표시(읽기 전용).
    async fn check_connection(&self, lang: crate::i18n::Lang) -> CheckItem {
        match self.run_admin(doc! { "ping": 1 }).await {
            Ok(_) => {
                let who = match &self.connection.username {
                    Some(u) => lang
                        .sel(&format!("user={u}, "), &format!("사용자={u}, "))
                        .to_string(),
                    None => String::new(),
                };
                CheckItem::ok(
                    "connection",
                    "connection",
                    lang.sel(
                        &format!("connected ({who}mechanism={})", self.connection.mechanism),
                        &format!("연결 성공({who}메커니즘={})", self.connection.mechanism),
                    ),
                )
                .with_value(self.connection.mechanism.to_string())
            }
            Err(e) => CheckItem::fail(
                "connection",
                "connection",
                lang.sel(
                    &format!("connection failed: {e}"),
                    &format!("연결 실패: {e}"),
                ),
            )
            .with_value("연결 실패"),
        }
    }

    /// 2) 권한 — connectionStatus {showPrivileges:true}로 부여 액션을 모아 필요 집합과 대조.
    async fn check_privileges(&self, lang: crate::i18n::Lang) -> CheckItem {
        let resp = self
            .run_admin(doc! { "connectionStatus": 1, "showPrivileges": true })
            .await;
        let doc = match resp {
            Ok(d) => d,
            Err(e) => {
                return CheckItem::fail(
                    "privileges",
                    "privileges",
                    lang.sel(
                        &format!("privilege lookup failed: {e}"),
                        &format!("권한 조회 실패: {e}"),
                    ),
                )
                .with_value("조회 실패")
            }
        };
        // 인증 비활성 서버(인증된 사용자 없음)는 사실상 전권 — 권한 누락으로 보지 않는다.
        // 단, 운영상 주의가 필요한 구성이므로 경고로 남긴다(백업은 막지 않음).
        if auth_is_disabled(&doc) {
            return CheckItem::warn(
                "privileges",
                "privileges",
                lang.sel(
                    "authentication is disabled on this server (no authenticated user) — the \
                     connecting principal has full privileges, so privileges are sufficient, but \
                     enabling authentication is recommended in production",
                    "인증이 비활성화된 서버입니다(인증된 사용자 없음) — 접속 주체가 전권을 가지므로 \
                     권한은 충분하나, 운영 환경에서는 인증 활성화를 권장합니다",
                ),
            )
            .with_value("인증 비활성");
        }
        let granted = collect_granted_actions(&doc);
        let missing = missing_actions(&granted);
        if missing.is_empty() {
            CheckItem::ok(
                "privileges",
                "privileges",
                lang.sel(
                    "has the privileges required for backup (all-DB read + oplog read equivalent)",
                    "백업에 필요한 권한(전 DB read + oplog 읽기 상당)을 보유",
                ),
            )
            .with_value(lang.sel("sufficient", "충분"))
        } else {
            CheckItem::fail(
                "privileges",
                "privileges",
                lang.sel(
                    &format!(
                        "missing required privileges: [{}] — grant backup/read roles (backup, read/readAnyDatabase)",
                        missing.join(", ")
                    ),
                    &format!(
                        "필요 권한 누락: [{}] — 백업/읽기 역할(backup, read/readAnyDatabase) 부여 필요",
                        missing.join(", ")
                    ),
                ),
            )
            .with_value(format!("누락 {}", missing.len()))
        }
    }

    /// 4) 토폴로지(핵심 서브셋) — 샤딩 거부 + standalone/replica set 판별만.
    fn check_topology_core(&self, hello: &Result<Document>, lang: crate::i18n::Lang) -> CheckItem {
        match hello {
            Ok(doc) if is_sharded(doc) => CheckItem::fail(
                "topology",
                "topology",
                lang.sel(
                    "sharded cluster (mongos) detected — out of initial scope. Refusing backup (PRD §4/§6.5)",
                    "샤딩 클러스터(mongos) 감지 — 1차 스코프 외입니다. 백업을 거부합니다(PRD §4/§6.5)",
                ),
            )
            .with_value("sharded"),
            Ok(doc) => {
                if doc.get_str("setName").is_ok() {
                    let set = doc.get_str("setName").unwrap_or("?");
                    CheckItem::ok(
                        "topology",
                        "topology",
                        format!("replica set(setName={set})"),
                    )
                    .with_value(format!("rs:{set}"))
                } else {
                    CheckItem::ok("topology", "topology", "standalone".to_string())
                        .with_value("standalone")
                }
            }
            Err(e) => CheckItem::fail(
                "topology",
                "topology",
                lang.sel(
                    &format!("hello lookup failed: {e}"),
                    &format!("hello 조회 실패: {e}"),
                ),
            )
            .with_value("조회 실패"),
        }
    }

    /// 4) 토폴로지(전체) — 핵심 판별 + replica set 멤버 상태(PRIMARY/lag) 요약.
    async fn check_topology_full(
        &self,
        hello: &Result<Document>,
        lang: crate::i18n::Lang,
    ) -> CheckItem {
        let core = self.check_topology_core(hello, lang);
        // 핵심에서 fail/standalone이면 그대로. replica set일 때만 멤버 상태를 덧붙인다.
        if core.status != CheckStatus::Ok {
            return core;
        }
        let is_rs = hello
            .as_ref()
            .map(|d| d.get_str("setName").is_ok())
            .unwrap_or(false);
        if !is_rs {
            return core; // standalone — 멤버 상태 없음.
        }

        match self.run_admin(doc! { "replSetGetStatus": 1 }).await {
            Ok(doc) => {
                let summary = summarize_replset(&doc);
                let status = replset_status(&summary);
                let lag = summary
                    .max_secondary_lag_secs
                    .map(|s| format!("{s}s"))
                    .unwrap_or_else(|| "n/a".to_string());
                let set = hello
                    .as_ref()
                    .ok()
                    .and_then(|d| d.get_str("setName").ok())
                    .unwrap_or("?");
                let primary_disp = if summary.has_primary {
                    lang.sel("yes", "있음")
                } else {
                    lang.sel("no", "없음")
                };
                let msg = lang.sel(
                    &format!(
                        "replica set(setName={set}, PRIMARY={primary_disp}, SECONDARY={}, max lag={lag})",
                        summary.secondary_count,
                    ),
                    &format!(
                        "replica set(setName={set}, PRIMARY={primary_disp}, SECONDARY={}, 최대 lag={lag})",
                        summary.secondary_count,
                    ),
                )
                .to_string();
                CheckItem {
                    key: "topology",
                    label: "topology",
                    status,
                    message: msg,
                    value: Some(format!("rs:{set}")),
                }
            }
            // replSetGetStatus 실패(권한 등)는 토폴로지 판별 자체는 됐으므로 경고로 강등.
            Err(e) => CheckItem::warn(
                "topology",
                "topology",
                lang.sel(
                    &format!("{} (member status lookup failed: {e})", core.message),
                    &format!("{} (멤버 상태 조회 실패: {e})", core.message),
                ),
            )
            .with_value(core.value.clone().unwrap_or_default()),
        }
    }

    /// 3) 버전 정합 — buildInfo(서버) + mongodump --version(도구).
    async fn check_version(&self, mongodump_program: &str, lang: crate::i18n::Lang) -> CheckItem {
        let server_version = match self.run_admin(doc! { "buildInfo": 1 }).await {
            Ok(doc) => doc.get_str("version").unwrap_or("unknown").to_string(),
            Err(e) => {
                return CheckItem::fail(
                    "version",
                    "version",
                    lang.sel(
                        &format!("buildInfo lookup failed: {e}"),
                        &format!("buildInfo 조회 실패: {e}"),
                    ),
                )
                .with_value("조회 실패")
            }
        };
        let tool_version = detect_mongodump_version(mongodump_program);
        let status = version_compat_status(&server_version, tool_version.as_deref());
        let tool_disp = tool_version.clone().unwrap_or_else(|| {
            lang.sel("not installed/not found", "미설치/탐색 실패")
                .to_string()
        });
        let msg = match status {
            CheckStatus::Fail => lang.sel(
                &format!(
                    "mongodump not found ('{mongodump_program}'). server version={server_version} \
                     — install Database Tools / check PATH (e.g. mongodb-database-tools)"
                ),
                &format!(
                    "mongodump를 찾을 수 없습니다('{mongodump_program}'). 서버 버전={server_version} \
                     — Database Tools 설치/PATH 확인(예: mongodb-database-tools)"
                ),
            )
            .to_string(),
            CheckStatus::Warn => lang.sel(
                &format!(
                    "server={server_version}, mongodump={tool_disp} — major-version gap may be incompatible, verification recommended"
                ),
                &format!(
                    "서버={server_version}, mongodump={tool_disp} — 메이저 차이로 비호환 가능, 검증 권장"
                ),
            )
            .to_string(),
            CheckStatus::Ok => {
                format!("server={server_version}, mongodump={tool_disp}")
            }
        };
        CheckItem {
            key: "version",
            label: "version",
            status,
            message: msg,
            // 비교는 서버 버전 기준(mongodump는 로컬 도구라 서버 간 diff 의미 없음).
            value: Some(server_version),
        }
    }

    /// 6) 저장 엔진 — serverStatus.storageEngine.name.
    async fn check_storage_engine(&self, lang: crate::i18n::Lang) -> CheckItem {
        match self.run_admin(doc! { "serverStatus": 1 }).await {
            Ok(doc) => {
                let engine = doc
                    .get_document("storageEngine")
                    .ok()
                    .and_then(|se| se.get_str("name").ok())
                    .unwrap_or("unknown");
                CheckItem::ok(
                    "storage_engine",
                    "storage engine",
                    format!("storageEngine={engine}"),
                )
                .with_value(engine.to_string())
            }
            // serverStatus는 clusterMonitor 권한이 필요할 수 있어 실패는 경고로 강등.
            Err(e) => CheckItem::warn(
                "storage_engine",
                "storage engine",
                lang.sel(
                    &format!("serverStatus lookup failed (privileges may be insufficient): {e}"),
                    &format!("serverStatus 조회 실패(권한 부족 가능): {e}"),
                ),
            )
            .with_value("조회 실패"),
        }
    }

    /// 5) oplog 윈도우 — 최소~최신 ts 시간 폭, config interval 대비 여유.
    async fn check_oplog_window(
        &self,
        hello: &Result<Document>,
        interval: &str,
        lang: crate::i18n::Lang,
    ) -> CheckItem {
        let is_rs = hello
            .as_ref()
            .map(|d| d.get_str("setName").is_ok())
            .unwrap_or(false);
        if !is_rs {
            return CheckItem::warn(
                "oplog_window",
                "oplog window",
                lang.sel(
                    "standalone — no oplog (incremental backup unavailable, full backup only)",
                    "standalone — oplog 없음(증분 백업 불가, 풀 백업만 가능)",
                ),
            )
            .with_value("없음(standalone)");
        }

        let oplog = self
            .client
            .database("local")
            .collection::<Document>("oplog.rs");

        let oldest = find_one_oplog_ts(&oplog, 1).await;
        let newest = find_one_oplog_ts(&oplog, -1).await;

        match (oldest, newest) {
            (Some(o), Some(n)) => {
                let window_secs = (n.time as i64 - o.time as i64).max(0) as u64;
                let interval_secs = parse_interval_secs(interval);
                let status = match interval_secs {
                    Some(iv) => oplog_window_status(window_secs, iv),
                    None => CheckStatus::Ok,
                };
                let iv_disp = interval_secs.map(|s| format!("{s}s")).unwrap_or_else(|| {
                    lang.sel(
                        &format!("'{interval}'(unparseable)"),
                        &format!("'{interval}'(파싱 불가)"),
                    )
                    .to_string()
                });
                let msg = match status {
                    CheckStatus::Warn => lang.sel(
                        &format!(
                            "window={window_secs}s is less than 2x the incremental interval ({iv_disp}) — gap risk (shorten interval / increase oplog, §6.2)"
                        ),
                        &format!(
                            "윈도우={window_secs}s 가 증분 주기({iv_disp})의 2배 미만 — gap 위험(주기 단축/oplog 증대 권고, §6.2)"
                        ),
                    )
                    .to_string(),
                    _ => lang.sel(
                        &format!("window={window_secs}s, incremental interval={iv_disp}"),
                        &format!("윈도우={window_secs}s, 증분 주기={iv_disp}"),
                    )
                    .to_string(),
                };
                CheckItem {
                    key: "oplog_window",
                    label: "oplog window",
                    status,
                    message: msg,
                    value: Some(format!("{window_secs}s")),
                }
            }
            _ => CheckItem::warn(
                "oplog_window",
                "oplog window",
                lang.sel(
                    "could not read oplog entries (privileges / empty oplog possible)",
                    "oplog 엔트리를 읽지 못함(권한/빈 oplog 가능)",
                ),
            )
            .with_value("읽기 실패"),
        }
    }

    /// 사용자 DB의 `dbStats`를 한 번 쓸어 합산한다(문서·컬렉션·인덱스 수, 데이터·인덱스 크기).
    ///
    /// `dbStats` 한 콜이 `objects`·`collections`·`indexes`·`dataSize`·`storageSize`·`indexSize`를
    /// 모두 돌려주므로, 크기·데이터 형상 항목을 추가 쿼리 없이 같은 합산에서 만든다.
    async fn db_stats_totals(&self) -> Result<DbTotals> {
        let db_names = self.client.list_database_names().await.map_err(|e| {
            XBackupError::Failure(format!("DB 목록 조회 실패(권한 부족 가능): {e}"))
        })?;
        let mut t = DbTotals::default();
        for name in &db_names {
            // 시스템 DB(local 등)는 백업 대상 추정에서 제외.
            if matches!(name.as_str(), "admin" | "config" | "local") {
                continue;
            }
            if let Ok(stats) = self
                .client
                .database(name)
                .run_command(doc! { "dbStats": 1 })
                .await
            {
                t.objects += read_num(&stats, "objects");
                t.collections += read_num(&stats, "collections");
                t.indexes += read_num(&stats, "indexes");
                t.data_size += read_num(&stats, "dataSize");
                t.storage_size += read_num(&stats, "storageSize");
                t.index_size += read_num(&stats, "indexSize");
            }
        }
        Ok(t)
    }

    /// 7) 예상 크기 — dataSize/storageSize(+ 인덱스 크기)를 [`DbTotals`]에서 만든다.
    fn estimated_size_item(totals: &DbTotals) -> CheckItem {
        CheckItem::ok(
            "estimated_size",
            "est. size",
            format!(
                "dataSize={} ({}), storageSize={} ({}), indexSize={}",
                totals.data_size,
                human_bytes(totals.data_size),
                totals.storage_size,
                human_bytes(totals.storage_size),
                human_bytes(totals.index_size),
            ),
        )
        // 비교는 dataSize 기준(논리 데이터량 — 백업 대상 크기에 가장 근접).
        .with_value(human_bytes(totals.data_size))
    }

    /// 데이터 형상 항목 — 문서 수·컬렉션 수·인덱스 수(+ 인덱스 크기). 비교 뷰의 drift 확인용.
    fn shape_items(totals: &DbTotals, lang: crate::i18n::Lang) -> Vec<CheckItem> {
        vec![
            CheckItem::ok(
                "doc_count",
                "documents",
                lang.sel(
                    &format!(
                        "~{} documents (sum of estimatedDocumentCount)",
                        totals.objects
                    ),
                    &format!("추정 문서 {}건(estimatedDocumentCount 합)", totals.objects),
                ),
            )
            .with_value(totals.objects.to_string()),
            CheckItem::ok(
                "collection_count",
                "collections",
                lang.sel(
                    &format!("{} user collections", totals.collections),
                    &format!("사용자 컬렉션 {}개", totals.collections),
                ),
            )
            .with_value(totals.collections.to_string()),
            CheckItem::ok(
                "index_count",
                "indexes",
                lang.sel(
                    &format!(
                        "{} indexes, index size {}",
                        totals.indexes,
                        human_bytes(totals.index_size)
                    ),
                    &format!(
                        "인덱스 {}개, 인덱스 크기 {}",
                        totals.indexes,
                        human_bytes(totals.index_size)
                    ),
                ),
            )
            .with_value(totals.indexes.to_string()),
        ]
    }

    /// 3.5) FCV(featureCompatibilityVersion) — 서버 버전과 별개의 호환성 경계.
    async fn check_fcv(&self, lang: crate::i18n::Lang) -> CheckItem {
        let resp = self
            .run_admin(doc! { "getParameter": 1, "featureCompatibilityVersion": 1 })
            .await;
        match resp {
            Ok(doc) => {
                let fcv = doc
                    .get_document("featureCompatibilityVersion")
                    .ok()
                    .and_then(|d| d.get_str("version").ok())
                    .unwrap_or("unknown")
                    .to_string();
                CheckItem::ok("fcv", "FCV", format!("featureCompatibilityVersion={fcv}"))
                    .with_value(fcv)
            }
            // FCV 조회는 권한이 필요할 수 있어 실패는 경고로 강등(백업을 막지 않음).
            Err(e) => CheckItem::warn(
                "fcv",
                "FCV",
                lang.sel(
                    &format!("FCV lookup failed (privileges may be insufficient): {e}"),
                    &format!("FCV 조회 실패(권한 부족 가능): {e}"),
                ),
            )
            .with_value("조회 실패"),
        }
    }

    /// 3.6) 서버 시계 — `hello.localTime`과 로컬 시각의 차(clock skew). PITR/oplog 안전성 신호.
    ///
    /// 왕복 지연만큼 오차가 있으므로 근사값이다. |skew| ≥ 5초면 경고(시계 동기 권장).
    async fn check_clock(&self, lang: crate::i18n::Lang) -> CheckItem {
        let before = Utc::now().timestamp_millis();
        let hello = self.run_admin(doc! { "hello": 1 }).await;
        let after = Utc::now().timestamp_millis();
        // bson DateTime → epoch millis(chrono feature 비의존).
        let server_ms = hello
            .as_ref()
            .ok()
            .and_then(|d| d.get_datetime("localTime").ok())
            .map(|dt| dt.timestamp_millis());
        let server_ms = match server_ms {
            Some(s) => s,
            None => {
                return CheckItem::warn(
                    "clock",
                    "server clock",
                    lang.sel("could not read server time", "서버 시각을 읽지 못함"),
                )
                .with_value("불명")
            }
        };
        // 클라이언트 시각의 중간값(왕복 보정)과 서버 시각의 차.
        let mid = before + (after - before) / 2;
        let skew_ms = server_ms - mid;
        let skew_s = skew_ms as f64 / 1000.0;
        let sign = if skew_ms >= 0 { "+" } else { "-" };
        let disp = format!("{sign}{:.1}s", skew_s.abs());
        let msg = lang
            .sel(
                &format!("server is {disp} relative to local (round-trip-corrected approximation)"),
                &format!("서버가 로컬 대비 {disp} (왕복 보정 근사)"),
            )
            .to_string();
        if skew_ms.abs() >= 5_000 {
            CheckItem::warn(
                "clock",
                "server clock",
                lang.sel(
                    &format!(
                        "{msg} — over 5s difference, NTP sync recommended (oplog/PITR accuracy)"
                    ),
                    &format!("{msg} — 5초 이상 차이, NTP 동기 권장(oplog/PITR 정확도)"),
                ),
            )
            .with_value(disp)
        } else {
            CheckItem::ok("clock", "server clock", msg).with_value(disp)
        }
    }

    /// 8) (선택) secondary 가용성 — prefer_secondary 구성 시 읽기 가능한 secondary 존재 여부.
    fn check_secondary_availability(
        &self,
        hello: &Result<Document>,
        lang: crate::i18n::Lang,
    ) -> CheckItem {
        let hosts = hello
            .as_ref()
            .ok()
            .and_then(|d| d.get_array("hosts").ok())
            .map(|a| a.len())
            .unwrap_or(0);
        let is_primary = hello
            .as_ref()
            .ok()
            .and_then(|d| d.get_bool("isWritablePrimary").ok())
            .unwrap_or(false);
        // 멤버가 2개 이상이면 secondary 후보가 있다고 본다(정밀 판정은 replSetGetStatus가 담당).
        if hosts >= 2 {
            CheckItem::ok(
                "secondary",
                "secondary availability",
                lang.sel(
                    &format!("{hosts} members — prefer_secondary backup possible (secondary candidate exists)"),
                    &format!("멤버 {hosts}개 — prefer_secondary 백업 가능(secondary 후보 존재)"),
                ),
            )
            .with_value(format!("멤버 {hosts}"))
        } else if is_primary {
            CheckItem::warn(
                "secondary",
                "secondary availability",
                lang.sel(
                    "prefer_secondary is configured but no readable secondary — backing up from PRIMARY",
                    "prefer_secondary 구성이나 읽을 secondary가 없음 — PRIMARY에서 백업됨",
                ),
            )
            .with_value("없음")
        } else {
            CheckItem::warn(
                "secondary",
                "secondary availability",
                lang.sel(
                    "insufficient member info to determine secondary availability",
                    "secondary 가용성을 판정할 멤버 정보가 부족",
                ),
            )
            .with_value("불명")
        }
    }
}

/// 인증이 **비활성**인 서버인지(인증된 사용자가 없는지) 판정한다.
///
/// `connectionStatus.authInfo.authenticatedUsers`가 비어 있으면 인증이 꺼져 있거나
/// (auth 미설정) localhost 예외로 익명 접속한 상태다 — 이 경우 접속 주체는 사실상
/// **전권**을 가지므로 권한 누락으로 볼 수 없다(`authenticatedUserPrivileges`도 비어
/// 권한 검사가 거짓 음성을 낸다). 백업을 막을 결함이 아니므로 별도 판정한다.
///
/// 배열이 없으면(필드 부재) 보수적으로 "인증된 사용자 없음(=auth 비활성)"으로 본다.
fn auth_is_disabled(conn_status: &Document) -> bool {
    let Ok(auth_info) = conn_status.get_document("authInfo") else {
        return true;
    };
    match auth_info.get_array("authenticatedUsers") {
        Ok(users) => users.is_empty(),
        Err(_) => true,
    }
}

/// `connectionStatus` 응답에서 부여된 모든 액션 이름을 합집합으로 모은다.
///
/// 구조: `authInfo.authenticatedUserPrivileges[].actions[]`. showPrivileges가 true일 때만
/// privileges가 채워진다. 권한이 없으면 빈 집합.
fn collect_granted_actions(conn_status: &Document) -> BTreeSet<String> {
    let mut actions = BTreeSet::new();
    let Ok(auth_info) = conn_status.get_document("authInfo") else {
        return actions;
    };
    let Ok(privs) = auth_info.get_array("authenticatedUserPrivileges") else {
        return actions;
    };
    for priv_entry in privs {
        let Some(p) = priv_entry.as_document() else {
            continue;
        };
        if let Ok(acts) = p.get_array("actions") {
            for a in acts {
                if let Some(name) = a.as_str() {
                    actions.insert(name.to_string());
                }
            }
        }
    }
    actions
}

/// oplog 컬렉션에서 `$natural` 정렬(1=오름차순 최소, -1=내림차순 최신) 1건의 ts를 읽는다.
async fn find_one_oplog_ts(
    oplog: &mongodb::Collection<Document>,
    natural: i32,
) -> Option<Timestamp> {
    let options = FindOneOptions::builder()
        .sort(doc! { "$natural": natural })
        .build();
    oplog
        .find_one(doc! {})
        .with_options(options)
        .await
        .ok()
        .flatten()
        .and_then(|d| d.get_timestamp("ts").ok())
}

/// Document에서 정수형 필드를 i64로 읽는다(dbStats 수치는 i32/i64/f64 혼재 가능).
fn read_num(doc: &Document, key: &str) -> i64 {
    if let Ok(v) = doc.get_i64(key) {
        return v;
    }
    if let Ok(v) = doc.get_i32(key) {
        return v as i64;
    }
    if let Ok(v) = doc.get_f64(key) {
        return v as i64;
    }
    0
}

/// 바이트 수를 사람이 읽는 단위로 근사 표기한다(표시 전용).
pub fn human_bytes(bytes: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// `mongodump --version`을 실행해 버전 문자열을 파싱한다(PATH/설치 경로 의존).
///
/// 출력 예: `mongodump version: 100.16.1` → `"100.16.1"`. 실행 실패/미설치면 None.
/// 읽기 전용·부작용 없음(외부 도구의 버전만 묻는다).
fn detect_mongodump_version(program: &str) -> Option<String> {
    let output = std::process::Command::new(program)
        .arg("--version")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parse_mongodump_version(&text)
}

/// `mongodump --version` 출력에서 `version:` 라인의 버전 토큰을 파싱한다(순수 함수).
pub fn parse_mongodump_version(output: &str) -> Option<String> {
    for line in output.lines() {
        let lower = line.to_ascii_lowercase();
        if lower.contains("version") {
            // "mongodump version: 100.16.1" → 마지막 토큰.
            if let Some(idx) = line.find(':') {
                let candidate = line[idx + 1..].trim();
                if !candidate.is_empty() {
                    return Some(candidate.to_string());
                }
            }
        }
    }
    None
}

/// 도구(mongodump) 존재 여부만 빠르게 점검한다(precheck 서브셋 항목).
///
/// 버전 파싱까지 가지 않고 실행 가능 여부만 본다 — 부재는 백업을 막는 실패다.
pub fn check_tool_presence(program: &str) -> CheckItem {
    match detect_mongodump_version(program) {
        Some(v) => CheckItem::ok("tool", "backup tool", format!("mongodump={v}")),
        None => CheckItem::fail(
            "tool",
            "backup tool",
            format!("mongodump not found ('{program}') — check install/PATH"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 신호등 합산 → exit code ──
    #[test]
    fn aggregate_picks_worst() {
        let items = vec![
            CheckItem::ok("a", "A", "x"),
            CheckItem::warn("b", "B", "y"),
            CheckItem::ok("c", "C", "z"),
        ];
        assert_eq!(aggregate(&items), CheckStatus::Warn);

        let with_fail = vec![
            CheckItem::warn("a", "A", "x"),
            CheckItem::fail("b", "B", "y"),
        ];
        assert_eq!(aggregate(&with_fail), CheckStatus::Fail);

        let all_ok = vec![CheckItem::ok("a", "A", "x")];
        assert_eq!(aggregate(&all_ok), CheckStatus::Ok);

        assert_eq!(aggregate(&[]), CheckStatus::Ok);
    }

    #[test]
    fn overall_exit_codes_match_prd() {
        assert_eq!(overall_exit_code(CheckStatus::Ok), 0);
        assert_eq!(overall_exit_code(CheckStatus::Fail), 3);
        assert_eq!(overall_exit_code(CheckStatus::Warn), 4);
    }

    #[test]
    fn report_exit_code_via_items() {
        let r = StatusReport::new(
            "p",
            vec![CheckItem::ok("a", "A", ""), CheckItem::fail("b", "B", "")],
        );
        assert_eq!(r.overall, CheckStatus::Fail);
        assert_eq!(r.exit_code(), 3);

        let warn = StatusReport::new("p", vec![CheckItem::warn("a", "A", "")]);
        assert_eq!(warn.exit_code(), 4);

        let ok = StatusReport::new("p", vec![CheckItem::ok("a", "A", "")]);
        assert_eq!(ok.exit_code(), 0);
    }

    // ── 권한 누락 집계 ──
    #[test]
    fn missing_actions_reports_gaps() {
        let mut granted = BTreeSet::new();
        granted.insert("find".to_string());
        // listCollections 누락.
        assert_eq!(
            missing_actions(&granted),
            vec!["listCollections".to_string()]
        );

        granted.insert("listCollections".to_string());
        assert!(missing_actions(&granted).is_empty());

        // 빈 권한 → 모두 누락.
        let empty = BTreeSet::new();
        assert_eq!(missing_actions(&empty).len(), REQUIRED_ACTIONS.len());
    }

    #[test]
    fn collect_granted_actions_unions_privileges() {
        let conn = doc! {
            "authInfo": {
                "authenticatedUserPrivileges": [
                    { "resource": { "db": "", "collection": "" }, "actions": ["find", "listCollections"] },
                    { "resource": { "db": "local", "collection": "oplog.rs" }, "actions": ["find"] },
                ]
            }
        };
        let actions = collect_granted_actions(&conn);
        assert!(actions.contains("find"));
        assert!(actions.contains("listCollections"));
        assert!(missing_actions(&actions).is_empty());
    }

    #[test]
    fn collect_granted_actions_empty_when_no_privileges() {
        let conn = doc! { "authInfo": { "authenticatedUsers": [] } };
        assert!(collect_granted_actions(&conn).is_empty());
    }

    // ── 인증 비활성 판정(no-auth 서버에서 권한 거짓 음성 방지) ──
    #[test]
    fn auth_disabled_when_no_authenticated_users() {
        // 인증된 사용자 배열이 비어 있으면 auth 비활성으로 본다.
        let conn = doc! { "authInfo": { "authenticatedUsers": [] } };
        assert!(auth_is_disabled(&conn));
        // authInfo 자체가 없어도 보수적으로 비활성으로 본다.
        assert!(auth_is_disabled(&doc! {}));
        // authenticatedUsers 필드가 없으면(부재) 비활성으로 본다.
        assert!(auth_is_disabled(&doc! { "authInfo": {} }));
    }

    #[test]
    fn auth_enabled_when_user_present() {
        let conn = doc! {
            "authInfo": {
                "authenticatedUsers": [ { "user": "backup", "db": "admin" } ],
                "authenticatedUserPrivileges": [
                    { "resource": { "db": "", "collection": "" }, "actions": ["find", "listCollections"] },
                ]
            }
        };
        assert!(!auth_is_disabled(&conn), "사용자가 있으면 auth 활성");
    }

    // ── oplog 윈도우 임계 ──
    #[test]
    fn oplog_window_warns_when_too_short() {
        // interval 900s(15m). 윈도우가 2배(1800s) 미만이면 경고.
        assert_eq!(oplog_window_status(1000, 900), CheckStatus::Warn);
        assert_eq!(oplog_window_status(1799, 900), CheckStatus::Warn);
        assert_eq!(oplog_window_status(1800, 900), CheckStatus::Ok);
        assert_eq!(oplog_window_status(7200, 900), CheckStatus::Ok);
        // interval 0이면 판단 불가 → ok.
        assert_eq!(oplog_window_status(10, 0), CheckStatus::Ok);
    }

    #[test]
    fn parse_interval_handles_units() {
        assert_eq!(parse_interval_secs("15m"), Some(900));
        assert_eq!(parse_interval_secs("2h"), Some(7200));
        assert_eq!(parse_interval_secs("30s"), Some(30));
        assert_eq!(parse_interval_secs("1d"), Some(86_400));
        assert_eq!(parse_interval_secs("45"), Some(45)); // 단위 없으면 초.
        assert_eq!(parse_interval_secs(""), None);
        assert_eq!(parse_interval_secs("abc"), None);
        assert_eq!(parse_interval_secs("m"), None);
    }

    // ── 샤딩 감지(가짜 hello) ──
    #[test]
    fn detects_sharded_mongos() {
        let mongos = doc! { "msg": "isdbgrid", "ok": 1.0 };
        assert!(is_sharded(&mongos));

        let rs = doc! { "setName": "rs0", "isWritablePrimary": true };
        assert!(!is_sharded(&rs));

        let standalone = doc! { "isWritablePrimary": true };
        assert!(!is_sharded(&standalone));
    }

    #[test]
    fn topology_core_rejects_sharding_with_fail() {
        // check_topology_core는 &self가 필요 없으므로 정적 검증을 위해 is_sharded + 메시지 로직만
        // 간접 확인한다(드라이버 의존 없는 순수 판정은 is_sharded가 담당).
        let mongos = doc! { "msg": "isdbgrid" };
        assert!(is_sharded(&mongos));
        // 신호등 합산: 샤딩 fail 1건이면 전체 fail → exit 3.
        let items = vec![CheckItem::fail("topology", "토폴로지", "샤딩")];
        assert_eq!(overall_exit_code(aggregate(&items)), 3);
    }

    // ── 버전 호환 ──
    #[test]
    fn version_compat_fails_when_tool_absent() {
        assert_eq!(version_compat_status("7.0.35", None), CheckStatus::Fail);
    }

    #[test]
    fn version_compat_ok_for_normal_pair() {
        assert_eq!(
            version_compat_status("7.0.35", Some("100.16.1")),
            CheckStatus::Ok
        );
        assert_eq!(
            version_compat_status("8.0.0", Some("100.16.1")),
            CheckStatus::Ok
        );
    }

    #[test]
    fn version_compat_warns_for_future_server_major() {
        // 서버 메이저 9+ 와 도구 100.x 조합은 미검증 → 경고.
        assert_eq!(
            version_compat_status("9.0.0", Some("100.16.1")),
            CheckStatus::Warn
        );
    }

    #[test]
    fn parse_mongodump_version_extracts_token() {
        assert_eq!(
            parse_mongodump_version("mongodump version: 100.16.1\ngit version: abc"),
            Some("100.16.1".to_string())
        );
        assert_eq!(parse_mongodump_version("no version line at all\n"), None);
    }

    // ── replica set 멤버 요약 ──
    #[test]
    fn replset_fails_without_primary() {
        let s = ReplSetSummary {
            has_primary: false,
            secondary_count: 2,
            max_secondary_lag_secs: Some(1),
        };
        assert_eq!(replset_status(&s), CheckStatus::Fail);
    }

    #[test]
    fn replset_warns_on_high_lag() {
        let s = ReplSetSummary {
            has_primary: true,
            secondary_count: 1,
            max_secondary_lag_secs: Some(300),
        };
        assert_eq!(replset_status(&s), CheckStatus::Warn);
    }

    #[test]
    fn replset_ok_with_primary_and_low_lag() {
        let s = ReplSetSummary {
            has_primary: true,
            secondary_count: 2,
            max_secondary_lag_secs: Some(5),
        };
        assert_eq!(replset_status(&s), CheckStatus::Ok);
    }

    #[test]
    fn summarize_replset_counts_members() {
        // optimeDate는 BSON DateTime(epoch millis)으로 구성한다.
        let primary_dt = bson::DateTime::from_millis(1_000_000);
        let sec_dt = bson::DateTime::from_millis(990_000); // 10s 뒤처짐.
        let status = doc! {
            "members": [
                { "stateStr": "PRIMARY", "optimeDate": primary_dt },
                { "stateStr": "SECONDARY", "optimeDate": sec_dt },
                { "stateStr": "ARBITER" },
            ]
        };
        let summary = summarize_replset(&status);
        assert!(summary.has_primary);
        assert_eq!(summary.secondary_count, 1);
        assert_eq!(summary.max_secondary_lag_secs, Some(10));
    }

    #[test]
    fn summarize_replset_empty_members() {
        let summary = summarize_replset(&doc! {});
        assert!(!summary.has_primary);
        assert_eq!(summary.secondary_count, 0);
        assert_eq!(summary.max_secondary_lag_secs, None);
    }

    // ── 도구 존재 점검 ──
    #[test]
    fn tool_presence_fails_for_missing_program() {
        let item = check_tool_presence("/nonexistent/x-fake-mongodump");
        assert_eq!(item.status, CheckStatus::Fail);
        assert_eq!(item.key, "tool");
    }

    // ── human_bytes 표기 ──
    #[test]
    fn human_bytes_scales_units() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MiB");
    }

    // ── JSON 직렬화: overall + items ──
    #[test]
    fn report_serializes_to_json_with_items_and_overall() {
        let report = StatusReport::new(
            "prod",
            vec![
                CheckItem::ok("connection", "연결·인증", "연결 성공"),
                CheckItem::warn("oplog_window", "oplog 윈도우", "윈도우 부족"),
            ],
        );
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["profile"], "prod");
        assert_eq!(json["overall"], "warn");
        assert_eq!(json["items"][0]["key"], "connection");
        assert_eq!(json["items"][0]["status"], "ok");
        assert_eq!(json["items"][1]["status"], "warn");
    }
}
