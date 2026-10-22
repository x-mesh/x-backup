//! 잡 실행 계층 — 웹 요청을 `x-backup <cmd> --json` **자식 프로세스**로 바꾼다.
//!
//! ## 이 계층이 존재하는 이유
//! [`crate::web`] 모듈 헤더의 최상위 불변식("웹은 도메인 로직을 재구현하지 않는다")을
//! 실행 가능한 코드로 만든 것이 이 모듈이다. 요청이 오면 도메인 함수를 부르는 대신
//! 우리 자신을 자식으로 다시 띄운다. 그 대가로 얻는 것:
//!
//! - **파일 락을 공짜로 상속한다.** 자식이 [`crate::lock`]으로 락을 잡으므로 cron이 띄운
//!   CLI와 웹이 띄운 잡이 자동으로 상호 배제된다. 웹이 별도 락을 만들면 이 성질이 깨진다
//!   (그래서 이 모듈에는 락 코드가 한 줄도 없다).
//! - **종료 코드 0~5와 생명주기 훅이 그대로 적용된다.** 웹이 판정을 다시 하지 않는다.
//! - **동작 드리프트가 불가능하다.** 도메인 경로가 하나뿐이므로 "콘솔에서는 되는데
//!   CLI에서는 안 되는" 상황이 만들어질 수 없다.
//!
//! ## 파일 구성 — 왜 셋으로 갈랐는가
//! 세 파일은 각각 하나의 질문에 답하고, 서로의 답을 신뢰한다:
//!
//! | 파일 | 질문 | 신뢰하는 것 |
//! |---|---|---|
//! | [`args`] | 이 문자열을 값으로 받아도 되는가? | 없음(입력의 최전선) |
//! | [`spec`] | 무엇을 실행할 것인가? | `args`가 값을 검증했다 |
//! | [`runner`] | 어떻게 띄울 것인가? | `spec`이 만든 argv는 안전하다 |
//!
//! 이 방향이 한쪽으로만 흐르는 것이 중요하다 — `runner`는 문자열을 검증하지 않고,
//! `args`는 프로세스를 모른다. 검증이 두 곳에 흩어지면 어느 쪽이 진짜 관문인지 아무도
//! 모르게 된다.
//!
//! ## 시크릿은 서버 환경에 상주하지 않는다
//! [`JobSecrets`]가 기동 시점에 시크릿 값을 프로세스 메모리로 옮기고 **환경변수에서
//! 제거**한다. 그 뒤 잡을 띄울 때마다 그 자식 하나의 env에만 다시 넣는다
//! ([`runner::JobRunner`]). 목적은 노출 창을 좁히는 것이다 — 시크릿이 살아 있는 위치가
//! "상주 서버 프로세스의 환경(항상)"에서 "잡이 도는 동안의 자식 env(잡 실행 순간)"로
//! 옮겨진다.
//!
//! **한계를 정직하게 밝힌다.** 이건 완전한 제거가 아니다:
//!
//! 1. `/proc/<pid>/environ`은 **exec 시점에 스택에 놓인 원본 환경 블록**을 보여준다.
//!    glibc의 `unsetenv`는 `environ` 포인터 배열만 고치고 그 원본 블록을 지우지 않으므로,
//!    제거 후에도 `/proc/<pid>/environ`에는 값이 남아 있을 수 있다. 즉 이 제거가 확실히
//!    막는 것은 *살아 있는 환경*을 읽는 경로다: 실수로 `std::env::vars()`를 로그에 찍는
//!    코드, 우리가 띄우는 다른 자식의 상속, 크래시 핸들러의 환경 덤프.
//!    **원본 블록까지 없애는 유일한 방법은 시크릿을 애초에 서버 환경에 넣지 않는 것**
//!    (파일·소켓으로 받는 것)이고, 그 경로는 [`JobSecrets::insert_value`]로 이미 열려
//!    있다 — env 경유는 CLI와 같은 방식을 지원하기 위한 것이다.
//! 2. 값은 여전히 프로세스 메모리에 평문으로 있다(메모리 덤프·코어 파일). 이 계층에서
//!    지울 수 있는 것이 아니다([`crate::web::mask`] 헤더의 같은 판단).
//! 3. [`std::env::remove_var`]는 프로세스 전역 상태를 바꾼다 — 다른 스레드가 동시에
//!    환경을 읽으면 경합이다(그래서 Rust 2024 edition에서 `unsafe`가 됐다. 이 크레이트는
//!    edition 2021이라 호출 자체는 안전 함수다). 그래서 [`JobSecrets::load`]는 **기동
//!    경로에서 한 번만** 부른다 — 리스너를 열기 전, 잡이 하나도 돌지 않는 시점이다.
//!    요청 처리 중에는 절대 부르지 않는다.
//!
//! ## age 개인키 경로는 예외적으로 서버 환경에 남긴다
//! `XB_AGE_IDENTITY_FILE`은 제거하지 않고 **복사**해 간다([`JobSecrets::carry_from_env`]).
//! 두 가지 이유다:
//!
//! 1. 그 값은 키가 아니라 **경로**다. 실제 키 material은 파일 안에 있고 파일 권한으로
//!    보호된다. 경로가 노출되는 것과 개인키가 노출되는 것은 위험도가 다르다.
//! 2. [`crate::web::auth::check_age_identity_permissions`]가 **기동 중에 이 env를 읽어**
//!    파일 권한을 검사한다(t6). 우리가 먼저 지워 버리면 그 검사가 "미설정"으로 보고
//!    조용히 통과한다 — 시크릿 노출을 줄이려다 개인키 권한 검사를 무력화하는 셈이다.
//!
//! 대칭키(`XB_AES_KEY_HEX`)는 반대다 — 그건 값 자체가 키 material이므로 제거 대상이다.

