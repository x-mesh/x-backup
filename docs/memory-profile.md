# 메모리 상한 실측 보고서 (Memory Profile)

> 대상: PRD `§11.1 #2` 릴리스 게이트 — **"데이터 크기와 무관하게 x-backup 프로세스 상주 메모리가 상수 상한(목표 ≤512 MiB)을 유지"**
> 작성: 2026-06-13 · 측정 스크립트: [`scripts/memory-profile.sh`](../scripts/memory-profile.sh) (재실행 가능)
> 기준 커밋: `ec51083` 시점의 `./target/release/x-backup` (v0.1.0)

---

## 1. TL;DR (판정)

| 항목 | 결과 |
|---|---|
| 데이터셋 | 1 GiB(small) · 6 GiB(large) — **6.0배 차이** |
| 측정 경로 | **기본 경로(zstd level 10 + age 암호화 ON)** — `--no-encrypt` 미사용 |
| backup 시 x-backup 본체 피크 RSS | small **54.8 MiB** vs large **54.6 MiB** → **평탄(1.00x)** |
| restore 시 x-backup 본체 피크 RSS | small **14.8 MiB** vs large **14.7 MiB** → **평탄(0.99x)** |
| 512 MiB 게이트 대비 여유 | backup **9.3×**(피크 54.8/512 = 10.7%) · restore **34.6×** |
| **게이트 판정** | ✅ **PASS** — RSS가 데이터 크기에 비례하지 않음(상수 상한)을 실측으로 입증 |

> **핵심:** 데이터가 6배 커져도 x-backup 본체 RSS는 사실상 변하지 않는다(±0.2 MiB). 이는 전 구간 스트리밍(전체 산출물을 메모리/디스크에 적재하지 않음, PRD §7) 설계가 실제로 작동함을 의미한다. 100 GB급에서도 동일 상한이 유지될 것으로 추론한다(근거 §6, 본실측 runbook은 §7).

---

## 2. 실측 환경

| 구성 | 버전/값 |
|---|---|
| 호스트 | Mac mini (Apple Silicon, **darwin/arm64**), CPU 12코어, RAM 48 GiB |
| OS | macOS **26.4.1** (build 25E253) |
| rustc / cargo | **1.93.0** (2026-01-19) |
| x-backup | **v0.1.0** (release 빌드, 바이너리 ~15.9 MiB) |
| mongod (fixture) | **mongo:7** (7.0.x), single-node replica set ×2 (Docker) |
| mongodump / mongorestore | **100.16.1** (공식 tarball, sha256 검증 — spike §2 경로) |
| mongosh | 2.8.3 (시드·검증용) |
| Docker | 29.4.0 |

도구 설치: mongodump/mongorestore는 이 호스트의 PATH에 없어, `docs/spike-oplog-archive.md` §2/§7의 공식 tarball을
내려받아 **sha256(`5bef906f…`)을 검증**한 뒤 사용했다. CI/Linux 서버는 패키지 매니저 또는 동일 tarball pin으로 설치한다.

age 키쌍 준비(방법 기록): 이 호스트에 `age-keygen`이 없어, **프로젝트가 의존하는 동일한 `age` 0.11 크레이트**로 일회용 키쌍을
생성했다(`src/crypto/age.rs` 테스트 헬퍼와 같은 `x25519::Identity::generate()`). 격리된 임시 cargo 프로젝트에서
공개키(`age1…`)와 개인키(`AGE-SECRET-KEY-1…`)를 각각 `age.pub`/`age.key`로 출력했다. 백업은 공개키만,
복구는 개인키(`XB_AGE_IDENTITY_FILE` 환경변수)로 복호화한다(§8.5 키 격리). 키는 보고서 산출물에 포함하지 않는다(일회용·폐기).

---

## 3. 측정 방법 (재현 가능)

측정 하네스는 [`scripts/memory-profile.sh`](../scripts/memory-profile.sh)에 담겨 있다(재실행 가능). 핵심:

