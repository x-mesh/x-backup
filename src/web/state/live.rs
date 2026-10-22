//! 이 **서버 인스턴스**의 라이브 잡 상태 — SSE 허브 레지스트리와 취소 진행 표시.
//!
//! ## 왜 이 파일이 생겼나 — 프로세스 전역 static을 걷어내려고
//! 두 레지스트리는 원래 `crate::web::routes::backup` 안의 `static`이었다. 그 파일이
//! [`crate::web::ServeConfig`]에 필드를 얹을 수 없는 상황(병렬 실행 중 파일 소유권이
//! 갈려 있었다)에서 나온 우회였고, 대가가 셋 있었다:
//!
//! 1. **테스트 격리가 약해진다.** 같은 프로세스의 모든 테스트가 한 레지스트리를 공유하므로,
//!    한 테스트가 남긴 허브·취소 표시가 다른 테스트에 보인다. 잡 id가 UUID v7이라 실제
//!    충돌은 나지 않지만, "이 테스트가 만든 것만 보인다"는 성질이 없으면 격리를 근거로
//!    삼는 단정을 쓸 수 없다.
//! 2. **한 프로세스에서 서버를 두 개 띄울 수 없다.** 지금 그럴 일이 없어도, 상태가
//!    프로세스에 매달려 있다는 사실 자체가 나중의 선택지를 지운다.
//! 3. **소유 관계가 코드에 안 적힌다.** 허브는 "이 서버가 도는 동안 이 잡의 이벤트를
//!    받는 통로"다. 그것이 `ServeConfig` 필드면 수명이 서버와 같다는 것이 타입에 드러나고,
//!    전역 static이면 아무 데서나 손댈 수 있는 열린 상태가 된다.
//!
//! `crate::web::sse` 헤더의 예시 코드가 이미 `State(registry): State<Arc<JobRegistry>>`
//! 모양을 가정하고 있었다 — 그 파일이 처음부터 이 자리를 예상하고 있었다.
//!
//! ## 여기 **들어오지 않은** 것 — [`super::jobs::shared`]
//! 잡 이력 저장소도 프로세스 전역이지만 그건 우회가 아니라 **올바른 경계**다. 그 함수는
//! `state_dir` **경로로** 인스턴스를 키잉한다. 저장소가 보장해야 하는 것은 "같은 디렉터리에
//! 쓰는 사람은 뮤텍스 하나를 공유한다"이고, 공유 단위는 서버 인스턴스가 아니라 **디렉터리**다.
//!
//! 그래서 이것을 `ServeConfig` 필드로 옮기면 오히려 깨진다: 같은 `state_dir`를 가리키는
//! `ServeConfig`가 둘 생기면 저장소도 둘이 되고, 그 순간 append 직렬화가 사라진다. 전역이
//! 아니라 **자원으로 키잉된 전역**이라는 점이 다르다.
//!
//! ## 잠금
//! 두 맵 모두 `std::sync::Mutex`다 — 감싸는 구간이 맵 조작 몇 줄뿐이고 `.await`를 넘지
//! 않는다(`crate::web::auth::AuthState`와 같은 판단). 잠금 오염은 값을 그대로 꺼내 계속
//! 쓴다: 보호 대상이 맵 하나씩이라 이전 패닉이 남길 불변식 위반이 없고, 여기서 패닉을
//! 전파하면 **잡 하나의 문제가 콘솔 전체를 500으로 만든다.**

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use super::jobs::JobId;
use crate::web::sse::JobHub;

/// 서버 인스턴스 하나가 들고 있는 라이브 잡 상태.
///
/// [`crate::web::ServeConfig::live`]로 접근한다. 직접 만들 필요는 [`LiveJobs::new`]로
/// 컨텍스트를 조립하는 자리(기동 경로와 테스트 생성자)뿐이다.
#[derive(Default)]
pub struct LiveJobs {
    /// 잡별 SSE 허브. 잡이 끝난 뒤에도 잠시 남는다(뒤늦게 붙는 뷰어를 위해 —
    /// 유지 기간은 등록하는 쪽의 정책이다).
    hubs: Mutex<HashMap<JobId, Arc<JobHub>>>,
    /// 취소가 진행 중인 잡. 중복 취소를 막는다([`LiveJobs::claim_cancel`]).
    cancelling: Mutex<HashSet<JobId>>,
}

/// `Debug`를 손으로 구현한다([`crate::web::ServeConfig`]가 `derive(Debug)`이므로 필요하다).
///
/// derive를 쓸 수 없는 이유는 [`JobHub`]가 `Debug`가 아니기 때문이고, 굳이 그쪽에
/// `Debug`를 붙이지 않는 편이 낫다 — 허브는 브로드캐스트 채널과 마스킹 레지스트리를 들고
/// 있어서, 무심코 찍으면 **시크릿 레지스트리 내용이 로그로 나갈 표면**이 열린다. 여기서는
/// 개수만 남긴다(진단에 필요한 것도 그것뿐이다).
impl std::fmt::Debug for LiveJobs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let hubs = self
            .hubs
            .lock()
            .map(|m| m.len())
            .unwrap_or_else(|e| e.into_inner().len());
        let cancelling = self
            .cancelling
            .lock()
            .map(|m| m.len())
            .unwrap_or_else(|e| e.into_inner().len());
        f.debug_struct("LiveJobs")
            .field("hubs", &hubs)
            .field("cancelling", &cancelling)
            .finish()
    }
}

impl LiveJobs {
    /// 빈 상태로 만든다.
    pub fn new() -> Self {
        Self::default()
    }

    // ---- SSE 허브 -------------------------------------------------------

