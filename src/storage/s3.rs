//! S3 호환 스토리지 백엔드(PRD FR-4, §11).
//!
//! object_store `aws` feature의 [`AmazonS3`]를 [`Storage`] trait로 래핑한다.
//! LocalFs(t3)와 동일한 trait·스트림 경계(`BoxAsyncRead`)·abort 처리 패턴을
//! 그대로 따른다 — 파이프라인은 백엔드를 런타임에 교체할 수 있다.
//!
//! ## S3 호환 대상
//! MinIO·Cloudflare R2·OCI Object Storage 등 커스텀 엔드포인트를 지원한다.
//! - `with_endpoint`로 커스텀 엔드포인트 지정.
//! - **path-style**(virtual-hosted 비활성)이 object_store 기본값이며, S3 호환
//!   구현(MinIO 등)은 가상 호스트 스타일을 지원하지 않는 경우가 많아 path-style을
//!   강제한다([`AmazonS3Builder::with_virtual_hosted_style_request(false)`]).
//! - HTTP 허용(`with_allow_http`)은 **엔드포인트 스킴이 `http://`일 때만** 켠다
//!   (로컬 MinIO 테스트용). 운영(HTTPS)에서는 평문 전송을 막는다.
//!
//! ## 스트리밍 멀티파트(디스크 경유 없음)
//! `put_stream`은 reader를 [`ReaderStream`]으로 청크화해 [`WriteMultipart`]에
//! 공급한다. 파트 크기는 [`compute_part_size`]로 적응 산정한다(아래 §파트 크기).
//! 전체를 메모리/디스크에 적재하지 않는다(PRD §7 스트리밍).
//!
//! ## 파트 크기 적응(리서치 pitfalls §5-1)
//! S3 멀티파트는 **파트당 최소 5MiB, 최대 10,000파트**다. 기본 16MiB로 시작하되,
//! `content_length_hint`가 크면 `ceil(hint / 9500)`(파트 수를 9,500 이하로 묶는
//! 안전 여유)과 비교해 더 큰 값을 채택한다 — 한도(10,000)에 닿기 전에 파트 크기를
//! 키워 대용량도 단일 업로드로 처리한다.
//!
//! ## 부분 산출물 정리(PRD §11 신뢰성)
//! 정상 완료 시에만 [`WriteMultipart::finish`]로 커밋한다. 도중 reader I/O 실패가
//! 나면 같은 함수에서 [`WriteMultipart::abort`]를 호출해 서버의 미완료 파트를
//! 제거한다 — abort 누락은 스토리지 비용 누수를 부른다(리서치 §5-2). 함수 경로를
//! 벗어나는 패닉/조기 반환에 대비해 [`UploadGuard`] RAII 가드를 2차 안전망으로 둔다.
//!
//! ## 무결성: ETag가 아닌 자체 sha256이 primary(리서치 §5-3)
//! 멀티파트 ETag는 구현체마다 합성 방식이 달라(MinIO #20649 등) 무결성 기준으로
//! 쓰지 않는다. 이 백엔드는 ETag를 노출하지 않으며([`StorageEntry`]에도 없음),
//! 무결성은 상위 파이프라인의 자체 sha256 사이드카가 담보한다.

use std::pin::Pin;

use futures::StreamExt;
use object_store::aws::{AmazonS3, AmazonS3Builder};
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, ObjectStoreExt, WriteMultipart};
use tokio::io::AsyncRead;
use tokio_util::io::{ReaderStream, StreamReader};

use super::{BoxAsyncRead, Storage, StorageEntry};
use crate::config::file::S3Config;
use crate::error::XBackupError;

/// `put_stream`이 reader에서 한 번에 읽어 업로드 버퍼로 넘기는 청크 폴링 단위.
///
/// 멀티파트 파트 크기가 아니라 reader 폴링 크기다. `WriteMultipart`가 이 청크를
/// 모아 [`compute_part_size`]가 정한 파트로 다시 분할하므로, 백프레셔/메모리
/// 균형점으로 8MiB를 택한다(LocalFs와 동일).
const READ_CHUNK_BYTES: usize = 8 * 1024 * 1024;

