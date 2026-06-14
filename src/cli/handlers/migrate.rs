//! `migrate` 핸들러 — source(프로파일) → target으로 파일 없이 직접 마이그레이션.
//!
//! config 로드 → source URI 해석 → [`run_migrate`] 직접 스트림. 백업과 달리 destination
//! (파일 저장소)·암호화·manifest를 쓰지 않는다. 잠금(FR-12)은 backup/restore/prune과
//! 같은 프로파일에서 직렬화한다(같은 source를 동시에 dump하지 않도록).

use std::io::IsTerminal;
use std::path::PathBuf;

use crate::cli::args::MigrateArgs;
use crate::cli::output::{OutputFlags, OutputMode};
use crate::cli::table::{self, Align, Table};
use crate::config::env::collect_overrides_from_process;
use crate::config::merged::MergeInput;
use crate::config::secret::Secret;
use crate::config::ResolvedConfig;
use crate::error::{Result, XBackupError};
use crate::pipeline::migrate::{run_migrate, MigratePlan, MigrateRequest};

/// `migrate` 핸들러 진입점.
pub async fn handle(config_path: Option<PathBuf>, args: MigrateArgs) -> Result<()> {
    // source를 동시에 dump하지 않도록 같은 프로파일 작업과 직렬화한다(FR-12).
    let _lock = crate::lock::acquire(&args.profile)?;

    // 1) config 로드 + 병합.
    let config_toml = match &config_path {
        Some(path) => Some(std::fs::read_to_string(path).map_err(|e| {
            XBackupError::Config(format!("config 파일 읽기 실패({}): {e}", path.display()))
        })?),
        None => None,
    };
    let overrides = collect_overrides_from_process();
    let resolved = ResolvedConfig::build(MergeInput {
        config_toml: config_toml.as_deref(),
        profile_name: &args.profile,
        overrides: &overrides,
    })?;

    // 2) source URI(프로파일 source) 확보.
    let source_uri = resolved.resolved_uri.clone().ok_or_else(|| {
        XBackupError::Config(format!(
            "프로파일 '{}'에 source.uri/uri_env가 없습니다(마이그레이션 원본)",
            resolved.profile_name
        ))
    })?;

    // 2.5) target URI 확보 — --target(URI 직접) 또는 --target-profile(다른 프로파일의
    //      source). 둘 다 없으면 사용법 오류, 둘 다 있으면 clap이 막는다.
    let target_uri = match (&args.target, &args.target_profile) {
        (Some(uri), _) => Secret::new(uri.clone()),
        (None, Some(tp)) => {
            let tcfg = ResolvedConfig::build(MergeInput {
                config_toml: config_toml.as_deref(),
                profile_name: tp,
                overrides: &overrides,
            })?;
            tcfg.resolved_uri.ok_or_else(|| {
                XBackupError::Config(format!(
                    "target 프로파일 '{tp}'에 source.uri/uri_env가 없습니다"
                ))
            })?
        }
        (None, None) => {
            return Err(XBackupError::Usage(
                "--target <uri> 또는 --target-profile <name> 중 하나가 필요합니다".into(),
            ))
        }
    };

    let mode = OutputMode::resolve_from_env(
        OutputFlags {
            json: args.json,
            quiet: args.quiet,
            progress: args.progress,
        },
        Some(resolved.profile.mode.output.as_str()),
    );

    // DB 종류 판별 — source/target이 같은 엔진이어야 한다(엔진 간 마이그레이션 미지원).
    use crate::engine::DbKind;
    let source_kind = DbKind::from_uri(source_uri.expose());
    let target_kind = DbKind::from_uri(target_uri.expose());
    if source_kind != target_kind {
        return Err(XBackupError::Usage(
            "source와 target의 DB 종류가 다릅니다 — 엔진 간 마이그레이션(예: Mongo↔PG)은 \
             지원하지 않습니다. 같은 종류끼리만 가능합니다."
                .into(),
        ));
    }

    // 전송 엔진은 source 프로파일의 mode.engine을 따른다(기본 native — 외부 도구 불필요).
    //   PG는 mode.engine과 무관하게 PG 엔진을 쓴다.
    let engine = crate::pipeline::backup::Engine::parse(&resolved.profile.mode.engine)?;
    let request = MigrateRequest {
        source_uri,
        target_uri,
        engine,
        mongodump_program: "mongodump".to_string(),
        mongorestore_program: "mongorestore".to_string(),
        db: args.db.clone(),
        collection: args.collection.clone(),
        drop: args.drop,
        dry_run: args.dry_run,
        timeout_secs: resolved.profile.source.connect_timeout_secs,
    };

    let is_tty = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let (plan, outcome) = match source_kind {
        DbKind::Postgres => {
            crate::pipeline::migrate::run_pg_migrate(&request, args.force, is_tty, prompt_confirm)
                .await?
        }
        DbKind::Mongo => run_migrate(&request, args.force, is_tty, prompt_confirm).await?,
    };

    // 3) 출력. dry-run은 계획을, 실제 실행은 완료 요약을 낸다(진행=stderr, 결과=stdout).
    if request.dry_run {
        print_plan(&plan, mode.emits_json());
    } else if let Some(out) = outcome {
        if mode.emits_json() {
            let summary = serde_json::json!({
                "migrated": true,
                "source_topology": out.source_topology,
                "target_had_data": out.target_had_data,
                "ns": plan.ns,
            });
            println!("{summary}");
        } else if mode.shows_human_summary() {
            println!("마이그레이션 완료(파일 없음, 직접 전송)");
            println!(
                "  source:   {} ({})",
                plan.source_server_version, out.source_topology
            );
            println!("  target:   {}", plan.target_server_version);
            match &plan.ns {
                Some(ns) => println!("  대상 ns:  {ns}"),
                None => println!("  대상 ns:  전체"),
            }
            if let Some(w) = &plan.version_warning {
                println!("  경고:     {w}");
            }
        }
    }

    Ok(())
}

