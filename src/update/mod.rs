//! `x-backup update` — 설치 소스를 감지해 알맞은 경로로 자기 갱신한다.
//!
//! gk(`x-mesh/gk`)의 update 설계를 따른다:
//! - **brew 설치**(Homebrew prefix 아래) → `brew upgrade x-mesh/tap/x-backup`으로 위임.
//! - **cargo install**(`~/.cargo/bin`) → 덮어쓰지 않고 갱신 명령만 안내.
//! - **manual**(install.sh — `~/.local/bin` 등) → GitHub 릴리스 자산을 내려받아
//!   `checksums.txt`로 sha256 검증 후 **원자적 rename**으로 자기 교체.
//!
//! private 저장소 단계에서는 GitHub API 호출·자산 다운로드에 토큰이 필요하다 —
//! `GITHUB_TOKEN` > `GH_TOKEN` > `gh auth token`(설치돼 있으면) 순으로 찾는다.
//! 저장소가 public이 되면 토큰 없이도 동작한다(있으면 rate-limit 완화용으로 사용).

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Result, XBackupError};

/// 갱신 대상 저장소(owner/repo).
pub const REPO: &str = "x-mesh/x-backup";
/// brew 탭의 formula 경로(brew upgrade 인자).
pub const BREW_FORMULA: &str = "x-mesh/tap/x-backup";

/// 실행 중인 바이너리가 어떻게 설치됐는지의 분류.
///
/// 분류는 의도적으로 관대하다 — 애매하면 [`Source::Manual`]로 보고 가장 안전한
/// 경로(다운로드 + 원자적 교체)를 탄다(gk와 동일한 원칙).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Homebrew prefix(Cellar/homebrew/linuxbrew) 아래 — `brew upgrade`로 위임.
    Brew,
    /// `~/.cargo/bin` 아래 — cargo가 소유하므로 덮어쓰지 않고 명령만 안내.
    CargoInstall,
    /// 그 외(install.sh의 `~/.local/bin`, `/usr/local/bin` 등) — 자기 교체 허용.
    Manual,
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Source::Brew => "brew",
            Source::CargoInstall => "cargo-install",
            Source::Manual => "manual",
        };
        f.write_str(s)
    }
}

/// 실행 파일 경로로 설치 소스를 분류한다(순수 함수 — 테스트 용이).
///
/// `home`은 `$HOME`(cargo bin 판별용). 경로 문자열 기반 휴리스틱이며,
/// 매칭 실패 시 [`Source::Manual`]이다.
pub fn detect_install(exe: &Path, home: Option<&Path>) -> Source {
    let p = exe.to_string_lossy();
    if p.contains("/Cellar/") || p.contains("/homebrew/") || p.contains("/linuxbrew/") {
        return Source::Brew;
    }
    if let Some(home) = home {
        if exe.starts_with(home.join(".cargo").join("bin")) {
            return Source::CargoInstall;
        }
    }
    Source::Manual
}

/// `vX.Y.Z`/`X.Y.Z` 태그를 (major, minor, patch)로 파싱한다. 그 외 형식은 None.
pub fn parse_semver(tag: &str) -> Option<(u64, u64, u64)> {
    let t = tag.trim().trim_start_matches('v');
    let mut it = t.splitn(3, '.');
    let maj = it.next()?.parse().ok()?;
    let min = it.next()?.parse().ok()?;
    // patch 뒤 pre-release(-rc.1 등)는 1차에서 지원하지 않는다 — 숫자만 허용.
    let pat = it.next()?.parse().ok()?;
    Some((maj, min, pat))
}

/// 현재 플랫폼의 릴리스 자산 이름 — gk 컨벤션: `{bin}_{os}_{arch}.tar.gz`.
pub fn platform_asset_name() -> Result<String> {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        other => {
            return Err(XBackupError::Failure(format!(
                "지원하지 않는 OS: {other} (darwin/linux만 지원)"
            )))
        }
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => {
            return Err(XBackupError::Failure(format!(
                "지원하지 않는 아키텍처: {other} (arm64/amd64만 지원)"
            )))
        }
    };
    Ok(format!("x-backup_{os}_{arch}.tar.gz"))
}

/// `checksums.txt`(`<sha256hex>  <name>` 행들)에서 자산의 기대 해시를 찾는다.
pub fn checksum_for<'a>(checksums: &'a str, asset: &str) -> Option<&'a str> {
    checksums.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let name = parts.next()?;
        (name == asset && hash.len() == 64).then_some(hash)
    })
}

