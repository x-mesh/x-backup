//! 웹 운영 콘솔(`x-backup serve`) — 상주 HTTP 서버 계층.
//!
//! ## 최상위 불변식 — 웹은 도메인 로직을 재구현하지 않는다
//! 이 계층은 [`engine`](crate::engine)·[`storage`](crate::storage)·[`crypto`](crate::crypto)를
//! **직접 호출하지 않는다.** 작업 요청이 오면 `x-backup <cmd> --json` 자식 프로세스를 띄우고
//! 그 출력을 중계한다. 이유:
//!
//! 1. `src/cli/handlers/*.rs`는 계산과 stdout 출력이 섞여 있고 구조화 반환값이 없다. 웹이
//!    직접 부르려면 11개 핸들러를 전부 갈라야 한다.
//! 2. 자식으로 띄우면 파일 락(exit 5)·종료 코드 0~5·생명주기 훅이 **자동으로** 그대로
//!    적용된다. cron으로 띄운 CLI와 웹이 같은 락을 공유하므로 이중 실행도 구조적으로 막힌다.
//! 3. 도메인 경로가 하나뿐이라 웹과 CLI가 다르게 동작할 수 없다(동작 드리프트 불가).
//!
//! 이 불변식을 깨는 순간(웹에서 engine을 직접 부르는 순간) 위 세 가지가 전부 무너진다.
//!
//! ## 모듈 구성
//! - [`server`]: 라우터 조립·리스너 바인딩·graceful shutdown.
//! - [`view`]: maud SSR 마크업과 컴파일 타임 임베드 에셋.
//! - [`routes`]: 화면별 핸들러(한 화면 = 한 모듈).
//! - [`reattach`]: 기동 시 고아 잡 재부착 스캔([`job::lifecycle`]과 [`state::jobs`]의 이음매).
//!
//! 이 파일(`mod.rs`)은 **"무엇을 서빙할지"**(입력 검증 — bind 주소·state 디렉터리)를,
//! [`server`]는 **"어떻게 서빙할지"**를 맡는다.
//!
//! ## 바인딩을 왜 이렇게 깐깐하게 검사하는가
//! 이 서버는 age 복호화 개인키와 프로덕션 DB 접속 정보를 상시 보유한다 — 열린 포트 하나가
//! 전 백업 복호화 + 프로덕션 덮어쓰기 경로다. 그래서 [`resolve_bind`]는 fail-closed다:
//!
//! - 기본은 [`DEFAULT_BIND`](`127.0.0.1:8787`) — 루프백.
//! - 루프백이 아닌 주소는 `--allow-remote` 없이는 **거부**한다(조용히 전 인터페이스에
//!   노출되는 사고를 문법 수준에서 막는다).
//! - 호스트명은 받지 않는다(IP 리터럴만). 이름 해석은 환경에 따라 결과가 달라지고,
//!   이름 하나가 루프백 밖 주소로 풀리면 위 가드가 무력화된다.
//! - 특권 포트(<1024)와 포트 0은 거부한다 — 백업 콘솔이 root로 돌 이유가 없고, 임의 포트
//!   배정은 "어디에 떴는지 모르는 서버"를 만든다.
//!
//! TLS는 이 바이너리가 종단하지 않는다 — 인증서 갱신·HSTS·리다이렉트는 앞단 리버스
//! 프록시(nginx/caddy)의 책임이다(PRD Q2).

pub mod audit;
pub mod auth;
pub mod cache;
pub mod config_write;
pub mod guard;
pub mod job;
pub mod jsonguard;
pub mod mask;
pub mod reattach;
pub mod routes;
pub mod schedule;
pub mod server;
pub mod sse;
pub mod state;
pub mod view;

use std::ffi::OsStr;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::cli::args::ServeArgs;
use crate::error::{Result, XBackupError};
use crate::i18n::Lang;

/// `--bind` 기본값 — 루프백 고정. 이 문자열이 clap `--help`에도 그대로 노출된다.
pub const DEFAULT_BIND: &str = "127.0.0.1:8787";

/// 바인딩을 허용하는 최소 포트. 이보다 낮으면 특권(root) 권한이 필요하다.
const MIN_UNPRIVILEGED_PORT: u16 = 1024;

/// IP 리터럴 대신 유일하게 허용하는 호스트 별칭. DNS를 거치지 않고 127.0.0.1로 고정 해석한다
/// (`/etc/hosts`가 localhost를 엉뚱한 주소로 매핑해도 루프백 가드가 유지된다).
const LOCALHOST_ALIAS: &str = "localhost";

