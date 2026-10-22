# PRD — PostgreSQL 물리 WAL 기반 PITR (opt-in 엔진)

> Barman에서 배운 정공법. 현재 x-backup의 PG PITR은 **logical decoding(pgoutput)** 기반이라
> DDL을 재현하지 못하고 replica identity에 의존한다. 이 PRD는 **물리 base backup + 연속 WAL
> 아카이빙 + WAL replay**로 "DDL 포함 완전 PITR"을 opt-in 엔진으로 추가하는 것을 정의한다.
> 기존 논리 PITR은 제거하지 않는다(§2.2, §9).

---

## 1. 개요

### 1.1 문제
현재 PG 증분/PITR(`src/engine/postgres/incremental.rs`, `src/pipeline/pg_pitr.rs`)은
`pg_logical_slot_peek_binary_changes`로 pgoutput 변경을 캡처해 복구 시 DML(I=upsert, U·D=키 기반)로
재생한다. 이 방식의 구조적 한계(코드 주석에 이미 기록됨):

- **DDL 재현 불가** — `ALTER TABLE`/`CREATE INDEX`/`DROP` 등이 논리 스트림에 실리지 않는다.
- **REPLICA IDENTITY 의존** — 키 없는 테이블은 UPDATE/DELETE를 안전하게 식별하지 못한다.
- **base↔slot 정렬 window** — consistent point와 덤프 스냅샷 사이 구간의 double-apply(idempotent로 무마).
- **논리 재생일 뿐, 물리 시점 복제본이 아님** — 재해복구(DR)용 "그 순간의 정확한 물리 상태"가 아니다.

### 1.2 목표
PostgreSQL의 표준 PITR(base backup + WAL 연속 아카이빙 + 목표 시점까지 WAL replay)을
**드라이버 네이티브(외부 도구 0개)**로 구현해, DDL을 포함한 바이트 단위 물리 복구를 제공한다.

### 1.3 설계 원칙 (x-backup 정체성 유지)
- **외부 도구 0개** — `pg_basebackup`/`pg_receivewal`/`restore_command`를 셸아웃하지 않고
  replication 프로토콜(`BASE_BACKUP`, `START_REPLICATION ... PHYSICAL`)을 직접 말한다.
- **암호화·압축 기본 경로 재사용** — 물리 tar 스트림과 WAL 세그먼트도 기존 파이프라인
  (compress → encrypt → store)을 그대로 통과한다.
- **opt-in 엔진** — 기본은 현행 논리 방식. `mode.pg_pitr = "physical"`일 때만 물리 경로.
- **스트리밍·flat memory** — base tar와 WAL을 메모리에 적재하지 않고 스트림 그대로 저장한다.

---

## 2. 목표 / 비목표

### 2.1 목표
1. **물리 base backup** — replication `BASE_BACKUP` 명령으로 데이터 디렉토리 tar 스트림 + `backup_label` + `tablespace_map` 수신·저장.
2. **연속 WAL 아카이빙** — 전용 physical replication slot에서 WAL 세그먼트를 스트리밍 수신해 순번대로 저장.
3. **WAL 보존 보장** — physical slot으로 "아카이브 완료 전까지 서버가 WAL을 못 지우게" 한다.
4. **물리 PITR 복구** — 데이터 디렉토리 복원 + `recovery.signal` + `recovery_target_time`/`_lsn`/`_name` 설정 → PG가 목표 시점까지 replay.
5. **논리 방식과의 명확한 선택** — 엔진별 능력/제약을 `status`·문서에 노출.

### 2.2 비목표
- 기존 논리 PITR 제거 — **유지**(가볍고 크로스버전·부분복구에 유리, §9 비교표).
- 서로 다른 PG major 버전 간 물리 복구 — 물리 base는 **동일 major·동일 플랫폼** 전제(NFR-2).
- x-backup이 대상 PG 프로세스를 기동/관리 — 데이터 디렉토리와 recovery 설정까지만 준비, 실제 기동은 운영자/오케스트레이터(§FR-4 참고).
- 증분 물리 base(PG 17 incremental basebackup) — 로드맵.

---

## 3. 유스케이스

| UC | 시나리오 |
|----|----------|
| UC-1 | 매일 물리 base backup + 상시 WAL 스트리밍 → 압축·암호화 → S3 |
| UC-2 | 장애 후 `--at <RFC3339>`로 물리 데이터 디렉토리 복원 → PG 기동 시 목표 시점까지 자동 replay |
| UC-3 | DDL(인덱스 추가/컬럼 변경)이 섞인 워크로드도 손실 없이 그 시점으로 복구 |
| UC-4 | `latest`로 최신 아카이브 WAL 끝까지 복구(재해복구) |
| UC-5 | 논리 PITR로는 부적격이던(키 없는 테이블·DDL 잦음) DB를 물리 엔진으로 커버 |

---

## 4. 아키텍처 개요