/// GitHub 릴리스 응답에서 쓰는 최소 필드.
#[derive(Debug, Deserialize)]
pub struct Release {
    /// 릴리스 태그(예: `v0.1.0`).
    pub tag_name: String,
    /// 자산 목록.
    pub assets: Vec<Asset>,
}

/// 릴리스 자산 — private 저장소에서는 `url`(API asset endpoint)로 받아야 한다
/// (`browser_download_url`은 토큰 인증으로 받을 수 없음).
#[derive(Debug, Deserialize)]
pub struct Asset {
    /// 자산 파일명.
    pub name: String,
    /// API asset endpoint (`.../releases/assets/{id}`).
    pub url: String,
}

/// 토큰 탐색: `GITHUB_TOKEN` > `GH_TOKEN` > `gh auth token`(있으면). 없으면 None.
pub fn github_token() -> Option<String> {
    for key in ["GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(v) = std::env::var(key) {
            let v = v.trim().to_string();
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    // gh CLI가 있으면 그 인증을 재사용한다(설치 환경에서 흔한 경로).
    let out = std::process::Command::new("gh")
        .args(["auth", "token"])
        .output()
        .ok()?;
    if out.status.success() {
        let tok = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !tok.is_empty() {
            return Some(tok);
        }
    }
    None
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("x-backup/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| XBackupError::Failure(format!("HTTP 클라이언트 생성 실패: {e}")))
}

fn auth_header(req: reqwest::RequestBuilder, token: &Option<String>) -> reqwest::RequestBuilder {
    match token {
        Some(t) => req.bearer_auth(t),
        None => req,
    }
}

/// 최신 릴리스를 조회한다. private + 토큰 부재(404)는 안내 메시지로 변환한다.
pub async fn fetch_latest(token: &Option<String>) -> Result<Release> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let resp = auth_header(http_client()?.get(&url), token)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| XBackupError::Failure(format!("릴리스 조회 실패: {e}")))?;

    match resp.status().as_u16() {
        200 => resp
            .json::<Release>()
            .await
            .map_err(|e| XBackupError::Failure(format!("릴리스 응답 파싱 실패: {e}"))),
        404 if token.is_none() => Err(XBackupError::Config(format!(
            "{REPO} 릴리스를 찾을 수 없습니다 — private 저장소면 GITHUB_TOKEN을 \
             설정하거나 gh auth login 후 다시 실행하세요."
        ))),
        s => Err(XBackupError::Failure(format!(
            "릴리스 조회 실패: HTTP {s} ({url})"
        ))),
    }
}

/// 자산을 API endpoint로 내려받는다(private 호환 — Accept: octet-stream).
pub async fn download_asset(asset: &Asset, token: &Option<String>) -> Result<Vec<u8>> {
    let resp = auth_header(http_client()?.get(&asset.url), token)
        .header("Accept", "application/octet-stream")
        .send()
        .await
        .map_err(|e| XBackupError::Failure(format!("{} 다운로드 실패: {e}", asset.name)))?;
    if !resp.status().is_success() {
        return Err(XBackupError::Failure(format!(
            "{} 다운로드 실패: HTTP {}",
            asset.name,
            resp.status()
        )));
    }
    resp.bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| XBackupError::Failure(format!("{} 수신 실패: {e}", asset.name)))
}

/// tar.gz 바이트에서 `x-backup` 단일 엔트리를 추출한다.
pub fn extract_binary(targz: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read;
    let gz = flate2::read::GzDecoder::new(targz);
    let mut archive = tar::Archive::new(gz);
    for entry in archive
        .entries()
        .map_err(|e| XBackupError::Failure(format!("tar 읽기 실패: {e}")))?
    {
        let mut entry =
            entry.map_err(|e| XBackupError::Failure(format!("tar 엔트리 실패: {e}")))?;
        let path = entry
            .path()
            .map_err(|e| XBackupError::Failure(format!("tar 경로 실패: {e}")))?
            .to_path_buf();
        if path.file_name().and_then(|n| n.to_str()) == Some("x-backup") {
            let mut buf = Vec::new();
            entry
                .read_to_end(&mut buf)
                .map_err(|e| XBackupError::Failure(format!("tar 추출 실패: {e}")))?;
            return Ok(buf);
        }
    }
    Err(XBackupError::Failure(
        "릴리스 tarball에 x-backup 바이너리가 없습니다".into(),
    ))
}