/// state 디렉터리의 마지막 경로 조각(XDG 하위 이름).
const STATE_DIR_NAME: &str = "x-backup";

/// `XDG_STATE_HOME` 미설정 시 `$HOME` 아래에 쓰는 상대 경로(XDG 기본값).
const STATE_DIR_HOME_FALLBACK: &str = ".local/state";

/// 서버가 기동할 때 확정되는 실행 컨텍스트.
///
/// 검증이 끝난 값만 담는다 — 문자열 `--bind`는 [`SocketAddr`]로, 상대 경로는 절대 경로로
/// 이미 접혀 있다. 라우트 핸들러는 이 구조체를 axum state로 공유받는다.
///
/// ## 왜 감사 로그·잡 러너·라이브 상태가 여기 있는가
/// 셋 다 **기동 시점에 한 번 만들어지고 요청마다 바뀌지 않는** 값이다.
///
/// [`audit`](ServeConfig::audit)와 [`jobs`](ServeConfig::jobs)는 특히 **함께** 있어야 한다:
/// 파괴적 작업은 감사 게이트로 receipt를 받아 그것을 잡 러너에 넘겨야 실행된다
/// ([`job::JobRunner::spawn_destructive`]). 둘을 서로 다른 경로로 흘리면 "감사 로그는 있는데
/// 러너가 없는" 핸들러를 쓸 수 있게 되고, 그러면 규약이 코드 배치에 의존하게 된다. 같은
/// state에 두면 파괴적 작업을 다루는 핸들러가 게이트를 건너뛸 이유가 애초에 없다.
///
/// [`live`](ServeConfig::live)는 다른 이유로 여기 있다 — 그 상태(잡별 SSE 허브·취소 진행
/// 표시)의 **수명이 서버와 같기 때문**이다. 프로세스 전역 static이었을 때의 대가와 걷어낸
/// 근거는 [`state::live`] 헤더에 있다.
///
/// ## 왜 인증 상태는 여기 **없는가**
/// [`auth::AuthState`]는 이 구조체의 필드가 아니다. 라우터가 그것을 두 갈래로 따로
/// 흘린다: 미들웨어에는 `middleware::from_fn_with_state`(그 계층만의 독립된 state),
/// 로그인 핸들러에는 `Extension<Arc<AuthState>>`([`server::router`] 참조).
///
/// 근거는 위 audit·jobs와 정반대다. 저 둘은 **함께 쓰여야** 해서 묶었는데, 인증은
/// **아무 핸들러도 직접 쓰지 않아야** 한다 — 인증은 라우터 계층이 강제하는 관문이고,
/// 화면 코드가 `ctx.auth`를 꺼낼 수 있으면 "이 핸들러만 직접 검사한다" 같은 우회 경로가
/// 생긴다. 필드로 두지 않는 것이 그 유혹을 타입 수준에서 없앤다.
///
/// ## `Arc`로 감싸는 이유
/// [`audit::AuditLog`]는 내부에 append 직렬화용 뮤텍스를 들고 있어 복제되면 그 직렬화가
/// 깨지고, [`job::JobRunner`]는 시크릿을 들고 있어 사본을 늘리지 않아야 하며,
/// [`state::live::LiveJobs`]는 모든 요청이 **같은** 레지스트리를 봐야 한다. 이 구조체
/// 자체는 `Clone`이어야 하므로(axum state) 공유 소유권으로 감싼다.
#[derive(Debug, Clone)]
pub struct ServeConfig {
    /// 검증된 바인딩 주소.
    pub bind: SocketAddr,
    /// 잡 이력·감사 로그·스케줄 정의가 쌓이는 디렉터리.
    pub state_dir: PathBuf,
    /// 자식 프로세스에 `--config`로 넘길 config 경로(미지정이면 env 주입만 유효).
    pub config_path: Option<PathBuf>,
    /// 설명 문구 언어(라벨은 항상 영문 — [`crate::i18n`] 규약).
    pub lang: Lang,
    /// append-only 감사 로그 — 파괴적 작업은 이것의 게이트를 통과해야 실행된다.
    pub audit: Arc<audit::AuditLog>,
    /// 잡 러너 — `x-backup <cmd> --json` 자식 프로세스를 띄운다.
    pub jobs: Arc<job::JobRunner>,
    /// 이 서버 인스턴스의 라이브 잡 상태 — SSE 허브와 취소 진행 표시.
    ///
    /// 프로세스 전역 static이 아니라 여기 있는 이유는 [`state::live`] 헤더에 있다(요약:
    /// 이 상태의 수명은 서버와 같고, 전역이면 테스트 격리와 다중 인스턴스 선택지를 잃는다).
    pub live: Arc<state::live::LiveJobs>,
    /// 라이브 모니터 — `status --watch` 자식 하나와 그 뷰어들.
    ///
    /// [`live`](ServeConfig::live)와 같은 이유로 여기 있다: 자식과 뷰어 수의 수명이 서버와
    /// 같다. 전역이면 테스트가 서로의 뷰어 수를 보고, 한 테스트가 띄운 자식이 다른
    /// 테스트에서 "이미 돌고 있다"로 읽힌다.
    pub monitor: Arc<state::monitor::LiveMonitor>,
    /// 파괴적 작업 확인 가드 — 발급된 일회용 확인 토큰을 들고 있다.
    ///
    /// [`live`](ServeConfig::live)와 같은 이유로 여기 있다: 미제출 확인 토큰의 수명이 서버와
    /// 같고, 프로세스 전역이면 테스트가 서로의 토큰을 본다. `prune`(t23)·`migrate`(t24)·
    /// `restore`(t25) 세 화면이 이 하나를 공유한다.
    pub guard: Arc<guard::DestructiveGuard>,
}

