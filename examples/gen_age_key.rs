//! age 키쌍 생성 헬퍼(개발·테스트 전용) — `age-keygen` 미설치 환경 대비.
//!
//! 프로젝트가 이미 의존하는 age 크레이트로 X25519 키쌍을 만들어
//! `<dir>/age.key`(identity, 0600)와 `<dir>/age.pub`(recipient)를 쓴다.
//! 실제 생성 로직은 라이브러리([`x_backup::crypto::generate_keypair_files`])를
//! 공유한다 — init 마법사의 "키 없으면 만들기"와 단일 구현이다.
//!
//! 사용: `cargo run --example gen_age_key -- <출력 디렉터리>`

use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = std::env::args()
        .nth(1)
        .ok_or("사용법: cargo run --example gen_age_key -- <출력 디렉터리>")?;

    let key_path = Path::new(&dir).join("age.key");
    let pub_path = Path::new(&dir).join("age.pub");
    let recipient = x_backup::crypto::generate_keypair_files(&key_path, &pub_path)?;

    println!("identity:  {} (0600)", key_path.display());
    println!("recipient: {} ({recipient})", pub_path.display());
    Ok(())
}
