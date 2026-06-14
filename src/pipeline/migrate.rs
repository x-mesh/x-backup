//! 직접 마이그레이션 파이프라인 — source MongoDB → target MongoDB로 **파일 없이** 복사.
//!
//! 백업/복구와 달리 저장 산출물(manifest·체크섬·암호화)을 만들지 않는다. 엔진에 따라
//! 두 경로가 있다(프로파일 `mode.engine`, 기본 `native`):
//! ```text
//! native    : NativeDumper(source) ─▶ xb-native-v1 스트림 ─▶ native_restore(target)  [외부 도구 불필요]
//! mongodump : mongodump --archive=- (source) ─▶ stdout │ stdin ◀─ mongorestore --archive=- (target)
//! ```
//! 어느 경로든 디스크를 경유하지 않고 한 번에 흐른다(상수 메모리).
//!
//! ## 일관성 주의
//! 직접 복사 경로에는 oplogReplay 단계가 없어(native·mongodump 모두 `--oplog` 미사용)
//! 컬렉션 간 단일 시점 일관성은 보장하지 않는다. 쓰기가 많은 운영 replica set을 정확한
//! 시점으로 옮기려면 파일 경유 경로(`backup` → `restore --target`, `--oplog`/PITR)를 쓴다.
//!
//! ## 시크릿·프로세스 안전
//! - native: 드라이버가 직접 연결(자식 프로세스 없음). URI는 시크릿으로만 다룬다.
//! - mongodump: source/target URI를 0600 임시 config(`--uri` 파일)로 전달(`ps` 노출 없음),
//!   양쪽 자식 stderr는 독립 drain, 종료는 exit code로 판정, 실패 시 kill+wait로 정리.

use mongodb::Client;

use crate::config::secret::Secret;
use crate::engine::mongo::{
    conn, DumpProcess, DumpSpec, MongoMeta, RestoreProcess, RestoreSpec, UriConfigFile,
};
use crate::engine::native::backup::NativeDumper;
use crate::engine::native::restore::native_restore;
use crate::engine::postgres::{backup::PgDumper, meta as pg_meta, restore as pg_restore};
use crate::error::{Result, XBackupError};
use crate::pipeline::backup::Engine;

/// 마이그레이션 요청.
pub struct MigrateRequest {
    /// source(원본) MongoDB URI 시크릿.
    pub source_uri: Secret,
    /// target(대상) MongoDB URI 시크릿.
    pub target_uri: Secret,
    /// 전송 엔진(native | mongodump). 기본 native(프로파일 `mode.engine`).
    pub engine: Engine,
    /// mongodump 실행파일 경로(보통 `"mongodump"`; mongodump 엔진에서만 사용).
    pub mongodump_program: String,
    /// mongorestore 실행파일 경로(보통 `"mongorestore"`; mongodump 엔진에서만 사용).
    pub mongorestore_program: String,
    /// 선택적 마이그레이션 — 특정 DB만(`--db`).
    pub db: Option<String>,
    /// 선택적 마이그레이션 — 특정 컬렉션만(`--collection`).
    pub collection: Option<String>,
    /// target의 기존 컬렉션을 복원 전 drop할지(가드 통과 시에만 true).
    pub drop: bool,
    /// 계획만 출력하고 실제 전송은 하지 않음.
    pub dry_run: bool,
    /// MongoDB 접속 타임아웃(초). `None`이면 기본 5초. source·target 연결 모두에 적용.
    pub timeout_secs: Option<u64>,
}

/// 마이그레이션 결과 요약(CLI 출력용).
#[derive(Debug, Clone)]
pub struct MigrateOutcome {
    /// source 토폴로지 문자열.
    pub source_topology: String,
    /// target에 기존 데이터(충돌 네임스페이스)가 있었는지.
    pub target_had_data: bool,
}

/// 마이그레이션 계획(dry-run 출력용).
#[derive(Debug, Clone)]
pub struct MigratePlan {
    /// source 서버 버전.
    pub source_server_version: String,
    /// source 토폴로지 라벨.
    pub source_topology: String,
    /// target 서버 버전.
    pub target_server_version: String,
    /// 선택적 마이그레이션 네임스페이스(`db`/`db.collection`), 없으면 전체.
    pub ns: Option<String>,
    /// target의 충돌(기존) 네임스페이스 목록.
    pub conflicting_namespaces: Vec<String>,
    /// 서버 버전 비호환 경고(있으면).
    pub version_warning: Option<String>,
    /// source 네임스페이스별 추정 문서 수(마이그레이션 범위 내, 정렬).
    pub source_counts: Vec<(String, u64)>,
    /// target 네임스페이스별 추정 문서 수(마이그레이션 범위 내, 정렬).
    pub target_counts: Vec<(String, u64)>,
}

