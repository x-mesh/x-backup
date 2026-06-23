//! Pipeline 계층 — 백업/복구 스트리밍 파이프 합성(PRD §7).
//!
//! - [`checksum`]: [`Sha256Reader`] — poll_read 내부 누산 sha256 tee(단일 패스).
//! - [`stage`]: [`StageStack`] — 단계 합성(t6 compress/encrypt 삽입 지점).
//! - [`backup`]: 풀 백업 오케스트레이션(dump → 단계 → tee → Storage → manifest).
//!
//! 백업 흐름: mongodump stdout → StageStack(identity|compress|encrypt) → sha256 tee
//! → put_stream → manifest(data→meta→사이드카).
//! 체크섬 기준점 = Storage에 쓰인 최종 바이트(키 없이 구조 검증 가능, PRD §8.5).
//!
//! 복구(get_stream → decrypt → decompress → mongorestore stdin)는 [`restore`](t5).
//! 복구 흐름: storage get_stream(data.bin) → reverse_stack(decrypt→decompress; 평문은
//! identity) → mongorestore --archive=- stdin. 사전 점검·가드레일·dry-run 포함.
//!
//! 무결성 검증(구조/심층/체인)은 [`verify`](R12) — manifest 사이드카·data.bin 체크섬
//! 재계산(키 불필요), --deep 디코드 소진, --chain base+증분 연속성 판정.

pub mod backup;
pub mod checksum;
pub mod incremental;
pub mod migrate;
pub mod mysql_pitr;
pub mod pg_pitr;
pub mod pitr;
pub mod prune;
pub mod restore;
pub mod stage;
pub mod verify;

pub use backup::{
    run_full_backup, run_full_backup_with_meta, BackupMeta, BackupOutcome, BackupRequest,
};
pub use checksum::{ChecksumHandle, Sha256Reader};
pub use incremental::{run_incremental_backup, IncrementalOutcome, IncrementalRequest};
pub use pg_pitr::{run_pg_pitr, PgPitrOutcome, PgPitrPlan, PgPitrRequest};
pub use pitr::{run_pitr, PitrOutcome, PitrPlan, PitrRequest};
pub use prune::{
    execute_prune, load_backups, plan_prune, PruneKind, PruneOutcome, PrunePlan, PruneTarget,
    RetentionPolicy,
};
pub use restore::{run_restore, RestoreOutcome, RestorePlan, RestoreRequest};
pub use stage::{reverse_stack_for, PipelineStage, StageStack};
pub use verify::{collect_manifest_ids, verify_backup, verify_chain_for, VerifyReport};