/// 멀티파트 기본 파트 크기(16MiB). S3 최소(5MiB)를 충분히 상회하며,
/// 16MiB × 10,000파트 = 160GiB까지 적응 없이 커버한다(리서치 §5-1).
const DEFAULT_PART_SIZE: usize = 16 * 1024 * 1024;

/// 적응 파트 크기 산정 시 묶을 목표 최대 파트 수. S3 한도(10,000)에 여유를 둔다
/// — 9,500으로 잡아 마지막 파트/반올림 오차에도 한도를 넘지 않게 한다(리서치 §5-1).
const TARGET_MAX_PARTS: u64 = 9_500;

/// 멀티파트 동시 업로드 파트 수 상한. `WriteMultipart`의 인플라이트 파트 수를
/// 제한해 메모리 사용을 묶는다(파트 크기 × 동시성 ≈ 피크 메모리).
const MAX_CONCURRENCY: usize = 4;

/// `content_length_hint`로부터 멀티파트 파트 크기를 적응 산정한다.
///
/// - hint가 없으면 [`DEFAULT_PART_SIZE`](16MiB)를 쓴다.
/// - hint가 있으면 `max(16MiB, ceil(hint / 9500))`을 택한다 — 파트 수를 9,500
///   이하로 묶어 S3의 10,000파트 한도에 닿기 전에 파트 크기를 키운다.
///
/// 반환값은 항상 16MiB 이상이므로 S3 최소 파트(5MiB) 제약을 자동으로 만족한다.
fn compute_part_size(content_length_hint: Option<u64>) -> usize {
    let Some(len) = content_length_hint else {
        return DEFAULT_PART_SIZE;
    };
    if len == 0 {
        return DEFAULT_PART_SIZE;
    }
    // ceil(len / TARGET_MAX_PARTS) — 정수 천장 나눗셈.
    let adaptive = len.div_ceil(TARGET_MAX_PARTS);
    // usize로 좁히되, 산정값이 usize를 넘으면(이론상 초대용량) usize::MAX로 포화.
    let adaptive = usize::try_from(adaptive).unwrap_or(usize::MAX);
    adaptive.max(DEFAULT_PART_SIZE)
}

/// S3 호환 자격증명(access key / secret key).
///
/// ## `credentials_env` 형식 (config 스키마 결정 사항)
/// config의 `destination.s3.credentials_env`는 **단일 환경변수 이름**을 가리키며,
/// 그 값은 `"<ACCESS_KEY>:<SECRET_KEY>"` 형식이다(콜론 1개 구분). 콜론 이후 전체를
/// secret으로 취급하므로 secret에 콜론이 포함돼도 안전하다(첫 콜론에서만 분리).
///
/// 예) `export S3_CREDS="AKIA...:wJalr..."` 후 config에 `credentials_env = "S3_CREDS"`.
///
/// 시크릿 평문은 [`Secret`](crate::config::Secret) 정책에 따라 로그/Debug에
/// 노출하지 않는다 — 이 구조체는 `build_store`가 빌더에 주입할 때만 잠깐 다룬다.
struct S3Credentials {
    access_key_id: String,
    secret_access_key: String,
}

impl S3Credentials {
    /// `"ACCESS:SECRET"` 형식 문자열을 파싱한다(첫 콜론에서 1회만 분리).
    fn parse(raw: &str) -> Result<Self, XBackupError> {
        let (access, secret) = raw.split_once(':').ok_or_else(|| {
            XBackupError::Config(
                "S3 credentials_env 형식 오류: 'ACCESS_KEY:SECRET_KEY' 형식이어야 합니다"
                    .to_string(),
            )
        })?;
        if access.is_empty() || secret.is_empty() {
            return Err(XBackupError::Config(
                "S3 credentials_env 형식 오류: access key 또는 secret key가 비어 있습니다"
                    .to_string(),
            ));
        }
        Ok(Self {
            access_key_id: access.to_string(),
            secret_access_key: secret.to_string(),
        })
    }
}