/// 네임스페이스별 source/target 문서 수를 합쳐 (ns, source, target) 행으로 만든다(정렬).
fn merge_ns_rows(plan: &MigratePlan) -> Vec<(String, u64, u64)> {
    use std::collections::BTreeMap;
    let mut map: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    for (ns, c) in &plan.source_counts {
        map.entry(ns.clone()).or_default().0 = *c;
    }
    for (ns, c) in &plan.target_counts {
        map.entry(ns.clone()).or_default().1 = *c;
    }
    map.into_iter().map(|(ns, (s, t))| (ns, s, t)).collect()
}

/// dry-run 계획 출력(연결·버전·네임스페이스별 source vs target diff — 무변경). 시크릿 미출력.
fn print_plan(plan: &MigratePlan, json: bool) {
    let rows = merge_ns_rows(plan);
    if json {
        let ns_items: Vec<serde_json::Value> = rows
            .iter()
            .map(|(ns, s, t)| {
                serde_json::json!({ "ns": ns, "source": s, "target": t, "transfer": s })
            })
            .collect();
        let summary = serde_json::json!({
            "dry_run": true,
            "source_server_version": plan.source_server_version,
            "source_topology": plan.source_topology,
            "target_server_version": plan.target_server_version,
            "ns": plan.ns,
            "conflicting_namespaces": plan.conflicting_namespaces,
            "version_warning": plan.version_warning,
            "namespaces": ns_items,
            "source_total": plan.source_total(),
            "target_total": plan.target_total(),
            "transfer_total": plan.source_total(),
        });
        println!("{summary}");
        return;
    }
    println!("마이그레이션 계획(dry-run) — 실제 전송 없음");
    println!(
        "  source: {} ({})    target: {}",
        plan.source_server_version, plan.source_topology, plan.target_server_version
    );
    match &plan.ns {
        Some(ns) => println!("  대상 ns: {ns}(선택적)"),
        None => println!("  대상 ns: 전체"),
    }

    // 네임스페이스별 source vs target diff 표(표시 폭 정렬 + 동작별 색).
    if rows.is_empty() {
        println!("  (source에 사용자 데이터 없음)");
    } else {
        let color = table::use_color();
        let mut t = Table::new(
            &["네임스페이스", "source", "target", "동작"],
            &[Align::Left, Align::Right, Align::Right, Align::Left],
        );
        for (ns, s, tgt) in &rows {
            // migrate는 source→target 복사다. source에 없는 target 컬렉션은 **건드리지 않는다**
            // (--drop은 전송하는 컬렉션만 drop). 따라서 s==0은 "유지(미전송)"가 맞다.
            let (action, code): (String, &'static str) = if *s == 0 {
                ("유지(source 없음 — 미전송)".to_string(), table::DIM)
            } else if *tgt == 0 {
                (format!("+{s} 신규"), table::GREEN)
            } else {
                (
                    format!("교체(--drop) — target {tgt}건 덮어씀"),
                    table::YELLOW,
                )
            };
            let cells = vec![ns.clone(), s.to_string(), tgt.to_string(), action];
            t.row_styled(cells, vec![code]);
        }
        println!("{}", t.render("  ", color));
        println!("  {:─<width$}", "", width = t.total_width().min(72));
        println!(
            "  합계: source={}  target={}  전송 예정={}",
            plan.source_total(),
            plan.target_total(),
            plan.source_total()
        );
    }

    if plan.conflicting_namespaces.is_empty() {
        println!("  target 충돌: 없음(빈 대상) — 그대로 복사 가능");
    } else {
        println!(
            "  target 충돌: {}개({}) → --drop --force 필요(없으면 거부)",
            plan.conflicting_namespaces.len(),
            plan.conflicting_namespaces.join(", ")
        );
    }
    if let Some(w) = &plan.version_warning {
        println!("  경고: {w}");
    }
}

/// target에 기존 데이터가 있을 때 대화형 확인(TTY). --drop으로 교체하기 전 동의를 받는다.
///
/// 이 시점에 도달했다면 이미 --drop이 지정된 상태다(데이터 있는 target은 --drop 필수).
/// 즉 확인은 "merge"가 아니라 **해당 컬렉션 교체(drop 후 재생성)**에 대한 동의다.
fn prompt_confirm(plan: &MigratePlan) -> bool {
    use std::io::Write;
    eprintln!(
        "경고: target의 다음 {}개 네임스페이스를 drop하고 source로 교체합니다: {}",
        plan.conflicting_namespaces.len(),
        plan.conflicting_namespaces.join(", ")
    );
    eprint!("진행하시겠습니까? [y/N] ");
    let _ = std::io::stderr().flush();
    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        return false;
    }
    matches!(input.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}
