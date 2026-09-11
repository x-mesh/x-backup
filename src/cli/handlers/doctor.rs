//! `doctor` — config 정적 점검(오프라인). 모든 프로파일의 source/destination/encryption과
//! 엔진별 설정을 **DB 연결 없이** 검사해 문제 상황을 경고한다(연결·서버 점검은 `status`가
//! 담당). 다중 DB 툴이라 엔진을 프로파일별로 명시하고, 교차 엔진 마이그레이션 불가 등 흔한
//! 함정을 안내한다.
//!
//! 종료 코드: 0(정상) / 4(경고) / 3(차단성 설정 오류).

use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::cli::args::DoctorArgs;
use crate::cli::table::{display_width, pad, paint, use_color, BOLD, DIM, GREEN, RED, YELLOW};
use crate::config::env::collect_overrides_from_process;
use crate::config::file::Config;
use crate::config::merged::MergeInput;
use crate::config::ResolvedConfig;
use crate::engine::mongo::status::CheckStatus;
use crate::engine::DbKind;
use crate::error::{Result, XBackupError};

/// 점검 항목 하나.
struct Item {
    status: CheckStatus,
    label: &'static str,
    message: String,
}

/// 한 프로파일의 점검 결과.
struct ProfileReport {
    name: String,
    db: Option<DbKind>,
    items: Vec<Item>,
}

fn rank(s: CheckStatus) -> u8 {
    match s {
        CheckStatus::Ok => 0,
        CheckStatus::Warn => 1,
        CheckStatus::Fail => 2,
    }
}

/// `doctor` 핸들러.
pub async fn handle(
    config_path: Option<PathBuf>,
    lang_flag: Option<crate::i18n::Lang>,
    args: DoctorArgs,
) -> Result<()> {
    let config_toml = match &config_path {
        Some(p) => std::fs::read_to_string(p)
            .map_err(|e| XBackupError::Config(format!("config 읽기 실패({}): {e}", p.display())))?,
        None => {
            return Err(XBackupError::Usage(
                "doctor에는 config가 필요합니다 — --config <PATH>로 지정하세요".into(),
            ))
        }
    };
    // from_toml_str로 통일 — v2 표면도 여기서 정규화된다(inline toml::from_str 금지).
    let cfg: Config = Config::from_toml_str(&config_toml)?;
    let lang = crate::i18n::activate_from_toml(lang_flag, Some(config_toml.as_str()));

    // 점검할 프로파일 — --profile 지정 시 그것만, 아니면 전체(이름순).
    let names: Vec<String> = match &args.profile {
        Some(p) => {
            if !cfg.profiles.contains_key(p) {
                return Err(XBackupError::Usage(format!(
                    "프로파일 '{p}'가 config에 없습니다"
                )));
            }
            vec![p.clone()]
        }
        None => {
            let mut v: Vec<String> = cfg.profiles.keys().cloned().collect();
            v.sort();
            v
        }
    };
    if names.is_empty() {
        return Err(XBackupError::Usage(
            "config에 프로파일이 없습니다([profiles.<name>])".into(),
        ));
    }

    let reports: Vec<ProfileReport> = names
        .iter()
        .map(|n| check_profile(&config_toml, &cfg, n, lang))
        .collect();
    let overall = reports
        .iter()
        .flat_map(|r| r.items.iter())
        .map(|i| i.status)
        .max_by_key(|s| rank(*s))
        .unwrap_or(CheckStatus::Ok);
    let engines: BTreeSet<&str> = reports
        .iter()
        .filter_map(|r| r.db.map(|d| d.label()))
        .collect();

    if args.json {
        render_json(&reports, overall);
    } else {
        // doctor는 config가 필수라 위에서 None을 이미 걸렀다 — 항상 참조 경로가 존재한다.
        let config_src = crate::cli::output::config_source_label(config_path.as_deref(), lang);
        render_human(&reports, overall, &engines, &config_src, lang);
    }

    match overall {
        CheckStatus::Fail => Err(XBackupError::PrecheckFailed(
            "doctor: 차단성 설정 문제가 있습니다(위 [FAIL] 항목 확인)".into(),
        )),
        CheckStatus::Warn => Err(XBackupError::Warning(
            "doctor: 경고가 있습니다(위 [WARN] 항목 확인)".into(),
        )),
        CheckStatus::Ok => Ok(()),
    }
}