/// S3 호환 스토리지 백엔드.
///
/// 생성 시 config의 `prefix`를 백엔드 루트로 삼아, 모든 trait 경로를 그 아래에
/// 결합한다(`<prefix>/<path>`). prefix가 없으면 버킷 루트가 기준이다.
#[derive(Debug)]
pub struct S3Compatible {
    inner: AmazonS3,
    /// config destination.s3.prefix(정규화: 앞뒤 `/` 제거). 비어 있으면 버킷 루트.
    prefix: String,
}

impl S3Compatible {
    /// config의 `[destination.s3]` 블록과 자격증명 원시 문자열로 백엔드를 만든다.
    ///
    /// `credentials_raw`는 `credentials_env`가 가리키는 환경변수의 **이미 해석된
    /// 값**이다(`"ACCESS:SECRET"`). env 조회는 호출자(팩토리)가 수행한다 — 이
    /// 계층은 형식만 파싱한다.
    ///
    /// HTTP 허용은 `endpoint`가 `http://`로 시작할 때만 켠다(로컬 테스트).
    pub fn new(cfg: &S3Config, credentials_raw: &str) -> Result<Self, XBackupError> {
        let bucket = cfg.bucket.as_deref().ok_or_else(|| {
            XBackupError::Config("S3 destination에 bucket이 필요합니다".to_string())
        })?;
        let creds = S3Credentials::parse(credentials_raw)?;

        let mut builder = AmazonS3Builder::new()
            .with_bucket_name(bucket)
            .with_access_key_id(creds.access_key_id)
            .with_secret_access_key(creds.secret_access_key)
            // path-style 강제(S3 호환 구현 호환성). object_store 기본값이지만 명시한다.
            .with_virtual_hosted_style_request(false);

        if let Some(region) = cfg.region.as_deref() {
            builder = builder.with_region(region);
        }

        if let Some(endpoint) = cfg.endpoint.as_deref() {
            // 커스텀 엔드포인트가 http면(로컬 MinIO 등) 평문 전송을 허용한다.
            let allow_http = endpoint.starts_with("http://");
            builder = builder.with_endpoint(endpoint).with_allow_http(allow_http);
        }

        let inner = builder.build().map_err(|e| {
            XBackupError::Config(format!("S3 백엔드 초기화 실패: {e}"))
        })?;

        Ok(Self {
            inner,
            prefix: normalize_prefix(cfg.prefix.as_deref()),
        })
    }

    /// trait의 `&str` 경로에 config prefix를 결합해 object_store `Path`로 변환한다.
    ///
    /// prefix가 비어 있으면 경로를 그대로, 있으면 `<prefix>/<path>`로 합성한다.
    /// `ObjectPath::from`은 세그먼트를 안전하게 인코딩한다(빈 세그먼트 정리 등).
    fn object_path(&self, path: &str) -> ObjectPath {
        if self.prefix.is_empty() {
            ObjectPath::from(path)
        } else {
            ObjectPath::from(format!("{}/{}", self.prefix, path))
        }
    }

    /// `list` 결과의 절대 object 경로에서 config prefix를 떼어 trait 상대 경로로 되돌린다.
    ///
    /// prefix가 없으면 그대로 반환한다. prefix로 시작하지 않는 경로는(이론상 없음)
    /// 그대로 둔다 — 방어적 처리.
    fn strip_prefix(&self, location: &str) -> String {
        if self.prefix.is_empty() {
            return location.to_string();
        }
        let with_sep = format!("{}/", self.prefix);
        location
            .strip_prefix(&with_sep)
            .unwrap_or(location)
            .to_string()
    }
}