impl MigratePlan {
    /// source 총 문서 수(전송 예정 규모).
    pub fn source_total(&self) -> u64 {
        self.source_counts.iter().map(|(_, c)| c).sum()
    }
    /// target 총 문서 수(현재 대상에 있는 양).
    pub fn target_total(&self) -> u64 {
        self.target_counts.iter().map(|(_, c)| c).sum()
    }
}

/// 마이그레이션 범위(`db`/`db.collection` 필터)에 맞게 네임스페이스 카운트를 거른다.
///
/// - `None`(전체) → 그대로.
/// - `Some("db")` → `db.`로 시작하는 것만.
/// - `Some("db.coll")` → 정확히 일치하는 것만.
fn filter_counts(counts: Vec<(String, u64)>, ns: &Option<String>) -> Vec<(String, u64)> {
    match ns {
        None => counts,
        Some(filter) if filter.contains('.') => {
            counts.into_iter().filter(|(n, _)| n == filter).collect()
        }
        Some(db) => {
            let prefix = format!("{db}.");
            counts
                .into_iter()
                .filter(|(n, _)| n.starts_with(&prefix))
                .collect()
        }
    }
}

/// 선택적 마이그레이션 네임스페이스 문자열을 만든다(`db` 또는 `db.collection`).
fn ns_of(db: &Option<String>, collection: &Option<String>) -> Option<String> {
    match (db, collection) {
        (Some(d), Some(c)) => Some(format!("{d}.{c}")),
        (Some(d), None) => Some(d.clone()),
        _ => None,
    }
}

/// 마이그레이션 계획을 수립한다(source/target 연결·버전·충돌 점검). 무변경.
///
/// dry-run·실제 실행이 공통으로 사용한다. 프로세스는 스폰하지 않는다.
pub async fn plan_migrate(request: &MigrateRequest) -> Result<MigratePlan> {
    let source = MongoMeta::connect(&request.source_uri, request.timeout_secs).await?;
    let source_meta = source.server_meta().await?;

    let target = MongoMeta::connect(&request.target_uri, request.timeout_secs)
        .await
        .map_err(|e| XBackupError::PrecheckFailed(format!("target 연결 실패: {e}")))?;
    let target_meta = target.server_meta().await?;

    // 서버 버전 메이저 불일치는 경고(마이그레이션을 막지는 않음).
    let version_warning =
        version_compat_warning(&source_meta.server_version, &target_meta.server_version);

    // target 충돌 네임스페이스 — 선택적이면 그 ns만, 아니면 사용자 db 전체.
    let ns = ns_of(&request.db, &request.collection);
    let conflicting = target
        .user_namespaces(ns.as_deref())
        .await
        .unwrap_or_default();

    // 네임스페이스별 추정 문서 수(source/target) — dry-run 상세 비교용. 마이그레이션
    // 범위(ns 필터)로 거른다. 카운트 조회 실패는 치명적이지 않게 빈 목록으로 둔다.
    let source_counts = filter_counts(source.namespace_counts().await.unwrap_or_default(), &ns);
    let target_counts = filter_counts(target.namespace_counts().await.unwrap_or_default(), &ns);

    Ok(MigratePlan {
        source_topology: format!("{:?}", source_meta.topology()),
        source_server_version: source_meta.server_version,
        target_server_version: target_meta.server_version,
        ns,
        conflicting_namespaces: conflicting,
        version_warning,
        source_counts,
        target_counts,
    })
}

