//! 통합 테스트 — verify(구조/--deep/--chain)와 list 카탈로그 E2E.
//!
//! DB·외부 도구 없이 **손으로 만든 백업 산출물**(LocalFs tempdir)로 검증한다. 실제
//! 압축·암호화 파이프라인 단계를 써서 평문·암호화 산출물을 만들고, verify가 변조를
//! 잡고 키 격리(§8.5)를 지키는지, list가 체인 상태·orphan을 표시하는지 확인한다.
//!
//! 기본 단위 테스트로 실행된다(feature gate 없음) — mongodump/mongorestore가 필요 없다.

use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

use x_backup::cli::handlers::list::build_catalog;
use x_backup::manifest::schema::{
    BackupManifest, BackupStatus, BackupType, CompressionMeta, EncryptionMeta, OplogRange,
    OplogTimestamp, Topology, FORMAT_VERSION,
};
use x_backup::manifest::store::{data_path, manifest_path};
use x_backup::manifest::ManifestStore;
use x_backup::pipeline::stage::{StageStack, ENV_AGE_IDENTITY_FILE};
use x_backup::pipeline::verify::{verify_backup, verify_chain_for};
use x_backup::storage::{BoxAsyncRead, LocalFs, Storage};

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// 기본 manifest(풀백업, complete). 호출자가 필드를 덮어쓴다.
fn base_manifest(id: &str, checksum: &str, stored: u64) -> BackupManifest {
    BackupManifest {
        format_version: FORMAT_VERSION,
        id: id.to_string(),
        created_at: "2026-06-12T00:00:00Z".into(),
        backup_type: BackupType::Full,
        base_id: None,
        topology: Topology::ReplicaSet,
        server_version: "7.0.35".into(),
        tool_versions: Default::default(),
        selective: false,
        original_size_bytes: stored,
        stored_size_bytes: stored,
        compression: None,
        encryption: None,
        checksum_sha256: checksum.to_string(),
        oplog_range: Some(OplogRange {
            start_ts: OplogTimestamp::new(100, 1),
            end_ts: OplogTimestamp::new(100, 1),
        }),
        oplog_count: None,
        promoted_from_gap: false,
        mysql_binlog: None,
        status: BackupStatus::Complete,
    }
}

/// 평문 data.bin + manifest를 기록한다(체크섬은 저장 바이트 기준으로 정확히 계산).
async fn seed_plaintext(fs: &LocalFs, id: &str, payload: &[u8]) -> BackupManifest {
    let reader: BoxAsyncRead = Box::pin(std::io::Cursor::new(payload.to_vec()));
    fs.put_stream(&data_path(id), reader, Some(payload.len() as u64))
        .await
        .unwrap();
    let m = base_manifest(id, &sha256_hex(payload), payload.len() as u64);
    ManifestStore::new(fs).write(&m).await.unwrap();
    m
}

/// 변조된 평문 백업: manifest는 원본 체크섬을 기록하지만 data.bin은 1바이트 flip.
#[tokio::test]
async fn tampered_artifact_fails_verify() {
    let dir = tempfile::tempdir().unwrap();
    let fs = LocalFs::new(dir.path()).unwrap();

    let payload = b"MongoDB archive payload that should stay intact";
    // manifest는 원본 기준 체크섬.
    let m = base_manifest("bk-tamper", &sha256_hex(payload), payload.len() as u64);
    // data.bin은 변조해서 저장.
    let mut tampered = payload.to_vec();
    tampered[10] ^= 0x01;
    let reader: BoxAsyncRead = Box::pin(std::io::Cursor::new(tampered));
    fs.put_stream(&data_path("bk-tamper"), reader, None)
        .await
        .unwrap();
    ManifestStore::new(&fs).write(&m).await.unwrap();

    let err = verify_backup(&fs, "bk-tamper", false).await.unwrap_err();
    assert_eq!(err.exit_code(), 1, "변조는 exit 1이어야 함: {err}");
}

/// 기본(구조) verify는 키 없이 평문 백업을 통과시킨다(§8.5 키 격리).
#[tokio::test]
async fn structural_verify_works_without_key() {
    let dir = tempfile::tempdir().unwrap();
    let fs = LocalFs::new(dir.path()).unwrap();
    seed_plaintext(&fs, "bk-plain", b"plaintext archive bytes").await;

    let report = verify_backup(&fs, "bk-plain", false).await.unwrap();
    assert!(report.is_ok());
    assert!(report.manifest_sidecar_ok && report.data_checksum_ok);
}

