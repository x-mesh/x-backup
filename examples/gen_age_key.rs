//! age 키쌍 생성 헬퍼(개발·테스트 전용) — `age-keygen` 미설치 환경 대비.
//!
//! 프로젝트가 이미 의존하는 age 크레이트로 X25519 키쌍을 만들어
//! `<dir>/age.key`(identity, 0600)와 `<dir>/age.pub`(recipient)를 쓴다.
//!
//! 사용: `cargo run --example gen_age_key -- <출력 디렉터리>`

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;

use age::secrecy::ExposeSecret;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = std::env::args()
        .nth(1)
        .ok_or("사용법: cargo run --example gen_age_key -- <출력 디렉터리>")?;
    std::fs::create_dir_all(&dir)?;

    let identity = age::x25519::Identity::generate();
    let recipient = identity.to_public();

    let key_path = format!("{dir}/age.key");
    let mut key_file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&key_path)?;
    writeln!(key_file, "{}", identity.to_string().expose_secret())?;

    let pub_path = format!("{dir}/age.pub");
    std::fs::write(&pub_path, format!("{recipient}\n"))?;

    println!("identity:  {key_path} (0600)");
    println!("recipient: {pub_path} ({recipient})");
    Ok(())
}