- **x-backup 본체 RSS:** `./target/release/x-backup` 프로세스를 **`ps -o rss=`로 0.5초 간격 폴링**해 피크를 기록한다.
  `ps rss`는 macOS/Linux 모두 KiB 단위이며, 보고서는 MiB로 환산했다.
- **mongodump/mongorestore 자식 RSS:** **별도로 폴링해 참고치로 기록**한다. 이 외부 도구는 x-backup이 오케스트레이션하는
  대상이며, PRD 상한의 직접 대상은 **x-backup 본체**다(자식은 참고).
- **기본 경로 측정:** config.toml에 `compression.algorithm=zstd, level=10` + `encryption.enabled=true, algorithm=age`를 두어
  **zstd→age 전체 파이프라인**(PRD §8.4 compress→encrypt 순서)을 통과시킨다. `--no-encrypt`는 쓰지 않는다.
- **데이터셋 격리:** 1 GiB는 `--db small`, 6 GiB는 `--db large`로 분리해 **오직 데이터 크기만 다르게** 한다(파이프라인 경로 동일).
- **복구 타깃 분리:** 백업 출처(rs `:27090`)와 다른 깨끗한 replica set(`:27091`)으로 `--target` 분리 복구한다 —
  프로덕션 가드레일(기존 데이터 덮어쓰기 거부)을 피하려고 빈 타깃에 `--force --skip-precheck`로 진행한다.

재현 절차:

```bash
# 0. release 빌드 + 도구(PATH) + age 키쌍(age.pub/age.key) 준비.
cargo build --release

# 1. 소스/복구 replica set 기동(픽스처 재사용, 포트만 분리).
XB_RS_CONTAINER=x-backup-mem     XB_RS_PORT=27090 XB_RS_NAME=rsmem tests/fixtures/replica-set.sh up
XB_RS_CONTAINER=x-backup-mem-rst XB_RS_PORT=27091 XB_RS_NAME=rsrst tests/fixtures/replica-set.sh up

# 2. 시드(벌크 insert → dbStats로 목표 dataSize 확인). 디스크 여유의 1/3 이내.
SEED_DB=small SEED_TARGET_MB=1024 mongosh "$SOURCE_URI" --quiet --file seed.js
SEED_DB=large SEED_TARGET_MB=6144 mongosh "$SOURCE_URI" --quiet --file seed.js

# 3. 측정(backup + restore, RSS 0.5s 폴링).
WORK=/tmp/xb-memprof XB_BIN=$PWD/target/release/x-backup \
  SOURCE_URI="mongodb://localhost:27090/?replicaSet=rsmem&directConnection=true" \
  RESTORE_URI="mongodb://localhost:27091/?replicaSet=rsrst&directConnection=true" \
  AGE_PUB=/tmp/xb-memprof/age.pub AGE_KEY=/tmp/xb-memprof/age.key \
  scripts/memory-profile.sh small small
#   ... 같은 방식으로 large large
```

> **시드 데이터 특성:** 문서당 4 KiB의 **난수 hex 패딩**을 넣어 압축률을 의도적으로 낮췄다(zstd 압축비 ≈ 1.49×).
> 압축이 과도하게 잘 되면 파이프라인 부하가 비현실적으로 작아져 메모리 측정이 낙관 편향될 수 있어서다.

---

## 4. 측정 결과 (수치 표)

### 4.1 데이터셋

| label | db | docs | dataSize | storageSize | 백업 산출물(암호문) | zstd 압축비 |
|---|---|---:|---:|---:|---:|---:|
| small | `small` | 258,000 | **1,026.3 MiB** | 1,044.7 MiB | 689.0 MiB (722,486,676 B) | 1.49× |
| large | `large` | 1,546,000 | **6,149.6 MiB** | 6,259.2 MiB | 4,128.6 MiB (4,329,200,433 B) | 1.49× |

데이터 크기 비 = **6,149.6 / 1,026.3 = 6.0×**.

### 4.2 피크 RSS (핵심)