/// 한 프로파일을 정적 점검한다(연결 없음).
fn check_profile(
    config_toml: &str,
    cfg: &Config,
    name: &str,
    lang: crate::i18n::Lang,
) -> ProfileReport {
    let mut items = Vec::new();
    let mut db = None;

    // 1) source — uri/uri_env 해석 시도(연결은 안 함). 엔진은 URI 스킴으로 판별.
    let overrides = collect_overrides_from_process();
    match ResolvedConfig::build(MergeInput {
        config_toml: Some(config_toml),
        profile_name: name,
        overrides: &overrides,
    }) {
        Ok(r) => match r.resolved_uri {
            Some(uri) => {
                let kind = DbKind::from_uri(uri.expose());
                db = Some(kind);
                items.push(Item {
                    status: CheckStatus::Ok,
                    label: "source",
                    message: format!("{} → {}", lang.sel("resolved", "해석됨"), kind.label()),
                });
            }
            None => items.push(Item {
                status: CheckStatus::Fail,
                label: "source",
                message: lang
                    .sel(
                        "source.uri/uri_env is missing",
                        "source.uri/uri_env가 없습니다",
                    )
                    .into(),
            }),
        },
        // uri_env 미설정·프로파일 오류 등 — 런타임에 막힌다(경고).
        Err(e) => items.push(Item {
            status: CheckStatus::Warn,
            label: "source",
            message: format!("{e}"),
        }),
    }

    let prof = &cfg.profiles[name];

    // endpoint 전용(복구/이관 대상) 프로파일은 destination/encryption이 없는 게 정상이다 —
    // backup 잡이 아니므로 해당 점검들을 건너뛰고 역할만 표기한다(오탐 FAIL/WARN 방지).
    // source(1) 점검은 위에서 이미 했다(대상 endpoint URI가 해석되는지 확인).
    if prof.is_endpoint_only() {
        items.push(Item {
            status: CheckStatus::Ok,
            label: "role",
            message: lang
                .sel(
                    "endpoint-only profile (restore/migrate target) — no backup storage",
                    "endpoint 전용 프로파일(복구/이관 대상) — 백업 저장소 없음",
                )
                .into(),
        });
        return ProfileReport {
            name: name.to_string(),
            db,
            items,
        };
    }

    // 2) destination — type(local/s3)과 필수 필드.
    let dests = prof.effective_destinations();
    if dests.is_empty() {
        items.push(Item {
            status: CheckStatus::Fail,
            label: "destination",
            message: lang
                .sel("no destination configured", "destination이 없습니다")
                .into(),
        });
    }
    for (i, d) in dests.iter().enumerate() {
        let tag = if dests.len() > 1 {
            format!("[{}] ", d.label(i))
        } else {
            String::new()
        };
        match d.r#type.as_deref() {
            Some("local") => match &d.path {
                Some(p) => items.push(Item {
                    status: CheckStatus::Ok,
                    label: "destination",
                    message: format!("{tag}local: {p}"),
                }),
                None => items.push(Item {
                    status: CheckStatus::Fail,
                    label: "destination",
                    message: format!(
                        "{tag}{}",
                        lang.sel("local but path is missing", "local인데 path가 없습니다")
                    ),
                }),
            },
            Some("s3") => match &d.s3 {
                Some(_) => items.push(Item {
                    status: CheckStatus::Ok,
                    label: "destination",
                    message: format!("{tag}s3"),
                }),
                None => items.push(Item {
                    status: CheckStatus::Fail,
                    label: "destination",
                    message: format!(
                        "{tag}{}",
                        lang.sel(
                            "s3 but [.s3] config is missing",
                            "s3인데 [.s3] 설정이 없습니다"
                        )
                    ),
                }),
            },
            Some(other) => items.push(Item {
                status: CheckStatus::Fail,
                label: "destination",
                message: format!(
                    "{tag}{}",
                    lang.sel(
                        &format!("unknown type '{other}' (local|s3)"),
                        &format!("알 수 없는 type '{other}'(local|s3)")
                    )
                ),
            }),
            None => items.push(Item {
                status: CheckStatus::Fail,
                label: "destination",
                message: format!(
                    "{tag}{}",
                    lang.sel(
                        "destination.type is missing (local|s3)",
                        "destination.type이 없습니다(local|s3)"
                    )
                ),
            }),
        }
    }

    // 3) encryption — 평문 경고 / age recipient 파일 존재 / aes 키 안내.
    let enc = &prof.features.encryption;
    if !enc.enabled {
        items.push(Item {
            status: CheckStatus::Warn,
            label: "encryption",
            message: lang
                .sel(
                    "disabled — backups are stored in plaintext",
                    "비활성 — 평문으로 백업됩니다",
                )
                .into(),
        });
    } else if enc.algorithm == "age" {
        match &enc.recipient_file {
            Some(f) if std::path::Path::new(f).exists() => items.push(Item {
                status: CheckStatus::Ok,
                label: "encryption",
                message: format!("age, recipient {f}"),
            }),
            Some(f) => items.push(Item {
                status: CheckStatus::Fail,
                label: "encryption",
                message: format!(
                    "{}: {f}",
                    lang.sel(
                        "age but recipient_file does not exist",
                        "age인데 recipient_file이 없습니다"
                    )
                ),
            }),
            None => items.push(Item {
                status: CheckStatus::Fail,
                label: "encryption",
                message: lang
                    .sel(
                        "age but recipient_file is not set",
                        "age인데 recipient_file 미지정",
                    )
                    .into(),
            }),
        }
    } else {
        items.push(Item {
            status: CheckStatus::Ok,
            label: "encryption",
            message: format!(
                "{} {}",
                enc.algorithm,
                lang.sel("(key from runtime env)", "(키는 런타임 env)")
            ),
        });
    }

    // 4) 엔진별 안내.
    match db {
        Some(DbKind::Postgres) => {
            if prof.features.incremental.pg_logical {
                items.push(Item {
                    status: CheckStatus::Ok,
                    label: "incremental",
                    message: lang
                        .sel(
                            "pg_logical=true (server requires wal_level=logical)",
                            "pg_logical=true(서버 wal_level=logical 필요)",
                        )
                        .into(),
                });
            }
        }
        Some(DbKind::Mongo) => {
            let eng = &prof.mode.engine;
            if eng == "mongodump" {
                items.push(Item {
                    status: CheckStatus::Ok,
                    label: "engine",
                    message: lang
                        .sel(
                            "mongodump (external tool required — native recommended)",
                            "mongodump(외부 도구 필요 — native 권장)",
                        )
                        .into(),
                });
            }
        }
        Some(DbKind::Mysql) if prof.features.incremental.mysql_binlog => {
            items.push(Item {
                status: CheckStatus::Ok,
                label: "incremental",
                message: lang
                    .sel(
                        "mysql_binlog=true (server requires log_bin=ON, binlog_format=ROW, binlog_row_image=FULL)",
                        "mysql_binlog=true(서버 log_bin=ON·binlog_format=ROW·binlog_row_image=FULL 필요)",
                    )
                    .into(),
            });
        }
        Some(DbKind::Mysql) => {}
        None => {}
    }

    ProfileReport {
        name: name.to_string(),
        db,
        items,
    }
}