/// config prefix를 정규화한다 — 앞뒤 `/`를 제거하고 빈 값은 빈 문자열로.
///
/// `None`/빈 문자열/`"/"` 모두 "prefix 없음"(버킷 루트)으로 매핑한다.
fn normalize_prefix(prefix: Option<&str>) -> String {
    prefix
        .unwrap_or("")
        .trim_matches('/')
        .to_string()
}

#[async_trait::async_trait]
impl Storage for S3Compatible {
    async fn put_stream(
        &self,
        path: &str,
        reader: BoxAsyncRead,
        content_length_hint: Option<u64>,
    ) -> Result<(), XBackupError> {
        let object_path = self.object_path(path);
        let part_size = compute_part_size(content_length_hint);

        // 커밋 전 Drop되면(패닉·조기 반환) best-effort 삭제를 시도하는 2차 안전망.
        let mut guard = UploadGuard::new(&self.inner, object_path.clone());

        let upload = self.inner.put_multipart(&object_path).await.map_err(|e| {
            XBackupError::StorageUpload(format!("'{path}' 멀티파트 업로드 시작 실패: {e}"))
        })?;
        // 적응 파트 크기로 WriteMultipart 구성 — 내부에서 청크를 모아 이 크기 파트로 분할.
        let mut writer = WriteMultipart::new_with_chunk_size(upload, part_size);

        // reader를 청크 스트림으로 변환해 업로드 버퍼에 순차 공급한다.
        let mut chunks = ReaderStream::with_capacity(reader, READ_CHUNK_BYTES);
        while let Some(chunk) = chunks.next().await {
            match chunk {
                Ok(bytes) => {
                    // 인플라이트 파트 수를 제한해 메모리를 묶는다(파트 크기 × 동시성).
                    // 용량 확보 실패(업로드 측 오류)는 abort 후 에러 반환.
                    if let Err(cap_err) = writer.wait_for_capacity(MAX_CONCURRENCY).await {
                        let _ = writer.abort().await;
                        guard.disarm();
                        return Err(XBackupError::StorageUpload(format!(
                            "'{path}' 업로드 용량 확보 실패: {cap_err}"
                        )));
                    }
                    writer.put(bytes);
                }
                Err(read_err) => {
                    // reader 측 I/O 실패: 미완료 멀티파트를 abort해 서버 잔여 파트 제거.
                    let _ = writer.abort().await;
                    // abort가 처리했으므로 가드의 추가 삭제는 불필요.
                    guard.disarm();
                    return Err(XBackupError::StorageUpload(format!(
                        "'{path}' 입력 스트림 읽기 실패: {read_err}"
                    )));
                }
            }
        }

        // 마지막 파트 flush + complete. object_store의 `WriteMultipart::finish`는
        // complete() 실패 시 내부에서 `upload.abort()`를 호출해 업로드된 파트를
        // 스스로 정리한다(upload.rs §finish). 따라서 finish 에러 경로의 서버측 잔여
        // 파트 정리는 object_store가 1차로 담당한다. finish가 writer를 소비하므로
        // 여기서 추가 abort는 호출할 수 없고, 가드는 disarm하지 않은 채 둔다 —
        // complete 직전 단계(wait_for_capacity) 실패 등 내부 abort가 닿지 않는
        // 드문 경로를 위한 2차 best-effort 삭제로 남긴다.
        if let Err(finish_err) = writer.finish().await {
            return Err(XBackupError::StorageUpload(format!(
                "'{path}' 멀티파트 업로드 완료 실패: {finish_err}"
            )));
        }

        // 정상 커밋 — 가드 해제(삭제 시도하지 않음).
        guard.disarm();
        Ok(())
    }

    async fn get_stream(&self, path: &str) -> Result<BoxAsyncRead, XBackupError> {
        let object_path = self.object_path(path);
        let result = self.inner.get(&object_path).await.map_err(|e| {
            XBackupError::StorageDownload(format!("'{path}' 다운로드 실패: {e}"))
        })?;

        // object_store 바이트 스트림(에러 타입)을 io::Error로 매핑해 StreamReader에 연결.
        let byte_stream = result
            .into_stream()
            .map(|r| r.map_err(std::io::Error::other));
        let reader = StreamReader::new(byte_stream);
        Ok(Box::pin(reader) as Pin<Box<dyn AsyncRead + Send>>)
    }

