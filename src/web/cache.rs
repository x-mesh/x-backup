//! 읽기 전용 자식 프로세스 관측 결과의 TTL 캐시 — 콘솔이 프로덕션 DB에 만드는
//! **조용한 지속 부하**를 시간 축에서 묶는다.
//!
//! ## 왜 이 모듈이 필요한가 — 아무도 막아주지 않는 부하
//! 이 콘솔의 도메인 조회는 전부 자식 프로세스다(`x-backup <cmd> --json` —
//! [`crate::web`] 최상위 불변식). 파괴적 작업은 파일 락이 이중 실행을 막아주고
//! (exit 5), 감사 게이트가 기록 없는 실행을 막는다. 그런데 **읽기 전용 명령에는 그
//! 두 방어가 전부 해당되지 않는다**: `status`는 락을 잡지 않고 감사 게이트도 지나지
//! 않는다. 즉 "새로고침을 누르는 횟수 = 프로덕션 DB에 붙는 연결 횟수"다.
//!
//! 대시보드처럼 **프로파일 전부를 한 화면에 모으는** 자리에서 그 곱셈이 드러난다.
//! 프로파일 50개짜리 config에서 새로고침 한 번이 자식 50개 + DB 연결 50개 +
//! destination 쓰기 프로브 50개다. 운영자 셋이 장애 대응 중에 각자 F5를 연타하면
//! 그게 초당 수백 연결이 된다 — 콘솔이 장애의 원인 쪽에 서는 순간이다.
//!
//! 그걸 막는 방법은 두 가지뿐이다: (1) 요청을 줄인다, (2) 요청당 자식을 줄인다.
//! 자동 갱신을 넣지 않는 것이 (1)이고([`crate::web::routes::dashboard`] 헤더의
//! "자동 갱신을 넣지 않는다"), 이 모듈이 (2)다.
//!
//! ## "뷰어가 없으면 폴링을 멈춘다" — 이 구조에서 그 말이 걸리는 자리
//! 요구사항(R38)이 말하는 폴링 정지는 **이 콘솔에 서버측 폴링 루프가 없다**는 사실 위에서
//! 읽어야 한다. 갱신은 전부 요청에서 시작하고(SSR), 자식은 요청이 있을 때만 뜬다. 즉
//! 아무도 화면을 열지 않으면 이 콘솔은 프로덕션에 **아무 요청도 하지 않는다** — 멈출
//! 루프가 애초에 없다.
//!
//! 그래서 실제로 걸리는 자리는 브라우저 쪽 두 곳뿐이고, 둘 다 이미 정리돼 있다:
//!
//! - **`verify` 진행 화면** — 4초마다 스스로 새로고침한다. `document.hidden`이면 멈추고
//!   다시 보일 때 재개한다([`crate::web::view::verify`]의 `AUTO_REFRESH_SCRIPT`). 잊고
//!   열어 둔 탭이 밤새 이력을 다시 읽는 것을 막는 자리가 여기다.
//! - **`backup` 실행 화면** — SSE(push)라 애초에 폴링이 아니다. 뷰어가 떠나면 브라우저가
//!   연결을 끊고 서버 쪽 구독자가 0이 된다.
//!
//! 장수 자식을 뷰어 수에 맞춰 끄는 이야기(라이브 모니터)는 별개 태스크(t18)의 몫이다 —
//! 그건 이 모듈이 아니라 그 화면이 소유한다.
//!
//! ## 무효화는 **쓰기가 끝난 자리**에서 부른다
//! [`invalidate_profile`]·[`invalidate_config`]는 이 모듈이 스스로 부르지 않는다. 부르는
//! 곳은 잡이 끝난 자리(`routes::backup`·`prune`·`migrate`·`restore`의 완료 태스크)와
//! config가 저장된 자리(`routes::config`)다. 그 배선이 빠지면 **캐시는 완벽하게 동작하면서
//! 화면은 계속 낡은 값을 보여준다** — 그래서 각 화면의 테스트가 캐시 함수가 아니라
//! *배선*을 단정한다(예: `routes::prune`의
//! `a_finished_prune_invalidates_that_profiles_probe`).
//!
//! ## 무엇을 캐시하는가 — **자식의 출력**이지 화면이 아니다
//! 캐시가 담는 것은 [`ProbeOutput`](자식 한 번의 관측 결과)이고, 그 stdout을 파싱해
//! 판정하고 마크업으로 접는 일은 **매 요청마다 새로 한다.** 비싼 것(프로세스 생성 +
//! DB 연결 + destination 쓰기)만 아끼고 싼 것(수 KB serde + maud)은 아끼지 않는다.
//!
//! 그 편이 나은 이유는 성능이 아니라 **정확성**이다. 화면을 캐시하면 캐시에 언어·
//! 마스킹 레지스트리·세션이 섞여 들어간다. 그러면 "ko로 먼저 열었더니 en 사용자에게
//! ko 화면이 나간다" 같은 사고가 캐시 키 설계 실수 한 번으로 생기고, 더 나쁘게는
//! 어느 세션의 토큰이 마스킹된 화면이 다른 세션에 재사용된다. 파싱 전 원본만 담으면
//! 그 표면이 아예 없다 — 캐시는 언어도 세션도 모른다.
//!
//! ## 실패도 캐시한다
//! 타임아웃·spawn 실패도 같은 TTL로 담는다([`ProbeOutcome`]에 실패 변종이 있는 이유).
//! 직관과 반대로 보이지만 이쪽이 안전하다: 멈춘 DB를 향해 운영자가 새로고침을 연타하면
//! 캐시가 없는 쪽이 **8초씩 매달리는 자식을 계속 쌓는다.** 실패를 캐시하면 그 프로파일은
//! TTL 동안 자식을 하나도 더 만들지 않는다. 대가(회복된 프로파일이 최대 TTL만큼 낡은
//! 실패를 보여줌)는 화면이 데이터 나이를 **표시**해서 갚는다(아래 "화면은 나이를 말한다").
//!
//! ## TTL 기본값 15초와 그 근거
//! [`PROBE_TTL`] doc에 적었다. 요점: `last_backup`이 분 단위로 표시되므로 15초는 그
//! 칸을 틀리게 만들 수 없고, 장애 대응 중 사람의 새로고침 간격(5~15초)보다 짧지 않아
//! 연타를 흡수한다.
//!
//! ## 화면은 나이를 말한다 — TTL이 짧다고 거짓말이 사라지는 게 아니다
//! "TTL이 길면 화면이 거짓말한다"는 문제의 진짜 해법은 TTL을 0에 가깝게 만드는 것이
//! 아니라(그러면 캐시가 없는 것과 같다) **몇 초 전 데이터인지 화면에 적는 것**이다.
//! [`TtlCache::fetch`]가 [`Cached::age`]를 함께 돌려주는 이유가 그것이고, 대시보드는
//! 행마다 그 값을 표시한다. 그러면 TTL은 "거짓말의 크기"가 아니라 "확인 가능한
//! 지연"이 된다.
//!
//! ## 단일 비행(single-flight) — 캐시만으로는 부족하다
//! TTL 캐시가 콜드 상태일 때 요청 열 개가 동시에 들어오면, 단순한 캐시는 **자식을
//! 열 벌 띄운다**(전부 캐시 미스로 보인다). 그건 이 모듈이 막으려는 상황 그 자체다.
//! 그래서 키마다 [`tokio::sync::Mutex`] 게이트를 두고, 두 번째 이후 호출자는 그
//! 게이트에서 기다린 뒤 **캐시를 다시 본다** — 앞선 생산자가 채워 뒀으므로 히트가 된다.
//! 자식은 키당 하나만 뜬다.
//!
//! ## 함정
//! - 맵 잠금(`std::sync::Mutex`)을 `await` 너머로 들고 가지 않는다. 그래서 잠금을
//!   만지는 코드는 전부 작은 동기 함수([`TtlCache::peek`]·`store`·`gate_for`)로 갈라
//!   두었다 — 인라인으로 쓰면 잠금이 퓨처에 잡혀 런타임을 막고 clippy도 잡는다.
//! - 캐시 키에 config 경로가 들어간다([`config_key`]). **키는 절대 마크업에 나가지
//!   않는다** — 서버 내부 경로는 화면에 싣지 않는다는 규약([`crate::web::view`] 헤더)이
//!   그대로 적용된다. 키에 넣는 이유는 config가 다르면 프로파일 이름이 같아도 다른
//!   대상이기 때문이다(테스트가 서로의 캐시를 오염시키는 것도 이것으로 막힌다).
//! - 무효화는 값을 지우지만, **그 순간 돌고 있는 생산자를 취소하지 않는다.** 이미
//!   뜬 자식은 끝까지 돌고 그 결과가 캐시에 들어간다(무효화 직후 한 번은 낡은 값이
//!   저장될 수 있다). 취소를 넣으려면 생산자에 취소 토큰을 물려야 하는데, 읽기 전용
//!   프로브 하나를 아끼기 위해 그 복잡도를 사는 것은 남는 거래가 아니다.

