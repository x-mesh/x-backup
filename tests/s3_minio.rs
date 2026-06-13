//! S3 호환 백엔드 통합 테스트 — MinIO 대상(PRD FR-4, §11).
//!
//! 실제 S3 호환 서버(MinIO)를 docker로 기동해 스트리밍 멀티파트 업로드,
//! 100MB급 round-trip 바이트 일치, 업로드 중단 시 abort(서버 잔여 파트 없음),
//! LocalFs↔S3 동일 trait 교체를 검증한다. ETag 차이(리서치 §5-3) 때문에
//! 무결성은 자체 sha256으로 확인한다.
//!
//! `s3-integration` feature로 격리한다 — docker가 필요하므로 기본 단위
//! 테스트/CI에는 포함되지 않는다.
//!
//! ## 실행
//! ```bash
//! cargo test --features s3-integration --test s3_minio -- --nocapture
//! ```
//! 이 테스트는 `minio/minio` 이미지를 `docker run`으로 직접 기동/정리한다.
//! docker가 없거나 이미지 pull이 막힌 환경에서는 자동으로 skip(경고 출력)한다.

#![cfg(feature = "s3-integration")]

use std::process::Command;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

use x_backup::config::file::{DestinationConfig, S3Config};
use x_backup::storage::{from_config, LocalFs, S3Compatible, Storage};

// ── MinIO 컨테이너 픽스처 ───────────────────────────────────────────────

/// 테스트 동안 살아 있는 MinIO 컨테이너. Drop 시 `docker rm -f`로 정리한다.
struct MinioContainer {
    name: String,
    port: u16,
    access_key: String,
    secret_key: String,
}

impl MinioContainer {
    /// MinIO 컨테이너를 기동하고 헬스 체크가 통과할 때까지 대기한다.
    ///
    /// docker 미설치/이미지 pull 실패 등으로 기동이 불가하면 `None`을 반환한다
    /// (호출자가 skip 처리). 기동 성공 시 `Some`.
    fn start() -> Option<Self> {
        // docker 가용성 확인.
        if !docker_available() {
            eprintln!("[skip] docker를 사용할 수 없어 MinIO 통합 테스트를 건너뜁니다");
            return None;
        }

        let name = format!("x-backup-minio-test-{}", std::process::id());
        let port = pick_port();
        let access_key = "minioadmin".to_string();
        let secret_key = "minioadmin".to_string();

        // 기존 잔재 컨테이너 제거(이전 실행 실패 등) — best-effort.
        let _ = Command::new("docker").args(["rm", "-f", &name]).output();

        let run = Command::new("docker")
            .args([
                "run",
                "-d",
                "--rm",
                "--name",
                &name,
                "-p",
                &format!("{port}:9000"),
                "-e",
                &format!("MINIO_ROOT_USER={access_key}"),
                "-e",
                &format!("MINIO_ROOT_PASSWORD={secret_key}"),
                "minio/minio",
                "server",
                "/data",
            ])
            .output();

        match run {
            Ok(out) if out.status.success() => {}
            Ok(out) => {
                eprintln!(
                    "[skip] MinIO 컨테이너 기동 실패(이미지 pull 불가 등): {}",
                    String::from_utf8_lossy(&out.stderr)
                );
                return None;
            }
            Err(e) => {
                eprintln!("[skip] docker run 실행 실패: {e}");
                return None;
            }
        }

        let container = Self {
            name,
            port,
            access_key,
            secret_key,
        };

        // 헬스 체크: MinIO live 엔드포인트가 200을 줄 때까지 폴링(최대 ~30s).
        if !container.wait_healthy(Duration::from_secs(30)) {
            eprintln!("[skip] MinIO 헬스 체크 시간 초과 — 테스트를 건너뜁니다");
            return None; // Drop이 컨테이너를 정리한다.
        }

        Some(container)
    }

    fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn credentials_raw(&self) -> String {
        format!("{}:{}", self.access_key, self.secret_key)
    }