impl ServeConfig {
    /// **테스트 전용** 컨텍스트 — 임시 디렉터리에 감사 로그를 열고 잡 러너를 만든다.
    ///
    /// 프로덕션 코드에서 부르지 말 것. 부르면 감사 로그가 `$TMPDIR` 아래에 열려
    /// 운영자가 지정한 state 디렉터리 밖에 기록이 쌓인다.
    ///
    /// ## 왜 `#[cfg(test)]`가 아니라 `#[doc(hidden)] pub`인가
    /// 같은 크레이트 안의 단위 테스트만 있으면 `#[cfg(test)]`가 맞다([`auth::AuthState`]의
    /// `for_test`가 그렇다). 하지만 `tests/web_secret_leak.rs`는 **별도 크레이트**이고,
    /// `cfg(test)` 항목은 그쪽에서 보이지 않는다. 보이지 않으면 그 테스트는 이 구조체를
    /// 손으로 조립해야 하고, 그러면 이 구조체에 필드가 하나 늘 때마다 **시크릿 누출
    /// 회귀 테스트가 깨진다.**
    ///
    /// 그게 왜 나쁜가: 깨진 보안 테스트는 "컴파일되는 아무 값"으로 메워지기 쉽다. 그
    /// 순간부터 그 테스트는 실제로 서버가 쓰는 것과 다른 컨텍스트를 검사하면서 통과한다 —
    /// 테스트가 죽었는데 초록색으로 보이는 상태다. 공개 생성자 하나로 그 낙수를 없애는
    /// 편이, 이름과 doc으로만 막히는 오용 위험보다 낫다고 판단했다.
    ///
    /// 오용은 조용하지도 않다 — 기동 배너가 `state:` 줄에 이 임시 경로를 그대로 찍는다
    /// ([`server`]의 `print_banner`).
    ///
    /// state 디렉터리는 호출마다 다른 경로를 쓴다(pid + 호출 순번). 병렬 테스트가 같은
    /// 감사 로그 파일에 섞여 쓰지 않게 하기 위함이다. 정리하지 않는다 — 임시 디렉터리에
    /// 빈 파일 하나가 남을 뿐이고, `tempfile::TempDir`를 들고 있으면 반환 타입에 수명이
    /// 붙어 호출부가 번거로워진다.
    #[doc(hidden)]
    pub fn for_test() -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);

        let dir = std::env::temp_dir().join(format!(
            "x-backup-web-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("테스트 state 디렉터리 생성 실패");
        Self {
            bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8787),
            state_dir: dir.clone(),
            config_path: None,
            lang: Lang::En,
            audit: Arc::new(audit::AuditLog::open(&dir).expect("테스트 감사 로그 열기 실패")),
            jobs: Arc::new(
                job::JobRunner::new(None, Lang::En, job::JobSecrets::new())
                    .expect("테스트 잡 러너 생성 실패"),
            ),
            live: Arc::new(state::live::LiveJobs::new()),
            monitor: Arc::new(state::monitor::LiveMonitor::new()),
            guard: Arc::new(guard::DestructiveGuard::new()),
        }
    }
}