| 단계 | 프로세스 | 1 GiB (small) | 6 GiB (large) | 증가율 | 512 MiB 대비 |
|---|---|---:|---:|---:|---:|
| **backup** | **x-backup (본체)** | **54.8 MiB** | **54.6 MiB** | **1.00×** | 10.7% (9.3× 여유) |
| backup | mongodump (자식, 참고) | 129.4 MiB | 135.1 MiB | 1.04× | — |
| **restore** | **x-backup (본체)** | **14.8 MiB** | **14.7 MiB** | **0.99×** | 2.9% (34.6× 여유) |
| restore | mongorestore (자식, 참고) | 172.2 MiB | 171.7 MiB | 1.00× | — |

모든 실행 exit code 0(backup rc=0, restore rc=0).

### 4.3 복구 정합(부수 확인)

타깃 분리 복구 후 문서 수 일치를 확인했다(메모리 측정의 부수 검증 — backup→restore가 데이터를 잃지 않음):

| label | 원본 `items` | 복구 후 `items` | 일치 |
|---|---:|---:|:---:|
| small | 258,000 | 258,000 | ✅ |
| large | 1,546,000 | 1,546,000 | ✅ |

> 정합의 본격 검증(콘텐츠 해시·PITR 등)은 PRD §11.1 #3의 별도 게이트이며, `docs/acceptance-report.md`가 다룬다. 여기서는 문서 수만 확인했다.

---

## 5. 평탄성 판정 (Flatness)

- 데이터가 **6.0배** 커졌을 때:
  - backup x-backup RSS: 54.8 → 54.6 MiB (**−0.4%**, 사실상 변화 없음)
  - restore x-backup RSS: 14.8 → 14.7 MiB (**−0.7%**, 사실상 변화 없음)
- RSS는 데이터 크기에 **비례하지 않는다**. 만약 메모리에 적재했다면 6배 데이터에 RSS도 ~6배(수 GiB)로 폭증해야 하나,
  실측은 **±1 MiB 이내 상수**다. ⇒ **상수 상한(평탄) 성립**.

### 샘플링 한계 보강(미스 스파이크 점검)

0.5초 폴링은 그 사이의 순간 스파이크를 놓칠 수 있다(샘플링 한계). 이를 보강하려고 6 GiB backup을 **0.1초 간격으로
1,252회 추가 폴링**했고, 피크는 **53.8 MiB**로 0.5초 결과(54.6 MiB)와 일치했다 — 숨은 스파이크 없음. 0.5초 폴링이
이 워크로드(수십 초~분 단위)에 충분함을 보였다.

### 왜 상수인가 (아키텍처 근거)

전 구간이 **고정 크기 버퍼**로만 동작하기 때문이다(총 데이터량과 무관):

- age 암호화 단계: `tokio::io::duplex` 버퍼 **64 KiB** + 펌프 청크 **64 KiB**로 백프레셔(`src/crypto/age.rs`).
- zstd 압축: async-compression의 스트리밍 어댑터(bufread, 청크 단위).
- 스토리지 쓰기: `ReaderStream` **8 MiB** 청크 + `tokio::io::copy`의 내부 고정 버퍼(`src/storage/local.rs`).
- 검증/PITR 펌프: **64 KiB** 버퍼(`src/pipeline/verify.rs`, `pitr.rs`).

즉 파이프라인 어느 단계도 "전체 dump"를 메모리에 모으지 않고, 입력을 읽는 즉시 다음 단계로 흘린다(PRD §7 스트리밍 원칙).
restore가 backup보다 RSS가 작은 이유: restore는 storage 읽기→복호화→압축해제→mongorestore stdin으로 **흘려보내기만**
하고, backup 쪽의 mongodump 스폰/stderr drain/manifest 체크섬 누적 등 부가 상태가 없기 때문이다.

---

## 6. 512 MiB 게이트 판정

| 단계 | 본체 피크(최대) | 게이트 | 판정 | 여유 |
|---|---:|---:|:---:|---:|
| backup | 54.8 MiB | ≤ 512 MiB | ✅ PASS | **9.3×** (사용률 10.7%) |
| restore | 14.8 MiB | ≤ 512 MiB | ✅ PASS | **34.6×** (사용률 2.9%) |