```text
[백업 시]
 BASE_BACKUP (replication)                 START_REPLICATION SLOT xb_<p>_phys PHYSICAL
   → base.tar 스트림 ─┐                       → WAL 세그먼트 스트림 ─┐
   backup_label ──────┤                       (XLogData 프레임) ─────┤
   tablespace_map ────┘                                             │
        │ compress → encrypt → store               compress → encrypt → store
        ▼                                                            ▼
   data/<base_id>/base.tar.zst.age                 wal/<timeline>/<segno>.zst.age
   manifest(backup_type=Full, archive_format=xb-pg-phys-v1,
            start_lsn, stop_lsn, timeline, wal_slot)

[복구 시  --at T]
   base 복원 → 데이터 디렉토리 펼침(+ backup_label, tablespace_map)
   필요한 WAL 세그먼트[start_lsn .. T] 준비(복호화·해제해 pg_wal/ 또는 restore용 디렉토리)
   postgresql.auto.conf += recovery_target_time='T', recovery_target_action='promote'
   recovery.signal 생성
   → 운영자가 PG 기동 → PG가 base 위에 WAL을 T까지 replay 후 promote
```

핵심: **물리 PITR은 base도 물리여야 한다.** WAL replay는 파일 레벨 물리 base 위에서만 성립하므로,
논리 덤프를 base로 재활용할 수 없다 → 별도 물리 엔진이 필요하다.

---

## 5. 핵심 기능 요구사항 (FR)

### FR-1. 물리 base backup
- replication 연결(`replication=database`)에서 `BASE_BACKUP (PROGRESS, MANIFEST 'no', WAL false)` 실행.
- base tar 스트림, `backup_label`, `tablespace_map`(+ 존재 시 tablespace별 tar)을 수신해 스트리밍 저장.
- 반환된 `START` LSN·timeline을 manifest에 기록(`start_lsn`, `timeline`).
- tablespace가 있으면 각 tar를 분리 저장하고 매핑을 manifest에 남긴다(복원 시 심볼릭 재구성).
- **제약:** `BASE_BACKUP`은 전체 클러스터 단위 → 선택적(`--db`/`--table`) 물리 백업은 불가(명시 거부).

### FR-2. 연속 WAL 아카이빙
- 프로파일별 **physical** slot(`xb_<profile>_phys`)을 생성·유지(논리 slot `xb_<profile>`와 별개).
- `START_REPLICATION SLOT xb_<profile>_phys PHYSICAL <lsn>`로 WAL을 스트리밍 수신, XLogData 프레임을
  세그먼트 경계로 잘라 `wal/<timeline>/<segno>` 경로에 순번대로 저장.
- 표준 keepalive에 응답하고, **저장이 성공 확인된 LSN까지만** standby status로 flush 위치를 보고해
  slot이 그 지점까지 전진(그 전에는 서버가 WAL을 보존).
- 실행 형태: `x-backup wal-archive <profile>`(장기 실행 데몬형, systemd/컨테이너에 위임) 또는
  주기 배치 `x-backup wal-archive --once`(cron: slot에 쌓인 것만 받아 저장 후 종료).

### FR-3. WAL 보존·gap 안전
- slot이 사라졌거나 invalidated면 gap → 다음 base backup을 강제(논리 방식의 FR-2와 동형, exit 4).
- `max_slot_wal_keep_size`로 서버가 slot을 invalidated 처리할 수 있음을 문서화하고, `status`에서 경고.
- base backup 시작 LSN보다 앞선(불필요한) 아카이브 WAL은 prune 대상(§FR 연계: PRD 02).

### FR-4. 물리 PITR 복구
- 입력: `--at <RFC3339>|<lsn>|latest`, `--target-dir <path>`(복원할 데이터 디렉토리), `--base-id`(고정, 선택).
- 절차:
  1. 목표 시점을 커버하는 최신 물리 base 선택(`--base-id`로 고정 가능).
  2. base tar를 `--target-dir`에 펼치고 `backup_label`·`tablespace_map` 배치.
  3. `start_lsn`부터 목표 지점까지 필요한 WAL 세그먼트를 복호화·해제해 복구용 디렉토리에 배치.
  4. `postgresql.auto.conf`에 `recovery_target_time`(또는 `_lsn`/`latest`)·`recovery_target_action='promote'`·
     `restore_command`(로컬 배치 디렉토리에서 복사) 기입 + `recovery.signal` 생성.
  5. **PG 기동은 x-backup이 하지 않는다** — 준비 완료 후 기동 절차를 안내(비-TTY면 JSON으로 경로·명령 반환).
- `--force`/충돌 가드: 비어있지 않은 `--target-dir`은 논리 복구와 동일한 덮어쓰기 가드 적용.

### FR-5. Manifest 확장
- `archive_format = "xb-pg-phys-v1"`, `backup_type = Full`.
- 추가 필드: `start_lsn`, `stop_lsn`(base backup 종료 LSN), `timeline`, `wal_slot`, `pg_version`, `tablespaces[]`.
- WAL 세그먼트는 base와 별도 카탈로그(`wal/<timeline>/...`)로 관리하되, 복구가 참조할 수 있게 인덱싱.

### FR-6. status / doctor 연계
- `status`에 물리 엔진일 때: 마지막 아카이브된 WAL LSN·timeline, slot restart_lsn, **PITR 가능 최신 시점**(PRD 03과 연계).
- `doctor`(오프라인)에서 물리 엔진 설정 정합성 검사(target-dir 쓰기권한 등은 실행 시).