/// 암호화(age) 백업 — 실제 압축→암호화 파이프라인으로 산출물을 만든다.
/// 1) 기본 verify(구조)는 키 없이 통과(체크섬은 저장 바이트 기준).
/// 2) --deep은 키 env가 없으면 거부(Config, exit 2 — §8.5).
/// 3) --deep은 키 env가 있으면 디코드 성공.
#[tokio::test]
async fn encrypted_backup_structural_ok_deep_needs_key() {
    use age::secrecy::ExposeSecret;
    use x_backup::compress::ZstdCompressStage;
    use x_backup::crypto::AgeEncryptStage;

    let dir = tempfile::tempdir().unwrap();
    let fs = LocalFs::new(dir.path()).unwrap();

    // age 키쌍 + identity 파일.
    let id = age::x25519::Identity::generate();
    let id_path = dir.path().join("identity.txt");
    std::fs::write(&id_path, id.to_string().expose_secret()).unwrap();

    // 평문 → compress(zstd) → encrypt(age)로 저장 바이트를 만든다(정방향 파이프라인).
    let payload: Vec<u8> = (0..50_000u32).map(|i| (i % 97) as u8).collect();
    let mut forward = StageStack::new();
    forward
        .push(Box::new(ZstdCompressStage::new(8)))
        .push(Box::new(AgeEncryptStage::from_recipient(id.to_public())));
    let mut stored_bytes = Vec::new();
    forward
        .apply(Box::pin(std::io::Cursor::new(payload.clone())))
        .read_to_end(&mut stored_bytes)
        .await
        .unwrap();
    assert_ne!(stored_bytes, payload, "암호화 후 산출물은 평문과 달라야 함");

    // 저장 바이트로 data.bin·manifest 기록(체크섬 = 저장 바이트 기준).
    let reader: BoxAsyncRead = Box::pin(std::io::Cursor::new(stored_bytes.clone()));
    fs.put_stream(
        &data_path("bk-enc"),
        reader,
        Some(stored_bytes.len() as u64),
    )
    .await
    .unwrap();
    let mut m = base_manifest(
        "bk-enc",
        &sha256_hex(&stored_bytes),
        stored_bytes.len() as u64,
    );
    m.compression = Some(CompressionMeta {
        algorithm: "zstd".into(),
        level: 8,
    });
    m.encryption = Some(EncryptionMeta {
        algorithm: "age".into(),
        key_id: Some(id.to_public().to_string()),
    });
    ManifestStore::new(&fs).write(&m).await.unwrap();

    // 1) 구조 verify는 키 없이 통과.
    unsafe {
        std::env::remove_var(ENV_AGE_IDENTITY_FILE);
    }
    let structural = verify_backup(&fs, "bk-enc", false).await.unwrap();
    assert!(structural.is_ok(), "구조 verify는 키 없이 통과해야 함");

    // 2) --deep인데 키 없음 → 거부(Config, exit 2).
    let deep_no_key = verify_backup(&fs, "bk-enc", true).await.unwrap_err();
    assert_eq!(
        deep_no_key.exit_code(),
        2,
        "키 부재 deep은 Config(2): {deep_no_key}"
    );
    assert!(
        deep_no_key.to_string().contains(ENV_AGE_IDENTITY_FILE),
        "키 격리 안내 누락: {deep_no_key}"
    );

    // 3) --deep + 키 → 디코드 성공.
    unsafe {
        std::env::set_var(ENV_AGE_IDENTITY_FILE, &id_path);
    }
    let deep_ok = verify_backup(&fs, "bk-enc", true).await.unwrap();
    unsafe {
        std::env::remove_var(ENV_AGE_IDENTITY_FILE);
    }
    assert_eq!(
        deep_ok.deep_decode_ok,
        Some(true),
        "키 있으면 deep 디코드 성공"
    );
}

