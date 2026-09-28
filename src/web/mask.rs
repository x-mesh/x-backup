//! 웹 계층 시크릿 마스킹 — HTTP 응답·로그로 나가는 문자열에서 시크릿 원문을 지우는
//! 2차 방어선.
//!
//! ## 1차 방어와의 관계 — 필드 부재가 먼저다
//! 1차 방어는 API 응답 스키마에 시크릿 값을 담을 필드를 아예 두지 않는 것이다(아래
//! [`ApiView`] 참고) — "마스킹된 값"이 아니라 "필드 자체가 없음"이 목표다. 이 파일의
//! [`SecretRegistry`]는 1차 방어가 뚫렸을 때(리뷰 실수, 또는 우리가 형태를 통제할 수
//! 없는 경로)를 대비한 2차 방어다. 2차 방어가 있다고 1차 방어를 생략해도 되는 건 아니다.
//!
//! ## 왜 정규식/형태 기반이 아니라 "값 등록 + 치환"인가
//! [`crate::cli::output::redact_uri`]와 [`crate::hooks::mask_secrets`]는 문자열이 URI
//! *형태*임을 전제로 userinfo·민감 쿼리 파라미터 위치를 안다. 하지만 웹 콘솔이 중계할
//! 잡 로그·SSE 프레임에는 자식 프로세스(mongodump/pg_dump/S3 SDK 등)가 stderr에 찍는
//! 임의의 텍스트가 그대로 들어간다 — 예를 들어
//! `Warning: could not authenticate with password 'hunter2pass'` 같은 문장은 URI 형태가
//! 아니라서 위 두 함수가 잡지 못한다. S3 secret key처럼 애초에 URI 구조가 없는 순수
//! 랜덤 문자열도 마찬가지다. 자식이 stderr에 무엇을 찍을지 우리가 통제할 수 없으므로
//! "형태를 알아야 잡는" 접근만으로는 부족하다.
//!
//! 그래서 [`SecretRegistry`]는 "이 프로세스가 실제로 알고 있는 시크릿 *값*의 집합"을
//! 들고 있다가, 외부로 나가는 문자열에서 그 값과 바이트 단위로 정확히 일치하는 구간을
//! 찾아 통째로 `[REDACTED]`로 치환한다. 형태를 몰라도 값만 알면 잡는다는 것이 핵심이다.
//!
//! ## 한계 — 정직하게 밝힌다
//! 값 자체가 변형되어 나타나면 못 잡는다: URL 퍼센트 인코딩, hex/base64 등 재인코딩,
//! 대소문자가 바뀐 경우, 시크릿의 일부만 노출된 경우. 이런 경계는 아래
//! `adversarial_*` 테스트로 명시한다 — 잡는 척하지 않는다. 자식 프로세스 stderr는
//! 대개 자격증명을 원문 그대로(또는 URI에 박아서) 찍으므로 실사용에서는 이 두 경로
//! (원문 그대로 / URI 형태)가 대부분을 차지하지만, 공격자가 의도적으로 인코딩해
//! 빼돌리는 경로까지 막지는 못한다.
//!
//! ## 최소 등록 길이
//! [`MIN_SECRET_LEN`] 미만인 값은 등록을 거부한다. 등록된 값은 전부 "발견 즉시 전체
//! 치환" 대상이 되므로, 짧은 값을 등록하면 무해한 텍스트가 우연히 일치해 뭉개진다.
//! 예를 들어 4자리 포트 번호(`"5432"`)나 짧은 DB 이름을 실수로 시크릿으로 등록하면,
//! 로그에서 그 문자열이 나오는 무관한 위치까지 전부 `[REDACTED]`로 덮여 운영자가 정작
//! 봐야 할 정보(어느 포트에서 실패했는지)를 잃는다. 8자는 흔한 비밀번호 최소 길이
//! 관행과 맞으면서, 우연한 충돌 확률을 실용적으로 낮추는 절충선이다.
//!
//! ## 수명·범위
//! [`SecretRegistry`]는 시크릿 값을 평문 `String`으로 프로세스 메모리에 들고 있는다.
//! 이게 노출 범위를 넓히는 건 아니다 — 이 값들은 이미 이 프로세스 자신의 환경변수와
//! (이 프로세스가 띄우는) 자식 프로세스의 env에 프로세스가 살아있는 동안 계속
//! 존재한다(같은 사용자 권한이면 `/proc/<pid>/environ`으로도 읽을 수 있는 값들이다).
//! 그래서 드롭 시점에 메모리를 지우는 코드는 넣지 않았다 — 지워도 진짜 시크릿(env)은
//! 그대로 남아 실질적 보호 효과가 없고, `unsafe` 코드와 zeroize류 의존성만 늘어난다
//! (이 태스크는 `Cargo.toml`을 건드릴 수 없는 범위이기도 하다). 대신 `Debug`를
//! derive하지 않고 개수만 보여주는 수동 구현을 둔다 — `{:?}`로 실수로 로그에 찍혀도
//! 값이 새지 않는다([`crate::config::secret::Secret`]과 같은 이유).
//!
//! 레지스트리 자체를 얼마나 오래 들고 있을지는 이 파일이 강제하지 않는다 — 등록/치환
//! 연산만 제공한다. 지금은 아직 어떤 서버 상태에도 연결돼 있지 않다
//! ([`register_from_env_names`] 참고) — 잡 실행기가 자식 env를 확정하는 이후 태스크가
//! `Arc<ServeConfig>`류 장수명 상태에 이 타입을 필드로 얹을 것으로 예상한다.