/// `serve` 핸들러 진입점 — 다른 서브커맨드와 같은 `handle(config, lang, args)` 형태.
///
/// 검증(bind·state-dir)을 먼저 전부 끝내고 나서 포트를 잡는다. 잘못된 설정으로 반쯤 뜬
/// 서버를 만들지 않기 위함이다.
///
/// ## 감사 로그를 열지 못하면 기동하지 않는다
/// [`audit::AuditLog::open`] 실패는 그대로 전파해 `serve`를 중단시킨다. 근거:
///
/// 1. 감사 로그를 못 쓰면 **파괴적 작업은 어차피 전부 거부된다**(게이트가 append 성공을
///    요구하므로). 그런 서버는 "복구 버튼이 죽은 콘솔"이고, 그 사실이 드러나는 순간은
///    운영자가 장애 대응 중에 복구를 누른 순간이다 — 가장 나쁜 타이밍이다.
/// 2. 기동 시점에 끊으면 운영자는 이미 터미널을 보고 있다. 권한·디스크를 그 자리에서
///    고칠 수 있다.
///
/// 같은 fail-closed 판단을 이 파일의 바인딩 검증과 [`server::serve`]의 인증 검사가
/// 공유한다 — "반쯤 동작하는 서버를 만들지 않는다".
///
/// 잡 러너 생성 실패(`current_exe()` 확인 불가)도 같은 이유로 기동을 막는다 — 자기
/// 바이너리 경로를 모르면 어떤 잡도 띄울 수 없다.
pub async fn handle(
    config_path: Option<PathBuf>,
    lang_flag: Option<Lang>,
    args: ServeArgs,
) -> Result<()> {
    // 출력 설명 언어 결정(라벨은 항상 영문, 설명만 ko/en 토글).
    let config_toml = config_path
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok());
    let lang = crate::i18n::resolve_from_toml(lang_flag, config_toml.as_deref());

    let bind = resolve_bind(&args.bind, args.allow_remote)?;
    let state_dir = resolve_state_dir(args.state_dir)?;
    ensure_state_dir(&state_dir)?;

    // 감사 로그는 state 디렉터리가 준비된 **직후**, 포트를 잡기 전에 연다.
    let audit = Arc::new(audit::AuditLog::open(&state_dir)?);

    // 시크릿을 프로세스 환경에서 잡 러너 메모리로 옮긴다 — 리스너를 열기 전, 잡이
    // 하나도 돌지 않는 이 시점에만 한다([`job`] 모듈 헤더의 스레드 안전성 주의).
    //
    // config 파싱 실패는 기동을 막지 않는다(시크릿 이름을 못 읽을 뿐이고, 그 판정은
    // `doctor`의 일이다 — [`job::JobSecrets::load`] doc 참조). 경고만 남긴다.
    let parsed_config = config_toml.as_deref().and_then(|toml| {
        match crate::config::file::Config::from_toml_str(toml) {
            Ok(cfg) => Some(cfg),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "config를 해석할 수 없어 시크릿 env 이름을 모으지 못했습니다 — \
                     잡이 설정 오류로 끊길 수 있습니다(`doctor`로 확인하세요)"
                );
                None
            }
        }
    });
    let secrets = job::JobSecrets::load(parsed_config.as_ref());
    let jobs = Arc::new(job::JobRunner::new(config_path.clone(), lang, secrets)?);

    server::serve(ServeConfig {
        bind,
        state_dir,
        config_path,
        lang,
        audit,
        jobs,
        live: Arc::new(state::live::LiveJobs::new()),
        monitor: Arc::new(state::monitor::LiveMonitor::new()),
        guard: Arc::new(guard::DestructiveGuard::new()),
    })
    .await
}

/// `--bind` 문자열을 검증된 [`SocketAddr`]로 접고, 루프백 가드를 적용한다.
///
/// `allow_remote`(`--allow-remote`)가 false면 루프백이 아닌 주소를 거부한다. 모듈 헤더의
/// "바인딩을 왜 이렇게 깐깐하게 검사하는가" 참조.
pub fn resolve_bind(input: &str, allow_remote: bool) -> Result<SocketAddr> {
    let addr = parse_bind(input)?;
    if !allow_remote && !addr.ip().is_loopback() {
        return Err(XBackupError::Usage(format!(
            "--bind {addr}는 루프백 주소가 아닙니다 — 웹 콘솔은 복호화 개인키와 프로덕션 접속 \
             정보를 상시 보유하므로 기본적으로 {DEFAULT_BIND}에만 바인딩합니다. 사내망/VPN 안이고 \
             앞단에 TLS 리버스 프록시를 두었다면 --allow-remote를 함께 지정하세요."
        )));
    }
    Ok(addr)
}

