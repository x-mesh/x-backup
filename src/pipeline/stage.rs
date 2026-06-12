//! 파이프라인 단계 합성 — t6(압축·암호화)의 삽입 지점(리서치 architecture §3).
//!
//! 백업 파이프라인은 dump stdout부터 Storage 입력까지 일련의 **스트림 변환 단계**로
//! 구성된다. 현재(t4)는 평문이므로 단계가 비어 있고, t6이 compress·encrypt 단계를
//! 이 합성 지점에 끼운다.
//!
//! ## 합성 모델: `Vec<Box<dyn PipelineStage>>`
//! 각 단계는 [`BoxAsyncRead`]를 받아 변환된 [`BoxAsyncRead`]를 돌려주는 어댑터다.
//! [`StageStack`]이 단계들을 **선언 순서대로** 합성한다:
//!
//! ```text
//! dump stdout ─▶ stage[0] ─▶ stage[1] ─▶ … ─▶ (sha256 tee) ─▶ put_stream
//! ```
//!
//! ### t6이 끼우는 방법
//! t6은 `Compress`/`Crypto`를 각각 [`PipelineStage`]로 구현한 뒤,
//! [`StageStack::push`]로 **compress → encrypt 순서**(PRD §8.4 고정)로 쌓기만 하면
//! 된다. 파이프라인(backup.rs)·체크섬·Storage 계층은 한 줄도 바뀌지 않는다 —
//! 단계 목록을 만드는 코드만 t6이 채운다. 순서 규칙(compress 먼저, encrypt 나중)은
//! push 순서로 표현되고, 체크섬은 항상 *마지막 단계의 출력*(= 저장 바이트)에 걸린다
//! (PRD §8.5, 리서치 §3 "체크섬 기준점 = 암호화 후 저장 바이트").
//!
//! ## 동기 변환 + 내부 async I/O
//! [`PipelineStage::wrap`]은 **동기 메서드**다(리서치 architecture §2: Crypto/Compress는
//! 스트림 어댑터를 즉시 반환, 실제 I/O는 어댑터의 `poll_read` 안에서 async). 따라서
//! 단계 합성 자체에는 await가 없고, 데이터가 흐를 때 각 어댑터의 `poll_read`가
//! 자연 백프레셔로 연결된다.

use crate::storage::BoxAsyncRead;

/// 단일 스트림 변환 단계.
///
/// 입력 reader를 소비해 변환된 reader를 반환한다. 압축·암호화처럼 바이트를
/// 변형하는 단계가 이 trait를 구현한다(t6). 단계는 상태를 가질 수 있으므로
/// (압축 레벨, age recipient 등) `self`를 통해 구성값을 보관한다.
pub trait PipelineStage: Send {
    /// `input`을 감싼 변환 reader를 반환한다.
    ///
    /// 동기 호출이며 즉시 어댑터를 돌려준다 — 실제 압축/암호화는 반환된 reader가
    /// 폴링될 때 수행된다.
    fn wrap(self: Box<Self>, input: BoxAsyncRead) -> BoxAsyncRead;

    /// 로깅·진단용 단계 이름(예: `"zstd"`, `"age"`).
    fn name(&self) -> &'static str;
}

/// 순서가 있는 단계 모음. push한 순서대로 입력에 적용된다.
///
/// t4 기본 구성은 비어 있다(평문 = identity). t6이 compress·encrypt 단계를 push해
/// 동일 [`apply`](Self::apply) 합성을 통과시킨다.
#[derive(Default)]
pub struct StageStack {
    stages: Vec<Box<dyn PipelineStage>>,
}

impl StageStack {
    /// 빈 스택(identity 파이프라인 — 평문)을 만든다.
    pub fn new() -> Self {
        Self::default()
    }

    /// 단계를 끝에 추가한다(later push = 데이터 흐름상 더 나중에 적용).
    ///
    /// t6은 `push(compress)` 후 `push(encrypt)`로 PRD §8.4 순서를 표현한다.
    pub fn push(&mut self, stage: Box<dyn PipelineStage>) -> &mut Self {
        self.stages.push(stage);
        self
    }

    /// 등록된 단계가 없으면 true(평문 파이프라인).
    pub fn is_identity(&self) -> bool {
        self.stages.is_empty()
    }

    /// 합성된 단계 이름 목록(로깅·manifest 진단용).
    pub fn stage_names(&self) -> Vec<&'static str> {
        self.stages.iter().map(|s| s.name()).collect()
    }

    /// 모든 단계를 선언 순서대로 `source`에 적용한 최종 reader를 반환한다.
    ///
    /// 단계가 없으면 `source`를 그대로 돌려준다(identity). 반환된 reader가
    /// sha256 tee로 감싸여 Storage로 흐른다.
    pub fn apply(self, source: BoxAsyncRead) -> BoxAsyncRead {
        let mut reader = source;
        for stage in self.stages {
            reader = stage.wrap(reader);
        }
        reader
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    fn reader_from(data: &[u8]) -> BoxAsyncRead {
        Box::pin(std::io::Cursor::new(data.to_vec()))
    }

    /// 빈 스택은 입력을 변형 없이 통과시킨다(identity = 평문 t4 경로).
    #[tokio::test]
    async fn identity_stack_passes_bytes_through() {
        let stack = StageStack::new();
        assert!(stack.is_identity());
        let mut out = Vec::new();
        stack
            .apply(reader_from(b"plaintext dump bytes"))
            .read_to_end(&mut out)
            .await
            .unwrap();
        assert_eq!(out, b"plaintext dump bytes");
    }

    /// 단계는 push 순서대로 합성된다 — t6의 compress→encrypt 순서 표현을 모사한다.
    /// 여기서는 "각 단계가 접미사를 붙이는" 가짜 변환으로 순서만 검증한다.
    #[tokio::test]
    async fn stages_compose_in_push_order() {
        // 입력 끝에 태그 바이트를 덧붙이는 테스트용 단계.
        struct AppendStage(&'static str);
        impl PipelineStage for AppendStage {
            fn wrap(self: Box<Self>, input: BoxAsyncRead) -> BoxAsyncRead {
                Box::pin(input.chain(std::io::Cursor::new(self.0.as_bytes().to_vec())))
            }
            fn name(&self) -> &'static str {
                "append"
            }
        }

        let mut stack = StageStack::new();
        stack
            .push(Box::new(AppendStage("|first")))
            .push(Box::new(AppendStage("|second")));
        assert_eq!(stack.stage_names(), vec!["append", "append"]);

        let mut out = Vec::new();
        stack
            .apply(reader_from(b"base"))
            .read_to_end(&mut out)
            .await
            .unwrap();
        // first가 먼저 감싸고, second가 그 위를 감싸므로 base|first|second 순.
        assert_eq!(out, b"base|first|second");
    }
}
