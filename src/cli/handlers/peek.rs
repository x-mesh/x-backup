//! `peek` 핸들러 — 데이터를 육안으로 확인한다(읽기 전용·무부작용).
//!
//! status가 "백업 가능 상태 점검"이라면, peek는 "실제 데이터가 들어 있나"를 눈으로
//! 본다. `--ns` 없으면 컬렉션별 추정 문서 수 + 각 최신 1건, `--ns db.coll`이면 그
//! 컬렉션의 최신 N건을 보여준다. 시크릿은 출력하지 않고 어떤 쓰기/변경도 하지 않는다.

use std::path::PathBuf;

use crate::cli::args::PeekArgs;
use crate::cli::output::{style, Tone};
use crate::cli::table::pad;
use crate::config::env::collect_overrides_from_process;
use crate::config::merged::MergeInput;
use crate::config::ResolvedConfig;
use crate::engine::mongo::MongoMeta;
use crate::error::{Result, XBackupError};

/// 사람용 출력에서 한 문서를 자를 최대 길이(긴 문서로 화면이 넘치지 않게).
const MAX_DOC_CHARS: usize = 200;

/// `peek` 핸들러 진입점.
pub async fn handle(
    config_path: Option<PathBuf>,
    lang_flag: Option<crate::i18n::Lang>,
    args: PeekArgs,
) -> Result<()> {
    let config_toml = match &config_path {
        Some(path) => Some(std::fs::read_to_string(path).map_err(|e| {
            XBackupError::Config(format!("config 파일 읽기 실패({}): {e}", path.display()))
        })?),
        None => None,
    };
    let lang = crate::i18n::resolve_from_toml(lang_flag, config_toml.as_deref());
    let overrides = collect_overrides_from_process();
    let resolved = ResolvedConfig::build(MergeInput {
        config_toml: config_toml.as_deref(),
        profile_name: &args.profile,
        overrides: &overrides,
    })?;
    let uri = resolved.resolved_uri.clone().ok_or_else(|| {
        XBackupError::Config(format!(
            "프로파일 '{}'에 source.uri/uri_env가 없습니다",
            resolved.profile_name
        ))
    })?;

    let timeout = resolved.profile.source.connect_timeout_secs;

    // 실행 컨텍스트(프로파일·DB) 표시 — 다중 DB 툴.
    let db = crate::engine::DbKind::from_uri(uri.expose());
    crate::cli::output::print_run_context(
        &resolved.profile_name,
        Some(db),
        crate::cli::output::context_mode(args.json),
    );

    // DB 종류 분기 — postgres/mysql URI면 드라이버 peek(테이블 행 수 + 최신 행), 그 외는 Mongo.
    if db == crate::engine::DbKind::Postgres {
        return peek_pg(&uri, timeout, &args, lang).await;
    }
    if db == crate::engine::DbKind::Mysql {
        return peek_mysql(&uri, timeout, &args, lang).await;
    }
    if db == crate::engine::DbKind::File {
        return Err(XBackupError::Usage(
            "peek는 파일 소스(file://)를 지원하지 않습니다 — 경로 점검은 status/doctor를 \
             사용하세요"
                .into(),
        ));
    }

    let mongo = MongoMeta::connect(&uri, timeout).await?;

    match &args.ns {
        Some(ns) => peek_namespace(&mongo, ns, args.limit, args.json, lang).await,
        None => peek_overview(&mongo, args.json, lang).await,
    }
}