---

## 6. 비기능 요구사항 (NFR)

- **NFR-1 스트리밍/메모리** — base tar·WAL 모두 청크 스트리밍, RSS 상수 유지(기존 파이프라인 계약).
- **NFR-2 호환 전제** — 물리 복구는 동일 PG major·동일 아키텍처/OS·동일 페이지 크기·checksum 설정 일치 필요. 불일치 감지 시 복구 거부.
- **NFR-3 권한** — `BASE_BACKUP`/physical replication은 `REPLICATION` 속성 롤 필요. `status`/`doctor`에서 사전 점검.
- **NFR-4 wal_level** — 물리 아카이빙만이면 `replica`로 충분(논리는 `logical` 필요). 문서에 매트릭스.
- **NFR-5 무결성** — WAL 세그먼트·base tar 각각 sha256 manifest, `verify --chain`이 WAL 순번 연속성 검사.

---

## 7. 엣지 케이스

- **timeline 분기** — 이전 복구/승격으로 timeline이 바뀐 WAL이 섞이면 복구 대상 timeline을 명시(`.history` 파일 저장·해석).
- **부분 WAL 세그먼트** — 데몬 재시작 시 미완 세그먼트는 재수신(flush LSN 이전만 신뢰).
- **tablespace** — 절대경로 tablespace는 복원 시 매핑 옵션 필요(경로 재지정).
- **slot 유실** — invalidated 시 base 강제 승격(FR-3).
- **디스크 부족(아카이브측)** — 저장 실패 시 slot을 전진시키지 않아 서버 WAL이 쌓임 → `status` 경고 + 운영자 알림.

---

## 8. 데이터/CLI 표면

```text
# config (opt-in)
[profiles.prod.mode]
pg_pitr = "physical"          # 기본 "logical"

[profiles.prod.pitr]
wal_slot = "xb_prod_phys"     # 기본: 자동 명명
wal_archive_dir = "wal/"      # storage 상대 경로

# 명령
x-backup backup <profile>                 # pg_pitr=physical이면 물리 base
x-backup wal-archive <profile> [--once]   # WAL 연속/1회 아카이빙
x-backup restore <profile> --at <T> --target-dir <path> [--base-id ID] [--force]
```

---

## 9. 논리 vs 물리 — 선택 매트릭스 (문서화 필수)

| 항목 | 논리(pgoutput, 현행) | 물리(WAL, 본 PRD) |
|------|:---:|:---:|
| DDL 복구 | ❌ | ✅ |
| 키 없는 테이블 | 제약(REPLICA IDENTITY) | ✅ |
| 크로스 major 버전 복구 | 유리 | ❌(동일 major) |
| 부분/테이블 단위 복구 | 가능 | ❌(클러스터 전체) |
| 복구 산출물 | 다른 타깃 DB에 DML 적용 | 데이터 디렉토리 + PG 기동 |
| wal_level | logical | replica |
| 정밀도 | commit ts(µs) | 목표 time/lsn |

→ 둘 다 유지하고 워크로드에 맞춰 고르게 한다. 기본은 논리(가벼움), DR·DDL 중시는 물리.

---

## 10. 마이그레이션 / 호환

- 기존 논리 백업/복구 경로·manifest는 무변경. 물리는 새 `archive_format`으로 공존.
- `prune`·`verify`·`status`는 `archive_format` 분기로 물리 산출물 인지(PRD 02·03 연계).

---

## 11. 테스트 전략

- **단위** — `BASE_BACKUP` 응답 파싱, XLogData 프레임 세그먼트 분할, backup_label/tablespace_map 처리, recovery 설정 생성.
- **통합(docker PG)** — full physical → WAL 스트리밍 → 특정 커밋 시점 `--at` 복구 → 데이터·DDL 일치 검증.
- **회귀** — timeline 분기 후 복구, slot 유실 후 base 승격, 대용량 tar 스트리밍 시 RSS 상수.
- **음성 테스트** — 버전/플랫폼 불일치 복구 거부, 비-TTY 충돌 가드.

---

## 12. 단계 (제안)

1. **P1** — physical slot 생성/유지 + `START_REPLICATION PHYSICAL` WAL 수신·저장(`wal-archive`).
2. **P2** — `BASE_BACKUP` 물리 base 저장 + manifest 확장.
3. **P3** — 물리 PITR 복구(데이터 디렉토리 준비 + recovery 설정 생성).
4. **P4** — status/doctor/verify/prune 연계, 논리↔물리 매트릭스 문서화.

## 13. 오픈 이슈

- tokio-postgres replication 모드로 `BASE_BACKUP` 원문 파싱을 직접 구현할지, replication 프레이밍만
  쓰고 tar는 자체 파서로 처리할지 결정 필요(외부 도구 0개 원칙 유지 범위).
- WAL 아카이버의 실행 모델(상시 데몬 vs cron `--once`)을 기본값으로 무엇을 권장할지.
- 매우 큰 클러스터의 base backup 소요시간 동안 slot WAL 누적 관리.