/// `--bind` 문자열의 문법·포트 정책만 검사한다(루프백 가드는 [`resolve_bind`]가 얹는다).
///
/// 호스트명 해석(DNS)을 하지 않는다 — 같은 문자열이 환경에 따라 다른 주소로 풀리면
/// "무엇에 바인딩되는가"를 테스트로 고정할 수 없고, 루프백 가드도 우회된다.
pub fn parse_bind(input: &str) -> Result<SocketAddr> {
    let spec = input.trim();
    if spec.is_empty() {
        return Err(XBackupError::Usage(format!(
            "--bind 값이 비어 있습니다 — `<IP>:<PORT>` 형태로 지정하세요(기본 {DEFAULT_BIND})."
        )));
    }
    let (host, port_raw) = split_host_port(spec)?;
    // 포트를 먼저 본다 — 사용자가 가장 자주 틀리는 쪽이고, 호스트 메시지보다 구체적이다.
    let port = parse_port(port_raw, spec)?;
    let ip = parse_host(host, spec)?;
    Ok(SocketAddr::new(ip, port))
}

/// `host:port`를 나눈다. IPv6는 `[::1]:8787`처럼 대괄호 형태만 받는다.
fn split_host_port(spec: &str) -> Result<(&str, &str)> {
    if let Some(rest) = spec.strip_prefix('[') {
        let (host, tail) = rest.split_once(']').ok_or_else(|| {
            XBackupError::Usage(format!(
                "--bind '{spec}'의 대괄호가 닫히지 않았습니다 — IPv6는 `[::1]:8787` 형태로 씁니다."
            ))
        })?;
        let port = tail.strip_prefix(':').ok_or_else(|| {
            XBackupError::Usage(format!(
                "--bind '{spec}'에 포트가 없습니다 — `[{host}]:<PORT>` 형태로 지정하세요."
            ))
        })?;
        return Ok((host, port));
    }
    match spec.rsplit_once(':') {
        // 대괄호 없는 IPv6(`::1:8787`)는 마지막 콜론이 포트 구분자인지 주소 일부인지
        // 사람도 기계도 확정할 수 없다 — 추측하지 않고 명시를 요구한다.
        Some((host, _)) if host.contains(':') => Err(XBackupError::Usage(format!(
            "--bind '{spec}'는 포트 경계가 모호합니다 — IPv6 주소는 대괄호로 감싸세요(예: `[::1]:8787`)."
        ))),
        Some((host, port)) => Ok((host, port)),
        None => Err(XBackupError::Usage(format!(
            "--bind '{spec}'에 포트가 없습니다 — `{spec}:<PORT>` 형태로 지정하세요(기본 {DEFAULT_BIND})."
        ))),
    }
}

/// 포트 문자열을 정책까지 함께 검사한다(범위·0·특권 포트).
fn parse_port(raw: &str, spec: &str) -> Result<u16> {
    let port: u16 = raw.parse().map_err(|_| {
        XBackupError::Usage(format!(
            "--bind '{spec}'의 포트 '{raw}'를 해석할 수 없습니다 — \
             {MIN_UNPRIVILEGED_PORT}~65535 범위의 정수여야 합니다."
        ))
    })?;
    if port == 0 {
        return Err(XBackupError::Usage(format!(
            "--bind '{spec}'의 포트 0은 OS가 임의 포트를 배정한다는 뜻입니다 — \
             콘솔 주소를 미리 알 수 없으므로 거부합니다. 포트를 명시하세요(기본 {DEFAULT_BIND})."
        )));
    }
    if port < MIN_UNPRIVILEGED_PORT {
        return Err(XBackupError::Usage(format!(
            "--bind '{spec}'의 포트 {port}는 특권 포트입니다(<{MIN_UNPRIVILEGED_PORT}) — \
             root 권한을 요구하며, 백업 콘솔을 root로 실행할 이유가 없습니다. \
             {MIN_UNPRIVILEGED_PORT} 이상을 쓰고 필요하면 리버스 프록시로 앞단 포트를 맡기세요."
        )));
    }
    Ok(port)
}

