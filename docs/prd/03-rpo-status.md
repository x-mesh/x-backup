# PRD — RPO / 복구 가능 시점 노출 (status)

> Barman `check`/`show`는 "이 서버, **지금 실제로 어느 시점까지 복구 가능한가**"를 보여준다.
> 현재 x-backup `status`(`src/cli/handlers/status.rs`)는 last-backup-age, 목적지 쓰기가능·여유공간 등
> 백업 **전** 사전점검에 강하다. 이 PRD는 여기에 **"현재 복구 가능 최신 시점(RPO)과 갭"**을 더해
> 운영자가 데이터 손실 위험을 한눈에 보게 한다.

---

## 1. 개요

### 1.1 문제
백업이 "존재"하는지는 알 수 있어도, **지금 장애가 나면 어느 시점까지 되살릴 수 있는지**는
드러나지 않는다. PITR 체계의 실효 지표는 **RPO(Recovery Point Objective) 갭** =
`now - (복구 가능한 최신 시점)`이다. WAL/oplog 아카이빙이 밀리거나 slot이 막히면 이 갭이 조용히
벌어지는데, 현재는 이를 표면화하지 않는다.

### 1.2 목표
`status`에 **복구 가능 최신 시점**, **RPO 갭**, **아카이빙 지연**을 추가하고, 임계치 초과 시
경고(비-정상 exit 또는 경고 플래그)로 알린다.

---

## 2. 목표 / 비목표

### 2.1 목표
1. **복구 가능 최신 시점** 계산·표시 — base + 이어지는 증분/oplog/WAL로 실제 도달 가능한 최신 시점.
2. **RPO 갭** = now − 위 시점, 사람이 읽는 형태(예: `2m 13s`)로 표시.
3. **아카이빙 지연** — 마지막으로 저장된 WAL/oplog와 서버 현재 위치(LSN/optime)의 차이.
4. **임계치 경고** — `rpo_warn_secs`/`rpo_crit_secs` 초과 시 경고/치명 표시 + exit code.
5. **`--json`** — 위 지표를 기계가 읽는 필드로 노출(모니터링/알림 연동).

### 2.2 비목표
- 상시 모니터링 데몬 — 로드맵(`--watch`는 현행 기능 재사용).
- 알림 전송(Slack/PagerDuty) — hook(PRD 04)에 위임.

---

## 3. 유스케이스

| UC | 시나리오 |
|----|----------|
| UC-1 | 운영자가 `status`로 "지금 복구하면 최대 1분 12초 손실"을 즉시 확인 |
| UC-2 | cron이 `status --json`으로 RPO 갭을 수집해 임계 초과 시 알림 |
| UC-3 | slot이 막혀 WAL 아카이빙이 30분 밀린 상황을 `status`가 CRIT로 표시 |
| UC-4 | CI 파이프라인이 배포 전 RPO 갭이 임계 이하인지 게이트 |

---

## 4. 기능 요구사항 (FR)

### FR-1. 복구 가능 최신 시점
- **논리/oplog**: 최신 완결 base + 체인된 증분들의 마지막 commit ts.
- **물리(PRD 01)**: 마지막으로 **안전 저장된** WAL 세그먼트가 커버하는 시점(대략 그 세그먼트의 마지막 커밋/타임라인 LSN).
- base가 없으면 "복구 불가"로 명시(갭 계산 불가).

### FR-2. RPO 갭 & 아카이빙 지연
- `rpo_gap = now - 복구가능최신시점`.
- `archive_lag`:
  - 물리: `서버 current WAL LSN − 마지막 아카이브 WAL LSN`(바이트) + slot restart_lsn.
  - 논리: `서버 current LSN − slot confirmed_flush_lsn`.
  - oplog: `서버 최신 optime − 마지막 증분 optime`.
- 서버 접속이 안 되면 아카이빙 지연은 "unknown"으로 표시(백업 카탈로그 기반 RPO는 계속 계산).

### FR-3. 임계치 & exit
- config `rpo_warn_secs`/`rpo_crit_secs`(선택). 표시 색/기호 + exit code로 구분:
  - 정상 → 0, WARN → 관례상 비-0(문서화), CRIT → 별도 코드. 기존 exit 계약(0–5)과 정합.
- 임계 미설정이면 정보만 표시(게이팅 없음).

### FR-4. 출력
- 사람용: `status` 요약에 한 줄 추가 —
  `PITR: 복구가능 ~2026-07-17T04:32:10Z (RPO 갭 1m12s) · WAL 지연 3.2MB [OK]`
- `--json`: `recoverable_until`, `rpo_gap_secs`, `archive_lag_bytes`, `archive_lag_secs`, `rpo_state`("ok"|"warn"|"crit"|"unknown").

### FR-5. doctor(오프라인) 연계
- DB 접속 없이 카탈로그만으로 "마지막 base/증분 기준 복구 가능 시점"을 계산(아카이빙 지연은 unknown).

---

## 5. 비기능 요구사항

- **NFR-1** — 추가 조회는 가벼운 카탈로그 읽기 + 단일 서버 LSN/optime 쿼리. status 지연 최소화.
- **NFR-2** — 서버 미접속/권한 부족에도 부분 결과(카탈로그 기반)를 낸다(현행 status 견고성 유지).
- **NFR-3** — 시계 오차(clock skew)는 기존 status 점검을 재사용해 RPO 갭에 주석.

---

## 6. 엣지 케이스

- **base 없음** → "복구 불가", 갭 표시 안 함.
- **증분 체인 끊김** → 복구 가능 시점을 마지막 **연속** 지점까지로 절단하고 경고.
- **시계 skew 큼** → 갭에 "clock skew 의심" 주석.
- **물리 slot invalidated** → 아카이빙 지연을 CRIT로, 다음 base 필요 안내(PRD 01 FR-3).

---

## 7. 테스트 전략

- **단위** — 카탈로그 조합별 복구가능시점 계산(base only / base+증분 / 끊김 / 물리 WAL).
- **통합** — 아카이빙을 인위로 지연시켜 archive_lag·RPO 갭·임계 상태 전이 검증.
- **스냅샷** — `--json` 필드 스키마 안정성.

## 8. 단계

1. **P1** — 논리/oplog 복구가능시점 + RPO 갭(카탈로그 기반) + `--json`.
2. **P2** — 서버 LSN/optime 대비 archive_lag + 임계치/exit.
3. **P3** — 물리 엔진(PRD 01) 연계 + doctor 오프라인 계산.