pub mod args;
pub mod exit;
pub mod lifecycle;
pub mod runner;
pub mod spec;
pub mod stream;

pub use args::ProfileName;
pub use runner::{
    JobCompletion, JobHandle, JobOutcome, JobRunner, RunningJob, FORWARDED_ENV_NAMES,
};
pub use spec::{JobCommand, JobCount, JobFlag, JobSpec};

use crate::config::file::Config;
use crate::config::secret::Secret;
use crate::web::mask::SecretRegistry;

/// 프로세스 env를 만지는 테스트를 직렬화한다 — 웹 계층 공용 가드
/// ([`crate::web::auth::ENV_GUARD`])를 쓰되, 잠금이 오염(poison)돼도 계속 잡는다.
///
/// 보호 대상이 `()`(데이터 없음)이므로 이전 패닉이 남긴 불변식 위반이 애초에 없다.
/// `unwrap()`을 쓰면 한 테스트의 실패가 같은 가드를 쓰는 **다른 파일의 테스트까지**
/// 연쇄로 무너뜨려, 원인 하나가 실패 여섯 개로 보인다 — 진단을 망친다.
#[cfg(test)]
pub(crate) fn env_guard() -> std::sync::MutexGuard<'static, ()> {
    crate::web::auth::ENV_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// 자식 env에 넣을 시크릿 하나.
struct SecretEntry {
    /// 환경변수 이름(시크릿이 아니다 — config에 평문으로 적혀 있는 값이다).
    name: String,
    /// 값. [`Secret`]에 담아 `Debug`/`Display`로 새지 않게 한다.
    value: Secret,
}

/// 자식에게만 주입할 시크릿 모음 + 그 값들을 지우기 위한 마스킹 레지스트리.
///
/// 수명·범위와 한계는 모듈 헤더 "시크릿은 서버 환경에 상주하지 않는다" 참조.
///
/// `Clone`을 파생하지 않는다 — 시크릿 사본이 여러 곳에 생기는 것을 문법 수준에서
/// 막는다. 러너 하나가 소유하고, 필요한 곳은 `&JobRunner`를 공유한다.
#[derive(Default)]
pub struct JobSecrets {
    entries: Vec<SecretEntry>,
    /// 자식 출력에서 이 값들을 지우는 데 쓴다(t7 [`SecretRegistry`]).
    registry: SecretRegistry,
    /// **자식에게 주입은 됐지만 마스킹 대상으로 등록되지 못한** env 이름들
    /// ([`JobSecrets::unmaskable_env_names`]). 값은 담지 않는다 — 이 필드의 존재 이유가
    /// "값이 로그에 남을 수 있다"를 알리는 것이므로, 그 경고를 만들면서 값을 또 복사해
    /// 들고 있으면 자기모순이다.
    unmaskable: Vec<String>,
}

