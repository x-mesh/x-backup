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
pub async fn handle(config_path: Option<PathBuf>, args: DoctorArgs) -> Result<()> {
    let config_toml = match &config_path {
        Some(p) => std::fs::read_to_string(p)
            .map_err(|e| XBackupError::Config(format!("config 읽기 실패({}): {e}", p.display())))?,
        None => {
            return Err(XBackupError::Usage(
                "doctor에는 config가 필요합니다 — --config <PATH>로 지정하세요".into(),
            ))
        }
    };
    let cfg: Config = toml::from_str(&config_toml)
        .map_err(|e| XBackupError::Config(format!("config 파싱 실패: {e}")))?;

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
        .map(|n| check_profile(&config_toml, &cfg, n))
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
        render_human(&reports, overall, &engines);
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
fn check_profile(config_toml: &str, cfg: &Config, name: &str) -> ProfileReport {
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
                    message: format!("해석됨 → {}", kind.label()),
                });
            }
            None => items.push(Item {
                status: CheckStatus::Fail,
                label: "source",
                message: "source.uri/uri_env가 없습니다".into(),
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

    // 2) destination — type(local/s3)과 필수 필드.
    let dests = prof.effective_destinations();
    if dests.is_empty() {
        items.push(Item {
            status: CheckStatus::Fail,
            label: "destination",
            message: "destination이 없습니다".into(),
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
                    message: format!("{tag}local인데 path가 없습니다"),
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
                    message: format!("{tag}s3인데 [.s3] 설정이 없습니다"),
                }),
            },
            Some(other) => items.push(Item {
                status: CheckStatus::Fail,
                label: "destination",
                message: format!("{tag}알 수 없는 type '{other}'(local|s3)"),
            }),
            None => items.push(Item {
                status: CheckStatus::Fail,
                label: "destination",
                message: format!("{tag}destination.type이 없습니다(local|s3)"),
            }),
        }
    }

    // 3) encryption — 평문 경고 / age recipient 파일 존재 / aes 키 안내.
    let enc = &prof.features.encryption;
    if !enc.enabled {
        items.push(Item {
            status: CheckStatus::Warn,
            label: "encryption",
            message: "비활성 — 평문으로 백업됩니다".into(),
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
                message: format!("age인데 recipient_file이 없습니다: {f}"),
            }),
            None => items.push(Item {
                status: CheckStatus::Fail,
                label: "encryption",
                message: "age인데 recipient_file 미지정".into(),
            }),
        }
    } else {
        items.push(Item {
            status: CheckStatus::Ok,
            label: "encryption",
            message: format!("{} (키는 런타임 env)", enc.algorithm),
        });
    }

    // 4) 엔진별 안내.
    match db {
        Some(DbKind::Postgres) => {
            if prof.features.incremental.pg_logical {
                items.push(Item {
                    status: CheckStatus::Ok,
                    label: "증분",
                    message: "pg_logical=true(서버 wal_level=logical 필요)".into(),
                });
            }
        }
        Some(DbKind::Mongo) => {
            let eng = &prof.mode.engine;
            if eng == "mongodump" {
                items.push(Item {
                    status: CheckStatus::Ok,
                    label: "엔진",
                    message: "mongodump(외부 도구 필요 — native 권장)".into(),
                });
            }
        }
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

fn render_human(reports: &[ProfileReport], overall: CheckStatus, engines: &BTreeSet<&str>) {
    let color = use_color();
    println!(
        "{}",
        paint(
            &format!("doctor — config 점검 ({}개 프로파일)", reports.len()),
            &[BOLD],
            color
        )
    );
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
                "참고: 한 config에 여러 DB가 있습니다. 마이그레이션은 같은 엔진끼리만 \
                 가능합니다(PG↔Mongo 변환 불가).",
                &[DIM],
                color
            )
        );
    }
    println!();
    let label = match overall {
        CheckStatus::Ok => "정상 (exit 0)",
        CheckStatus::Warn => "경고 동반 (exit 4)",
        CheckStatus::Fail => "차단성 문제 (exit 3)",
    };
    println!("전체: {} {}", signal(overall, color), label);
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