use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::web::job::JobOutcome;
use crate::web::ServeConfig;

/// 읽기 전용 프로브 캐시의 기본 수명.
///
/// ## 15초의 근거
/// 위쪽 경계(더 길면 화면이 사람을 오도한다):
/// - 이 화면이 보여주는 값 중 가장 빨리 변하는 것은 연결 가능 여부다. 장애가 시작된
///   뒤 운영자가 "지금 괜찮은가"를 묻는 간격은 경험적으로 5~15초(새로고침 연타)다.
///   TTL이 그보다 훨씬 길면 운영자는 이미 죽은 DB를 초록으로 보면서 판단한다.
/// - `last_backup`은 "3h ago"처럼 **분 이상 단위**로 표시된다. 15초는 그 칸을 틀리게
///   만들 수 없다 — 즉 이 TTL이 거짓을 만들 수 있는 칸은 상태 계열뿐이고, 그쪽은
///   화면이 데이터 나이를 함께 적어 확인 가능하게 만든다.
///
/// 아래쪽 경계(더 짧으면 캐시가 의미 없다):
/// - 한 프로파일 프로브가 로컬 관측으로 수십~수백 ms다. TTL이 그 수준이면 새로고침
///   두 번이 곧 자식 두 벌이고, 캐시가 있는 이유가 사라진다.
/// - 실제로 막고 싶은 것은 "새로고침 연타"와 "여러 운영자 동시 접속"이다. 그 두
///   패턴의 시간 규모가 초 단위이므로, 초 단위 TTL이어야 흡수된다.
///
/// 그 사이에서 15초는 "사람이 두 번 누르는 동안 자식은 한 번"이 되는 가장 작은 값에
/// 가깝다. 운영 중에 조정하고 싶어지면 이 상수 하나만 바꾸면 된다 — 캐시를 쓰는
/// 코드에는 값이 박혀 있지 않다.
pub const PROBE_TTL: Duration = Duration::from_secs(15);