impl std::fmt::Debug for JobSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 이름은 보여도 무해하지만(config에 평문으로 있다) 값은 절대 찍지 않는다.
        let names: Vec<&str> = self.entries.iter().map(|e| e.name.as_str()).collect();
        f.debug_struct("JobSecrets").field("names", &names).finish()
    }
}

impl JobSecrets {
    /// 빈 모음.
    pub fn new() -> Self {
        Self::default()
    }

    /// config가 선언한 시크릿 env와 암호화 키 env를 모아 온다 — **기동 경로에서 한 번만**
    /// 호출한다(모듈 헤더의 스레드 안전성 주의).
    ///
    /// `config`가 `None`이거나 파싱에 실패해 넘어오지 않으면 config 기반 시크릿 없이
    /// 진행한다. 여기서 서버 기동을 막지 않는 이유: config가 성한지 판정하는 것은
    /// `doctor`의 일이고, 콘솔의 존재 이유 중 하나가 **그 판정 결과를 보여주는 것**이다.
    /// config가 깨졌다고 콘솔이 뜨지 않으면 운영자는 터미널로 돌아가야 한다. 시크릿이
    /// 없으면 잡은 자식 쪽에서 설정 오류(exit 2)로 끊기고, 그 메시지가 화면에 그대로
    /// 나온다 — 조용한 실패가 아니다.
    pub fn load(config: Option<&Config>) -> Self {
        let mut secrets = Self::new();

        let names = collect_secret_env_names(config);
        let taken = secrets.take_from_env(&names);
        // 대칭키는 값 자체가 키 material이므로 제거 대상이다(모듈 헤더).
        let aes = secrets.take_from_env(&[crate::pipeline::stage::ENV_AES_KEY_HEX.to_string()]);
        // age identity는 경로이고 기동 중 권한 검사가 읽어야 하므로 복사만 한다.
        let age = secrets.carry_from_env(crate::pipeline::stage::ENV_AGE_IDENTITY_FILE);

        tracing::debug!(
            declared = names.len(),
            taken = taken + aes,
            carried = age as usize,
            "잡 자식에 주입할 시크릿을 수집했습니다"
        );
        secrets
    }

