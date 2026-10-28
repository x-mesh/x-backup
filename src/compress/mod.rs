//! Compress 계층 — 스트리밍 압축(PRD §FR-6, 리서치 stack §3).
//!
//! 압축은 [`PipelineStage`]로 구현되어 백업 파이프라인의 **첫 변환 단계**가 된다
//! (PRD §8.4 compress→encrypt 고정 순서). dump stdout을 받아 zstd로 압축한 바이트
//! 스트림을 다음 단계(암호화)나 Storage로 흘린다.
//!
//! ## 구현: async-compression(zstd)
//! `async_compression::tokio::bufread`의 `ZstdEncoder`/`ZstdDecoder`는
//! `AsyncBufRead`를 받아 `AsyncRead`를 반환하는 어댑터다. [`PipelineStage::wrap`]의
//! `AsyncRead → AsyncRead` 시그니처에 그대로 맞으므로(입력을 `BufReader`로 감싸기만
//! 하면 됨) 별도 채널·태스크 없이 자연 백프레셔로 합성된다.
//!
//! ## 알고리즘
//! `zstd`(기본), `gzip`, `none`(압축 단계 없음)을 지원한다. `none`은 manifest에 압축
//! 메타를 남기지 않으므로 복구 경로는 해제 단계를 만들지 않는다.
//!
//! ## 레벨
//! 압축 레벨은 config `features.compression.level` 기본값 위에 CLI `--compress-level`을
//! 우선 적용한다(우선순위는 호출자 backup.rs가 해석). zstd는 1~22, gzip은 1~9로 클램프한다.
//!
//! ## 역방향(복구)
//! 복구 경로(t5)는 [`ZstdDecompressStage`]로 압축을 푼다. 정방향과 동일한
//! `PipelineStage` 인터페이스라 `reverse_stack_for`가 manifest 메타 기반으로 역순
//! 스택을 조립할 수 있다.

use async_compression::tokio::bufread::{GzipDecoder, GzipEncoder, ZstdDecoder, ZstdEncoder};
use async_compression::Level;
use tokio::io::BufReader;

use crate::error::{Result, XBackupError};
use crate::pipeline::stage::PipelineStage;
use crate::storage::BoxAsyncRead;

/// 압축 알고리즘 식별자(manifest `compression.algorithm`).
pub const ALGORITHM_ZSTD: &str = "zstd";
/// gzip(deflate) 압축 식별자.
pub const ALGORITHM_GZIP: &str = "gzip";
/// 압축하지 않음. manifest에는 기록되지 않고 config 값으로만 쓰인다.
pub const ALGORITHM_NONE: &str = "none";
/// config `compress_algorithm`이 받는 값 전부.
pub const ALGORITHMS: &[&str] = &[ALGORITHM_ZSTD, ALGORITHM_GZIP, ALGORITHM_NONE];

/// 알고리즘과 레벨로 압축 단계를 만든다. `none`이면 None이고, 단계와 함께 클램프 후
/// 레벨을 돌려준다(manifest 기록용).
pub fn compress_stage(
    algorithm: &str,
    level: i32,
) -> Result<Option<(Box<dyn PipelineStage>, i32)>> {
    match algorithm {
        ALGORITHM_ZSTD => {
            let stage = ZstdCompressStage::new(level);
            let level = stage.level();
            Ok(Some((Box::new(stage), level)))
        }
        ALGORITHM_GZIP => {
            let stage = GzipCompressStage::new(level);
            let level = stage.level();
            Ok(Some((Box::new(stage), level)))
        }
        ALGORITHM_NONE => Ok(None),
        other => Err(XBackupError::Config(crate::tr!(
            "unknown compression algorithm: '{other}' (supported: zstd, gzip, none)",
            "알 수 없는 압축 알고리즘: '{other}'(zstd, gzip, none만 지원)"
        ))),
    }
}