/// 시크릿을 치환할 때 쓰는 표식. [`crate::config::secret::Secret`]의 `Display`와 같은
/// 문구를 써서, 값이 어느 방어선(타입 래퍼든 이 레지스트리든)을 거쳐 지워졌는지와
/// 무관하게 로그·응답에 같은 신호가 나가게 한다.
pub const REDACTED_PLACEHOLDER: &str = "[REDACTED]";

/// 등록을 거부하는 최소 길이(바이트 수). 근거는 모듈 헤더의 "최소 등록 길이" 참고.
///
/// 바이트 길이 기준이다 — 등록 대상(URI 비밀번호, S3 secret key, age 키 등)은 전부
/// ASCII 토큰이므로 바이트 수와 문자 수가 일치한다는 전제를 깐다. 멀티바이트 UTF-8
/// 시크릿(사실상 없음)은 이 전제를 벗어나지만, 그 경우도 바이트 길이가 더 길게 나올
/// 뿐이라 하한을 우회하는 방향으로 작동하지는 않는다.
pub const MIN_SECRET_LEN: usize = 8;

/// 서버가 아는 시크릿 값의 집합 — 외부로 나가는 문자열에서 이 값들을 지운다.
///
/// `Debug`를 derive하지 않고 직접 구현한다(모듈 헤더의 "수명·범위" 참고) — derive는
/// `Vec<String>` 필드를 그대로 이어붙이므로 시크릿 원문이 `{:?}`로 새 나간다.
#[derive(Default, Clone)]
pub struct SecretRegistry {
    // 등록 길이 내림차순으로 유지한다 — 한쪽이 다른 쪽의 부분 문자열인 시크릿이 섞여
    // 있어도(예: source URI와 read-replica URI가 같은 비밀번호를 공유하되 한쪽 값이
    // 다른 값에 포함되는 경우) 긴 시크릿부터 치환해야 짧은 시크릿의 잔여 조각이 남지
    // 않는다. `mask`는 이 순서를 그대로 순회하므로 등록 시점에 정렬을 유지해 둔다.
    secrets: Vec<String>,
}

impl std::fmt::Debug for SecretRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretRegistry")
            .field("registered", &self.secrets.len())
            .finish()
    }
}

impl SecretRegistry {
    /// 빈 레지스트리를 만든다.
    pub fn new() -> Self {
        Self::default()
    }

    /// 시크릿 값을 등록한다. [`MIN_SECRET_LEN`] 미만(빈 문자열 포함)이면 등록을
    /// 거부하고 `false`를 돌려준다. 이미 등록된 값이면 중복 삽입 없이 `true`.
    pub fn register(&mut self, value: impl Into<String>) -> bool {
        let value = value.into();
        if value.len() < MIN_SECRET_LEN {
            return false;
        }
        if self.secrets.iter().any(|s| s == &value) {
            return true;
        }
        // 길이 내림차순 유지 — "구조체 헤더" 주석 참고. 목록이 수백 개 규모라
        // 선형 스캔으로 삽입 위치를 찾아도 등록은 보통 기동 시 1회뿐이라 비용이 없다.
        let pos = self.secrets.partition_point(|s| s.len() >= value.len());
        self.secrets.insert(pos, value);
        true
    }