/// --chain: 끊어진 지점(gap)을 구체적으로 보고한다.
#[tokio::test]
async fn chain_verify_reports_break() {
    let dir = tempfile::tempdir().unwrap();
    let fs = LocalFs::new(dir.path()).unwrap();
    let store = ManifestStore::new(&fs);

    // base(end=100,1) → i1(100,1→150,2) → i2(GAP: 160,0→200,3)
    let base = base_manifest("base", "x", 0);
    let mut i1 = base_manifest("i1", "x", 0);
    i1.backup_type = BackupType::Incremental;
    i1.base_id = Some("base".into());
    i1.oplog_range = Some(OplogRange {
        start_ts: OplogTimestamp::new(100, 1),
        end_ts: OplogTimestamp::new(150, 2),
    });
    let mut i2 = base_manifest("i2", "x", 0);
    i2.backup_type = BackupType::Incremental;
    i2.base_id = Some("base".into());
    i2.oplog_range = Some(OplogRange {
        start_ts: OplogTimestamp::new(160, 0),
        end_ts: OplogTimestamp::new(200, 3),
    });
    for m in [&base, &i1, &i2] {
        store.write(m).await.unwrap();
    }

    let report = verify_chain_for(&fs, "i2").await.unwrap();
    assert_eq!(report.base_id, "base");
    assert!(!report.is_continuous(), "gap이 있는데 연속으로 판정됨");
    // 끊긴 지점이 구체적으로 보고된다.
    assert!(!report.breaks.is_empty());
    let msg = report
        .breaks
        .iter()
        .map(|b| b.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        msg.contains("i1") && msg.contains("i2"),
        "끊긴 지점 메시지: {msg}"
    );
}

/// list 카탈로그: 정상 체인 ok, broken chain·orphan 표시.
#[tokio::test]
async fn list_catalog_shows_chain_status_and_orphan() {
    let dir = tempfile::tempdir().unwrap();
    let fs = LocalFs::new(dir.path()).unwrap();
    let store = ManifestStore::new(&fs);

    // 연속 체인: base → i1.
    let base = base_manifest("aaaa-base", "x", 100);
    let mut i1 = base_manifest("bbbb-i1", "x", 50);
    i1.backup_type = BackupType::Incremental;
    i1.base_id = Some("aaaa-base".into());
    i1.oplog_range = Some(OplogRange {
        start_ts: OplogTimestamp::new(100, 1),
        end_ts: OplogTimestamp::new(150, 2),
    });
    // broken: 다른 base에 gap이 있는 증분.
    let base2 = base_manifest("cccc-base2", "x", 100);
    let mut bad = base_manifest("dddd-bad", "x", 50);
    bad.backup_type = BackupType::Incremental;
    bad.base_id = Some("cccc-base2".into());
    bad.oplog_range = Some(OplogRange {
        start_ts: OplogTimestamp::new(999, 0), // base end(100,1)과 불연속.
        end_ts: OplogTimestamp::new(1000, 1),
    });
    for m in [&base, &i1, &base2, &bad] {
        store.write(m).await.unwrap();
    }
    // orphan: manifest 없이 data.bin만.
    let ghost: BoxAsyncRead = Box::pin(std::io::Cursor::new(b"ghost".to_vec()));
    fs.put_stream(&data_path("eeee-ghost"), ghost, None)
        .await
        .unwrap();

    let rows = build_catalog(&fs).await.unwrap();
    let find = |id: &str| rows.iter().find(|r| r.id == id).cloned().unwrap();

    // 연속 체인은 ok.
    assert_eq!(find("aaaa-base").chain_status, "ok");
    assert_eq!(find("bbbb-i1").chain_status, "ok");
    assert_eq!(find("bbbb-i1").kind, "incr");
    assert_eq!(find("bbbb-i1").base_id.as_deref(), Some("aaaa-base"));
    // broken 체인은 broken.
    assert_eq!(find("dddd-bad").chain_status, "broken");
    // orphan 표시.
    let orphan = find("eeee-ghost");
    assert_eq!(orphan.kind, "orphan");
    assert_eq!(orphan.chain_status, "orphan");
}

/// manifest 자체 정합 위반(사이드카 불일치)도 verify가 잡는다.
#[tokio::test]
async fn manifest_sidecar_mismatch_fails() {
    let dir = tempfile::tempdir().unwrap();
    let fs = LocalFs::new(dir.path()).unwrap();
    seed_plaintext(&fs, "bk", b"data").await;

    // manifest.json을 사이드카와 어긋나게 덮어쓴다(사이드카는 그대로).
    let bad: BoxAsyncRead = Box::pin(std::io::Cursor::new(br#"{"x":1}"#.to_vec()));
    fs.put_stream(&manifest_path("bk"), bad, None)
        .await
        .unwrap();

    let err = verify_backup(&fs, "bk", false).await.unwrap_err();
    assert_eq!(err.exit_code(), 1);
}