/// 마이그레이션을 실행한다(계획 → 가드 → dump|restore 직접 스트림).
///
/// `confirm`은 target에 기존 데이터가 있을 때 대화형 확인 콜백이다(force가 false이고
/// TTY일 때 호출). dry-run이면 [`MigratePlan`]만 만들고 전송하지 않는다.
pub async fn run_migrate<C>(
    request: &MigrateRequest,
    force: bool,
    is_tty: bool,
    confirm: C,
) -> Result<(MigratePlan, Option<MigrateOutcome>)>
where
    C: FnOnce(&MigratePlan) -> bool,
{
    let plan = plan_migrate(request).await?;

    if request.dry_run {
        return Ok((plan, None));
    }

    // 가드레일: 데이터가 있는 target은 **--drop 필수**다(순수 판정은 [`migrate_guard`]).
    let target_had_data = !plan.conflicting_namespaces.is_empty();
    match migrate_guard(target_had_data, request.drop, force) {
        GuardOutcome::Proceed => {}
        GuardOutcome::NeedDrop => {
            return Err(XBackupError::Usage(format!(
                "target에 기존 데이터가 있습니다({}개 네임스페이스). 마이그레이션은 교체를 \
                 의미하므로 --drop이 필요합니다(--drop 없는 복사는 어중간한 merge가 됩니다). \
                 빈 target으로 옮기거나 --drop --force를 쓰세요.",
                plan.conflicting_namespaces.len()
            )));
        }
        GuardOutcome::NeedConfirm => {
            // --drop은 파괴적이므로 --force가 없으면 TTY 대화형 확인을 받는다.
            if !(is_tty && confirm(&plan)) {
                return Err(XBackupError::Failure(
                    "target 기존 데이터를 --drop으로 교체하려면 --force 또는 대화형 확인이 \
                     필요합니다(프로덕션 가드레일)."
                        .into(),
                ));
            }
        }
    }

    // 엔진별 전송. native는 드라이버 직접(외부 도구 없음), mongodump는 자식 프로세스 파이프.
    match request.engine {
        Engine::Native => native_transfer(request, &plan).await?,
        Engine::Mongodump => mongodump_transfer(request, &plan).await?,
    }

    let source_topology = plan.source_topology.clone();
    Ok((
        plan,
        Some(MigrateOutcome {
            source_topology,
            target_had_data,
        }),
    ))
}

/// native 전송 — `NativeDumper`(source) 스트림을 `native_restore`(target)로 곧장 흘린다.
///
/// 백업 경로와 동일한 패턴: 덤프 task가 xb-native-v1 프레임을 [`DuplexStream`]에 쓰고,
/// 복구가 그 스트림을 읽어 create(옵션)+createIndexes+insert_many로 target에 적재한다.
/// EOF 후 덤프 핸들을 회수해 source 측 오류를 전파한다(외부 도구·디스크 없음).
///
/// [`DuplexStream`]: tokio::io::DuplexStream
async fn native_transfer(request: &MigrateRequest, plan: &MigratePlan) -> Result<()> {
    let dumper = NativeDumper::connect(&request.source_uri, request.timeout_secs).await?;
    let mut stream = dumper.dump_stream(request.db.clone(), request.collection.clone());
    let handle = stream.handle();

    // target 드라이버 클라이언트(복구 경로와 동일 정책).
    let options = conn::client_options(&request.target_uri, request.timeout_secs)
        .await
        .map_err(|e| XBackupError::PrecheckFailed(format!("target 연결 준비 실패: {e}")))?;
    let client = Client::with_options(options)
        .map_err(|e| XBackupError::Failure(format!("target 클라이언트 생성 실패: {e}")))?;

    let restore_res = native_restore(&mut stream, &client, request.drop, plan.ns.as_deref()).await;
    // 리더를 닫아 덤프 task가 EOF/BrokenPipe로 끝나게 한 뒤 결과를 회수한다.
    drop(stream);
    let dump_res = handle.finish().await;

    let inserted = restore_res?; // target 적재 오류 우선 전파.
    dump_res?; // 그 다음 source 덤프 오류.
    tracing::info!(inserted, "네이티브 마이그레이션 완료(파일 없음)");
    Ok(())
}