/// 호스트 조각을 IP로 접는다. `localhost`만 별칭으로 허용하고 그 외 이름은 거부한다.
fn parse_host(raw: &str, spec: &str) -> Result<IpAddr> {
    if raw.is_empty() {
        return Err(XBackupError::Usage(format!(
            "--bind '{spec}'에 호스트가 없습니다 — 어디에 바인딩할지 IP를 명시하세요\
             (전 인터페이스는 `0.0.0.0:<PORT> --allow-remote`)."
        )));
    }
    if raw.eq_ignore_ascii_case(LOCALHOST_ALIAS) {
        return Ok(IpAddr::V4(Ipv4Addr::LOCALHOST));
    }
    raw.parse::<IpAddr>().map_err(|_| {
        XBackupError::Usage(format!(
            "--bind '{spec}'의 호스트 '{raw}'가 IP 리터럴이 아닙니다 — 호스트명 대신 IP를 \
             쓰세요(예: `127.0.0.1:8787`, `[::1]:8787`). 이름 해석은 환경에 따라 결과가 달라지고, \
             이름이 루프백 밖 주소로 풀리면 바인딩 가드가 무력화됩니다."
        ))
    })
}

/// state 디렉터리를 정한다 — `--state-dir` > `$XDG_STATE_HOME/x-backup` > `$HOME/.local/state/x-backup`.
pub fn resolve_state_dir(explicit: Option<PathBuf>) -> Result<PathBuf> {
    let xdg = std::env::var_os("XDG_STATE_HOME");
    let home = std::env::var_os("HOME");
    state_dir_from(explicit, xdg.as_deref(), home.as_deref())
}