/// 새 바이너리로 자기 자신을 **원자적으로** 교체한다.
///
/// 같은 디렉터리에 임시 파일(0755)을 쓴 뒤 `rename`한다 — 같은 파일시스템이라
/// 원자성이 보장되고, 실행 중인 프로세스는 영향이 없다(inode 교체).
pub fn replace_binary(current: &Path, new_bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let dir = current.parent().ok_or_else(|| {
        XBackupError::Failure(format!(
            "실행 파일 경로가 비정상입니다: {}",
            current.display()
        ))
    })?;
    let tmp: PathBuf = dir.join(".x-backup.update.tmp");
    // 잔재가 있으면 제거(이전 실패의 흔적).
    let _ = std::fs::remove_file(&tmp);

    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o755)
        .open(&tmp)
        .map_err(|e| {
            XBackupError::Failure(format!(
                "{} 쓰기 실패: {e} — 설치 디렉터리에 쓰기 권한이 없으면 \
                 소유자 권한으로 다시 실행하세요",
                tmp.display()
            ))
        })?;
    f.write_all(new_bytes)
        .and_then(|_| f.sync_all())
        .map_err(|e| XBackupError::Failure(format!("새 바이너리 쓰기 실패: {e}")))?;
    drop(f);

    std::fs::rename(&tmp, current).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        XBackupError::Failure(format!("바이너리 교체(rename) 실패: {e}"))
    })
}

/// sha256 hex 계산(자산 검증용).
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_brew_paths() {
        let home = PathBuf::from("/Users/u");
        for p in [
            "/opt/homebrew/Cellar/x-backup/0.1.0/bin/x-backup",
            "/home/linuxbrew/.linuxbrew/bin/x-backup",
            "/usr/local/Cellar/x-backup/0.1.0/bin/x-backup",
        ] {
            assert_eq!(
                detect_install(Path::new(p), Some(&home)),
                Source::Brew,
                "{p}"
            );
        }
    }

    #[test]
    fn detect_cargo_and_manual() {
        let home = PathBuf::from("/Users/u");
        assert_eq!(
            detect_install(Path::new("/Users/u/.cargo/bin/x-backup"), Some(&home)),
            Source::CargoInstall
        );
        for p in ["/Users/u/.local/bin/x-backup", "/usr/local/bin/x-backup"] {
            assert_eq!(
                detect_install(Path::new(p), Some(&home)),
                Source::Manual,
                "{p}"
            );
        }
        // home 미상이면 cargo 경로도 manual(안전한 쪽) — 다만 brew 휴리스틱은 유지.
        assert_eq!(
            detect_install(Path::new("/Users/u/.cargo/bin/x-backup"), None),
            Source::Manual
        );
    }

    #[test]
    fn semver_parse_and_compare() {
        assert_eq!(parse_semver("v0.1.0"), Some((0, 1, 0)));
        assert_eq!(parse_semver("1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_semver("v1.2"), None);
        assert_eq!(parse_semver("v1.2.3-rc.1"), None);
        assert!(parse_semver("v0.2.0") > parse_semver("v0.1.9"));
    }

    #[test]
    fn checksum_lookup() {
        let sums = format!(
            "{}  x-backup_darwin_arm64.tar.gz\n{}  x-backup_linux_amd64.tar.gz\n",
            "a".repeat(64),
            "b".repeat(64)
        );
        assert_eq!(
            checksum_for(&sums, "x-backup_linux_amd64.tar.gz"),
            Some("b".repeat(64).as_str()).map(|_| sums
                .lines()
                .nth(1)
                .unwrap()
                .split_whitespace()
                .next()
                .unwrap())
        );
        assert_eq!(checksum_for(&sums, "missing.tar.gz"), None);
    }

    #[test]
    fn tar_round_trip_extract_and_replace() {
        // tar.gz 만들기 → 추출 → 교체까지 한 번에 검증.
        let payload = b"#!/bin/sh\necho new-binary\n".to_vec();
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        let mut header = tar::Header::new_gnu();
        header.set_size(payload.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, "x-backup", payload.as_slice())
            .unwrap();
        let targz = builder.into_inner().unwrap().finish().unwrap();

        let extracted = extract_binary(&targz).unwrap();
        assert_eq!(extracted, payload);

        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("x-backup");
        std::fs::write(&bin, b"old").unwrap();
        replace_binary(&bin, &extracted).unwrap();
        assert_eq!(std::fs::read(&bin).unwrap(), payload);
        // 0755 권한 확인.
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&bin).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        // 임시 파일 잔재 없음.
        assert!(!dir.path().join(".x-backup.update.tmp").exists());
    }

    #[test]
    fn sha256_matches_known_vector() {
        // sha256("abc") 표준 테스트 벡터.
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
