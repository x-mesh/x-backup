//! 증분 체인 연속성 검증 — PITR 전제(PRD §FR-7 verify --chain, §6.4).
//!
//! PITR은 base 풀백업 + 그 위에 시간순으로 이어지는 증분 슬라이스들을 **끊김 없이**
//! 재생할 수 있어야 가능하다. 이 모듈은 체인의 연속성·무결성을 **순수 함수**로
//! 판정한다(I/O·스토리지 의존 없음 → 단위 테스트 용이). 입력은 manifest 메타의
//! 얇은 투영([`ChainNode`])이고, 출력은 구체적인 진단([`ChainReport`])이다.
//!
//! ## 연속성 규칙(t9 PITR이 소비하는 계약)
//! base(풀백업)를 0번으로 두고, 같은 `base_id`를 가리키는 증분들을 `oplog_range`
//! 순서로 정렬했을 때:
//! 1. **base 존재·적격:** base manifest가 있어야 하고, base는 풀백업이며 selective가
//!    아니어야 한다(selective base는 증분 base 부적격, FR-1). incomplete면 끊김.
//! 2. **증분 적격:** 각 증분은 incomplete가 아니어야 하고 `oplog_range`가 있어야 한다
//!    (oplog 구간을 모르면 연속성 판정 불가).
//! 3. **인접 연속성:** 정렬된 인접 슬라이스에서 `다음.start_ts == 이전.end_ts`여야
//!    한다. 빈 슬라이스(start==end, oplog_count==Some(0))도 동일 규칙을 따르며,
//!    빈 슬라이스는 연속성을 깨지 않는다(이전 end에서 시작·종료 → 다음이 그 지점에서
//!    이어짐).
//! 4. **base 접점:** 첫 증분의 `start_ts`는 base의 `oplog_range.end_ts`와 같아야 한다
//!    (base가 oplog_range를 가질 때 — replica set + --oplog). base에 oplog_range가
//!    없으면(standalone 등) base 접점 검사는 생략하되 경고로 남긴다.
//!
//! 위 규칙 중 하나라도 어긋나면 끊어진 지점을 [`ChainBreak`]로 구체적으로 보고한다.

use crate::manifest::schema::{BackupManifest, BackupStatus, BackupType, OplogTimestamp};

/// 체인 판정에 필요한 manifest의 얇은 투영(순수 함수 입력).
///
/// 실제 [`BackupManifest`]에서 [`ChainNode::from_manifest`]로 만든다. I/O와
/// 분리해 단위 테스트에서 손으로 노드를 구성할 수 있게 한다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainNode {
    /// 백업 ID.
    pub id: String,
    /// 백업 유형(full/incremental).
    pub backup_type: BackupType,
    /// 증분의 base 풀백업 ID(full이면 None).
    pub base_id: Option<String>,
    /// oplog 구간(연속성 판정의 핵심; 없으면 판정 불가 신호).
    pub oplog_range: Option<(OplogTimestamp, OplogTimestamp)>,
    /// 빈 슬라이스 여부(`oplog_count == Some(0)`) — 진단 메시지용.
    pub is_empty_slice: bool,
    /// 선택적 백업 여부(true면 증분 base 부적격, FR-1).
    pub selective: bool,
    /// 완료 상태(incomplete면 체인에 쓸 수 없음).
    pub status: BackupStatus,
}

impl ChainNode {
    /// 실제 manifest에서 체인 판정용 노드를 만든다.
    pub fn from_manifest(m: &BackupManifest) -> Self {
        Self {
            id: m.id.clone(),
            backup_type: m.backup_type,
            base_id: m.base_id.clone(),
            oplog_range: m.oplog_range.map(|r| (r.start_ts, r.end_ts)),
            is_empty_slice: m.oplog_count == Some(0),
            selective: m.selective,
            status: m.status,
        }
    }

    /// 시작 ts(있으면).
    fn start_ts(&self) -> Option<OplogTimestamp> {
        self.oplog_range.map(|(s, _)| s)
    }