/// PostgreSQL peek — `--ns` 없으면 테이블별 행 수 + 최신 1행, 있으면 그 테이블 최신 N행.
async fn peek_pg(
    uri: &crate::config::secret::Secret,
    timeout: Option<u64>,
    args: &PeekArgs,
    lang: crate::i18n::Lang,
) -> Result<()> {
    use crate::engine::postgres::meta;
    let pg = meta::connect(uri, timeout).await?;
    let client = pg.client();

    if let Some(ns) = &args.ns {
        let rows = meta::latest_rows(client, ns, args.limit.max(1)).await?;
        if args.json {
            // 각 행은 이미 JSON 텍스트 — 배열로 합쳐 그대로 출력.
            println!(
                "{}",
                serde_json::json!({ "ns": ns, "rows": rows.iter().map(|r| serde_json::from_str::<serde_json::Value>(r).unwrap_or(serde_json::Value::Null)).collect::<Vec<_>>() })
            );
            return Ok(());
        }
        println!(
            "{ns} — {} {}",
            style(lang.sel("latest", "최신"), Tone::Label),
            style(
                lang.sel(
                    &format!("{} rows", rows.len()),
                    &format!("{}행", rows.len())
                ),
                Tone::Value,
            )
        );
        if rows.is_empty() {
            println!(
                "  {}",
                style(lang.sel("(empty)", "(비어 있음)"), Tone::Muted)
            );
        }
        for r in &rows {
            println!("  {}", truncate(r, lang));
        }
        return Ok(());
    }

    let counts = meta::table_counts_exact(client).await?;
    if args.json {
        let mut items = Vec::new();
        for (ns, count) in &counts {
            let latest = meta::latest_rows(client, ns, 1).await?.into_iter().next();
            items.push(serde_json::json!({
                "ns": ns,
                "count": count,
                "latest": latest.and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()),
            }));
        }
        println!("{}", serde_json::json!({ "namespaces": items }));
        return Ok(());
    }
    if counts.is_empty() {
        println!(
            "{}",
            style(
                lang.sel(
                    "(no user data — empty database)",
                    "(사용자 데이터 없음 — 비어 있는 데이터베이스)"
                ),
                Tone::Muted,
            )
        );
        return Ok(());
    }
    for (ns, count) in &counts {
        let latest = meta::latest_rows(client, ns, 1).await?.into_iter().next();
        let preview = match latest {
            Some(s) => truncate(&s, lang),
            None => lang.sel("(empty)", "(비어 있음)").to_string(),
        };
        let ns_cell = pad(ns, 28);
        println!(
            "  {} {}={}  {} {}",
            style(&ns_cell, Tone::Value),
            style("count", Tone::Label),
            style(&count.to_string(), Tone::Value),
            style("latest:", Tone::Label),
            preview
        );
    }
    Ok(())
}

/// MySQL peek — `--ns`(db.table) 없으면 테이블별 행 수 + 최신 1행, 있으면 그 테이블 최신 N행.
async fn peek_mysql(
    uri: &crate::config::secret::Secret,
    timeout: Option<u64>,
    args: &PeekArgs,
    lang: crate::i18n::Lang,
) -> Result<()> {
    use crate::engine::mysql::meta;
    let mut my = meta::connect(uri, timeout).await?;

    if let Some(ns) = &args.ns {
        let rows = meta::latest_rows(my.conn_mut(), ns, args.limit.max(1)).await?;
        if args.json {
            println!(
                "{}",
                serde_json::json!({ "ns": ns, "rows": rows.iter().map(|r| serde_json::from_str::<serde_json::Value>(r).unwrap_or(serde_json::Value::Null)).collect::<Vec<_>>() })
            );
            return Ok(());
        }
        println!(
            "{ns} — {} {}",
            style(lang.sel("latest", "최신"), Tone::Label),
            style(
                lang.sel(
                    &format!("{} rows", rows.len()),
                    &format!("{}행", rows.len())
                ),
                Tone::Value,
            )
        );
        if rows.is_empty() {
            println!(
                "  {}",
                style(lang.sel("(empty)", "(비어 있음)"), Tone::Muted)
            );
        }
        for r in &rows {
            println!("  {}", truncate(r, lang));
        }
        return Ok(());
    }

    let counts = meta::table_counts_exact(my.conn_mut()).await?;
    if args.json {
        let mut items = Vec::new();
        for (ns, count) in &counts {
            let latest = meta::latest_rows(my.conn_mut(), ns, 1)
                .await?
                .into_iter()
                .next();
            items.push(serde_json::json!({
                "ns": ns,
                "count": count,
                "latest": latest.and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()),
            }));
        }
        println!("{}", serde_json::json!({ "namespaces": items }));
        return Ok(());
    }
    if counts.is_empty() {
        println!(
            "{}",
            style(
                lang.sel(
                    "(no user data — empty database)",
                    "(사용자 데이터 없음 — 비어 있는 데이터베이스)"
                ),
                Tone::Muted,
            )
        );
        return Ok(());
    }
    for (ns, count) in &counts {
        let latest = meta::latest_rows(my.conn_mut(), ns, 1)
            .await?
            .into_iter()
            .next();
        let preview = match latest {
            Some(s) => truncate(&s, lang),
            None => lang.sel("(empty)", "(비어 있음)").to_string(),
        };
        let ns_cell = pad(ns, 28);
        println!(
            "  {} {}={}  {} {}",
            style(&ns_cell, Tone::Value),
            style("count", Tone::Label),
            style(&count.to_string(), Tone::Value),
            style("latest:", Tone::Label),
            preview
        );
    }
    Ok(())
}