fn signal(status: CheckStatus, color: bool) -> String {
    let (t, c) = match status {
        CheckStatus::Ok => ("[OK  ]", GREEN),
        CheckStatus::Warn => ("[WARN]", YELLOW),
        CheckStatus::Fail => ("[FAIL]", RED),
    };
    paint(t, &[BOLD, c], color)
}

fn render_human(
    reports: &[ProfileReport],
    overall: CheckStatus,
    engines: &BTreeSet<&str>,
    config_src: &str,
    lang: crate::i18n::Lang,
) {
    let color = use_color();
    println!(
        "{}",
        paint(
            &format!(
                "doctor — config check ({} {})",
                reports.len(),
                lang.sel("profiles", "프로파일")
            ),
            &[BOLD],
            color
        )
    );
    println!("{}", paint(&format!("config: {config_src}"), &[DIM], color));
    for r in reports {
        let db = r.db.map(|d| d.label()).unwrap_or("?");
        println!();
        println!(
            "{}",
            paint(&format!("[{}] · DB {db}", r.name), &[BOLD], color)
        );
        let label_w = r
            .items
            .iter()
            .map(|i| display_width(i.label))
            .max()
            .unwrap_or(0);
        for it in &r.items {
            println!(
                "  {} {}  {}",
                signal(it.status, color),
                pad(it.label, label_w),
                it.message
            );
        }
    }
    // 교차 엔진 안내(다중 DB 툴 함정).
    if engines.len() > 1 {
        println!();
        println!(
            "{}",
            paint(
                lang.sel(
                    "note: this config contains multiple DBs. Migration is only possible \
                     between the same engine (PG↔Mongo conversion is not supported).",
                    "참고: 한 config에 여러 DB가 있습니다. 마이그레이션은 같은 엔진끼리만 \
                     가능합니다(PG↔Mongo 변환 불가).",
                ),
                &[DIM],
                color
            )
        );
    }
    println!();
    let label = match overall {
        CheckStatus::Ok => lang.sel("ok (exit 0)", "정상 (exit 0)"),
        CheckStatus::Warn => lang.sel("with warnings (exit 4)", "경고 동반 (exit 4)"),
        CheckStatus::Fail => lang.sel("blocking problem (exit 3)", "차단성 문제 (exit 3)"),
    };
    println!("overall: {} {}", signal(overall, color), label);
}

fn render_json(reports: &[ProfileReport], overall: CheckStatus) {
    let profiles: Vec<serde_json::Value> = reports
        .iter()
        .map(|r| {
            serde_json::json!({
                "profile": r.name,
                "db": r.db.map(|d| d.label()),
                "items": r.items.iter().map(|i| serde_json::json!({
                    "label": i.label,
                    "status": format!("{:?}", i.status).to_lowercase(),
                    "message": i.message,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    println!(
        "{}",
        serde_json::json!({
            "overall": format!("{overall:?}").to_lowercase(),
            "profiles": profiles,
        })
    );
}