    async fn list(&self, prefix: &str) -> Result<Vec<StorageEntry>, XBackupError> {
        // trait prefix에 config prefix를 결합한다. 빈 trait prefix는 config 루트 전체.
        let object_prefix = if prefix.is_empty() {
            if self.prefix.is_empty() {
                None
            } else {
                Some(ObjectPath::from(self.prefix.clone()))
            }
        } else {
            Some(self.object_path(prefix))
        };

        let mut stream = self.inner.list(object_prefix.as_ref());
        let mut entries = Vec::new();
        while let Some(meta) = stream.next().await {
            let meta = meta.map_err(|e| {
                XBackupError::StorageDownload(format!("'{prefix}' 목록 조회 실패: {e}"))
            })?;
            entries.push(StorageEntry {
                // 절대 경로에서 config prefix를 떼어 trait 상대 경로로 되돌린다.
                path: self.strip_prefix(meta.location.as_ref()),
                size: meta.size,
                last_modified: Some(meta.last_modified.to_rfc3339()),
            });
        }
        Ok(entries)
    }

    async fn delete(&self, path: &str) -> Result<(), XBackupError> {
        let object_path = self.object_path(path);
        self.inner.delete(&object_path).await.map_err(|e| {
            XBackupError::StorageDownload(format!("'{path}' 삭제 실패: {e}"))
        })
    }
}

/// 업로드 부분 산출물 정리용 RAII 가드(LocalFs와 동일 전략, 리서치 architecture §4).
///
/// `put_stream`이 정상 커밋하면 [`disarm`](Self::disarm)으로 무장 해제된다. 커밋
/// 전에 Drop되거나(패닉·조기 반환) `finish`가 실패하면, `Drop`에서 best-effort로
/// 완성된 객체 삭제를 시도한다.
///
/// ## abort vs 가드의 역할 분리
/// - reader I/O 실패 등 **예상 경로**는 `put_stream`이 직접 [`WriteMultipart::abort`]로
///   서버의 미완료 파트를 즉시 제거하고 가드를 disarm한다(서버측 잔여 파트 정리).
/// - `finish` 실패·패닉 등 **비정상 경로**는 가드의 best-effort `delete`가 2차로
///   덮는다(이미 일부 커밋됐을 수 있는 객체 제거).
///
/// ## 비동기 Drop 제약(LocalFs 주석과 동일 한계)
/// Rust에 async `Drop`이 없으므로, 현재 tokio 런타임이 살아 있을 때만
/// [`Handle::spawn`](tokio::runtime::Handle)으로 정리 태스크를 띄우는 best-effort
/// 전략이다. 런타임 종료/즉시 프로세스 종료 시 완료를 보장하지 못한다 — 따라서
/// 가드는 정상 경로의 abort/finish를 대체하지 않는 2차 안전망이다. 영구적 잔재는
/// 상위 계층(orphan 스캔 + manifest "incomplete" 표기)과 버킷 Lifecycle
/// 정책(AbortIncompleteMultipartUpload)이 최종 담보한다(리서치 §5-2, §7-2).
struct UploadGuard {
    /// 정리 대상 object 경로. `disarm` 후에는 `None`.
    target: Option<ObjectPath>,
    /// 삭제를 수행할 백엔드 클론(`AmazonS3`은 내부 Arc로 저렴하게 클론 가능).
    store: AmazonS3,
}

impl UploadGuard {
    fn new(store: &AmazonS3, target: ObjectPath) -> Self {
        Self {
            target: Some(target),
            store: store.clone(),
        }
    }

    /// 정상 커밋·명시적 abort 후 호출 — Drop 시 삭제를 시도하지 않게 한다.
    fn disarm(&mut self) {
        self.target = None;
    }
}