/// mongodump 전송 — `mongodump --archive=-` stdout을 `mongorestore --archive=-` stdin으로 복사.
///
/// 직접 복사 경로라 `--oplog`는 쓰지 않는다(모듈 문서 참조). source/target URI는 0600 임시
/// config로 전달하고, 실패 시 양쪽 자식을 kill+wait로 정리한다(좀비 방지).
async fn mongodump_transfer(request: &MigrateRequest, plan: &MigratePlan) -> Result<()> {
    // source/target URI를 각각 0600 임시 config로(argv 노출 금지). 핸들은 종료까지 유지.
    let source_cfg = UriConfigFile::create(&request.source_uri)?;
    let target_cfg = UriConfigFile::create(&request.target_uri)?;

    let dump_spec = DumpSpec {
        program: request.mongodump_program.clone(),
        uri_config_path: source_cfg.path().to_string(),
        oplog: false,
        db: request.db.clone(),
        collection: request.collection.clone(),
    };
    let mut dump = DumpProcess::spawn(&dump_spec)?;
    let mut dump_stdout = dump.take_stdout()?;

    // mongorestore(target) 스폰 — stdin으로 archive를 받는다.
    let restore_spec = RestoreSpec {
        program: request.mongorestore_program.clone(),
        uri_config_path: target_cfg.path().to_string(),
        ns_include: plan.ns.clone(),
        drop: request.drop,
    };
    let mut restore = RestoreProcess::spawn(&restore_spec)?;
    let mut restore_stdin = restore.take_stdin()?;

    // dump stdout → restore stdin 직접 복사(파일 없음). 끝나면 stdin을 닫아 EOF.
    let copy_result = tokio::io::copy(&mut dump_stdout, &mut restore_stdin).await;
    use tokio::io::AsyncWriteExt;
    let _ = restore_stdin.shutdown().await;
    drop(restore_stdin);

    if let Err(copy_err) = copy_result {
        // 파이프 실패: 양쪽 프로세스를 kill+wait로 정리(좀비 방지).
        dump.abort().await;
        restore.abort().await;
        return Err(XBackupError::Failure(format!(
            "마이그레이션 스트리밍 실패: {copy_err}"
        )));
    }

    // 양쪽 종료 코드 판정. dump 먼저 확인하되, 실패해도 restore도 정리한다.
    if let Err(dump_err) = dump.wait().await {
        restore.abort().await;
        return Err(dump_err);
    }
    restore.wait().await
}

/// PostgreSQL 마이그레이션 계획 — source/target 연결·버전·테이블 행 수(정확)·충돌. 무변경.
pub async fn plan_pg_migrate(request: &MigrateRequest) -> Result<MigratePlan> {
    let src = pg_meta::connect(&request.source_uri, request.timeout_secs).await?;
    let tgt = pg_meta::connect(&request.target_uri, request.timeout_secs)
        .await
        .map_err(|e| XBackupError::PrecheckFailed(format!("target 연결 실패: {e}")))?;
    let source_server_version = pg_server_version(src.client()).await;
    let target_server_version = pg_server_version(tgt.client()).await;
    let version_warning = version_compat_warning(&source_server_version, &target_server_version);

    let ns = ns_of(&request.db, &request.collection);
    let source_counts = filter_counts(
        pg_meta::table_counts_exact(src.client())
            .await
            .unwrap_or_default(),
        &ns,
    );
    let target_counts = filter_counts(
        pg_meta::table_counts_exact(tgt.client())
            .await
            .unwrap_or_default(),
        &ns,
    );
    // 충돌 = target에 이미 존재하는 테이블(있으면 --drop 필수). 빈 테이블도 "존재"로 본다.
    let conflicting_namespaces = target_counts.iter().map(|(n, _)| n.clone()).collect();

    Ok(MigratePlan {
        source_topology: "PostgreSQL".to_string(),
        source_server_version,
        target_server_version,
        ns,
        conflicting_namespaces,
        version_warning,
        source_counts,
        target_counts,
    })
}

/// PostgreSQL 마이그레이션 실행 — PgDumper(source) 스트림을 restore_into(target)로 곧장 흘린다.
///
/// 외부 도구 없이 드라이버 COPY로 source→target 직접 복사(파일·디스크 경유 없음). 가드는
/// Mongo 경로와 동일(데이터 있는 target은 --drop 필수, --force/대화형 확인).
pub async fn run_pg_migrate<C>(
    request: &MigrateRequest,
    force: bool,
    is_tty: bool,
    confirm: C,
) -> Result<(MigratePlan, Option<MigrateOutcome>)>
where
    C: FnOnce(&MigratePlan) -> bool,
{
    let plan = plan_pg_migrate(request).await?;
    if request.dry_run {
        return Ok((plan, None));
    }

    let target_had_data = !plan.conflicting_namespaces.is_empty();
    match migrate_guard(target_had_data, request.drop, force) {
        GuardOutcome::Proceed => {}
        GuardOutcome::NeedDrop => {
            return Err(XBackupError::Usage(format!(
                "target에 기존 테이블이 있습니다({}개). 마이그레이션은 교체를 의미하므로 \
                 --drop이 필요합니다(--drop 없는 복사는 어중간한 merge가 됩니다). \
                 빈 target으로 옮기거나 --drop --force를 쓰세요.",
                plan.conflicting_namespaces.len()
            )));
        }
        GuardOutcome::NeedConfirm => {
            if !(is_tty && confirm(&plan)) {
                return Err(XBackupError::Failure(
                    "target 기존 테이블을 --drop으로 교체하려면 --force 또는 대화형 확인이 \
                     필요합니다(프로덕션 가드레일)."
                        .into(),
                ));
            }
        }
    }

    // 전송: PgDumper(source) → restore_into(target). EOF 후 양측 결과 회수.
    let dumper = PgDumper::connect(&request.source_uri, request.timeout_secs).await?;
    let mut stream = dumper.dump_stream(request.db.clone(), request.collection.clone());
    let handle = stream.handle();
    let tgt = pg_meta::connect(&request.target_uri, request.timeout_secs)
        .await
        .map_err(|e| XBackupError::PrecheckFailed(format!("target 연결 실패: {e}")))?;
    let restore_res = pg_restore::restore_into(&mut stream, tgt.client(), request.drop).await;
    drop(stream);
    let dump_res = handle.finish().await;
    let inserted = restore_res?;
    dump_res?;
    tracing::info!(inserted, "PG 마이그레이션 완료(파일 없음)");

    Ok((
        plan,
        Some(MigrateOutcome {
            source_topology: "PostgreSQL".to_string(),
            target_had_data,
        }),
    ))
}