    /// 이 잡의 허브를 등록한다. 같은 id로 다시 부르면 덮어쓴다.
    pub fn register_hub(&self, id: JobId, hub: Arc<JobHub>) {
        self.hubs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, hub);
    }

    /// 허브를 내린다 — 이후 `/events`는 404가 된다(이력은 남으므로 데이터가 사라지는
    /// 것은 아니다).
    pub fn unregister_hub(&self, id: &JobId) {
        self.hubs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id);
    }

    /// 이 잡의 허브(있으면).
    pub fn hub_for(&self, id: &JobId) -> Option<Arc<JobHub>> {
        self.hubs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned()
    }

    // ---- 취소 진행 표시 -------------------------------------------------

    /// 이 잡의 취소 진행 표시를 잡는다. 이미 진행 중이면 `None`.
    ///
    /// 받는 형태가 `&Arc<Self>`인 이유: 돌려주는 [`CancelClaim`]이 드롭될 때 스스로
    /// 표시를 지워야 하므로 이 상태를 계속 붙잡고 있어야 한다.
    pub fn claim_cancel(self: &Arc<Self>, id: &JobId) -> Option<CancelClaim> {
        let inserted = self
            .cancelling
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(*id);
        inserted.then(|| CancelClaim {
            live: Arc::clone(self),
            id: *id,
        })
    }

    /// 이 잡의 취소가 진행 중인가.
    pub fn cancel_in_flight(&self, id: &JobId) -> bool {
        self.cancelling
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(id)
    }
}

/// 취소 진행 표시. **살아 있는 동안** 그 잡의 다른 취소 요청을 막고, 드롭될 때 스스로
/// 표시를 지운다.
///
/// RAII로 만든 이유: 취소를 수행하는 백그라운드 태스크가 어느 경로로 끝나든(성공·거부·
/// 패닉) 표시가 남지 않아야 한다. 표시가 남으면 그 잡은 **재기동 전까지 다시 취소할 수
/// 없게 된다** — 실패한 취소를 재시도할 수 없는 콘솔은 취소 버튼이 없는 것보다 나쁘다.
#[derive(Debug)]
pub struct CancelClaim {
    live: Arc<LiveJobs>,
    id: JobId,
}

impl Drop for CancelClaim {
    fn drop(&mut self) {
        self.live
            .cancelling
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::mask::SecretRegistry;

    fn live() -> Arc<LiveJobs> {
        Arc::new(LiveJobs::new())
    }

    fn hub() -> Arc<JobHub> {
        Arc::new(JobHub::new(SecretRegistry::new()))
    }

    #[test]
    fn hubs_are_found_by_id_and_gone_after_unregister() {
        let live = live();
        let id = JobId::generate();
        let other = JobId::generate();

        assert!(live.hub_for(&id).is_none(), "빈 상태에서 허브가 나왔다");

        live.register_hub(id, hub());
        assert!(live.hub_for(&id).is_some());
        assert!(live.hub_for(&other).is_none(), "무관한 잡의 허브가 나왔다");

        live.unregister_hub(&id);
        assert!(live.hub_for(&id).is_none(), "내린 허브가 아직 보인다");
    }

    /// **두 인스턴스는 서로를 보지 못한다** — 프로세스 전역 static을 걷어낸 이유 그 자체다.
    #[test]
    fn two_instances_do_not_share_state() {
        let first = live();
        let second = live();
        let id = JobId::generate();

        first.register_hub(id, hub());
        assert!(
            second.hub_for(&id).is_none(),
            "다른 서버 인스턴스의 허브가 보인다 — 상태가 공유되고 있다"
        );

        let _claim = first.claim_cancel(&id).expect("표시를 잡지 못했다");
        assert!(
            !second.cancel_in_flight(&id),
            "다른 인스턴스의 취소 진행 표시가 보인다"
        );
        assert!(
            second.claim_cancel(&id).is_some(),
            "다른 인스턴스가 같은 잡 때문에 막혔다"
        );
    }

    #[test]
    fn a_claim_blocks_a_second_one_until_dropped() {
        let live = live();
        let id = JobId::generate();

        let first = live
            .claim_cancel(&id)
            .expect("첫 취소가 표시를 잡지 못했다");
        assert!(live.cancel_in_flight(&id));
        assert!(
            live.claim_cancel(&id).is_none(),
            "취소가 도는 중인데 두 번째 취소가 시작됐다"
        );

        // 표시는 잡 단위다 — 무관한 잡은 막히지 않는다.
        let other = JobId::generate();
        assert!(live.claim_cancel(&other).is_some());

        drop(first);
        assert!(!live.cancel_in_flight(&id), "표시가 지워지지 않았다");
        assert!(
            live.claim_cancel(&id).is_some(),
            "취소가 끝났는데 다시 취소할 수 없다 — 재기동 전까지 취소 불가가 된다"
        );
    }

    /// 표시를 잡은 채로 패닉해도 표시가 남지 않는다(Drop이 도는 것을 확인).
    #[test]
    fn a_claim_is_released_even_when_the_holder_panics() {
        let live = live();
        let id = JobId::generate();

        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let caught = std::panic::catch_unwind({
            let live = Arc::clone(&live);
            move || {
                let _claim = live.claim_cancel(&id).expect("표시를 잡지 못했다");
                panic!("취소 도중 패닉");
            }
        });
        std::panic::set_hook(previous);

        assert!(caught.is_err(), "패닉이 일어나지 않았다");
        assert!(
            !live.cancel_in_flight(&id),
            "패닉으로 표시가 남았다 — 그 잡은 재기동 전까지 취소 불가가 된다"
        );
    }
}