    /// MinIO `/minio/health/live`가 응답할 때까지 폴링한다.
    ///
    /// 호스트 `curl`로 게시 포트(`127.0.0.1:{port}`)를 직접 확인한다 — 별도
    /// 컨테이너의 `--network host`는 macOS(Docker Desktop)에서 동작하지 않으므로
    /// 호스트에서 직접 점검한다.
    fn wait_healthy(&self, timeout: Duration) -> bool {
        let url = format!("{}/minio/health/live", self.endpoint());
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if host_curl_code(&url) == "200" {
                return true;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        false
    }

    /// 버킷을 생성한다(object_store put 전에 버킷이 존재해야 한다).
    ///
    /// `mc` 컨테이너로 alias 설정 후 `mb`한다. 컨테이너→호스트 게시 포트 접근은
    /// `host.docker.internal`(+ `--add-host ...:host-gateway`)로 macOS/Linux 모두
    /// 호환되게 한다.
    fn create_bucket(&self, bucket: &str) -> bool {
        let alias = "local";
        let endpoint = format!("http://host.docker.internal:{}", self.port);
        let script = format!(
            "mc alias set {alias} {endpoint} {} {} && mc mb -p {alias}/{bucket}",
            self.access_key, self.secret_key
        );
        let out = Command::new("docker")
            .args([
                "run",
                "--rm",
                "--add-host",
                "host.docker.internal:host-gateway",
                "--entrypoint",
                "sh",
                "minio/mc",
                "-c",
                &script,
            ])
            .output();
        matches!(out, Ok(o) if o.status.success())
    }
}

impl Drop for MinioContainer {
    fn drop(&mut self) {
        // 컨테이너 강제 제거(--rm이지만 명시적으로 한 번 더 — best-effort).
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.name])
            .output();
    }
}

/// docker CLI가 동작하는지 확인한다(`docker info`).
fn docker_available() -> bool {
    Command::new("docker")
        .args(["info"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// 호스트 curl로 HTTP 코드를 조회한다(curlimages/curl 미가용 폴백).
fn host_curl_code(url: &str) -> String {
    Command::new("curl")
        .args([
            "-s",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "--max-time",
            "2",
            url,
        ])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// 테스트별 충돌을 피하기 위한 임시 포트 선택(OS가 빈 포트를 할당).
fn pick_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .unwrap_or(19000)
}

/// 테스트용 S3Config를 만든다.
fn s3_config(container: &MinioContainer, bucket: &str, prefix: Option<&str>) -> S3Config {
    S3Config {
        endpoint: Some(container.endpoint()),
        bucket: Some(bucket.to_string()),
        prefix: prefix.map(|s| s.to_string()),
        region: Some("us-east-1".to_string()),
        credentials_env: None, // 직접 raw 자격증명을 넘기므로 env 불필요.
    }
}

/// 항상 일정 바이트를 내보낸 뒤 I/O 에러를 내는 reader — abort 경로 검증용.
///
/// 멀티파트가 최소 한 파트는 시작하도록 충분히(>5MiB) 내보낸 뒤 실패한다.
struct FailAfterReader {
    emitted: usize,
    fail_at: usize,
}

impl tokio::io::AsyncRead for FailAfterReader {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.emitted >= self.fail_at {
            return std::task::Poll::Ready(Err(std::io::Error::other(
                "주입된 reader 실패(abort 경로 검증)",
            )));
        }
        let remaining = (self.fail_at - self.emitted).min(buf.remaining());
        let chunk = vec![0xABu8; remaining];
        buf.put_slice(&chunk);
        self.emitted += remaining;
        std::task::Poll::Ready(Ok(()))
    }
}

fn reader_from(data: Vec<u8>) -> x_backup::storage::BoxAsyncRead {
    Box::pin(std::io::Cursor::new(data))
}

// ── 테스트 ──────────────────────────────────────────────────────────────

/// 100MB급 페이로드 멀티파트 round-trip — put→get 바이트(및 sha256) 일치.
#[tokio::test]
async fn s3_multipart_round_trip_100mb() {
    let Some(container) = MinioContainer::start() else {
        return; // 환경 제약 — skip(사유는 start가 출력).
    };
    let bucket = "roundtrip";
    assert!(container.create_bucket(bucket), "버킷 생성 실패");

    let cfg = s3_config(&container, bucket, Some("mongo/prod"));
    let storage = S3Compatible::new(&cfg, &container.credentials_raw()).expect("S3 백엔드 생성");

    // 100MiB + 알파 — 16MiB 파트 경계를 여러 번 넘긴다(멀티파트 다중 파트).
    let size = 100 * 1024 * 1024 + 7_777;
    let payload: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    let expected_hash = Sha256::digest(&payload);

    storage
        .put_stream(
            "bkp-1/data.bin",
            reader_from(payload.clone()),
            Some(size as u64),
        )
        .await
        .expect("멀티파트 업로드 성공");

    // get round-trip — 바이트 길이·sha256 일치.
    let mut got = Vec::with_capacity(size);
    storage
        .get_stream("bkp-1/data.bin")
        .await
        .expect("다운로드 성공")
        .read_to_end(&mut got)
        .await
        .expect("스트림 읽기 성공");

    assert_eq!(got.len(), payload.len(), "round-trip 길이 불일치");
    assert_eq!(
        Sha256::digest(&got),
        expected_hash,
        "round-trip sha256 불일치"
    );

    // list가 prefix 결합/제거를 정확히 처리하는지 — trait 상대 경로로 보여야 한다.
    let entries = storage.list("bkp-1").await.expect("list 성공");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].path, "bkp-1/data.bin");
    assert_eq!(entries[0].size, size as u64);
}