impl Drop for UploadGuard {
    fn drop(&mut self) {
        let Some(target) = self.target.take() else {
            return; // 무장 해제됨 — 정상 경로.
        };

        // 동기 Drop에서 async delete를 직접 await할 수 없다. 현재 tokio 런타임
        // 핸들이 있으면 best-effort로 정리 태스크를 spawn한다(위 한계 주석 참조).
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let store = self.store.clone();
            handle.spawn(async move {
                // 실패(이미 없음 등)는 무시 — best-effort.
                let _ = store.delete(&target).await;
            });
        }
        // 런타임 핸들이 없으면 동기 삭제 경로가 없으므로 건너뛴다. 잔재는
        // 상위 계층(orphan 스캔/incomplete 표기) + 버킷 Lifecycle이 최종 처리한다.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// hint가 없으면 기본 파트 크기(16MiB)를 쓴다.
    #[test]
    fn part_size_defaults_without_hint() {
        assert_eq!(compute_part_size(None), DEFAULT_PART_SIZE);
        assert_eq!(compute_part_size(Some(0)), DEFAULT_PART_SIZE);
    }

    /// 작은 hint(160GiB 이하)는 기본 16MiB로 충분 — 적응 없이 16MiB.
    #[test]
    fn part_size_stays_at_default_for_small_hints() {
        // 16MiB × 9500 = 152GiB 미만이면 ceil(hint/9500) < 16MiB → 16MiB 채택.
        let small = 100 * 1024 * 1024; // 100MiB
        assert_eq!(compute_part_size(Some(small)), DEFAULT_PART_SIZE);

        // 정확히 16MiB × 9500 경계 직전.
        let near = DEFAULT_PART_SIZE as u64 * TARGET_MAX_PARTS - 1;
        assert_eq!(compute_part_size(Some(near)), DEFAULT_PART_SIZE);
    }

    /// 큰 hint는 파트 수를 9,500 이하로 묶도록 파트 크기를 키운다.
    #[test]
    fn part_size_grows_adaptively_for_large_hints() {
        // 16MiB × 9500을 초과하면 ceil(hint/9500) > 16MiB → 적응 증가.
        let large = DEFAULT_PART_SIZE as u64 * TARGET_MAX_PARTS + 1;
        let ps = compute_part_size(Some(large));
        assert!(ps > DEFAULT_PART_SIZE, "큰 hint는 파트 크기를 키워야 한다: {ps}");

        // 파트 수가 한도(10,000) 아래인지 — 산정 파트 크기로 나눈 파트 수 검증.
        let parts = large.div_ceil(ps as u64);
        assert!(parts <= 10_000, "적응 파트 크기로도 10,000파트를 넘었다: {parts}");
    }

    /// 초대용량(1TiB)도 10,000파트 한도 안에서 단일 업로드로 처리 가능해야 한다.
    #[test]
    fn part_size_handles_terabyte_within_part_limit() {
        let one_tib = 1024u64 * 1024 * 1024 * 1024;
        let ps = compute_part_size(Some(one_tib));
        let parts = one_tib.div_ceil(ps as u64);
        assert!(parts <= 10_000, "1TiB가 10,000파트를 넘었다: {parts}");
        // 파트 크기는 S3 최소(5MiB) 이상이어야 한다.
        assert!(ps >= 5 * 1024 * 1024, "파트 크기가 S3 최소(5MiB) 미만: {ps}");
    }

    /// credentials 파싱: 정상 형식("ACCESS:SECRET")을 access/secret으로 분리한다.
    #[test]
    fn credentials_parse_splits_on_first_colon() {
        let creds = S3Credentials::parse("AKIAEXAMPLE:wJalrSecretKey").unwrap();
        assert_eq!(creds.access_key_id, "AKIAEXAMPLE");
        assert_eq!(creds.secret_access_key, "wJalrSecretKey");
    }