    /// 시크릿 값을 직접 등록한다 — 자식 env에 넣고, 마스킹 대상으로도 등록한다.
    ///
    /// env를 거치지 않는 주입 경로다(파일·소켓으로 받은 값). 이 경로로 들어온 값은
    /// 서버 환경에 애초에 존재하지 않으므로 모듈 헤더가 밝힌 `/proc/<pid>/environ`
    /// 한계가 적용되지 않는다.
    ///
    /// ## 등록이 거부되면 조용히 넘기지 않는다
    /// [`SecretRegistry::register`]는 값이 [`crate::web::mask::MIN_SECRET_LEN`] 미만이면
    /// `false`를 돌려준다(짧은 값을 등록하면 무해한 텍스트까지 `[REDACTED]`로 뭉개지므로 —
    /// 그 하한 자체의 근거는 [`crate::web::mask`] 참조). 문제는 **그 반환값을 버리면
    /// "주입은 하는데 마스킹은 못 한다"는 조합이 아무 흔적 없이 성립한다**는 것이다: 7자짜리
    /// DB 비밀번호는 자식 env로 그대로 들어가지만 잡 로그·SSE·잡 이력에서 한 번도 지워지지
    /// 않는다. 그래서 여기서 반환값을 받아 (1) 그 자리에서 경고를 남기고 (2) env 이름을
    /// [`unmaskable_env_names`](Self::unmaskable_env_names)에 모아 기동 배너가 한 번 더
    /// 요약할 수 있게 한다.
    ///
    /// **경고에 값은 절대 싣지 않는다.** "이 값이 마스킹되지 않는다"고 알리는 로그가 그 값을
    /// 로그에 적는다면 그 로그가 곧 누출이다. env 이름은 config에 평문으로 있으므로 무해하다
    /// ([`std::fmt::Debug`] 구현이 같은 판단을 한다).
    ///
    /// 하한을 낮추는 방향은 택하지 않았다 — 과잉 마스킹은 진단 판독성을 망치고, 그건 장애
    /// 대응 중에 더 큰 손해다. 대신 운영자가 "짧은 비밀번호를 쓰면 로그에 남는다"를 알고
    /// 비밀번호를 늘리도록 만드는 것이 옳은 방향이다.
    pub fn insert_value(&mut self, name: impl Into<String>, value: impl Into<String>) {
        let name = name.into();
        let value = value.into();
        if !self.registry.register(value.clone()) {
            tracing::warn!(
                env = %name,
                min_len = crate::web::mask::MIN_SECRET_LEN,
                "이 env의 값이 너무 짧아 마스킹 대상으로 등록하지 못했습니다 — 값은 자식 \
                 프로세스에 그대로 주입되지만 잡 로그·SSE·잡 이력에서 지워지지 않습니다. \
                 최소 {}바이트 이상의 값을 쓰세요.",
                crate::web::mask::MIN_SECRET_LEN
            );
            if !self.unmaskable.contains(&name) {
                self.unmaskable.push(name.clone());
            }
        }
        self.push(name, value);
    }

    /// 주입은 됐지만 마스킹 대상으로 등록되지 못한 env 이름들(값이 하한 미달 —
    /// [`insert_value`](Self::insert_value) 참조).
    ///
    /// 기동 배너([`crate::web::server`])가 이 목록으로 운영자에게 한 번 더 알린다. 이름만
    /// 담고 값은 담지 않는다.
    pub fn unmaskable_env_names(&self) -> &[String] {
        &self.unmaskable
    }

    /// **값이 아니라 참조(경로 등)**를 등록한다 — 자식 env에는 넣지만 마스킹 대상으로는
    /// 등록하지 않는다.
    ///
    /// 경로를 마스킹 대상으로 등록하면 해로운 쪽이 크다: 잡 로그에서 그 경로가 나오는
    /// 모든 진단(예: "identity 파일을 열 수 없습니다: /etc/xb/age.key")이 `[REDACTED]`로
    /// 덮여, 운영자가 정작 봐야 할 정보를 잃는다([`crate::web::mask`]의 "최소 등록 길이"와
    /// 같은 판단 — 과잉 마스킹은 그 자체로 사고다).
    pub fn insert_reference(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.push(name.into(), value.into());
    }

    /// 이름 목록에 해당하는 env를 읽어 등록하고, **프로세스 환경에서 제거**한다.
    ///
    /// 반환값은 실제로 가져온 개수다. 없거나 빈 값인 이름은 조용히 건너뛴다 — config에
    /// 선언됐지만 이 배포에서는 쓰지 않는 프로파일의 env일 수 있고, 그 판정은 잡을
    /// 실행할 때 자식이 한다.
    pub fn take_from_env(&mut self, names: &[String]) -> usize {
        let mut taken = 0;
        for name in names {
            let Ok(value) = std::env::var(name) else {
                continue;
            };
            if value.is_empty() {
                continue;
            }
            self.insert_value(name.clone(), value);
            // 여기서 서버 환경의 상주가 끝난다(한계는 모듈 헤더).
            std::env::remove_var(name);
            taken += 1;
        }
        taken
    }