**결론:** PRD §11.1 #2 릴리스 게이트 — x-backup 본체 상주 메모리가 데이터 크기와 무관하게 상수 상한을 유지하며,
512 MiB 목표를 큰 여유로 만족한다. ✅ **PASS**.

> 참고: mongodump/mongorestore 자식도 ~130–172 MiB로 데이터 크기에 거의 무관하게 상수였다(이 도구들 역시 스트리밍).
> 단 이는 x-backup이 제어하지 않는 외부 도구의 특성이며, 게이트의 직접 대상은 아니다.

---

## 7. 100 GB 본실측 Runbook (Linux 서버 기준)

이 macOS 개발 머신은 디스크 여유(측정 시 약 68 → 32 GiB)상 100 GB 원본을 안전하게 시드/백업할 수 없어 본실측을
**별도 Linux 서버에서 수행**한다. 절차는 위 §3과 동일하며, 규모·환경만 바꾼다. 예상 근거는 §6의 상수-버퍼 아키텍처와
6배 스케일에서 RSS가 평탄했던 실측(±1%)이다 — 100 GB(약 16배 추가 스케일)에서도 동일 상한이 유지될 것으로 추론한다.

### 7.1 사전 준비 (Linux x86_64)

```bash
# 1) 도구 — 공식 tarball(Linux x86_64) pin + sha256 검증, 또는 패키지 매니저.
curl -fsSLo tools.tgz \
  https://fastdl.mongodb.org/tools/db/mongodb-database-tools-<distro>-x86_64-100.16.1.tgz
# sha256 검증 후 압축 해제, bin/을 PATH에 추가.

# 2) age 키쌍 — Linux는 age-keygen 사용 가능.
age-keygen -o age.key            # 개인키
grep 'public key' age.key | sed 's/.*: //' > age.pub   # 공개키(age1...)
#   (없으면 본 보고서 §2의 age 크레이트 일회용 생성기 방식 사용)

# 3) x-backup release 빌드(또는 musl 정적 바이너리, acceptance-report 참조).
cargo build --release
```

### 7.2 디스크 산정 (필수)

- 원본 100 GB(dataSize) → Mongo storageSize ≈ 100–110 GB.
- 백업 산출물(zstd+age) ≈ **압축비에 따라 30–70 GB**(데이터 압축성에 좌우. 본 실측의 난수 hex는 ~67%).
- 타깃 분리 복구를 같은 서버에서 하면 복구 데이터 ≈ 추가 100–110 GB.
- ⇒ **여유 디스크 ≥ (원본 + 산출물 + 복구본)** 을 확보(보수적으로 ≥ 300 GB 권장). 부족하면 복구 타깃을 별도 서버/볼륨으로 분리.

### 7.3 절차

```bash
# A. replica set 기동(또는 운영 스냅샷 복제본). 소스 + 복구 타깃 분리.
#    운영 데이터가 이미 100 GB라면 시드 생략하고 그 인스턴스를 SOURCE로 쓴다.

# B. (시드가 필요하면) 목표 100 GiB까지 벌크 insert.
SEED_DB=big SEED_TARGET_MB=102400 SEED_PAD=4096 SEED_BATCH=2000 \
  mongosh "$SOURCE_URI" --quiet --file seed.js
mongosh "$SOURCE_URI" --quiet --eval 'printjson(db.getSiblingDB("big").stats(1073741824))'  # dataSize(GiB) 확인

# C. 측정 — 본 보고서의 스크립트를 그대로 재사용(폴링·기본 경로 동일).
WORK=/var/tmp/xb-memprof XB_BIN=$PWD/target/release/x-backup \
  SOURCE_URI="$SOURCE_URI" RESTORE_URI="$RESTORE_URI" \
  AGE_PUB=$PWD/age.pub AGE_KEY=$PWD/age.key \
  scripts/memory-profile.sh big big

# D. 결과: WORK/result-big.txt 의 backup_peak_xbackup_mib / restore_peak_xbackup_mib 이 ≤512 인지 확인.
```