/// config를 지정하지 않고 띄운 서버의 캐시 키 조각.
///
/// 빈 문자열이 아니라 눈에 보이는 표식을 쓰는 이유는 [`crate::web::job::JobSpec::audit_target`]
/// 이 `-`를 쓰는 것과 같다 — 진단 중에 "값이 없음"과 "필드가 비었음"을 구분해야 한다.
const NO_CONFIG_KEY: &str = "-";

// ---------------------------------------------------------------------------
// 캐시가 담는 값 — 자식 한 번의 관측 결과
// ---------------------------------------------------------------------------

/// 프로브 자식 하나가 어떻게 끝났는가.
///
/// **성공만 담지 않는다**(모듈 헤더 "실패도 캐시한다"). 네 변종을 따로 두는 이유는
/// 화면이 운영자에게 의심할 곳을 다르게 안내해야 하기 때문이다: 종료 코드가 있으면
/// 그 코드를 읽고([`crate::web::job::exit`]), spawn 실패는 바이너리·권한을 의심하고,
/// 타임아웃은 대상 서버나 멈춘 마운트를 의심한다.
#[derive(Debug, Clone)]
pub enum ProbeOutcome {
    /// 자식이 끝났다 — 종료 코드가 무엇이든(0도, 3도, 5도) 여기로 온다.
    ///
    /// 종료 코드 분류는 [`JobOutcome`]이 이미 해 놨다. 이 계층은 그것을 **다시 하지
    /// 않는다** — exit 4(경고 동반 성공)·exit 5(락 충돌)를 실패로 접는 사고는 분류가
    /// 두 곳에 있을 때 생긴다.
    Completed {
        /// 분류된 종료 상태.
        outcome: JobOutcome,
        /// 자식 stdout 전체(`--json` 문서가 여기 온다).
        stdout: String,
        /// 자식 stderr 전체(사람용 진단).
        stderr: String,
    },
    /// 자식을 띄우지 못했다(실행 권한·파일 없음 등).
    SpawnFailed(String),
    /// 자식을 기다리는 중 I/O 오류.
    WaitFailed(String),
    /// 상한 시간을 넘겨 자식을 죽였다.
    TimedOut(Duration),
}

/// 프로브 한 번의 결과 + 실측 소요 시간.
///
/// `elapsed`가 값의 일부인 이유: 이 화면이 보여줘야 하는 것 중에 **지연**이 있고
/// (R11), 그 값은 자식이 알려주는 것이 아니라 우리가 재는 것이다. 캐시에 함께 담아야
/// 캐시 히트로 그려진 행도 "지난 실측이 몇 ms였는지"를 말할 수 있다 — 히트일 때
/// 0ms로 그리면 화면이 없는 성능을 보고한다.
#[derive(Debug, Clone)]
pub struct ProbeOutput {
    /// 어떻게 끝났는가.
    pub outcome: ProbeOutcome,
    /// spawn부터 종료(또는 상한)까지의 실측 소요 시간.
    pub elapsed: Duration,
}

/// 프로브 캐시의 키 어휘 — **닫힌 enum**이다.
///
/// 문자열 하나로 합치지 않는 이유: `"prod"`라는 프로파일과 `"prod"`라는 config가
/// 같은 키가 되는 사고를 문법 수준에서 막는다. 각 변종이 `config`를 들고 있는 근거는
/// 모듈 헤더 "함정" 참조.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ProbeKey {
    /// 프로파일 목록(+엔진 라벨) — `doctor --json` 한 번.
    Roster {
        /// 이 서버가 자식에게 넘기는 config 경로([`config_key`]).
        config: String,
    },
    /// 프로파일 하나의 상태 — `status --profile <name> --json` 한 번.
    Profile {
        /// 이 서버가 자식에게 넘기는 config 경로([`config_key`]).
        config: String,
        /// 프로파일 이름.
        profile: String,
    },
}

impl ProbeKey {
    /// 이 키가 속한 config 조각 — [`invalidate_config`]가 범위를 가르는 데 쓴다.
    pub fn config(&self) -> &str {
        match self {
            Self::Roster { config } | Self::Profile { config, .. } => config,
        }
    }
}