    /// secret에 콜론이 포함돼도 첫 콜론에서만 분리한다(secret 보존).
    #[test]
    fn credentials_parse_keeps_colons_in_secret() {
        let creds = S3Credentials::parse("ACCESS:sec:ret:value").unwrap();
        assert_eq!(creds.access_key_id, "ACCESS");
        assert_eq!(creds.secret_access_key, "sec:ret:value");
    }

    /// 콜론이 없거나 한쪽이 비면 Config 에러(exit 2).
    #[test]
    fn credentials_parse_rejects_malformed() {
        assert!(S3Credentials::parse("no-colon").is_err());
        assert!(S3Credentials::parse(":secret").is_err());
        assert!(S3Credentials::parse("access:").is_err());

        // S3Credentials는 시크릿을 담아 Debug를 구현하지 않으므로 직접 매칭한다.
        match S3Credentials::parse("bad") {
            Err(e) => assert_eq!(e.exit_code(), 2),
            Ok(_) => panic!("malformed credentials는 에러여야 한다"),
        }
    }

    /// prefix 정규화: None/빈/슬래시는 모두 "prefix 없음".
    #[test]
    fn normalize_prefix_handles_empty_and_slashes() {
        assert_eq!(normalize_prefix(None), "");
        assert_eq!(normalize_prefix(Some("")), "");
        assert_eq!(normalize_prefix(Some("/")), "");
        assert_eq!(normalize_prefix(Some("mongo/prod")), "mongo/prod");
        assert_eq!(normalize_prefix(Some("/mongo/prod/")), "mongo/prod");
    }

    /// object_path: prefix가 없으면 경로 그대로, 있으면 결합한다.
    #[test]
    fn object_path_combines_prefix() {
        let cfg = S3Config {
            endpoint: Some("http://localhost:9000".to_string()),
            bucket: Some("test-bucket".to_string()),
            prefix: Some("mongo/prod".to_string()),
            region: Some("us-east-1".to_string()),
            credentials_env: Some("CREDS".to_string()),
        };
        let s3 = S3Compatible::new(&cfg, "access:secret").unwrap();
        assert_eq!(s3.object_path("bkp-1/data.bin").as_ref(), "mongo/prod/bkp-1/data.bin");

        // prefix 없는 경우.
        let cfg_no_prefix = S3Config {
            prefix: None,
            ..cfg
        };
        let s3_np = S3Compatible::new(&cfg_no_prefix, "access:secret").unwrap();
        assert_eq!(s3_np.object_path("bkp-1/data.bin").as_ref(), "bkp-1/data.bin");
    }

    /// strip_prefix: list 절대 경로에서 config prefix를 떼어 trait 상대 경로로 되돌린다.
    #[test]
    fn strip_prefix_restores_relative_path() {
        let cfg = S3Config {
            endpoint: Some("http://localhost:9000".to_string()),
            bucket: Some("test-bucket".to_string()),
            prefix: Some("mongo/prod".to_string()),
            region: None,
            credentials_env: None,
        };
        let s3 = S3Compatible::new(&cfg, "access:secret").unwrap();
        assert_eq!(
            s3.strip_prefix("mongo/prod/bkp-1/data.bin"),
            "bkp-1/data.bin"
        );
        // prefix로 시작하지 않으면 그대로(방어적).
        assert_eq!(s3.strip_prefix("other/x"), "other/x");
    }

    /// bucket 누락 시 Config 에러로 거부한다.
    #[test]
    fn new_rejects_missing_bucket() {
        let cfg = S3Config {
            endpoint: Some("http://localhost:9000".to_string()),
            bucket: None,
            prefix: None,
            region: None,
            credentials_env: None,
        };
        let err = S3Compatible::new(&cfg, "access:secret").unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    /// 잘못된 credentials는 Config 에러로 거부한다(build 이전 단계).
    #[test]
    fn new_rejects_malformed_credentials() {
        let cfg = S3Config {
            endpoint: Some("http://localhost:9000".to_string()),
            bucket: Some("b".to_string()),
            prefix: None,
            region: None,
            credentials_env: None,
        };
        let err = S3Compatible::new(&cfg, "no-colon").unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }
}
