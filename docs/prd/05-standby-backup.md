# PRD — 스탠바이(복제본)에서 백업

> Barman은 primary 부하를 줄이려 **standby(읽기 복제본)에서 백업**을 뜬다.
> x-backup `status`는 이미 토폴로지를 점검하므로(연결·역할·복제 지연), "읽기 복제본 URI를 주면
> 거기서 백업"은 자연스러운 확장이다. 프로덕션 primary의 I/O·커넥션 부담을 백업에서 분리한다.

---

## 1. 개요

### 1.1 문제
백업은 대량 읽기(전체 스캔/스냅샷/replication 스트림)를 유발해 primary의 성능에 영향을 준다.
운영에선 보통 읽기 복제본이 이미 존재하는데, x-backup은 이를 백업 소스로 삼는 1급 경로가 없다.

### 1.2 목표
**읽기 복제본(standby/secondary)을 백업 소스로 지정**하고, 복제본 특유의 제약(읽기 전용, 복제 지연,
슬롯 위치)을 안전하게 처리한다. primary는 최소한만 건드린다.

---

## 2. 목표 / 비목표

### 2.1 목표
1. **복제본 소스 지정** — 프로파일에 `read_source_uri`(백업 읽기용)와 기존 소스(메타/제어용)를 분리 지정.
2. **엔진별 안전 처리**:
   - **MongoDB** — secondary read preference로 데이터/oplog 읽기(replica set 전제, 현행과 정합).
   - **PostgreSQL** — standby에서 논리/물리 백업 시 제약 처리(§4).
   - **MySQL** — replica에서 논리 백업 + binlog(복제본의 binlog 좌표) 처리(§4).
3. **복제 지연 가드** — 지연이 임계 초과면 경고/거부(백업이 너무 뒤처진 데이터를 담지 않게).
4. **status 연계** — 소스가 복제본임을 인지하고 지연·역할을 점검.

### 2.2 비목표
- 자동 복제본 탐색/페일오버 — 운영자가 URI로 명시(조용한 토폴로지 우회 금지, 현행 원칙).
- 복제본 프로비저닝/관리 — 스코프 외.

---

## 3. 유스케이스

| UC | 시나리오 |
|----|----------|
| UC-1 | prod primary 부하 분리를 위해 읽기 복제본에서 매일 풀 백업 |
| UC-2 | 복제 지연이 5분을 넘으면 백업을 거부(너무 오래된 스냅샷 방지) |
| UC-3 | Mongo secondary read preference로 백업, primary는 메타 조회만 |
| UC-4 | PG standby에서 물리 base backup으로 primary WAL/I/O 영향 최소화 |

---

## 4. 엔진별 제약 (핵심)

### 4.1 MongoDB
- replica set의 secondary read preference로 데이터·oplog 읽기. 현행 토폴로지 점검과 정합.
- 제약: secondary도 oplog를 공유하므로 증분 체인 시맨틱 유지. standalone은 대상 아님(현행과 동일).

### 4.2 PostgreSQL
- **논리 decoding(현행 증분)** — 논리 슬롯은 전통적으로 primary에서 생성. PG 16+는 standby 논리 슬롯을
  지원하나 전제조건(`hot_standby_feedback` 등)이 있어, standby 논리 증분은 **버전·설정 게이트** 후 허용.
  미충족이면 "풀 백업만 standby, 증분은 primary" 조합을 명시 안내.
- **물리 base backup(PRD 01)** — standby에서 `BASE_BACKUP` 가능(부하 분리에 이상적). WAL은 standby의
  timeline/피드백을 고려해 아카이빙.
- 지연·복구 상태(`pg_is_in_recovery`, `pg_last_wal_replay_lsn`) 점검.

### 4.3 MySQL
- replica에서 논리 백업 + binlog 좌표는 **복제본 자신의 binlog**(또는 GTID)를 기준으로 기록.
- 증분/PITR(binlog ROW)은 복제본의 `log_bin`·`log_replica_updates` 설정 게이트.
- `Seconds_Behind_Source`로 지연 점검.

---

## 5. 기능 요구사항 (FR)

### FR-1. 소스 분리
- `read_source_uri`(백업 읽기)와 `source_uri`(제어/메타) 분리. `read_source_uri` 미지정이면 기존 단일 소스.
- 자격증명·암호화 처리는 기존 secret 경로 재사용.

### FR-2. 복제본 검증
- 백업 시작 전 소스가 실제 복제본(읽기 전용)인지 확인(PG `pg_is_in_recovery`, Mongo 멤버 상태, MySQL replica 상태).
- 복제본이 아니면 경고(정상 진행) 또는 `--require-standby`면 거부.

### FR-3. 복제 지연 가드
- `max_replica_lag_secs`(config/CLI). 초과 시:
  - 기본: 경고 후 진행.
  - `--fail-on-lag`(또는 config): 거부(exit 계약 매핑).
- 지연 측정: PG replay lag, Mongo optime 차, MySQL `Seconds_Behind_Source`.

### FR-4. status 연계
- `status`가 소스 역할(primary/standby)·복제 지연·복제본에서의 백업 적격성을 표시.
- 증분/PITR이 현재 소스에서 불가하면 그 이유와 대안(primary 조합)을 명시.

### FR-5. config / CLI 표면
```toml
[profiles.prod]
source_uri      = "postgresql://.../ (primary, 제어)"   # 선택
read_source_uri = "postgresql://replica.../"            # 백업 읽기

[profiles.prod.standby]
max_replica_lag_secs = 300
require_standby      = false
fail_on_lag          = false
```
```text
x-backup backup <profile> [--read-source <uri>] [--require-standby] [--fail-on-lag]
```

---

## 6. 비기능 요구사항 (NFR)

- **NFR-1** — 복제본 백업이 primary에 만드는 부하는 메타 조회 수준으로 제한.
- **NFR-2** — 지연·역할 점검 실패 시 조용히 진행하지 않고 명시적으로 경고/거부.
- **NFR-3** — 기존 단일 소스 프로파일은 무변경(신규 키 미설정 시 현행 동작).

---

## 7. 엣지 케이스

- **복제본이 승격됨(primary가 됨)** — 백업 중 역할 변경 감지 시 경고, 증분 체인 좌표 정합성 재확인.
- **복제 끊김** — 소스가 갱신 안 되는 stale 복제본이면 지연 가드가 잡아냄(FR-3).
- **PG standby 논리 슬롯 미지원 버전** — 증분은 primary로 유도, 풀만 standby.
- **MySQL GTID vs 파일·좌표 혼재** — 좌표 기준을 manifest에 명시.

---

## 8. 테스트 전략

- **통합(docker)** — PG primary+standby, Mongo replica set, MySQL source+replica 구성에서
  복제본 백업 → primary 복구/검증 라운드트립.
- **가드** — 인위적 지연 주입으로 `max_replica_lag_secs` 경고/거부 전이.
- **역할 검증** — non-standby URI에 `--require-standby` 거부.

## 9. 단계

1. **P1** — `read_source_uri` 분리 + 복제본 검증(FR-2) + Mongo secondary 경로.
2. **P2** — 복제 지연 가드(FR-3) + status 연계(FR-4).
3. **P3** — PG standby(논리 게이트/물리 base) + MySQL replica binlog 좌표.