/// 이 서버의 config를 가리키는 캐시 키 조각.
///
/// 경로 문자열을 쓴다 — 같은 프로세스 안에서 config는 기동 시점에 하나로 고정되므로
/// ([`ServeConfig::config_path`]) 사실상 상수이고, 그런데도 키에 넣는 이유는 **같은
/// 프로세스에서 여러 컨텍스트가 도는 유일한 경우**인 테스트가 서로의 캐시를 보지
/// 않게 하기 위함이다. 이 값이 마크업에 나가지 않는다는 것은 모듈 헤더 "함정" 참조.
pub fn config_key(ctx: &ServeConfig) -> String {
    ctx.config_path
        .as_ref()
        .map_or_else(|| NO_CONFIG_KEY.to_string(), |p| p.display().to_string())
}

// ---------------------------------------------------------------------------
// 캐시 — 일반형(키·값에 대해 아무것도 모른다)
// ---------------------------------------------------------------------------

/// 캐시에서 꺼낸 값 + 그 값의 출처와 나이.
///
/// `hit`과 `age`를 값과 함께 돌려주는 이유는 관측 가능성이다. 호출자가 "이 화면이
/// 방금 실측한 것인가, 몇 초 전 것인가"를 말할 수 있어야 TTL이 정직해진다(모듈 헤더
/// "화면은 나이를 말한다"). 테스트도 이 두 값으로 캐시 동작을 단정한다.
#[derive(Debug, Clone)]
pub struct Cached<V> {
    /// 값(캐시본 또는 방금 만든 것).
    pub value: V,
    /// 캐시에서 나왔는가. `false`면 이번 호출이 생산자를 실제로 돌렸다.
    pub hit: bool,
    /// 값이 만들어진 뒤 흐른 시간. 미스면 [`Duration::ZERO`]에 가깝다.
    pub age: Duration,
}

/// 키 하나의 자리 — 저장된 값과 그 키의 단일 비행 게이트.
///
/// 값과 게이트를 **같은 자리**에 두는 것이 요점이다. 맵을 둘로 나누면 "값 맵에는
/// 있는데 게이트 맵에는 없는" 상태가 생기고, 그 틈에서 두 생산자가 동시에 통과한다.
struct Slot<V> {
    /// 저장된 값과 저장 시각. `None`은 아직(또는 더 이상) 값이 없다는 뜻이다.
    stored: Option<(V, Instant)>,
    /// 이 키의 생산자를 한 번에 하나로 묶는 게이트(모듈 헤더 "단일 비행").
    gate: Arc<tokio::sync::Mutex<()>>,
}

impl<V> Slot<V> {
    /// 값 없는 빈 자리.
    fn empty() -> Self {
        Self {
            stored: None,
            gate: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
}

/// 수명이 있는 메모이제이션 표 — **키·값의 의미를 모른다.**
///
/// 이 타입이 일반형인 이유: 캐시의 어려운 부분(만료 판정·단일 비행·무효화)은 대시보드
/// 고유 문제가 아니다. 나중에 다른 읽기 전용 화면(카탈로그·verify 요약)이 같은 문제를
/// 만나면 이 타입을 그대로 인스턴스화하면 되고, 그때 대시보드 코드를 읽을 필요가 없다.
///
/// `V: Clone`을 요구하지만 실제 사용은 [`Arc`]로 감싼 값이므로 복제 비용은 참조 카운트
/// 하나다 — 캐시가 값을 소유하고 호출자에게 사본을 주는 형태가, 잠금을 들고 나가는
/// 형태(참조 반환)보다 훨씬 다루기 쉽다.
pub struct TtlCache<K, V> {
    /// 값의 수명.
    ttl: Duration,
    /// 키 → 자리. **이 잠금은 `await` 너머로 나가지 않는다**(모듈 헤더 "함정").
    slots: Mutex<HashMap<K, Slot<V>>>,
}

impl<K: Eq + Hash + Clone, V: Clone> std::fmt::Debug for TtlCache<K, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 키·값을 찍지 않는다. 키에는 서버 내부 경로가, 값에는 자식 출력이 들어 있다 —
        // 진단에 필요한 것은 "몇 개가 살아 있는가"다(`JobRunner`의 Debug와 같은 판단).
        //
        // 트레이트 경계가 `K: Debug`·`V: Debug`가 아니라 캐시 본체와 같은 경계인 것에
        // 유의: 값을 찍지 않으므로 `Debug`가 필요 없고, 대신 자리 수를 세기 위해
        // 아래 impl 블록의 메서드를 쓴다.
        f.debug_struct("TtlCache")
            .field("ttl", &self.ttl)
            .field("slots", &self.slot_count())
            .finish()
    }
}