    /// 종료 ts(있으면).
    fn end_ts(&self) -> Option<OplogTimestamp> {
        self.oplog_range.map(|(_, e)| e)
    }

    fn is_incomplete(&self) -> bool {
        matches!(self.status, BackupStatus::Incomplete)
    }
}

/// 체인이 끊어진 구체적 지점(진단·보고).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainBreak {
    /// base 풀백업 manifest를 찾지 못함(누락 base).
    MissingBase {
        /// 증분이 가리킨 base ID.
        base_id: String,
    },
    /// base가 풀백업이 아니거나 selective라 증분 base로 부적격(FR-1).
    IneligibleBase {
        /// base ID.
        base_id: String,
        /// 부적격 사유.
        reason: String,
    },
    /// base 또는 증분이 incomplete라 체인에 사용할 수 없음.
    IncompleteMember {
        /// 해당 백업 ID.
        id: String,
    },
    /// 증분에 oplog_range가 없어 연속성 판정 불가.
    MissingOplogRange {
        /// 해당 증분 ID.
        id: String,
    },
    /// 인접 슬라이스의 ts가 불연속(다음.start != 이전.end) — gap 또는 중첩.
    Discontinuity {
        /// 이전(앞선) 슬라이스 ID.
        prev_id: String,
        /// 다음(뒤따르는) 슬라이스 ID.
        next_id: String,
        /// 이전 슬라이스의 end_ts.
        prev_end: OplogTimestamp,
        /// 다음 슬라이스의 start_ts.
        next_start: OplogTimestamp,
    },
    /// 첫 증분의 start_ts가 base의 end_ts와 불연속(base 접점 끊김).
    BaseJoinGap {
        /// base ID.
        base_id: String,
        /// 첫 증분 ID.
        first_incr_id: String,
        /// base의 end_ts.
        base_end: OplogTimestamp,
        /// 첫 증분의 start_ts.
        incr_start: OplogTimestamp,
    },
}

impl std::fmt::Display for ChainBreak {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChainBreak::MissingBase { base_id } => {
                write!(f, "base 풀백업 누락: '{base_id}'를 찾을 수 없습니다")
            }
            ChainBreak::IneligibleBase { base_id, reason } => {
                write!(f, "base '{base_id}' 부적격: {reason}")
            }
            ChainBreak::IncompleteMember { id } => {
                write!(f, "'{id}'가 incomplete 상태라 체인에 사용할 수 없습니다")
            }
            ChainBreak::MissingOplogRange { id } => {
                write!(f, "증분 '{id}'에 oplog_range가 없어 연속성 판정 불가")
            }
            ChainBreak::Discontinuity {
                prev_id,
                next_id,
                prev_end,
                next_start,
            } => write!(
                f,
                "불연속: '{prev_id}'(end t:{},i:{}) → '{next_id}'(start t:{},i:{}) — \
                 인접 슬라이스의 ts가 이어지지 않습니다(gap 또는 중첩)",
                prev_end.t, prev_end.i, next_start.t, next_start.i
            ),
            ChainBreak::BaseJoinGap {
                base_id,
                first_incr_id,
                base_end,
                incr_start,
            } => write!(
                f,
                "base 접점 끊김: base '{base_id}'(end t:{},i:{}) → 첫 증분 '{first_incr_id}'\
                 (start t:{},i:{})",
                base_end.t, base_end.i, incr_start.t, incr_start.i
            ),
        }
    }
}

/// 비차단성 경고(체인은 유효하나 운영자에게 알릴 사항).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainWarning {
    /// base에 oplog_range가 없어 base 접점 검사를 생략함(standalone 등).
    BaseHasNoOplogRange {
        /// base ID.
        base_id: String,
    },
}

impl std::fmt::Display for ChainWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChainWarning::BaseHasNoOplogRange { base_id } => write!(
                f,
                "base '{base_id}'에 oplog_range가 없어 base↔첫 증분 접점 검사를 생략했습니다"
            ),
        }
    }
}