> Linux의 `ps -o rss=`도 KiB 단위라 스크립트 수정 없이 동작한다. 대안으로 `/proc/<pid>/status`의 `VmRSS`
> 또는 cgroup `memory.peak`(cgroup v2)을 쓰면 폴링 미스 없이 커널이 집계한 피크를 얻을 수 있다 — Linux 본실측에서는
> **`memory.peak` 병행 기록을 권장**(폴링 한계 완전 제거).

### 7.4 예상 근거 (왜 100 GB도 통과할 것인가)

- 메모리 사용은 §6의 **고정 크기 버퍼 합**(수십 MiB)으로 상한이 결정되며, 입력 총량과 무관하다.
- 실측에서 6배 스케일(1→6 GiB)에 RSS 변화가 **±1% 이내**였다. 버퍼가 상수인 한 16배 추가 스케일(→100 GB)에서도
  본체 RSS는 동일 수준(수십 MiB)에 머문다. 512 MiB까지 약 9× 헤드룸이 있어, 예측 불확실성을 흡수하고도 통과 마진이 크다.
- 잔여 위험(아래 §8 미실측 참조): 매우 많은 컬렉션 수에서 mongodump/mongorestore **자식**의 메모리가 늘 수 있으나,
  이는 x-backup 본체가 아닌 자식 특성이며 게이트 직접 대상이 아니다.

---

## 8. 한계 / 미실측 (정직성)

- **100 GB 본실측: [미실측 — 호스트 디스크 한계]**. macOS 개발 머신 디스크 여유로 100 GB 원본을 안전 시드/백업할 수
  없어 1 GiB·6 GiB로 평탄성을 입증하고, 100 GB는 §7 runbook으로 위임했다. 추론 근거는 §6/§7.4.
- **컬렉션 다양성: [미실측]**. 단일 컬렉션(`items`) 대량 문서만 측정했다. 수천 개 컬렉션·인덱스가 많은 스키마에서
  mongodump/mongorestore **자식**의 메모리는 달라질 수 있다(x-backup 본체는 스트림만 다루므로 영향 적음으로 추정).
- **S3 destination: [미실측]**. 본 측정은 `destination=local`이다. S3 멀티파트 업로드 경로(`src/storage/s3.rs`)는
  파트 버퍼링 정책에 따라 본체 RSS가 다를 수 있어 별도 실측이 필요하다. local 결과가 S3 상한을 보장하지는 않는다.
- **증분(oplog) 경로: [미실측]**. 본 측정은 풀 백업/복구다. 증분 캡처는 드라이버 oplog 질의→압축→암호화로
  데이터량이 작아 본체 RSS가 더 작을 것으로 추정하나, 대량 oplog 슬라이스는 별도 확인 권장.
- **압축성 의존:** 산출물 크기·CPU 부하는 데이터 압축성에 좌우된다(여기선 난수 hex로 보수적 측정). 메모리 상한 자체는
  버퍼 크기로 정해져 압축성과 무관하나, 매우 잘 압축되는 데이터에서 파이프라인이 더 빨라 폴링 표본이 적어질 수 있다.
- **폴링 vs 커널 피크:** 본 측정은 `ps` 폴링(0.5s, 보강 0.1s)이다. 0.1s 재측정으로 미스 스파이크 없음을 보였으나,
  절대적 미스 제로는 아니다. Linux 본실측에서는 cgroup `memory.peak` 병행을 권장(§7.3).

---

## 9. 산출물·정리

- 측정 스크립트: [`scripts/memory-profile.sh`](../scripts/memory-profile.sh) — 재실행 가능(ENV로 경로·URI 주입).
- 시드 스크립트(seed.js)·키쌍·대형 백업 산출물·시드 데이터는 측정 후 정리했다(보고서·스크립트만 잔존).
- 컨테이너(`x-backup-mem`, `x-backup-mem-rst`)는 측정 후 `tests/fixtures/replica-set.sh down`으로 종료했다.