/// `--ns` 미지정 — 컬렉션별 문서 수 + 각 최신 1건.
async fn peek_overview(mongo: &MongoMeta, json: bool, lang: crate::i18n::Lang) -> Result<()> {
    let counts = mongo.namespace_counts().await?;

    if json {
        let mut items = Vec::new();
        for (ns, count) in &counts {
            let latest = latest_one(mongo, ns).await?;
            items.push(serde_json::json!({
                "ns": ns,
                "count": count,
                "latest": latest.map(doc_to_json),
            }));
        }
        println!("{}", serde_json::json!({ "namespaces": items }));
        return Ok(());
    }

    if counts.is_empty() {
        println!(
            "{}",
            style(
                lang.sel(
                    "(no user data — empty server)",
                    "(사용자 데이터 없음 — 비어 있는 서버)"
                ),
                Tone::Muted,
            )
        );
        return Ok(());
    }
    for (ns, count) in &counts {
        let latest = latest_one(mongo, ns).await?;
        let preview = match latest {
            Some(d) => truncate(&doc_to_line(&d), lang),
            None => lang.sel("(empty)", "(비어 있음)").to_string(),
        };
        let ns_cell = pad(ns, 28);
        println!(
            "  {} {}={}  {} {}",
            style(&ns_cell, Tone::Value),
            style("count", Tone::Label),
            style(&count.to_string(), Tone::Value),
            style("latest:", Tone::Label),
            preview
        );
    }
    Ok(())
}

/// `--ns db.coll` — 그 컬렉션의 최신 N건.
async fn peek_namespace(
    mongo: &MongoMeta,
    ns: &str,
    limit: i64,
    json: bool,
    lang: crate::i18n::Lang,
) -> Result<()> {
    let (db, coll) = ns.split_once('.').ok_or_else(|| {
        XBackupError::Usage(format!("--ns는 db.collection 형식이어야 합니다: '{ns}'"))
    })?;
    let docs = mongo.latest_documents(db, coll, limit.max(1)).await?;

    if json {
        let arr: Vec<serde_json::Value> = docs.iter().cloned().map(doc_to_json).collect();
        println!("{}", serde_json::json!({ "ns": ns, "documents": arr }));
        return Ok(());
    }

    println!(
        "{ns} — {} {}",
        style(lang.sel("latest", "최신"), Tone::Label),
        style(
            lang.sel(
                &format!("{} documents", docs.len()),
                &format!("{}건", docs.len())
            ),
            Tone::Value,
        )
    );
    if docs.is_empty() {
        println!(
            "  {}",
            style(lang.sel("(empty)", "(비어 있음)"), Tone::Muted)
        );
    }
    for d in &docs {
        println!("  {}", doc_to_line(d));
    }
    Ok(())
}

/// 한 네임스페이스의 최신 1건(없으면 None).
async fn latest_one(mongo: &MongoMeta, ns: &str) -> Result<Option<bson::Document>> {
    let (db, coll) = match ns.split_once('.') {
        Some(parts) => parts,
        None => return Ok(None),
    };
    Ok(mongo
        .latest_documents(db, coll, 1)
        .await?
        .into_iter()
        .next())
}

/// 문서를 한 줄 JSON 문자열로(사람용 — relaxed 형식).
fn doc_to_line(doc: &bson::Document) -> String {
    bson::Bson::Document(doc.clone())
        .into_relaxed_extjson()
        .to_string()
}

/// 문서를 serde_json 값으로(--json 출력용).
fn doc_to_json(doc: bson::Document) -> serde_json::Value {
    bson::Bson::Document(doc).into_relaxed_extjson()
}

/// 사람용 미리보기를 MAX_DOC_CHARS로 자른다(긴 문서 줄바꿈 방지).
fn truncate(s: &str, lang: crate::i18n::Lang) -> String {
    if s.chars().count() <= MAX_DOC_CHARS {
        s.to_string()
    } else {
        let cut: String = s.chars().take(MAX_DOC_CHARS).collect();
        format!("{cut}… {}", lang.sel("(truncated)", "(잘림)"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_short_and_cuts_long() {
        let lang = crate::i18n::Lang::Ko;
        assert_eq!(truncate("short", lang), "short");
        let long = "x".repeat(MAX_DOC_CHARS + 50);
        let out = truncate(&long, lang);
        assert!(out.ends_with("… (잘림)"));
        assert!(out.chars().count() < long.chars().count());
    }

    #[test]
    fn doc_to_line_renders_fields() {
        let d = bson::doc! { "_id": 1_i64, "name": "a" };
        let line = doc_to_line(&d);
        assert!(line.contains("\"name\""));
        assert!(line.contains("\"a\""));
    }
}