/// manifest에 기록된 알고리즘의 해제 단계. 모르는 알고리즘이면 None.
pub fn decompress_stage(algorithm: &str) -> Option<Box<dyn PipelineStage>> {
    match algorithm {
        ALGORITHM_ZSTD => Some(Box::new(ZstdDecompressStage::new())),
        ALGORITHM_GZIP => Some(Box::new(GzipDecompressStage)),
        _ => None,
    }
}

/// zstd 압축 단계 — dump stdout을 압축해 다음 단계로 흘린다(정방향).
///
/// `level`은 manifest에 기록되며, 복구 시 디코더는 레벨을 몰라도 되지만(zstd 프레임에
/// 내장) 진단·재현성을 위해 보존한다.
pub struct ZstdCompressStage {
    level: i32,
}

impl ZstdCompressStage {
    /// 지정 레벨의 zstd 압축 단계를 만든다(zstd 유효 범위 1~22로 클램프).
    pub fn new(level: i32) -> Self {
        Self {
            level: clamp_level(level),
        }
    }

    /// 적용된(클램프 후) 압축 레벨 — manifest 기록용.
    pub fn level(&self) -> i32 {
        self.level
    }
}

impl PipelineStage for ZstdCompressStage {
    fn wrap(self: Box<Self>, input: BoxAsyncRead) -> BoxAsyncRead {
        // async-compression read 어댑터는 AsyncBufRead를 요구하므로 BufReader로 감싼다.
        let buffered = BufReader::new(input);
        let encoder = ZstdEncoder::with_quality(buffered, Level::Precise(self.level));
        Box::pin(encoder)
    }

    fn name(&self) -> &'static str {
        ALGORITHM_ZSTD
    }
}

/// zstd 해제 단계 — 복구 경로에서 압축을 푼다(역방향, t5/`reverse_stack_for` 소비).
pub struct ZstdDecompressStage;

impl ZstdDecompressStage {
    /// zstd 해제 단계를 만든다(파라미터 없음 — 레벨은 프레임에 내장).
    pub fn new() -> Self {
        Self
    }
}

impl Default for ZstdDecompressStage {
    fn default() -> Self {
        Self::new()
    }
}

impl PipelineStage for ZstdDecompressStage {
    fn wrap(self: Box<Self>, input: BoxAsyncRead) -> BoxAsyncRead {
        let buffered = BufReader::new(input);
        let decoder = ZstdDecoder::new(buffered);
        Box::pin(decoder)
    }

    fn name(&self) -> &'static str {
        ALGORITHM_ZSTD
    }
}

/// zstd 유효 레벨 범위(1~22)로 클램프한다. 범위 밖 값은 가장 가까운 경계로 보정한다.
fn clamp_level(level: i32) -> i32 {
    level.clamp(1, 22)
}

/// gzip 압축 단계(정방향). deflate 레벨 범위는 1~9다.
pub struct GzipCompressStage {
    level: i32,
}

impl GzipCompressStage {
    /// 지정 레벨의 gzip 압축 단계를 만든다(1~9로 클램프).
    pub fn new(level: i32) -> Self {
        Self {
            level: level.clamp(1, 9),
        }
    }

    /// 적용된(클램프 후) 압축 레벨 — manifest 기록용.
    pub fn level(&self) -> i32 {
        self.level
    }
}

impl PipelineStage for GzipCompressStage {
    fn wrap(self: Box<Self>, input: BoxAsyncRead) -> BoxAsyncRead {
        let buffered = BufReader::new(input);
        let encoder = GzipEncoder::with_quality(buffered, Level::Precise(self.level));
        Box::pin(encoder)
    }

    fn name(&self) -> &'static str {
        ALGORITHM_GZIP
    }
}

/// gzip 해제 단계(역방향).
pub struct GzipDecompressStage;

impl PipelineStage for GzipDecompressStage {
    fn wrap(self: Box<Self>, input: BoxAsyncRead) -> BoxAsyncRead {
        let buffered = BufReader::new(input);
        let decoder = GzipDecoder::new(buffered);
        Box::pin(decoder)
    }

    fn name(&self) -> &'static str {
        ALGORITHM_GZIP
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    fn reader_from(data: &[u8]) -> BoxAsyncRead {
        Box::pin(std::io::Cursor::new(data.to_vec()))
    }