impl<K: Eq + Hash + Clone, V: Clone> TtlCache<K, V> {
    /// 빈 캐시를 만든다.
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            slots: Mutex::new(HashMap::new()),
        }
    }

    /// 이 캐시의 수명 — 화면이 "N초 캐시"라고 말할 때 쓴다.
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// 살아 있는 값이 있으면 꺼낸다(생산자를 돌리지 않는다).
    ///
    /// 만료된 값은 **없는 것으로 본다** — 지우지는 않는다. 지우려면 쓰기 잠금이
    /// 필요하고, 어차피 다음 [`fetch`](Self::fetch)가 덮어쓴다.
    pub fn peek(&self, key: &K) -> Option<Cached<V>> {
        let slots = self.lock();
        let (value, stored_at) = slots.get(key)?.stored.as_ref()?;
        let age = stored_at.elapsed();
        if age >= self.ttl {
            return None;
        }
        Some(Cached {
            value: value.clone(),
            hit: true,
            age,
        })
    }

    /// 살아 있는 값을 주거나, 없으면 `produce`를 돌려 만든 값을 주고 저장한다.
    ///
    /// **키당 생산자는 한 번에 하나만 돈다**(모듈 헤더 "단일 비행"). 그래서 콜드
    /// 상태에 동시 요청 열 개가 들어와도 `produce`는 한 번만 호출된다 — 나머지 아홉은
    /// 게이트에서 기다린 뒤 캐시 히트로 돌아간다.
    ///
    /// `produce`가 실패를 표현하는 값을 돌려도 그대로 저장된다(모듈 헤더 "실패도
    /// 캐시한다"). 실패를 저장하고 싶지 않다면 호출자가 [`invalidate`](Self::invalidate)
    /// 를 부르면 된다 — 이 함수가 값의 의미를 판단하지 않는 편이 낫다(무엇이 실패인지는
    /// 도메인 지식이고, 이 타입은 도메인을 모른다).
    pub async fn fetch<F, Fut>(&self, key: K, produce: F) -> Cached<V>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = V>,
    {
        // 1) 빠른 경로 — 게이트를 만지지 않고 히트를 낸다(대부분의 요청이 여기서 끝난다).
        if let Some(cached) = self.peek(&key) {
            return cached;
        }
        // 2) 이 키의 생산 권한을 잡는다. 맵 잠금은 `gate_for` 안에서 끝난다.
        let gate = self.gate_for(&key);
        let _held = gate.lock().await;
        // 3) 기다리는 동안 앞선 생산자가 채웠을 수 있다 — 다시 본다.
        //    이 재확인이 없으면 단일 비행은 "직렬화"일 뿐 생산 횟수를 줄이지 못한다.
        if let Some(cached) = self.peek(&key) {
            return cached;
        }
        // 4) 실제 생산. 여기서만 비싼 일(자식 프로세스)이 벌어진다.
        let value = produce().await;
        self.store(key, value.clone());
        Cached {
            value,
            hit: false,
            age: Duration::ZERO,
        }
    }

    /// 키 하나의 값을 버린다. 게이트는 남긴다 — 지금 그 게이트를 들고 생산 중인
    /// 호출자가 있을 수 있고, 게이트를 새로 만들면 그 순간 두 생산자가 생긴다.
    pub fn invalidate(&self, key: &K) {
        if let Some(slot) = self.lock().get_mut(key) {
            slot.stored = None;
        }
    }

    /// 조건에 맞는 키만 남기고 나머지 값을 버린다 — 범위 무효화([`invalidate_config`]).
    pub fn invalidate_matching(&self, mut hit: impl FnMut(&K) -> bool) {
        for (key, slot) in self.lock().iter_mut() {
            if hit(key) {
                slot.stored = None;
            }
        }
    }

    /// 전부 버린다.
    pub fn invalidate_all(&self) {
        for slot in self.lock().values_mut() {
            slot.stored = None;
        }
    }

    /// 살아 있는(만료되지 않은) 값의 개수 — 테스트·진단용.
    pub fn fresh_count(&self) -> usize {
        let slots = self.lock();
        slots
            .values()
            .filter(|s| {
                s.stored
                    .as_ref()
                    .is_some_and(|(_, at)| at.elapsed() < self.ttl)
            })
            .count()
    }

    /// 자리(키) 개수 — 만료·무효화된 것까지 센다. `Debug`와 진단에만 쓴다.
    pub fn slot_count(&self) -> usize {
        self.lock().len()
    }

    /// 이 키의 게이트를 얻는다(없으면 만든다). **맵 잠금은 이 함수 안에서 끝난다.**
    fn gate_for(&self, key: &K) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(
            &self
                .lock()
                .entry(key.clone())
                .or_insert_with(Slot::empty)
                .gate,
        )
    }

    /// 값을 저장한다(시각을 함께 찍는다).
    fn store(&self, key: K, value: V) {
        self.lock().entry(key).or_insert_with(Slot::empty).stored = Some((value, Instant::now()));
    }

    /// 맵 잠금 — 오염(poison)돼도 계속 잡는다.
    ///
    /// 보호 대상이 `HashMap` 하나이고, 이전 패닉이 남길 수 있는 "불변식 위반"이
    /// 없다(자리가 하나 비어 있으면 다음 요청이 다시 채운다). `unwrap()`으로 두면
    /// 어느 요청의 패닉 하나가 **그 뒤 모든 대시보드 요청을 영구히 500으로 만든다** —
    /// 캐시는 성능 장치이므로 그런 식으로 화면을 죽일 자격이 없다
    /// ([`crate::web::job::env_guard`]의 같은 판단).
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<K, Slot<V>>> {
        self.slots.lock().unwrap_or_else(|e| e.into_inner())
    }
}

// ---------------------------------------------------------------------------
// 프로세스 전역 프로브 캐시 + 무효화 진입점
// ---------------------------------------------------------------------------