    /// env를 읽어 등록하지만 **제거하지 않는다** — 값이 시크릿이 아니라 참조일 때만
    /// 쓴다(모듈 헤더의 age identity 예외).
    pub fn carry_from_env(&mut self, name: &str) -> bool {
        match std::env::var(name) {
            Ok(value) if !value.is_empty() => {
                self.insert_reference(name.to_string(), value);
                true
            }
            _ => false,
        }
    }

    /// 등록된 시크릿 개수.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 등록된 시크릿이 없는지.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 등록된 env 변수 이름(값은 노출하지 않는다) — 진단·테스트용.
    pub fn names(&self) -> Vec<&str> {
        self.entries.iter().map(|e| e.name.as_str()).collect()
    }

    /// 자식 출력 마스킹에 쓸 레지스트리.
    pub fn registry(&self) -> &SecretRegistry {
        &self.registry
    }

    /// 자식 [`Command`](tokio::process::Command)에 시크릿을 넣는다.
    ///
    /// [`runner::JobRunner`]가 `env_clear()` 직후에만 호출한다 — 이 함수는 "무엇을
    /// 넣을지"만 알고 "무엇을 지웠는지"는 모른다.
    pub(crate) fn apply(&self, cmd: &mut tokio::process::Command) {
        for entry in &self.entries {
            cmd.env(&entry.name, entry.value.expose());
        }
    }

    /// 같은 이름이 이미 있으면 값을 교체한다(마지막 승리) — 같은 env를 두 경로로
    /// 등록했을 때 자식 env에 무엇이 들어가는지가 모호해지지 않게 한다.
    fn push(&mut self, name: String, value: String) {
        let value = Secret::new(value);
        match self.entries.iter_mut().find(|e| e.name == name) {
            Some(existing) => existing.value = value,
            None => self.entries.push(SecretEntry { name, value }),
        }
    }
}