    /// 등록된 시크릿 개수.
    pub fn len(&self) -> usize {
        self.secrets.len()
    }

    /// 등록된 시크릿이 하나도 없는지.
    pub fn is_empty(&self) -> bool {
        self.secrets.is_empty()
    }

    /// 문자열에서 등록된 모든 시크릿 값을 [`REDACTED_PLACEHOLDER`]로 치환한다.
    ///
    /// 등록된 시크릿 수를 n, 입력 길이를 m이라 하면 `O(n·m)`이다(각 시크릿에 대해
    /// `contains` 탐색 후 필요할 때만 `replace` 1회 — 전부 자연스러운 문자열 스캔이라
    /// 인위적인 폭발 없이 시크릿 수에 선형으로 늘어난다). 수백 개 등록 규모에서
    /// 이 정도면 응답 하나를 마스킹하는 데 충분히 빠르다 — `adversarial_*` 아래
    /// 스트레스 테스트로 고정해 둔다. 등록 규모가 수천 단위로 커지면 Aho-Corasick 같은
    /// 다중 패턴 자동자로 바꿔야 하지만, 그 규모가 아니라 지금은 새 의존성을 들이지
    /// 않는다.
    pub fn mask(&self, input: &str) -> String {
        if self.secrets.is_empty() || input.is_empty() {
            return input.to_string();
        }
        let mut out = input.to_string();
        for secret in &self.secrets {
            if out.contains(secret.as_str()) {
                out = out.replace(secret.as_str(), REDACTED_PLACEHOLDER);
            }
        }
        out
    }
}

/// [`register_from_env_names`]의 순수 함수 본체 — env를 직접 읽지 않고 `lookup`
/// 클로저로 받는다. 테스트가 실제 프로세스 환경변수를 건드리지 않고(병렬 테스트 간
/// 경합 없이) 이 로직을 검증할 수 있게 한다([`crate::config::merged::resolve_uri_pair`]와
/// 같은 패턴 — `src/config/merged.rs`의 lookup 클로저 참고).
pub fn register_looked_up<'a, I, F>(registry: &mut SecretRegistry, names: I, mut lookup: F)
where
    I: IntoIterator<Item = &'a str>,
    F: FnMut(&str) -> Option<String>,
{
    for name in names {
        if let Some(value) = lookup(name) {
            registry.register(value);
        }
    }
}

/// config가 담고 있는 시크릿 env 변수 *이름* 목록(`uri_env`/`read_uri_env`/
/// `credentials_env`)에서 실제 값을 읽어 등록한다.
///
/// config 파일에는 변수 *이름*만 있다(PRD §11, [`crate::config::secret`] 참고) — 실제
/// 시크릿 값은 이 함수가 프로세스 환경에서 읽는다. 이름이 가리키는 env가 없거나 값이
/// [`MIN_SECRET_LEN`] 미만이면 조용히 건너뛴다 — 마스킹은 2차 방어이므로 등록 실패가
/// 서버 기동을 막아서는 안 된다(설정 오류 자체는 자식 프로세스 실행 시점에 별도로
/// 걸린다).
///
/// 아직 이 함수를 호출하는 곳이 없다 — 이 태스크 시점에는 웹 서버가 config를 직접
/// 해석해 `ServeConfig`에 시크릿을 실어 두지 않는다. 잡 실행기가 자식 프로세스에 넘길
/// 시크릿 env를 확정하는 이후 태스크가 이 함수로 `SecretRegistry`를 채우는 진입점으로
/// 쓰라고 미리 만들어 둔 것이다.
pub fn register_from_env_names<'a, I>(registry: &mut SecretRegistry, names: I)
where
    I: IntoIterator<Item = &'a str>,
{
    register_looked_up(registry, names, |name| std::env::var(name).ok());
}

// ---------------------------------------------------------------------------
// API 뷰 타입 규약 — "필드 부재"를 타입 수준에서 강제한다.
// ---------------------------------------------------------------------------