/// 대시보드가 쓰는 프로브 캐시(프로세스 하나에 하나).
///
/// ## 왜 [`ServeConfig`]의 필드가 아니라 전역인가
/// 캐시는 요청 사이에 살아남아야 하므로 요청 수명 값일 수 없고, `ServeConfig`는 이
/// 브랜치에서 여러 태스크가 동시에 만지는 파일에 있다(필드를 늘리면 배선 충돌이 된다).
/// 더 중요한 이유는 **무효화 호출부**다: 백업 잡이 끝난 자리에서 그 프로파일의 status를
/// 낡은 것으로 표시해야 하는데, 그 코드는 대시보드를 모르고 알 이유도 없다. 전역
/// 진입점([`invalidate_profile`])이면 `crate::web::cache::invalidate_profile(&ctx, "prod")`
/// 한 줄이고, state 필드라면 그 자리까지 캐시 핸들을 들고 가야 한다.
///
/// 전역의 대가는 테스트 격리다. 그래서 (1) [`TtlCache`]는 독립 인스턴스로 만들 수 있고
/// (캐시 동작 테스트는 전역을 쓰지 않는다), (2) 키에 config 경로가 들어가 서로 다른
/// 테스트 컨텍스트가 같은 자리를 보지 않는다([`config_key`]).
static PROBE_CACHE: LazyLock<TtlCache<ProbeKey, Arc<ProbeOutput>>> =
    LazyLock::new(|| TtlCache::new(PROBE_TTL));

/// 전역 프로브 캐시 핸들.
pub fn probe_cache() -> &'static TtlCache<ProbeKey, Arc<ProbeOutput>> {
    &PROBE_CACHE
}

/// **쓰기 작업이 끝난 뒤 부르는 자리** — 그 프로파일의 상태 캐시를 낡은 것으로 만든다.
///
/// ## 왜 필요한가
/// 백업이 방금 끝났으면 그 프로파일의 `last_backup`은 "3h ago"가 아니라 "방금"이다.
/// TTL이 지나기를 기다리면 운영자는 자기가 방금 누른 백업이 반영되지 않은 화면을 보고
/// "실패했나?"를 되묻는다 — 콘솔이 자기 행동의 결과를 못 보여주는 것은 기능 결함이다.
///
/// ## 호출 지점(리더 배선 대상)
/// 잡이 **종료된 직후**, 프로파일을 아는 자리에서 부른다. 성공/실패를 가리지 않는다 —
/// 실패한 백업도 destination 상태를 바꿨을 수 있고(부분 산출물), 무효화의 비용은 다음
/// 대시보드 로드에서 프로브 한 번이다. 과잉 무효화는 안전한 방향의 실패다.
///
/// 프로파일을 모르는 잡(프로파일 없는 명령)은 이 함수의 대상이 아니다 — 그때는
/// [`invalidate_config`]가 맞다.
pub fn invalidate_profile(ctx: &ServeConfig, profile: &str) {
    probe_cache().invalidate(&ProbeKey::Profile {
        config: config_key(ctx),
        profile: profile.to_string(),
    });
}

/// config가 바뀐 뒤 부르는 자리 — 그 config의 **모든** 프로브를 낡은 것으로 만든다.
///
/// config 편집은 프로파일 목록 자체를 바꿀 수 있으므로(추가·삭제·이름 변경) 프로파일
/// 하나가 아니라 명부(roster)까지 함께 버려야 한다. 프로파일이 사라졌는데 명부가
/// 캐시되어 있으면 화면이 없는 프로파일을 계속 프로브한다.
pub fn invalidate_config(ctx: &ServeConfig) {
    let config = config_key(ctx);
    probe_cache().invalidate_matching(|key| key.config() == config);
}