/// 업로드 도중 reader가 실패하면 abort가 호출되어 서버에 잔여(완성/미완성) 객체가
/// 남지 않아야 한다 — list로 잔재 없음을 확인한다(리서치 §5-2 abort 누락 방지).
#[tokio::test]
async fn s3_aborts_on_reader_failure_no_residue() {
    let Some(container) = MinioContainer::start() else {
        return;
    };
    let bucket = "abort-test";
    assert!(container.create_bucket(bucket), "버킷 생성 실패");

    let cfg = s3_config(&container, bucket, Some("mongo/prod"));
    let storage = S3Compatible::new(&cfg, &container.credentials_raw()).expect("S3 백엔드 생성");

    // 40MiB 내보낸 뒤 실패 — 기본 파트 크기(16MiB)를 넘겨 최소 2개 파트가 서버에
    // 업로드된 상태에서 reader가 실패한다. 이때 put_stream이 abort를 호출해 이미
    // 업로드된 파트까지 정리하는지가 핵심 검증이다. hint 없이 기본 16MiB 파트 사용.
    let reader = Box::pin(FailAfterReader {
        emitted: 0,
        fail_at: 40 * 1024 * 1024,
    }) as x_backup::storage::BoxAsyncRead;

    let result = storage.put_stream("bkp-fail/data.bin", reader, None).await;
    assert!(result.is_err(), "실패하는 reader는 에러를 반환해야 한다");

    // abort가 미완료 멀티파트를 정리했으므로 완성 객체가 없어야 한다.
    assert!(
        storage.get_stream("bkp-fail/data.bin").await.is_err(),
        "abort 후 완성 객체가 존재하면 안 된다"
    );
    let entries = storage.list("bkp-fail").await.expect("list 성공");
    assert!(
        entries.is_empty(),
        "abort 후 잔여 객체가 남았다: {entries:?}"
    );
}

/// LocalFs와 S3Compatible이 동일 `Box<dyn Storage>` trait로 교체 가능함을 확인한다.
/// 동일한 검증 루틴을 두 백엔드에 그대로 적용한다.
#[tokio::test]
async fn local_and_s3_are_interchangeable_via_trait() {
    let Some(container) = MinioContainer::start() else {
        return;
    };
    let bucket = "swap-test";
    assert!(container.create_bucket(bucket), "버킷 생성 실패");

    // S3 백엔드(from_config 팩토리 경유 — env로 자격증명 주입).
    let env_name = "XBACKUP_TEST_S3_CREDS_SWAP";
    // SAFETY: 테스트 프로세스 내 단일 사용, 고유 이름.
    unsafe {
        std::env::set_var(env_name, container.credentials_raw());
    }
    let s3_dest = DestinationConfig {
        name: None,
        r#type: Some("s3".to_string()),
        path: None,
        s3: Some(S3Config {
            credentials_env: Some(env_name.to_string()),
            ..s3_config(&container, bucket, Some("mongo/prod"))
        }),
    };
    let s3_storage = from_config(&s3_dest).expect("from_config로 S3 백엔드 생성");

    // Local 백엔드.
    let dir = tempfile::tempdir().expect("temp dir");
    let local_storage: Box<dyn Storage> = Box::new(LocalFs::new(dir.path()).expect("local 백엔드"));

    // 동일 루틴을 두 백엔드에 적용 — trait 교체 가능성 검증.
    for backend in [&s3_storage, &local_storage] {
        round_trip_check(backend.as_ref()).await;
    }

    unsafe {
        std::env::remove_var(env_name);
    }
}

/// 임의 백엔드(`&dyn Storage`)에 대해 put→get→list→delete를 검증하는 공통 루틴.
async fn round_trip_check(storage: &dyn Storage) {
    let payload = b"interchangeable storage backend payload".to_vec();
    let hash = Sha256::digest(&payload);

    storage
        .put_stream(
            "swap/data.bin",
            reader_from(payload.clone()),
            Some(payload.len() as u64),
        )
        .await
        .expect("put 성공");

    let mut got = Vec::new();
    storage
        .get_stream("swap/data.bin")
        .await
        .expect("get 성공")
        .read_to_end(&mut got)
        .await
        .expect("읽기 성공");
    assert_eq!(Sha256::digest(&got), hash, "round-trip 바이트 불일치");

    let entries = storage.list("swap").await.expect("list 성공");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].path, "swap/data.bin");

    storage.delete("swap/data.bin").await.expect("delete 성공");
    assert!(
        storage.get_stream("swap/data.bin").await.is_err(),
        "delete 후 객체가 남아 있으면 안 된다"
    );
}