/// config가 선언한 시크릿 env 변수 **이름**을 전 프로파일에서 모은다.
///
/// config에는 시크릿 값이 아니라 이름만 있다(FR-10, [`crate::config::secret`]) — 이
/// 함수가 모으는 것도 이름이다. [`crate::cli::handlers`]의 backup이 훅에서 지울 이름을
/// 모으는 것과 같은 집합이다(`uri_env` + `read_uri_env` + destination별
/// `credentials_env`). 그쪽은 "지울 목록"이고 여기는 "넣을 목록"이라는 점만 다르다.
///
/// **전** 프로파일에서 모으는 이유: 어느 프로파일의 잡이 들어올지 기동 시점에는 모른다.
/// 그래서 자식 하나는 자기 프로파일이 쓰지 않는 시크릿까지 받는다 — 같은 config 파일과
/// 같은 운영자 권한 아래 있는 값들이라 신뢰 경계가 같지만, 좁힐 여지가 있는 것은
/// 사실이다([`JobSpec`]이 필요한 이름을 함께 나르면 프로파일 단위로 줄일 수 있다).
/// 지금 그렇게 하지 않은 이유는 노출 창이 이미 "잡 하나가 도는 동안"으로 좁혀져 있어
/// 추가 이득이 작고, 프로파일→시크릿 매핑을 웹이 판정하기 시작하면 config 해석을 웹이
/// 이중 구현하는 방향으로 번지기 때문이다.
pub fn collect_secret_env_names(config: Option<&Config>) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let Some(config) = config else {
        return names;
    };
    let mut push = |name: Option<&String>| {
        if let Some(name) = name {
            if !name.is_empty() && !names.iter().any(|n| n == name) {
                names.push(name.clone());
            }
        }
    };
    for profile in config.profiles.values() {
        push(profile.source.uri_env.as_ref());
        // 복제본 자격증명도 반드시 포함한다 — 빠지면 read_uri로 백업하는 프로파일이
        // 웹에서만 실패한다.
        push(profile.source.read_uri_env.as_ref());
        for dest in profile.effective_destinations() {
            if let Some(s3) = &dest.s3 {
                push(s3.credentials_env.as_ref());
            }
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 프로파일 두 개 + S3 destination을 담은 config에서 시크릿 env 이름을 모두 모은다.
    #[test]
    fn collect_secret_env_names_gathers_all_declared_names() {
        let toml = r#"
default_profile = "prod"

[profiles.prod.source]
uri_env      = "PROD_MONGO_URI"
read_uri_env = "PROD_REPLICA_URI"

[profiles.prod.destination]
type = "s3"

[profiles.prod.destination.s3]
bucket          = "b"
credentials_env = "PROD_S3_CREDS"

[profiles.dr.source]
uri_env = "DR_MONGO_URI"

[profiles.dr.destination]
type = "local"
path = "/tmp/dr"
"#;
        let config = Config::from_toml_str(toml).expect("config 파싱 실패");
        let mut names = collect_secret_env_names(Some(&config));
        names.sort();
        assert_eq!(
            names,
            vec![
                "DR_MONGO_URI",
                "PROD_MONGO_URI",
                "PROD_REPLICA_URI",
                "PROD_S3_CREDS"
            ]
        );
    }

    /// config가 없으면 이름도 없다(기동을 막지 않는다).
    #[test]
    fn collect_secret_env_names_without_config_is_empty() {
        assert!(collect_secret_env_names(None).is_empty());
    }

    /// 같은 env 이름을 여러 프로파일이 공유하면 한 번만 모은다.
    #[test]
    fn collect_secret_env_names_dedupes_shared_names() {
        let toml = r#"
[profiles.a.source]
uri_env = "SHARED_URI"

[profiles.b.source]
uri_env = "SHARED_URI"
"#;
        let config = Config::from_toml_str(toml).unwrap();
        assert_eq!(collect_secret_env_names(Some(&config)), vec!["SHARED_URI"]);
    }

    /// 직접 등록한 시크릿은 자식 env 목록에 들어가고 마스킹 대상이 된다.
    #[test]
    fn insert_value_registers_for_masking() {
        let mut secrets = JobSecrets::new();
        secrets.insert_value("MY_URI", "mongodb://u:hunter2pass@h/db");
        assert_eq!(secrets.names(), vec!["MY_URI"]);
        assert_eq!(secrets.len(), 1);
        assert!(!secrets.is_empty());
        let masked = secrets
            .registry()
            .mask("failed to connect to mongodb://u:hunter2pass@h/db");
        assert!(
            masked.contains(crate::web::mask::REDACTED_PLACEHOLDER),
            "시크릿이 마스킹되지 않았다: {masked}"
        );
    }

    /// **하한 미달 시크릿은 주입되지만 마스킹되지 않는다 — 그 사실이 드러나야 한다.**
    ///
    /// 예전에는 [`JobSecrets::insert_value`]가 `register()`의 반환값을 버려서, 7자짜리 DB
    /// 비밀번호가 자식 env로 들어가면서 **마스킹 대상에서는 조용히 빠졌다.** 경고 한 줄도
    /// 없었다. 이제 그 이름이 [`JobSecrets::unmaskable_env_names`]에 남아 기동 배너가 운영자에게
    /// 알린다(경고 자체는 `tracing::warn!`으로도 나가지만, 로그 출력은 단위 테스트가 붙잡기
    /// 어려우므로 이 테스트는 같은 판정에서 나오는 **관측 가능한 상태**를 고정한다).
    #[test]
    fn short_value_is_injected_but_reported_as_unmaskable() {
        let mut secrets = JobSecrets::new();
        // 7바이트 — MIN_SECRET_LEN(8) 미달.
        let short = "abc1234";
        assert!(
            short.len() < crate::web::mask::MIN_SECRET_LEN,
            "테스트 전제"
        );
        secrets.insert_value("SHORT_PW", short);

        // 주입은 된다 — 자식이 그 값 없이는 접속할 수 없으므로 여기서 버리면 안 된다.
        assert_eq!(secrets.names(), vec!["SHORT_PW"]);
        // 마스킹은 안 된다(하한의 근거는 `mask.rs` — 과잉 마스킹 방지).
        assert_eq!(secrets.registry().len(), 0);
        assert_eq!(
            secrets.registry().mask(&format!("connect failed: {short}")),
            format!("connect failed: {short}"),
            "테스트 전제: 하한 미달 값은 실제로 지워지지 않는다"
        );
        // 그리고 그 사실이 이름으로 드러난다 — 값은 절대 담지 않는다.
        assert_eq!(secrets.unmaskable_env_names(), ["SHORT_PW"]);
        assert!(
            !secrets.unmaskable_env_names().iter().any(|n| n == short),
            "경고 목록에 값이 들어갔다"
        );
    }

    /// 하한을 넘는 값은 조용히 등록된다 — 경고 목록에 아무것도 남지 않는다.
    #[test]
    fn long_enough_value_is_registered_without_a_warning() {
        let mut secrets = JobSecrets::new();
        // 정확히 하한 길이(8바이트) — 경계값이 경고 쪽으로 넘어가지 않는지 함께 본다.
        let boundary = "abcd1234";
        assert_eq!(
            boundary.len(),
            crate::web::mask::MIN_SECRET_LEN,
            "테스트 전제"
        );
        secrets.insert_value("OK_PW", boundary);
        assert_eq!(secrets.registry().len(), 1);
        assert!(
            secrets.unmaskable_env_names().is_empty(),
            "등록에 성공했는데 경고 목록에 남았다: {:?}",
            secrets.unmaskable_env_names()
        );
    }

    /// 같은 짧은 env를 두 번 넣어도 경고 목록에 한 번만 남는다(소음 방지).
    #[test]
    fn unmaskable_names_are_not_duplicated() {
        let mut secrets = JobSecrets::new();
        secrets.insert_value("SHORT_PW", "short1");
        secrets.insert_value("SHORT_PW", "short2");
        assert_eq!(secrets.unmaskable_env_names(), ["SHORT_PW"]);
    }

    /// 참조(경로)는 자식 env에 들어가지만 마스킹 대상은 아니다 — 과잉 마스킹으로
    /// 진단을 잃지 않는다.
    #[test]
    fn insert_reference_is_not_registered_for_masking() {
        let mut secrets = JobSecrets::new();
        let path = "/etc/x-backup/age-identity.key";
        secrets.insert_reference("XB_AGE_IDENTITY_FILE", path);
        assert_eq!(secrets.names(), vec!["XB_AGE_IDENTITY_FILE"]);
        let message = format!("identity 파일을 열 수 없습니다: {path}");
        assert_eq!(
            secrets.registry().mask(&message),
            message,
            "경로가 마스킹되면 운영자가 원인을 볼 수 없다"
        );
    }

    /// 같은 이름을 두 번 등록하면 마지막 값이 남는다(자식 env가 모호해지지 않는다).
    #[test]
    fn duplicate_name_keeps_last_value() {
        let mut secrets = JobSecrets::new();
        secrets.insert_value("DUP_URI", "first-value-long-enough");
        secrets.insert_value("DUP_URI", "second-value-long-enough");
        assert_eq!(secrets.len(), 1, "같은 이름이 두 번 등록됐다");
        // 두 값 모두 마스킹 대상으로 남는다 — 첫 값이 이미 자식이나 로그에 흘러갔을
        // 수 있으므로 등록을 되돌리지 않는 편이 안전하다.
        assert_eq!(secrets.registry().len(), 2);
    }

    /// `Debug`는 값을 노출하지 않고 이름만 보여준다.
    #[test]
    fn debug_hides_secret_values() {
        let mut secrets = JobSecrets::new();
        secrets.insert_value("LEAKY", "absolutely-secret-value");
        let rendered = format!("{secrets:?}");
        assert!(
            !rendered.contains("absolutely-secret-value"),
            "시크릿 값이 Debug로 새어 나갔다: {rendered}"
        );
        assert!(
            rendered.contains("LEAKY"),
            "이름은 진단에 필요하다: {rendered}"
        );
    }

    /// `carry_from_env`는 값을 가져오지만 프로세스 환경에서 지우지 않는다 —
    /// 기동 중 권한 검사(t6)가 같은 env를 읽어야 한다.
    #[test]
    fn carry_from_env_leaves_the_variable_in_place() {
        let _guard = env_guard();
        let name = "XB_TEST_CARRIED_PATH";
        std::env::set_var(name, "/tmp/x-backup-test-identity.key");

        let mut secrets = JobSecrets::new();
        assert!(secrets.carry_from_env(name), "값을 가져오지 못했다");
        assert!(
            std::env::var(name).is_ok(),
            "carry는 서버 환경에서 지우지 않아야 한다"
        );
        assert_eq!(secrets.names(), vec![name]);

        std::env::remove_var(name);
    }

    /// 없는 이름·빈 값은 조용히 건너뛴다(기동을 막지 않는다).
    #[test]
    fn missing_and_empty_env_are_skipped() {
        let _guard = env_guard();
        let empty = "XB_TEST_EMPTY_SECRET";
        std::env::set_var(empty, "");

        let mut secrets = JobSecrets::new();
        let taken = secrets.take_from_env(&[
            empty.to_string(),
            "XB_TEST_DEFINITELY_NOT_SET_12345".to_string(),
        ]);

        std::env::remove_var(empty);

        assert_eq!(taken, 0, "빈 값·미설정을 가져왔다");
        assert!(secrets.is_empty());
        assert!(!secrets.carry_from_env("XB_TEST_DEFINITELY_NOT_SET_12345"));
    }

    /// `load`는 config 시크릿을 제거하고, age identity 경로는 남기고, 둘 다 자식 목록에
    /// 담는다 — 모듈 헤더의 두 정책이 한 호출에서 동시에 성립하는지 본다.
    #[test]
    fn load_takes_config_secrets_and_carries_age_identity() {
        let _guard = env_guard();
        let uri_env = "XB_TEST_LOAD_URI";
        let age_env = crate::pipeline::stage::ENV_AGE_IDENTITY_FILE;
        let aes_env = crate::pipeline::stage::ENV_AES_KEY_HEX;

        let previous_age = std::env::var(age_env).ok();
        let previous_aes = std::env::var(aes_env).ok();

        std::env::set_var(uri_env, "mongodb://u:hunter2pass@h/db");
        std::env::set_var(age_env, "/tmp/x-backup-test-age.key");
        std::env::set_var(aes_env, "00112233445566778899aabbccddeeff");

        let toml = format!("[profiles.p.source]\nuri_env = \"{uri_env}\"\n");
        let config = Config::from_toml_str(&toml).unwrap();
        let secrets = JobSecrets::load(Some(&config));

        let uri_after = std::env::var(uri_env).ok();
        let age_after = std::env::var(age_env).ok();
        let aes_after = std::env::var(aes_env).ok();

        // 환경 원상복구(다른 테스트에 영향 없게).
        std::env::remove_var(uri_env);
        match previous_age {
            Some(v) => std::env::set_var(age_env, v),
            None => std::env::remove_var(age_env),
        }
        match previous_aes {
            Some(v) => std::env::set_var(aes_env, v),
            None => std::env::remove_var(aes_env),
        }

        // config 시크릿과 대칭키는 서버 환경에서 사라진다.
        assert_eq!(uri_after, None, "config 시크릿이 서버 환경에 남아 있다");
        assert_eq!(aes_after, None, "대칭키가 서버 환경에 남아 있다");
        // age identity 경로는 남는다(기동 중 권한 검사가 읽어야 한다).
        assert!(
            age_after.is_some(),
            "age identity 경로를 지우면 t6의 권한 검사가 무력화된다"
        );

        // 셋 다 자식에게는 전달된다.
        let mut names = secrets.names();
        names.sort_unstable();
        let mut expected = vec![uri_env, age_env, aes_env];
        expected.sort_unstable();
        assert_eq!(names, expected);

        // 마스킹 대상은 값 시크릿 둘뿐이다(경로는 등록하지 않는다).
        assert_eq!(secrets.registry().len(), 2);
    }
}
