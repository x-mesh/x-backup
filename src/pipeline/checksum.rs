//! sha256 tee — 단일 패스 스트리밍 체크섬(리서치 architecture §3).
//!
//! [`Sha256Reader`]는 감싼 reader를 통과하는 모든 바이트를 `poll_read` 내부에서
//! sha256 hasher에 누산하는 커스텀 [`AsyncRead`]다. **채널·별도 태스크 없이**
//! 데이터가 흐르는 그대로 해시를 계산하므로 추가 백프레셔 문제가 없다.
//!
//! ## 기준점
//! 파이프라인(§7)은 이 reader를 **Storage에 실제로 쓰이는 최종 바이트**(현 단계는
//! 평문 dump, t6 이후 압축·암호화 후 바이트) 직전에 끼운다. 따라서 `finalize`가
//! 반환하는 해시는 저장된 바이트와 1:1 대응하며, 키 없이도 구조 검증이 가능하다
//! (PRD §8.5 공개키-only 호스트).
//!
//! ## finalize 접근
//! hasher 상태를 [`Arc<Mutex<…>>`]로 공유한다. [`Sha256Reader::handle`]로 얻은
//! [`ChecksumHandle`]은 reader가 EOF까지 소비된 *뒤에* `finalize`로 hex 다이제스트를
//! 회수한다. reader 자신이 `Storage::put_stream`에 move-out되더라도(소유권 이전)
//! 핸들은 외부에 남아 업로드 완료 후 해시를 읽을 수 있다 — oneshot 대신 공유 상태를
//! 택한 이유다(EOF 시점에 값을 "밀어내지" 않고, 호출자가 필요할 때 "당겨온다").

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, ReadBuf};

/// hasher 공유 상태. `Some`이면 누산 중, `finalize` 후에는 `None`.
type SharedHasher = Arc<Mutex<Option<Sha256>>>;

/// 통과하는 바이트를 sha256에 누산하는 [`AsyncRead`] 래퍼.
///
/// `R`은 감쌀 내부 reader(현재는 dump stdout, t6 이후 압축/암호화 어댑터).
pub struct Sha256Reader<R> {
    inner: R,
    hasher: SharedHasher,
}

impl<R: AsyncRead> Sha256Reader<R> {
    /// `inner`를 감싸 통과 바이트를 누산하는 reader를 만든다.
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: Arc::new(Mutex::new(Some(Sha256::new()))),
        }
    }

    /// 업로드 완료 후 다이제스트를 회수할 핸들을 반환한다.
    ///
    /// reader를 `put_stream`에 넘기기 *전에* 핸들을 떠 두면, 업로드가 끝난 뒤
    /// [`ChecksumHandle::finalize`]로 hex 해시를 읽을 수 있다.
    pub fn handle(&self) -> ChecksumHandle {
        ChecksumHandle {
            hasher: Arc::clone(&self.hasher),
        }
    }
}

/// [`Sha256Reader`]의 누산 결과를 회수하는 핸들.
///
/// reader가 소비된 뒤(EOF) [`finalize`](Self::finalize)를 호출한다. 한 번만
/// 유효하다 — `finalize`가 내부 hasher를 꺼내(take) 소비하기 때문이다.
#[derive(Clone)]
pub struct ChecksumHandle {
    hasher: SharedHasher,
}

impl ChecksumHandle {
    /// 누산된 sha256을 소문자 hex 문자열로 확정한다.
    ///
    /// reader가 끝까지 소비되지 않은 채 호출하면 *그 시점까지의* 부분 해시를
    /// 반환하므로, 반드시 업로드(`put_stream`) 완료 후에 호출해야 한다.
    /// 이미 finalize되었으면 `None`.
    pub fn finalize(&self) -> Option<String> {
        let mut guard = self.hasher.lock().expect("sha256 hasher mutex poisoned");
        guard.take().map(|h| hex::encode(h.finalize()))
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for Sha256Reader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        // 누산 대상은 이번 poll에서 *새로* 채워진 바이트뿐이므로, 호출 전 길이를
        // 기록해 두고 차이만 hasher에 넣는다(이미 버퍼에 있던 바이트 재누산 방지).
        let before = buf.filled().len();
        let inner = Pin::new(&mut self.inner);
        let poll = inner.poll_read(cx, buf);

        if let Poll::Ready(Ok(())) = &poll {
            let newly = &buf.filled()[before..];
            if !newly.is_empty() {
                if let Some(hasher) = self
                    .hasher
                    .lock()
                    .expect("sha256 hasher mutex poisoned")
                    .as_mut()
                {
                    hasher.update(newly);
                }
            }
        }
        poll
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    /// 빈 입력의 sha256은 알려진 상수와 같아야 한다.
    #[tokio::test]
    async fn empty_input_known_digest() {
        let reader = Sha256Reader::new(std::io::Cursor::new(Vec::<u8>::new()));
        let handle = reader.handle();
        let mut reader = reader;
        let mut out = Vec::new();
        reader.read_to_end(&mut out).await.unwrap();
        assert_eq!(
            handle.finalize().unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    /// "abc"의 sha256은 FIPS 180-2 부록 예제 벡터와 같아야 한다.
    #[tokio::test]
    async fn known_vector_abc() {
        let reader = Sha256Reader::new(std::io::Cursor::new(b"abc".to_vec()));
        let handle = reader.handle();
        let mut reader = reader;
        let mut out = Vec::new();
        reader.read_to_end(&mut out).await.unwrap();
        // 통과 바이트는 원본 그대로여야 한다(tee는 변형하지 않음).
        assert_eq!(out, b"abc");
        assert_eq!(
            handle.finalize().unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    /// 청크 경계를 넘는 큰 입력도 표준 라이브러리 일괄 계산과 일치해야 한다.
    #[tokio::test]
    async fn large_input_matches_oneshot() {
        let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let expected = hex::encode(Sha256::digest(&payload));

        let reader = Sha256Reader::new(std::io::Cursor::new(payload.clone()));
        let handle = reader.handle();
        let mut reader = reader;
        let mut out = Vec::new();
        reader.read_to_end(&mut out).await.unwrap();

        assert_eq!(out, payload);
        assert_eq!(handle.finalize().unwrap(), expected);
    }

    /// finalize는 한 번만 값을 반환한다(두 번째는 None).
    #[tokio::test]
    async fn finalize_is_single_shot() {
        let reader = Sha256Reader::new(std::io::Cursor::new(b"x".to_vec()));
        let handle = reader.handle();
        let mut reader = reader;
        let mut out = Vec::new();
        reader.read_to_end(&mut out).await.unwrap();
        assert!(handle.finalize().is_some());
        assert!(handle.finalize().is_none());
    }
}