/// 체인 검증 결과.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainReport {
    /// base 풀백업 ID.
    pub base_id: String,
    /// 체인에 포함된 증분 슬라이스(정렬된) ID 순서.
    pub incremental_ids: Vec<String>,
    /// 끊어진 지점들(비어 있으면 연속).
    pub breaks: Vec<ChainBreak>,
    /// 비차단 경고.
    pub warnings: Vec<ChainWarning>,
}

impl ChainReport {
    /// 끊어진 지점이 하나도 없으면 연속(PITR 가능).
    pub fn is_continuous(&self) -> bool {
        self.breaks.is_empty()
    }
}

/// `target_id`가 속한 체인을 역추적해 연속성을 판정한다(순수 함수).
///
/// `nodes`는 동일 destination에서 수집한 모든 백업의 [`ChainNode`] 목록이다.
/// `target_id`가 풀백업이면 그것을 base로, 증분이면 그 `base_id`를 base로 삼는다.
///
/// ## 절차
/// 1. target에서 base를 식별한다(target이 full이면 자신, incr이면 base_id 추적).
/// 2. base 적격성을 검사한다(존재·full·non-selective·complete).
/// 3. 같은 base를 가리키는 모든 증분을 모아 `(start_ts, id)` 사전순으로 정렬한다.
/// 4. base 접점·인접 연속성을 검사해 [`ChainBreak`]를 수집한다.
///
/// 어떤 단계에서 base 자체를 식별/적격화하지 못하면 그 사실만 담은 보고를 돌려준다
/// (가능한 한 끝까지 진단을 모은다).
pub fn verify_chain(nodes: &[ChainNode], target_id: &str) -> ChainReport {
    // 1) base 식별 — target이 풀이면 자신, 증분이면 base_id.
    let base_id = match find_node(nodes, target_id) {
        Some(t) => match t.backup_type {
            BackupType::Full => t.id.clone(),
            BackupType::Incremental => match &t.base_id {
                Some(b) => b.clone(),
                None => {
                    // base_id 없는 증분 — 끊긴 체인(고아 증분).
                    return ChainReport {
                        base_id: String::new(),
                        incremental_ids: Vec::new(),
                        breaks: vec![ChainBreak::MissingBase {
                            base_id: format!("(증분 '{target_id}'에 base_id 없음)"),
                        }],
                        warnings: Vec::new(),
                    };
                }
            },
        },
        None => {
            // target manifest 자체가 없음 — 누락 base로 보고(가장 근접한 진단).
            return ChainReport {
                base_id: target_id.to_string(),
                incremental_ids: Vec::new(),
                breaks: vec![ChainBreak::MissingBase {
                    base_id: target_id.to_string(),
                }],
                warnings: Vec::new(),
            };
        }
    };

    let mut breaks = Vec::new();
    let mut warnings = Vec::new();

    // 2) base 적격성.
    let base = find_node(nodes, &base_id);
    let base = match base {
        None => {
            return ChainReport {
                base_id: base_id.clone(),
                incremental_ids: Vec::new(),
                breaks: vec![ChainBreak::MissingBase { base_id }],
                warnings,
            };
        }
        Some(b) => b,
    };
    if !matches!(base.backup_type, BackupType::Full) {
        breaks.push(ChainBreak::IneligibleBase {
            base_id: base_id.clone(),
            reason: "base가 풀백업이 아닙니다".to_string(),
        });
    }
    if base.selective {
        breaks.push(ChainBreak::IneligibleBase {
            base_id: base_id.clone(),
            reason: "selective 백업은 증분 base가 될 수 없습니다(FR-1)".to_string(),
        });
    }
    if base.is_incomplete() {
        breaks.push(ChainBreak::IncompleteMember {
            id: base_id.clone(),
        });
    }

    // 3) 같은 base를 가리키는 증분 수집·정렬.
    let mut incrementals: Vec<&ChainNode> = nodes
        .iter()
        .filter(|n| {
            matches!(n.backup_type, BackupType::Incremental)
                && n.base_id.as_deref() == Some(base_id.as_str())
        })
        .collect();
    // (start_ts, id) 사전순. oplog_range 없는 증분은 뒤로 보내되 별도로 끊김 보고한다.
    incrementals.sort_by(|a, b| match (a.start_ts(), b.start_ts()) {
        (Some(sa), Some(sb)) => sa.cmp(&sb).then(a.id.cmp(&b.id)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.id.cmp(&b.id),
    });

    let incremental_ids: Vec<String> = incrementals.iter().map(|n| n.id.clone()).collect();

    // 4) 각 증분 적격성(incomplete·oplog_range 부재).
    for incr in &incrementals {
        if incr.is_incomplete() {
            breaks.push(ChainBreak::IncompleteMember {
                id: incr.id.clone(),
            });
        }
        if incr.oplog_range.is_none() {
            breaks.push(ChainBreak::MissingOplogRange {
                id: incr.id.clone(),
            });
        }
    }

    // 5) base 접점 — 첫 증분 start == base end.
    if let Some(first) = incrementals.first() {
        match (base.end_ts(), first.start_ts()) {
            (Some(base_end), Some(incr_start)) => {
                if base_end != incr_start {
                    breaks.push(ChainBreak::BaseJoinGap {
                        base_id: base_id.clone(),
                        first_incr_id: first.id.clone(),
                        base_end,
                        incr_start,
                    });
                }
            }
            (None, _) => {
                // base에 oplog_range가 없으면(standalone 등) 접점 검사 생략(경고).
                warnings.push(ChainWarning::BaseHasNoOplogRange {
                    base_id: base_id.clone(),
                });
            }
            (Some(_), None) => {
                // 첫 증분에 start 없음 — 이미 MissingOplogRange로 보고됨.
            }
        }
    }

    // 6) 인접 연속성 — 다음.start == 이전.end.
    for pair in incrementals.windows(2) {
        let prev = pair[0];
        let next = pair[1];
        if let (Some(prev_end), Some(next_start)) = (prev.end_ts(), next.start_ts()) {
            if prev_end != next_start {
                breaks.push(ChainBreak::Discontinuity {
                    prev_id: prev.id.clone(),
                    next_id: next.id.clone(),
                    prev_end,
                    next_start,
                });
            }
        }
    }

    ChainReport {
        base_id,
        incremental_ids,
        breaks,
        warnings,
    }
}

/// id로 노드를 찾는다.
fn find_node<'a>(nodes: &'a [ChainNode], id: &str) -> Option<&'a ChainNode> {
    nodes.iter().find(|n| n.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(t: u32, i: u32) -> OplogTimestamp {
        OplogTimestamp::new(t, i)
    }

    /// 풀백업 base 노드.
    fn full(id: &str, end: OplogTimestamp) -> ChainNode {
        ChainNode {
            id: id.to_string(),
            backup_type: BackupType::Full,
            base_id: None,
            // base의 oplog_range는 start=end=캡처 직후 ts로 단순화(접점 = end).
            oplog_range: Some((end, end)),
            is_empty_slice: false,
            selective: false,
            status: BackupStatus::Complete,
        }
    }

    /// 증분 노드.
    fn incr(id: &str, base: &str, start: OplogTimestamp, end: OplogTimestamp) -> ChainNode {
        ChainNode {
            id: id.to_string(),
            backup_type: BackupType::Incremental,
            base_id: Some(base.to_string()),
            oplog_range: Some((start, end)),
            is_empty_slice: start == end,
            selective: false,
            status: BackupStatus::Complete,
        }
    }

    /// 정상 체인: base → incr1 → incr2 가 연속이면 끊김 없음.
    #[test]
    fn continuous_chain_has_no_breaks() {
        let base = full("base", ts(100, 1));
        let i1 = incr("i1", "base", ts(100, 1), ts(150, 2));
        let i2 = incr("i2", "base", ts(150, 2), ts(200, 3));
        let nodes = vec![base, i1, i2];

        let report = verify_chain(&nodes, "i2");
        assert!(report.is_continuous(), "끊김: {:?}", report.breaks);
        assert_eq!(report.base_id, "base");
        assert_eq!(report.incremental_ids, vec!["i1", "i2"]);
    }

    /// 풀백업 자신을 target으로 줘도 base로 식별하고 그 위 증분들을 검사한다.
    #[test]
    fn target_full_uses_itself_as_base() {
        let base = full("base", ts(100, 1));
        let i1 = incr("i1", "base", ts(100, 1), ts(150, 2));
        let nodes = vec![base, i1];
        let report = verify_chain(&nodes, "base");
        assert!(report.is_continuous());
        assert_eq!(report.incremental_ids, vec!["i1"]);
    }

    /// 불연속(gap): i2.start가 i1.end와 다르면 Discontinuity로 보고한다.
    #[test]
    fn ts_discontinuity_is_reported() {
        let base = full("base", ts(100, 1));
        let i1 = incr("i1", "base", ts(100, 1), ts(150, 2));
        // i2가 150,2가 아니라 160,0에서 시작 → gap.
        let i2 = incr("i2", "base", ts(160, 0), ts(200, 3));
        let nodes = vec![base, i1, i2];

        let report = verify_chain(&nodes, "i2");
        assert!(!report.is_continuous());
        let has_disc = report.breaks.iter().any(|b| {
            matches!(
                b,
                ChainBreak::Discontinuity { prev_id, next_id, .. }
                    if prev_id == "i1" && next_id == "i2"
            )
        });
        assert!(has_disc, "Discontinuity 누락: {:?}", report.breaks);
    }

    /// 빈 슬라이스(start==end)는 연속성을 깨지 않는다.
    /// base(end=100,1) → empty(100,1→100,1) → i2(100,1→200,3) 가 연속.
    #[test]
    fn empty_slice_preserves_continuity() {
        let base = full("base", ts(100, 1));
        let empty = incr("e", "base", ts(100, 1), ts(100, 1)); // 빈 슬라이스.
        let i2 = incr("i2", "base", ts(100, 1), ts(200, 3));
        let nodes = vec![base, empty, i2];

        let report = verify_chain(&nodes, "i2");
        assert!(
            report.is_continuous(),
            "빈 슬라이스가 끊김 유발: {:?}",
            report.breaks
        );
        // 빈 슬라이스가 정렬상 먼저(같은 start면 id 사전순 e < i2).
        assert_eq!(report.incremental_ids, vec!["e", "i2"]);
    }

    /// 연속된 빈 슬라이스 두 개도 연속을 유지한다(같은 지점에 머묾).
    #[test]
    fn consecutive_empty_slices_ok() {
        let base = full("base", ts(100, 1));
        let e1 = incr("e1", "base", ts(100, 1), ts(100, 1));
        let e2 = incr("e2", "base", ts(100, 1), ts(100, 1));
        let nodes = vec![base, e1, e2];
        let report = verify_chain(&nodes, "e2");
        assert!(report.is_continuous(), "끊김: {:?}", report.breaks);
    }

    /// selective base는 부적격으로 보고한다(FR-1).
    #[test]
    fn selective_base_is_ineligible() {
        let mut base = full("base", ts(100, 1));
        base.selective = true;
        let i1 = incr("i1", "base", ts(100, 1), ts(150, 2));
        let nodes = vec![base, i1];

        let report = verify_chain(&nodes, "i1");
        assert!(!report.is_continuous());
        let has = report
            .breaks
            .iter()
            .any(|b| matches!(b, ChainBreak::IneligibleBase { reason, .. } if reason.contains("selective")));
        assert!(has, "selective 부적격 누락: {:?}", report.breaks);
    }

    /// 누락 base: 증분이 가리키는 base manifest가 없으면 MissingBase.
    #[test]
    fn missing_base_is_reported() {
        let i1 = incr("i1", "ghost-base", ts(100, 1), ts(150, 2));
        let nodes = vec![i1];
        let report = verify_chain(&nodes, "i1");
        assert!(!report.is_continuous());
        let has = report
            .breaks
            .iter()
            .any(|b| matches!(b, ChainBreak::MissingBase { base_id } if base_id == "ghost-base"));
        assert!(has, "MissingBase 누락: {:?}", report.breaks);
    }

    /// incomplete 증분은 IncompleteMember로 보고한다.
    #[test]
    fn incomplete_member_is_reported() {
        let base = full("base", ts(100, 1));
        let mut i1 = incr("i1", "base", ts(100, 1), ts(150, 2));
        i1.status = BackupStatus::Incomplete;
        let nodes = vec![base, i1];
        let report = verify_chain(&nodes, "i1");
        assert!(!report.is_continuous());
        let has = report
            .breaks
            .iter()
            .any(|b| matches!(b, ChainBreak::IncompleteMember { id } if id == "i1"));
        assert!(has, "IncompleteMember 누락: {:?}", report.breaks);
    }

    /// base 접점 끊김: 첫 증분 start가 base end와 다르면 BaseJoinGap.
    #[test]
    fn base_join_gap_is_reported() {
        let base = full("base", ts(100, 1));
        // 첫 증분이 base end(100,1)이 아니라 105,0에서 시작.
        let i1 = incr("i1", "base", ts(105, 0), ts(150, 2));
        let nodes = vec![base, i1];
        let report = verify_chain(&nodes, "i1");
        assert!(!report.is_continuous());
        let has = report
            .breaks
            .iter()
            .any(|b| matches!(b, ChainBreak::BaseJoinGap { .. }));
        assert!(has, "BaseJoinGap 누락: {:?}", report.breaks);
    }

    /// base에 oplog_range가 없으면(standalone 등) 접점 검사를 생략하고 경고만 남긴다.
    #[test]
    fn base_without_oplog_range_warns_not_breaks() {
        let mut base = full("base", ts(100, 1));
        base.oplog_range = None; // standalone base.
        let i1 = incr("i1", "base", ts(100, 1), ts(150, 2));
        let nodes = vec![base, i1];
        let report = verify_chain(&nodes, "i1");
        // 접점 끊김은 보고하지 않되 경고는 남는다.
        assert!(
            report.is_continuous(),
            "끊김으로 잘못 보고: {:?}",
            report.breaks
        );
        assert!(report
            .warnings
            .iter()
            .any(|w| matches!(w, ChainWarning::BaseHasNoOplogRange { .. })));
    }

    /// oplog_range 없는 증분은 MissingOplogRange로 보고한다.
    #[test]
    fn incr_without_oplog_range_is_reported() {
        let base = full("base", ts(100, 1));
        let mut i1 = incr("i1", "base", ts(100, 1), ts(150, 2));
        i1.oplog_range = None;
        let nodes = vec![base, i1];
        let report = verify_chain(&nodes, "i1");
        assert!(!report.is_continuous());
        let has = report
            .breaks
            .iter()
            .any(|b| matches!(b, ChainBreak::MissingOplogRange { id } if id == "i1"));
        assert!(has, "MissingOplogRange 누락: {:?}", report.breaks);
    }

    /// from_manifest: oplog_count==Some(0)이면 is_empty_slice=true.
    #[test]
    fn from_manifest_maps_empty_slice() {
        use crate::manifest::schema::{
            BackupManifest, BackupStatus, BackupType, OplogRange, Topology, FORMAT_VERSION,
        };
        let m = BackupManifest {
            format_version: FORMAT_VERSION,
            id: "i1".into(),
            created_at: "2026-06-12T00:00:00Z".into(),
            backup_type: BackupType::Incremental,
            base_id: Some("base".into()),
            topology: Topology::ReplicaSet,
            server_version: "7.0.35".into(),
            tool_versions: Default::default(),
            selective: false,
            original_size_bytes: 0,
            stored_size_bytes: 0,
            compression: None,
            encryption: None,
            checksum_sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                .into(),
            oplog_range: Some(OplogRange {
                start_ts: ts(100, 1),
                end_ts: ts(100, 1),
            }),
            oplog_count: Some(0),
            promoted_from_gap: false,
            status: BackupStatus::Complete,
        };
        let node = ChainNode::from_manifest(&m);
        assert!(node.is_empty_slice);
        assert_eq!(node.base_id.as_deref(), Some("base"));
    }
}
