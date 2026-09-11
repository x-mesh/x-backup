//! `update` 핸들러 — 설치 소스 감지 → brew 위임 / 안내 / 자기 교체.

use crate::cli::args::UpdateArgs;
use crate::cli::output::{field_line, field_line_toned, style, Tone};
use crate::error::{Result, XBackupError};
use crate::i18n::Lang;
use crate::update::{
    self, checksum_for, detect_install, download_asset, extract_binary, fetch_latest, parse_semver,
    platform_asset_name, replace_binary, sha256_hex, Source,
};

const CURRENT: &str = env!("CARGO_PKG_VERSION");

pub async fn handle(lang_flag: Option<Lang>, args: UpdateArgs) -> Result<()> {
    // update는 config를 읽지 않으므로 언어는 CLI `--lang`/env `XB_LANG` 또는 기본 en으로만 정한다.
    let lang = crate::i18n::activate(lang_flag, None);

    let exe = std::env::current_exe()
        .map_err(|e| XBackupError::Failure(format!("실행 파일 경로 확인 실패: {e}")))?;
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let source = detect_install(&exe, home.as_deref());

    // 1) 최신 버전 조회(모든 소스 공통 — --check와 up-to-date 판단에 필요).
    let token = update::github_token();
    let release = fetch_latest(&token).await?;
    let latest = parse_semver(&release.tag_name).ok_or_else(|| {
        XBackupError::Failure(format!("릴리스 태그 파싱 실패: {}", release.tag_name))
    })?;
    let current = parse_semver(CURRENT)
        .ok_or_else(|| XBackupError::Failure(format!("현재 버전 파싱 실패: {CURRENT}")))?;

    // label(current/latest)은 항상 영문, 괄호 안 설치 소스 설명만 언어에 따라.
    println!(
        "{}",
        field_line(
            "current",
            format!(
                "{} {}",
                style(&format!("v{CURRENT}"), Tone::Value),
                style(
                    lang.sel(&format!("({source} install)"), &format!("({source} 설치)")),
                    Tone::Muted
                )
            ),
            10,
        )
    );
    println!(
        "{}",
        field_line_toned("latest", &release.tag_name, 10, Tone::Value)
    );

    if latest <= current {
        println!(
            "{}",
            style(
                lang.sel("Already up to date.", "이미 최신 버전입니다."),
                Tone::Success
            )
        );
        return Ok(());
    }
    if args.check {
        println!(
            "{}",
            style(
                lang.sel(
                    "A new version is available — run `x-backup update` to upgrade.",
                    "새 버전이 있습니다 — `x-backup update`로 갱신하세요.",
                ),
                Tone::Warning,
            )
        );
        return Ok(());
    }

    // 2) 소스별 갱신 경로.
    match source {
        Source::Brew => {
            // brew가 소유한 바이너리는 brew로 갱신한다(gk와 동일하게 위임).
            println!(
                "{}",
                style(
                    lang.sel(
                        &format!(
                            "brew install detected — running `brew upgrade {}`",
                            update::BREW_FORMULA
                        ),
                        &format!(
                            "brew 설치 감지 — `brew upgrade {}` 실행",
                            update::BREW_FORMULA
                        ),
                    ),
                    Tone::Plan,
                )
            );
            let status = std::process::Command::new("brew")
                .args(["upgrade", update::BREW_FORMULA])
                .status()
                .map_err(|e| XBackupError::Failure(format!("brew 실행 실패: {e}")))?;
            if !status.success() {
                return Err(XBackupError::Failure(
                    "brew upgrade가 실패했습니다 — 위 brew 출력을 확인하세요".into(),
                ));
            }
            Ok(())
        }
        Source::CargoInstall => {
            // cargo가 소유 — 덮어쓰지 않고 명령만 안내(정보성 성공).
            println!(
                "{}",
                style(
                    lang.sel(
                        &format!(
                            "cargo install detected — not overwriting directly. To upgrade:\n  \
                             cargo install --git https://github.com/{} --tag {}",
                            update::REPO,
                            release.tag_name
                        ),
                        &format!(
                            "cargo install 설치 감지 — 직접 덮어쓰지 않습니다. 갱신하려면:\n  \
                             cargo install --git https://github.com/{} --tag {}",
                            update::REPO,
                            release.tag_name
                        ),
                    ),
                    Tone::Plan,
                )
            );
            Ok(())
        }
        Source::Manual => {
            let asset_name = platform_asset_name()?;
            let asset = release
                .assets
                .iter()
                .find(|a| a.name == asset_name)
                .ok_or_else(|| {
                    XBackupError::Failure(format!(
                        "릴리스 {}에 {asset_name} 자산이 없습니다",
                        release.tag_name
                    ))
                })?;
            let sums_asset = release
                .assets
                .iter()
                .find(|a| a.name == "checksums.txt")
                .ok_or_else(|| XBackupError::Failure("릴리스에 checksums.txt가 없습니다".into()))?;

            println!(
                "{}",
                field_line(
                    "download",
                    format!(
                        "{} ({})",
                        style(&asset_name, Tone::Value),
                        style(&release.tag_name, Tone::Muted)
                    ),
                    10,
                )
            );
            let targz = download_asset(asset, &token).await?;
            let sums =
                String::from_utf8_lossy(&download_asset(sums_asset, &token).await?).into_owned();

            let expected = checksum_for(&sums, &asset_name).ok_or_else(|| {
                XBackupError::Failure(format!("checksums.txt에 {asset_name} 항목이 없습니다"))
            })?;
            let actual = sha256_hex(&targz);
            if expected != actual {
                return Err(XBackupError::Failure(format!(
                    "sha256 불일치 — 기대 {expected}, 실제 {actual}. 갱신을 중단합니다."
                )));
            }

            let new_bin = extract_binary(&targz)?;
            replace_binary(&exe, &new_bin)?;
            println!(
                "{}",
                style(
                    lang.sel(
                        &format!(
                            "updated: v{CURRENT} → {} ({})",
                            release.tag_name,
                            exe.display()
                        ),
                        &format!(
                            "갱신 완료: v{CURRENT} → {} ({})",
                            release.tag_name,
                            exe.display()
                        ),
                    ),
                    Tone::Success,
                )
            );
            Ok(())
        }
    }
}