/// PG 서버 버전 문자열(없으면 "postgresql").
async fn pg_server_version(client: &tokio_postgres::Client) -> String {
    client
        .query_one("SHOW server_version", &[])
        .await
        .ok()
        .map(|r| r.get::<_, String>(0))
        .unwrap_or_else(|| "postgresql".to_string())
}

/// 데이터 있는 target에 대한 마이그레이션 가드 판정(순수 — 테스트 용이).
#[derive(Debug, PartialEq, Eq)]
enum GuardOutcome {
    /// 진행 가능(빈 target, 또는 --drop + --force).
    Proceed,
    /// 데이터 있는 target인데 --drop이 없음 → 거부(merge 방지).
    NeedDrop,
    /// --drop은 있으나 --force가 없음 → TTY 대화형 확인 필요.
    NeedConfirm,
}

/// migrate 가드 규칙: 빈 target은 무조건 진행. 데이터 있으면 --drop 필수이고,
/// --force가 없으면 대화형 확인이 필요하다.
fn migrate_guard(target_had_data: bool, drop: bool, force: bool) -> GuardOutcome {
    if !target_had_data {
        return GuardOutcome::Proceed;
    }
    if !drop {
        return GuardOutcome::NeedDrop;
    }
    if !force {
        return GuardOutcome::NeedConfirm;
    }
    GuardOutcome::Proceed
}

/// 서버 버전 메이저가 다르면 경고 문자열, 같으면 None.
fn version_compat_warning(source: &str, target: &str) -> Option<String> {
    let major = |v: &str| v.split('.').next().unwrap_or("").to_string();
    if major(source) != major(target) {
        Some(format!(
            "source({source})와 target({target})의 메이저 버전이 다릅니다 — \
             마이그레이션 후 호환성을 확인하세요"
        ))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ns_of_combines_db_and_collection() {
        assert_eq!(
            ns_of(&Some("app".into()), &Some("users".into())),
            Some("app.users".into())
        );
        assert_eq!(ns_of(&Some("app".into()), &None), Some("app".into()));
        assert_eq!(ns_of(&None, &None), None);
        // collection만 있고 db가 없으면 네임스페이스를 만들 수 없다(None).
        assert_eq!(ns_of(&None, &Some("users".into())), None);
    }

    #[test]
    fn version_warning_on_major_mismatch() {
        assert!(version_compat_warning("6.0.5", "7.0.1").is_some());
        assert!(version_compat_warning("7.0.35", "7.0.1").is_none());
        assert!(version_compat_warning("7.0.0", "7.2.0").is_none());
    }

    /// 가드 규칙: 빈 target은 항상 진행, 데이터 있으면 --drop 필수, --drop 있고 --force
    /// 없으면 확인 필요, --drop+--force면 진행.
    #[test]
    fn migrate_guard_matrix() {
        // 빈 target — drop/force 무관하게 진행.
        assert_eq!(migrate_guard(false, false, false), GuardOutcome::Proceed);
        assert_eq!(migrate_guard(false, true, true), GuardOutcome::Proceed);
        // 데이터 있는 target — --drop 없으면 거부(merge 방지).
        assert_eq!(migrate_guard(true, false, false), GuardOutcome::NeedDrop);
        assert_eq!(migrate_guard(true, false, true), GuardOutcome::NeedDrop);
        // --drop 있으나 --force 없음 → 대화형 확인.
        assert_eq!(migrate_guard(true, true, false), GuardOutcome::NeedConfirm);
        // --drop + --force → 진행.
        assert_eq!(migrate_guard(true, true, true), GuardOutcome::Proceed);
    }
}
