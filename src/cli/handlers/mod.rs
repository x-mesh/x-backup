//! 서브커맨드 핸들러 — CLI 인자를 도메인 로직으로 연결한다.
//!
//! [`backup`](t4)·[`restore`](t5)가 실제 구현이고, 나머지는 [`super::exit`]가 미구현
//! 스텁으로 둔다. 후속 태스크가 각 핸들러를 추가한다(status=t14 등).

pub mod backup;
pub mod restore;