/// [`resolve_state_dir`]의 순수 함수 본체 — env를 인자로 받아 테스트에서 프로세스 환경을
/// 건드리지 않고 두 경로(XDG 설정/미설정)를 검증할 수 있게 한다(`config::env`와 같은 패턴).
fn state_dir_from(
    explicit: Option<PathBuf>,
    xdg_state_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Result<PathBuf> {
    if let Some(dir) = explicit {
        return Ok(dir);
    }
    // XDG Base Directory 스펙: 값이 비었거나 절대 경로가 아니면 "미설정"으로 취급한다.
    if let Some(xdg) = xdg_state_home.filter(|v| !v.is_empty()) {
        let base = Path::new(xdg);
        if base.is_absolute() {
            return Ok(base.join(STATE_DIR_NAME));
        }
    }
    let home = home.filter(|v| !v.is_empty()).ok_or_else(|| {
        XBackupError::Config(
            "state 디렉터리를 정할 수 없습니다 — XDG_STATE_HOME도 HOME도 설정되어 있지 \
             않습니다. --state-dir로 직접 지정하세요."
                .to_string(),
        )
    })?;
    Ok(Path::new(home)
        .join(STATE_DIR_HOME_FALLBACK)
        .join(STATE_DIR_NAME))
}

/// state 디렉터리를 만들고(없으면) 소유자 전용 권한을 보장한다.
///
/// 기동 시점에 만드는 이유: 이 디렉터리에는 감사 로그(append-only)·잡 로그가 쌓이므로,
/// 첫 백업을 누른 순간이 아니라 **부팅 때** 못 쓰는 경로를 발견해야 한다(fail-closed).
///
/// 새로 만들 때만 0700으로 조인다 — 잡 로그에는 마스킹을 통과한 뒤에도 네임스페이스·경로 같은
/// 운영 정보가 남고, 감사 로그는 다른 사용자가 읽을 것이 아니다. 이미 있는 디렉터리의 권한은
/// 사용자의 선택이므로 건드리지 않는다.
fn ensure_state_dir(dir: &Path) -> Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(dir).map_err(|e| {
        XBackupError::Config(format!(
            "state 디렉터리를 만들 수 없습니다: {} — {e}. --state-dir로 쓰기 가능한 경로를 지정하세요.",
            dir.display()
        ))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // 실패는 치명적이지 않다(파일시스템이 권한을 지원하지 않을 수 있다) — 경고만 남긴다.
        if let Err(e) = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)) {
            tracing::warn!(dir = %dir.display(), error = %e, "state 디렉터리 권한을 0700으로 조이지 못했습니다");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 기본 bind 상수는 루프백 + 8787이며, 그대로 파싱된다.
    #[test]
    fn default_bind_is_loopback_8787() {
        let addr = resolve_bind(DEFAULT_BIND, false).expect("기본값은 항상 유효해야 함");
        assert_eq!(addr, SocketAddr::from(([127, 0, 0, 1], 8787)));
        assert!(addr.ip().is_loopback(), "기본 바인딩은 루프백이어야 함");
    }

    /// clap 기본값(= `--bind` 미지정)이 `127.0.0.1:8787`로 접힌다 — 인자 표면과 해석이
    /// 같은 상수를 공유하는지 확인한다.
    #[test]
    fn unspecified_bind_resolves_to_default() {
        use clap::Parser;
        let cli = crate::cli::Cli::try_parse_from(["x-backup", "serve"]).expect("파싱 실패");
        let crate::cli::Command::Serve(args) = cli.command else {
            panic!("serve가 아님");
        };
        assert_eq!(args.bind, DEFAULT_BIND);
        assert!(!args.allow_remote, "--allow-remote 기본은 꺼짐");
        assert_eq!(args.state_dir, None);
        assert_eq!(
            resolve_bind(&args.bind, args.allow_remote).unwrap(),
            SocketAddr::from(([127, 0, 0, 1], 8787))
        );
    }

    /// 잘못된 `--bind` 문자열은 전부 Usage(exit 2)로 거부되고, 메시지가 원인을 지목한다.
    ///
    /// 각 케이스의 `expect_hint`는 "사용자가 무엇을 고쳐야 하는지"가 메시지에 실제로
    /// 들어 있는지 보는 것이다 — 범용 문구로 뭉개지는 회귀를 막는다.
    #[test]
    fn invalid_bind_strings_are_rejected_with_reason() {
        let cases: &[(&str, &str)] = &[
            // 1. 빈 문자열.
            ("", "비어 있습니다"),
            // 2. 포트 없음.
            ("127.0.0.1", "포트가 없습니다"),
            // 3. 포트 범위 초과(u16 밖).
            ("localhost:99999", "65535"),
            // 4. 포트 0 — OS 임의 배정.
            ("[::]:0", "임의 포트"),
            // 5. 특권 포트.
            ("0.0.0.0:22", "특권 포트"),
            // 6. 루프백 아님(가드 플래그 없음) — IPv4 전 인터페이스.
            ("0.0.0.0:8787", "--allow-remote"),
            // 7. 루프백 아님 — IPv6 전 인터페이스.
            ("[::]:8787", "--allow-remote"),
            // 8. 호스트명(IP 리터럴 아님).
            ("console.internal:8787", "IP 리터럴이"),
            // 9. 잘못된 IPv4 옥텟.
            ("256.1.1.1:8787", "IP 리터럴이"),
            // 10. 대괄호 없는 IPv6 — 포트 경계 모호.
            ("::1:8787", "대괄호로 감싸세요"),
            // 11. 닫히지 않은 대괄호.
            ("[::1:8787", "대괄호가 닫히지"),
            // 12. 호스트 없음.
            (":8787", "호스트가 없습니다"),
        ];
        for (input, expect_hint) in cases {
            let err = resolve_bind(input, false).expect_err(&format!("'{input}'는 거부되어야 함"));
            assert_eq!(
                err.exit_code(),
                crate::error::exit_codes::USAGE,
                "'{input}'는 사용법 오류(exit 2)여야 함: {err}"
            );
            let msg = err.to_string();
            assert!(
                msg.contains(expect_hint),
                "'{input}' 메시지에 '{expect_hint}'가 없다: {msg}"
            );
        }
    }

    /// 유효한 루프백 표기는 모두 통과한다(IPv4·IPv6·localhost 별칭).
    #[test]
    fn loopback_forms_are_accepted() {
        assert_eq!(
            resolve_bind("127.0.0.1:8787", false).unwrap(),
            SocketAddr::from(([127, 0, 0, 1], 8787))
        );
        assert_eq!(
            resolve_bind("[::1]:9000", false).unwrap().port(),
            9000,
            "IPv6 루프백"
        );
        assert!(resolve_bind("[::1]:9000", false)
            .unwrap()
            .ip()
            .is_loopback());
        // localhost는 DNS를 거치지 않고 127.0.0.1로 고정 해석한다.
        assert_eq!(
            resolve_bind("LocalHost:8787", false).unwrap(),
            SocketAddr::from(([127, 0, 0, 1], 8787)),
            "localhost 별칭은 대소문자 무시 + 루프백 고정"
        );
        // 앞뒤 공백은 셸 인용 사고를 흔히 만든다 — 잘라서 받는다.
        assert_eq!(
            resolve_bind("  127.0.0.1:8787 ", false).unwrap().port(),
            8787
        );
    }

    /// `--allow-remote`를 주면 루프백 밖 주소가 허용된다(문법·포트 정책은 그대로 적용).
    #[test]
    fn allow_remote_opens_non_loopback_only() {
        assert_eq!(
            resolve_bind("0.0.0.0:8787", true).unwrap(),
            SocketAddr::from(([0, 0, 0, 0], 8787))
        );
        assert!(resolve_bind("[::]:8787", true).is_ok(), "IPv6 와일드카드");
        // 가드 해제는 루프백 판정에만 적용된다 — 특권 포트·포트 0은 여전히 거부.
        assert!(
            resolve_bind("0.0.0.0:22", true).is_err(),
            "--allow-remote가 특권 포트까지 열어주면 안 됨"
        );
        assert!(
            resolve_bind("0.0.0.0:0", true).is_err(),
            "포트 0은 여전히 거부"
        );
    }

    /// `--state-dir`이 있으면 env를 보지 않는다.
    #[test]
    fn explicit_state_dir_wins() {
        let dir = state_dir_from(
            Some(PathBuf::from("/srv/xb-state")),
            Some(OsStr::new("/xdg")),
            Some(OsStr::new("/home/op")),
        )
        .unwrap();
        assert_eq!(dir, PathBuf::from("/srv/xb-state"));
    }

    /// `XDG_STATE_HOME`이 설정되면 그 아래 `x-backup`을 쓴다.
    #[test]
    fn xdg_state_home_is_used_when_set() {
        let dir = state_dir_from(
            None,
            Some(OsStr::new("/home/op/.local/state")),
            Some(OsStr::new("/home/op")),
        )
        .unwrap();
        assert_eq!(dir, PathBuf::from("/home/op/.local/state/x-backup"));
    }

    /// `XDG_STATE_HOME` 미설정이면 `$HOME/.local/state/x-backup`으로 떨어진다.
    #[test]
    fn home_fallback_when_xdg_unset() {
        let dir = state_dir_from(None, None, Some(OsStr::new("/home/op"))).unwrap();
        assert_eq!(dir, PathBuf::from("/home/op/.local/state/x-backup"));
    }

    /// 빈 값·상대 경로 `XDG_STATE_HOME`은 XDG 스펙대로 "미설정" 취급한다.
    /// (상대 경로를 그대로 쓰면 state가 현재 작업 디렉터리에 흩어진다.)
    #[test]
    fn empty_or_relative_xdg_falls_back_to_home() {
        let empty =
            state_dir_from(None, Some(OsStr::new("")), Some(OsStr::new("/home/op"))).unwrap();
        assert_eq!(empty, PathBuf::from("/home/op/.local/state/x-backup"));
        let relative = state_dir_from(
            None,
            Some(OsStr::new("state")),
            Some(OsStr::new("/home/op")),
        )
        .unwrap();
        assert_eq!(relative, PathBuf::from("/home/op/.local/state/x-backup"));
    }

    /// XDG도 HOME도 없으면 설정 오류(exit 2)로 끊는다 — 임의 경로를 추측하지 않는다.
    #[test]
    fn missing_xdg_and_home_is_config_error() {
        let err = state_dir_from(None, None, None).expect_err("경로를 추측하면 안 됨");
        assert_eq!(err.exit_code(), crate::error::exit_codes::USAGE);
        assert!(
            err.to_string().contains("--state-dir"),
            "해결책 안내 누락: {err}"
        );
    }

    /// state 디렉터리는 없으면 만들어지고(중첩 경로 포함), 두 번 호출해도 안전하다.
    #[test]
    fn ensure_state_dir_creates_nested_path_idempotently() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("nested/x-backup");
        ensure_state_dir(&dir).unwrap();
        assert!(dir.is_dir(), "디렉터리가 생성되지 않았다");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode, 0o700,
                "새로 만든 state 디렉터리는 소유자 전용이어야 함"
            );
        }
        ensure_state_dir(&dir).unwrap();
    }

    /// 파일이 이미 그 경로를 차지하고 있으면 설정 오류로 끊는다(조용히 무시 금지).
    #[test]
    fn ensure_state_dir_rejects_path_taken_by_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("occupied");
        std::fs::write(&path, b"not a dir").unwrap();
        let err = ensure_state_dir(&path).expect_err("파일 경로는 거부되어야 함");
        assert_eq!(err.exit_code(), crate::error::exit_codes::USAGE);
    }
}