/// 전역 캐시를 통째로 버린다 — 테스트와 "강제 새로고침" 경로용.
pub fn invalidate_all() {
    probe_cache().invalidate_all();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 테스트용 짧은 TTL — 만료를 실제로 관측하려면 테스트가 기다릴 수 있어야 한다.
    const SHORT: Duration = Duration::from_millis(120);

    /// 호출 횟수를 세는 생산자. "자식이 몇 번 떴는가"의 대역이다 — 실제 프로세스
    /// 카운트는 `routes::dashboard`가 가짜 실행 파일로 따로 확인한다.
    fn counter() -> (Arc<AtomicUsize>, impl Fn() -> usize) {
        let calls = Arc::new(AtomicUsize::new(0));
        let read = {
            let calls = Arc::clone(&calls);
            move || calls.load(Ordering::SeqCst)
        };
        (calls, read)
    }

    /// TTL 안의 두 번째 요청은 생산자를 부르지 않는다 — 이 모듈의 존재 이유.
    #[tokio::test]
    async fn second_fetch_within_ttl_does_not_produce() {
        let cache: TtlCache<&str, u32> = TtlCache::new(SHORT);
        let (calls, read) = counter();

        let first = cache
            .fetch("k", || {
                let calls = Arc::clone(&calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    7
                }
            })
            .await;
        assert!(!first.hit, "첫 호출이 히트로 보고됐다");
        assert_eq!(first.value, 7);

        let second = cache
            .fetch("k", || {
                let calls = Arc::clone(&calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    99
                }
            })
            .await;
        assert!(second.hit, "TTL 안인데 미스로 보고됐다");
        assert_eq!(second.value, 7, "캐시본이 아니라 새 값이 나왔다");
        assert_eq!(read(), 1, "생산자가 두 번 돌았다 — 캐시가 동작하지 않는다");
    }

    /// TTL이 지나면 다시 생산한다(캐시가 값을 영구히 붙잡지 않는다).
    #[tokio::test]
    async fn expired_value_is_produced_again() {
        let cache: TtlCache<&str, u32> = TtlCache::new(SHORT);
        let (calls, read) = counter();
        let produce = || {
            let calls = Arc::clone(&calls);
            async move { calls.fetch_add(1, Ordering::SeqCst) as u32 }
        };

        cache.fetch("k", produce).await;
        tokio::time::sleep(SHORT + Duration::from_millis(40)).await;
        let after = cache.fetch("k", produce).await;

        assert!(!after.hit, "만료된 값이 히트로 나왔다");
        assert_eq!(read(), 2, "만료 후 재생산이 일어나지 않았다");
        assert_eq!(cache.fresh_count(), 1);
    }

    /// 무효화하면 TTL이 남아 있어도 다시 생산한다 — 쓰기 작업 뒤 반영의 기반.
    #[tokio::test]
    async fn invalidate_forces_a_fresh_production() {
        let cache: TtlCache<&str, u32> = TtlCache::new(Duration::from_secs(3600));
        let (calls, read) = counter();
        let produce = || {
            let calls = Arc::clone(&calls);
            async move { calls.fetch_add(1, Ordering::SeqCst) as u32 }
        };

        cache.fetch("k", produce).await;
        assert!(
            cache.fetch("k", produce).await.hit,
            "대조군이 히트여야 한다"
        );
        assert_eq!(read(), 1);

        cache.invalidate(&"k");
        assert_eq!(cache.fresh_count(), 0, "무효화 후에도 살아 있는 값이 있다");
        let again = cache.fetch("k", produce).await;
        assert!(!again.hit, "무효화 후에도 캐시본이 나왔다");
        assert_eq!(read(), 2, "무효화가 재생산을 유발하지 않았다");
        // 게이트 자리는 남는다(생산 중인 호출자를 잃지 않기 위해 — 본문 doc 참조).
        assert_eq!(cache.slot_count(), 1);
    }

    /// 키가 서로를 오염시키지 않는다.
    #[tokio::test]
    async fn keys_are_independent() {
        let cache: TtlCache<&str, &str> = TtlCache::new(Duration::from_secs(3600));
        cache.fetch("a", || async { "A" }).await;
        cache.fetch("b", || async { "B" }).await;
        assert_eq!(cache.fetch("a", || async { "x" }).await.value, "A");
        assert_eq!(cache.fetch("b", || async { "x" }).await.value, "B");
        assert_eq!(cache.fresh_count(), 2);

        cache.invalidate(&"a");
        assert!(!cache.fetch("a", || async { "A2" }).await.hit);
        assert!(
            cache.fetch("b", || async { "x" }).await.hit,
            "옆 키가 함께 지워졌다"
        );
    }

    /// 범위 무효화 — 조건에 맞는 키만 버린다.
    #[tokio::test]
    async fn matching_invalidation_is_scoped() {
        let cache: TtlCache<String, u32> = TtlCache::new(Duration::from_secs(3600));
        for key in ["cfgA:p1", "cfgA:p2", "cfgB:p1"] {
            cache.fetch(key.to_string(), || async { 1 }).await;
        }
        cache.invalidate_matching(|k| k.starts_with("cfgA:"));
        assert_eq!(cache.fresh_count(), 1, "범위 밖 키까지 지워졌다");
        assert!(
            cache.fetch("cfgB:p1".to_string(), || async { 2 }).await.hit,
            "다른 config의 값이 함께 지워졌다"
        );
    }

    /// **콜드 상태 동시 요청이 생산자를 한 번만 부른다** — 단일 비행 회귀 고정.
    ///
    /// 이것이 없으면 대시보드가 열리는 순간 프로파일마다 자식이 요청 수만큼 뜬다
    /// (모듈 헤더 "단일 비행"). 생산자를 일부러 느리게 만들어 경합 창을 벌린다.
    #[tokio::test]
    async fn concurrent_cold_fetches_produce_once() {
        let cache: Arc<TtlCache<&str, u32>> = Arc::new(TtlCache::new(Duration::from_secs(3600)));
        let (calls, read) = counter();

        let mut tasks = Vec::new();
        for _ in 0..12 {
            let cache = Arc::clone(&cache);
            let calls = Arc::clone(&calls);
            tasks.push(tokio::spawn(async move {
                cache
                    .fetch("k", || async move {
                        tokio::time::sleep(Duration::from_millis(60)).await;
                        calls.fetch_add(1, Ordering::SeqCst) as u32
                    })
                    .await
            }));
        }
        let results = futures::future::join_all(tasks).await;
        assert_eq!(
            read(),
            1,
            "동시 요청이 생산자를 여러 번 돌렸다 — 캐시가 부하 폭증을 못 막는다"
        );
        // 전부 같은 값을 본다(미스 하나 + 히트 나머지).
        let hits = results
            .iter()
            .filter(|r| r.as_ref().expect("태스크 패닉").hit)
            .count();
        assert_eq!(hits, 11, "히트 수가 맞지 않는다");
    }

    /// 나이가 값과 함께 나온다 — 화면이 "몇 초 전 데이터"라고 말할 수 있어야 한다.
    #[tokio::test]
    async fn cached_value_reports_its_age() {
        let cache: TtlCache<&str, u32> = TtlCache::new(Duration::from_secs(3600));
        let miss = cache.fetch("k", || async { 1 }).await;
        assert!(miss.age < Duration::from_millis(50), "미스의 나이가 크다");
        tokio::time::sleep(Duration::from_millis(80)).await;
        let hit = cache.fetch("k", || async { 2 }).await;
        assert!(
            hit.hit && hit.age >= Duration::from_millis(60),
            "나이가 보고되지 않는다: {hit:?}"
        );
    }

    /// [`peek`](TtlCache::peek)은 생산하지 않는다.
    #[tokio::test]
    async fn peek_never_produces() {
        let cache: TtlCache<&str, u32> = TtlCache::new(Duration::from_secs(3600));
        assert!(cache.peek(&"k").is_none());
        assert_eq!(cache.slot_count(), 0, "peek이 자리를 만들었다");
        cache.fetch("k", || async { 5 }).await;
        assert_eq!(cache.peek(&"k").expect("값이 있어야 한다").value, 5);
    }

    /// 키 어휘가 config를 함께 담아, 프로파일 이름이 같아도 다른 자리를 쓴다.
    #[test]
    fn probe_keys_separate_configs() {
        let a = ProbeKey::Profile {
            config: "/etc/a.toml".into(),
            profile: "prod".into(),
        };
        let b = ProbeKey::Profile {
            config: "/etc/b.toml".into(),
            profile: "prod".into(),
        };
        assert_ne!(a, b, "config가 달라도 같은 키가 된다");
        assert_eq!(a.config(), "/etc/a.toml");
        // 명부와 프로파일도 섞이지 않는다.
        assert_ne!(
            ProbeKey::Roster {
                config: "/etc/a.toml".into()
            },
            a
        );
    }

    /// config 미지정 서버도 안정된 키를 얻는다(빈 문자열이 아니다).
    #[test]
    fn config_key_is_stable_without_a_config() {
        let ctx = ServeConfig::for_test();
        assert_eq!(config_key(&ctx), NO_CONFIG_KEY);
        let mut with = ServeConfig::for_test();
        with.config_path = Some(std::path::PathBuf::from("/etc/x-backup/demo.toml"));
        assert_eq!(config_key(&with), "/etc/x-backup/demo.toml");
    }

    /// 전역 무효화 진입점이 실제로 그 config·프로파일 자리만 건드린다.
    #[tokio::test]
    async fn global_invalidation_entry_points_are_scoped() {
        let mut ctx = ServeConfig::for_test();
        // 이 테스트만의 config 경로를 쓴다 — 전역 캐시를 다른 테스트와 공유하므로
        // 키가 겹치면 서로를 흔든다(모듈 헤더 "전역의 대가").
        ctx.config_path = Some(std::path::PathBuf::from(
            "/nonexistent/cache-test-scope.toml",
        ));
        let cfg = config_key(&ctx);
        let key_p1 = ProbeKey::Profile {
            config: cfg.clone(),
            profile: "p1".into(),
        };
        let key_p2 = ProbeKey::Profile {
            config: cfg.clone(),
            profile: "p2".into(),
        };
        let key_roster = ProbeKey::Roster { config: cfg };

        let sample = || {
            Arc::new(ProbeOutput {
                outcome: ProbeOutcome::TimedOut(Duration::from_secs(1)),
                elapsed: Duration::from_secs(1),
            })
        };
        for key in [key_p1.clone(), key_p2.clone(), key_roster.clone()] {
            probe_cache().fetch(key, || async { sample() }).await;
        }

        invalidate_profile(&ctx, "p1");
        assert!(probe_cache().peek(&key_p1).is_none(), "p1이 남아 있다");
        assert!(probe_cache().peek(&key_p2).is_some(), "p2까지 지워졌다");
        assert!(
            probe_cache().peek(&key_roster).is_some(),
            "명부까지 지워졌다"
        );

        invalidate_config(&ctx);
        assert!(
            probe_cache().peek(&key_p2).is_none(),
            "config 무효화가 프로파일을 남겼다"
        );
        assert!(
            probe_cache().peek(&key_roster).is_none(),
            "config 무효화가 명부를 남겼다"
        );
    }

    /// [`Debug`]가 키·값을 흘리지 않는다 — 키에는 config 경로가, 값에는 자식 출력이 있다.
    #[tokio::test]
    async fn debug_does_not_leak_keys_or_values() {
        let cache: TtlCache<&str, &str> = TtlCache::new(SHORT);
        cache
            .fetch("/etc/x-backup/secret-path.toml", || async {
                "child stdout with mongodb://u:p@h/db"
            })
            .await;
        let text = format!("{cache:?}");
        assert!(!text.contains("secret-path"), "키가 노출됐다: {text}");
        assert!(!text.contains("mongodb://"), "값이 노출됐다: {text}");
        assert!(text.contains("slots: 1"), "진단 정보가 없다: {text}");
    }
}
