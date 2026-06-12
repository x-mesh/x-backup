//! `update` 핸들러 — 설치 소스 감지 → brew 위임 / 안내 / 자기 교체.

use crate::cli::args::UpdateArgs;
use crate::error::{Result, XBackupError};
use crate::update::{
    self, checksum_for, detect_install, download_asset, extract_binary, fetch_latest, parse_semver,
    platform_asset_name, replace_binary, sha256_hex, Source,
};

const CURRENT: &str = env!("CARGO_PKG_VERSION");

pub async fn handle(args: UpdateArgs) -> Result<()> {
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

    println!("현재 버전:  v{CURRENT} ({source} 설치)");
    println!("최신 릴리스: {}", release.tag_name);

    if latest <= current {
        println!("이미 최신 버전입니다.");
        return Ok(());
    }
    if args.check {
        println!("새 버전이 있습니다 — `x-backup update`로 갱신하세요.");
        return Ok(());
    }

    // 2) 소스별 갱신 경로.
    match source {
        Source::Brew => {
            // brew가 소유한 바이너리는 brew로 갱신한다(gk와 동일하게 위임).
            println!(
                "brew 설치 감지 — `brew upgrade {}` 실행",
                update::BREW_FORMULA
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
                "cargo install 설치 감지 — 직접 덮어쓰지 않습니다. 갱신하려면:\n  \
                 cargo install --git https://github.com/{} --tag {}",
                update::REPO,
                release.tag_name
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

            println!("다운로드: {asset_name} ({})", release.tag_name);
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
                "갱신 완료: v{CURRENT} → {} ({})",
                release.tag_name,
                exe.display()
            );
            Ok(())
        }
    }
}