/// 라우트 핸들러가 JSON으로 돌려주는 값이어야 함을 표시하는 마커 트레이트.
///
/// ## 이 트레이트 자체는 약한 방어다 — 진짜 방어는 [`crate::config::secret::Secret`]의
/// 부재
/// `ApiView`를 구현하라는 컨벤션만으로는 누구든 시크릿을 담은 `String` 필드를 넣고도
/// 구현할 수 있다(컴파일러가 "이 `String`이 시크릿인지"는 알 수 없다). 실제로 컴파일
/// 타임에 걸리는 경우는 딱 하나다 — **`Secret` *타입*으로 필드를 선언하면** 막힌다.
/// `Secret`은 [`serde::Serialize`]를 의도적으로 구현하지 않으므로, 뷰 구조체에
/// `Secret` 필드를 넣고 `#[derive(Serialize)]`를 걸면 그 자리에서 컴파일이 실패한다.
///
/// 즉 이 트레이트가 주는 실질적 보증은: **뷰 타입에 담긴 값이 하나라도 `Secret`으로
/// 감싸여 있었다면, 그 뷰는 애초에 존재할 수 없다**(컴파일이 안 되므로). 아래
/// doctest(`compile_fail`)로 이 성질을 고정한다 — `Secret`이 나중에 실수로
/// `Serialize`를 얻으면 이 doctest가 깨져 CI에서 잡힌다.
///
/// ```compile_fail
/// #[derive(serde::Serialize)]
/// struct LeakyView {
///     mongo_uri: x_backup::config::secret::Secret,
/// }
/// ```
///
/// ## 이 트레이트가 못 막는 것
/// `String`/`&str` 필드에 시크릿 값을 손으로 복붙하는 실수는 타입 시스템이 잡을 수
/// 없다 — 그건 [`SecretRegistry`]의 런타임 마스킹(2차 방어)과 코드 리뷰의 몫이다.
/// 그래서 라우트 핸들러 작성 시 규약은 두 단계다: (1) 도메인 타입(`ResolvedConfig`
/// 등, `Secret` 필드를 가진 것)을 그대로 반환하지 말고 항상 별도 뷰 구조체로 골라
/// 옮겨 담을 것, (2) 옮겨 담을 때 시크릿이 필요한 필드는 아예 뷰 구조체에 선언하지
/// 말 것.
pub trait ApiView: serde::Serialize {}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 기본 등록/치환 동작 ----

    #[test]
    fn register_accepts_value_at_min_len() {
        let mut reg = SecretRegistry::new();
        assert!(
            reg.register("a".repeat(MIN_SECRET_LEN)),
            "하한값 자체는 등록되어야 함"
        );
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn register_rejects_value_below_min_len() {
        // 팀 리드 지시의 예시(4자 이하)를 그대로 검증한다.
        let mut reg = SecretRegistry::new();
        assert!(!reg.register("abcd"), "4자짜리는 거부되어야 함");
        assert!(reg.is_empty(), "거부된 값은 등록되면 안 됨");
    }

    #[test]
    fn register_rejects_empty_value() {
        let mut reg = SecretRegistry::new();
        assert!(!reg.register(""));
    }

    #[test]
    fn register_dedupes_identical_values() {
        let mut reg = SecretRegistry::new();
        assert!(reg.register("hunter2pass"));
        assert!(
            reg.register("hunter2pass"),
            "이미 등록된 값도 성공으로 취급"
        );
        assert_eq!(reg.len(), 1, "중복 삽입되면 안 됨");
    }

    #[test]
    fn mask_is_noop_when_registry_empty() {
        let reg = SecretRegistry::new();
        assert_eq!(reg.mask("아무 로그 텍스트"), "아무 로그 텍스트");
    }

    #[test]
    fn mask_leaves_unregistered_text_untouched() {
        let mut reg = SecretRegistry::new();
        reg.register("totally-unrelated-secret-value");
        let input = "job finished: uploaded 42 files to s3://bucket/prefix";
        assert_eq!(reg.mask(input), input);
    }

    #[test]
    fn mask_redacts_exact_registered_value() {
        let mut reg = SecretRegistry::new();
        reg.register("mySup3rSecretPw");
        let input = "mongodump failed: auth error for password mySup3rSecretPw on host db1";
        let masked = reg.mask(input);
        assert!(
            !masked.contains("mySup3rSecretPw"),
            "원문이 남아 있으면 안 됨: {masked}"
        );
        assert!(masked.contains(REDACTED_PLACEHOLDER));
    }

    #[test]
    fn mask_redacts_multiple_occurrences() {
        let mut reg = SecretRegistry::new();
        reg.register("repeatedSecret1");
        let input = "repeatedSecret1 ... retry with repeatedSecret1 again";
        let masked = reg.mask(input);
        assert!(!masked.contains("repeatedSecret1"));
        assert_eq!(masked.matches(REDACTED_PLACEHOLDER).count(), 2);
    }

    #[test]
    fn mask_redacts_multiple_distinct_secrets() {
        let mut reg = SecretRegistry::new();
        reg.register("firstSecretValue");
        reg.register("secondSecretVal2");
        let input = "uri1 has firstSecretValue, uri2 has secondSecretVal2";
        let masked = reg.mask(input);
        assert!(!masked.contains("firstSecretValue"));
        assert!(!masked.contains("secondSecretVal2"));
    }

    /// 한쪽 시크릿이 다른 쪽의 부분 문자열인 경우(예: source URI와 read-replica URI가
    /// 같은 비밀번호를 공유) 긴 쪽부터 지워야 짧은 쪽의 잔여 조각이 안 남는다.
    #[test]
    fn mask_redacts_longer_secret_before_nested_shorter_one() {
        let mut reg = SecretRegistry::new();
        let short = "sharedPassw0rd!"; // 15자
        let long = format!("{short}-with-suffix-extra"); // short를 포함하는 더 긴 값
        reg.register(short);
        reg.register(long.clone());

        let input = format!("primary used {long}, replica used {short}");
        let masked = reg.mask(&input);
        assert!(
            !masked.contains(short),
            "짧은 시크릿 잔여 조각이 남으면 안 됨: {masked}"
        );
        assert!(!masked.contains(&long));
    }

    // ---- API 뷰 타입 규약 ----

    /// 위 [`ApiView`] doctest(`compile_fail`)와 짝을 이루는 대조군이다. 같은 모양이지만
    /// `Secret` 필드가 없으면 정상적으로 파생·직렬화된다는 것을 확인해, doctest의
    /// 컴파일 실패가 `Secret` 필드 때문이지 다른 문법 실수 때문이 아님을 보장한다.
    #[test]
    fn view_type_without_secret_serializes_fine() {
        #[derive(serde::Serialize)]
        struct OkView {
            profile: String,
        }
        impl ApiView for OkView {}

        let v = OkView {
            profile: "prod".to_string(),
        };
        let json = serde_json::to_string(&v).expect("시크릿 없는 뷰는 직렬화되어야 함");
        assert_eq!(json, r#"{"profile":"prod"}"#);
    }

    // ---- register_from_env_names / register_looked_up ----

    #[test]
    fn register_looked_up_reads_only_present_names() {
        use std::collections::HashMap;
        let mut env: HashMap<&str, String> = HashMap::new();
        env.insert("MONGO_URI_ENV", "value-from-env-long-enough".to_string());
        // "S3_CREDS_ENV"는 의도적으로 미설정 — 없는 이름은 조용히 건너뛰어야 한다.

        let mut reg = SecretRegistry::new();
        register_looked_up(&mut reg, ["MONGO_URI_ENV", "S3_CREDS_ENV"], |name| {
            env.get(name).cloned()
        });

        assert_eq!(reg.len(), 1, "설정된 이름만 등록되어야 함");
        assert!(reg
            .mask("value-from-env-long-enough")
            .contains(REDACTED_PLACEHOLDER));
    }

    #[test]
    fn register_looked_up_skips_values_below_min_len() {
        let mut env = std::collections::HashMap::new();
        env.insert("SHORT_ENV", "sh".to_string()); // 하한 미달
        let mut reg = SecretRegistry::new();
        register_looked_up(&mut reg, ["SHORT_ENV"], |name| env.get(name).cloned());
        assert!(
            reg.is_empty(),
            "하한 미달 값은 env 경유 등록에서도 거부되어야 함"
        );
    }

    // ---- 적대적 입력: "잡는 척하지 않는다" — 경계를 명시한다 ----

    /// 모든 바이트를 퍼센트 인코딩한다(테스트 전용 — 실제 URL 인코딩은 예약 문자만
    /// 바꾸지만, 여기서는 "원문 바이트열이 결과에 그대로 남지 않는다"를 보장하는 게
    /// 목적이라 전부 바꾼다).
    fn percent_encode_all(s: &str) -> String {
        s.bytes().map(|b| format!("%{b:02X}")).collect()
    }

    #[test]
    fn adversarial_url_encoded_secret_is_not_caught() {
        let secret = "plainTextSecretValue";
        let mut reg = SecretRegistry::new();
        reg.register(secret);

        let encoded = percent_encode_all(secret);
        let input = format!("redirected to /login?token={encoded}");
        let masked = reg.mask(&input);
        // 정직한 한계: 인코딩된 형태는 원문 바이트열과 다르므로 못 잡는다.
        assert_eq!(
            masked, input,
            "퍼센트 인코딩된 시크릿은 이 레지스트리가 못 잡는다(알려진 한계)"
        );
    }

    #[test]
    fn adversarial_hex_encoded_secret_is_not_caught() {
        // base64 크레이트는 이 crate의 의존성에 없어(Cargo.toml 변경은 이 태스크 범위
        // 밖) 이미 트리에 있는 hex 크레이트로 같은 성질(재인코딩은 못 잡음)을 보인다.
        let secret = "anotherPlainSecret99";
        let mut reg = SecretRegistry::new();
        reg.register(secret);

        let encoded = hex::encode(secret.as_bytes());
        let input = format!("child stderr dumped hex: {encoded}");
        let masked = reg.mask(&input);
        assert_eq!(
            masked, input,
            "hex/base64 등 재인코딩된 시크릿은 못 잡는다(알려진 한계)"
        );
    }

    #[test]
    fn adversarial_partial_substring_is_not_caught() {
        let secret = "fullSecretValueTwelve";
        let mut reg = SecretRegistry::new();
        reg.register(secret);

        let partial = &secret[..secret.len() / 2];
        let input = format!("truncated in log: {partial}");
        let masked = reg.mask(&input);
        // 등록된 값과 "정확히 일치하는 전체"만 치환 대상이다 — 부분 노출은 설계상
        // 애초에 다루지 않는다(짧은 임의 문자열이 "어느 시크릿의 일부"인지는 알 방법이
        // 없다).
        assert_eq!(masked, input);
    }

    #[test]
    fn adversarial_case_changed_secret_is_not_caught() {
        let secret = "CaseSensitiveSecret1";
        let mut reg = SecretRegistry::new();
        reg.register(secret);

        let input = format!("logged as: {}", secret.to_lowercase());
        let masked = reg.mask(&input);
        assert_eq!(
            masked, input,
            "대소문자가 바뀌면 바이트 단위 일치가 깨져 못 잡는다"
        );
    }

    /// 등록된 시크릿이 수백 개 규모여도 마스킹이 시크릿 수에 선형으로만 늘어나는지
    /// 본다(알고리즘 폭발 감지가 목적이지 정밀 성능 측정이 목적이 아니다 — 그래서
    /// 임계값을 넉넉히 잡는다: 실제 실행은 이보다 훨씬 빠르다).
    #[test]
    fn mask_scales_with_secret_count_without_blowing_up() {
        let mut reg = SecretRegistry::new();
        for i in 0..600 {
            reg.register(format!("stressTestSecretValue{i:04}ZZ"));
        }
        assert_eq!(reg.len(), 600);

        // 시크릿이 하나도 등장하지 않는 수십 KB짜리 텍스트 — 최악에 가까운 경로
        // (모든 등록 값에 대해 `contains` 탐색까지는 해야 하고 `replace`는 안 탐).
        let haystack = "the quick brown fox jumps over the lazy dog. ".repeat(1000);

        let started = std::time::Instant::now();
        let masked = reg.mask(&haystack);
        let elapsed = started.elapsed();

        assert_eq!(masked, haystack, "매칭될 게 없으면 입력이 그대로 나와야 함");
        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "600개 시크릿 마스킹이 비정상적으로 오래 걸림(알고리즘 폭발 의심): {elapsed:?}"
        );
    }
}