    async fn drain(reader: BoxAsyncRead) -> Vec<u8> {
        let mut out = Vec::new();
        let mut r = reader;
        r.read_to_end(&mut out).await.unwrap();
        out
    }

    /// zstd 압축 → 해제가 원본을 손실 없이 복원한다(round-trip).
    #[tokio::test]
    async fn zstd_round_trip() {
        // 압축이 유의미하도록 반복 패턴이 섞인 페이로드.
        let payload: Vec<u8> = (0..50_000u32).map(|i| (i % 17) as u8).collect();

        let compress = Box::new(ZstdCompressStage::new(10));
        let compressed = drain(compress.wrap(reader_from(&payload))).await;

        // 압축 산출물은 원본과 달라야 하고(실제 변환), 반복 패턴이라 더 작아야 한다.
        assert_ne!(compressed, payload);
        assert!(
            compressed.len() < payload.len(),
            "압축이 크기를 줄이지 못함: {} >= {}",
            compressed.len(),
            payload.len()
        );

        let decompress = Box::new(ZstdDecompressStage::new());
        let restored = drain(decompress.wrap(reader_from(&compressed))).await;
        assert_eq!(restored, payload, "round-trip 후 원본과 불일치");
    }

    /// 빈 입력도 round-trip이 성립한다(엣지 케이스).
    #[tokio::test]
    async fn zstd_round_trip_empty() {
        let compress = Box::new(ZstdCompressStage::new(3));
        let compressed = drain(compress.wrap(reader_from(b""))).await;
        let decompress = Box::new(ZstdDecompressStage::new());
        let restored = drain(decompress.wrap(reader_from(&compressed))).await;
        assert!(restored.is_empty());
    }

    /// 레벨은 zstd 유효 범위(1~22)로 클램프된다.
    #[test]
    fn level_is_clamped() {
        assert_eq!(ZstdCompressStage::new(0).level(), 1);
        assert_eq!(ZstdCompressStage::new(-5).level(), 1);
        assert_eq!(ZstdCompressStage::new(100).level(), 22);
        assert_eq!(ZstdCompressStage::new(10).level(), 10);
    }

    #[tokio::test]
    async fn gzip_round_trip_through_the_factories() {
        let payload: Vec<u8> = (0..50_000u32).map(|i| (i % 17) as u8).collect();

        let (compress, level) = compress_stage(ALGORITHM_GZIP, 6).unwrap().unwrap();
        assert_eq!(level, 6);
        assert_eq!(compress.name(), "gzip");
        let compressed = drain(compress.wrap(reader_from(&payload))).await;
        assert_eq!(&compressed[..2], &[0x1f, 0x8b], "gzip magic이 아님");
        assert!(compressed.len() < payload.len());

        let decompress = decompress_stage(ALGORITHM_GZIP).unwrap();
        let restored = drain(decompress.wrap(reader_from(&compressed))).await;
        assert_eq!(restored, payload);
    }

    #[test]
    fn factories_cover_every_algorithm() {
        assert_eq!(compress_stage(ALGORITHM_ZSTD, 30).unwrap().unwrap().1, 22);
        assert_eq!(compress_stage(ALGORITHM_GZIP, 30).unwrap().unwrap().1, 9);
        assert_eq!(compress_stage(ALGORITHM_GZIP, 0).unwrap().unwrap().1, 1);
        assert!(compress_stage(ALGORITHM_NONE, 10).unwrap().is_none());
        assert!(compress_stage("lz4", 1).is_err());

        assert!(decompress_stage(ALGORITHM_ZSTD).is_some());
        assert!(decompress_stage(ALGORITHM_GZIP).is_some());
        assert!(decompress_stage(ALGORITHM_NONE).is_none());
    }

    /// 단계 이름은 manifest 알고리즘 식별자와 일치한다.
    #[test]
    fn stage_name_is_zstd() {
        assert_eq!(ZstdCompressStage::new(10).name(), "zstd");
        assert_eq!(ZstdDecompressStage::new().name(), "zstd");
    }
}
