# PRD — 복구 보장 리텐션 (Recovery Window Retention)

> Barman의 핵심 규율: **리텐션은 "무엇을 지울까"가 아니라 "어느 시점까지 복구를 보장할까"**이다.
> 현재 x-backup prune은 체인 안전 + `keep_full/keep_days/keep_last` 합집합까지 왔다
> (`src/pipeline/prune.rs`). 이 PRD는 여기에 **recovery-window 의미론**과 **최소 이중화 안전장치**,
> **PITR용 오플로그/WAL 동반 보존**을 더해 "prune이 복구 가능성을 절대 깨지 않음"을 보장한다.

---

## 1. 개요

### 1.1 문제
현행 `keep_days D`는 "체인 최신 구성원의 **생성 시각**이 now-D 이내면 보존"이다. 이는
"D일 전 시점으로 **복구 가능**"을 보장하지 않는다. 예: keep_days=7인데 마지막 풀백업이 8일 전이면
7일 전 시점을 복구할 base가 사라진다. Barman의 `RECOVERY WINDOW OF n DAYS`는 이 함정을 막는다 —
**윈도우 경계보다 바깥의 가장 오래된 base 하나까지 보존**해 경계 시점 복구를 성립시킨다.

### 1.2 목표
"지난 N일 중 임의 시점으로 복구 가능"을 **불변식**으로 보장하는 리텐션 정책을 추가한다.
prune은 이 불변식을 깨는 삭제를 스스로 거부한다.

---

## 2. 목표 / 비목표

### 2.1 목표
1. **recovery-window 정책** — `recovery_window_days = N`: 지난 N일 임의 시점 복구를 보장.
2. **경계 바깥 base 보존** — 윈도우 시작점을 커버하는 가장 오래된 base 체인을 보존(핵심 규칙).
3. **PITR 동반 보존** — 보존되는 체인의 PITR에 필요한 증분/oplog/WAL을 함께 보존(고아 방지·구멍 방지).
4. **최소 이중화** — `min_redundancy = M`: 정책상 지워도 되더라도 최소 M개 풀 체인은 남긴다.
5. **안전 거부** — 정책 적용 결과가 불변식을 깨면 prune이 삭제를 거부하고 사유를 보고.

### 2.2 비목표
- 완전한 GFS(Grandfather-Father-Son) rotation — 로드맵(recovery-window로 대다수 요구 충족).
- 스토리지 용량 상한 기반 삭제(size-based) — 로드맵.

---

## 3. 유스케이스

| UC | 시나리오 |
|----|----------|
| UC-1 | "최근 30일 어느 시점이든 복구 가능" 정책을 걸고, prune이 그 보장을 유지 |
| UC-2 | 풀백업 주기가 길어 윈도우 경계가 마지막 풀백업보다 앞서도, 경계 바깥 base가 보존돼 복구 가능 |
| UC-3 | prune이 base를 지우면 그에 딸린 증분/WAL도 함께 정리해 고아 WAL이 남지 않음 |
| UC-4 | 정책이 공격적이어도 최소 2개 풀 체인은 항상 남아 단일 손상에 대비 |

---

## 4. 핵심 불변식 (Invariants)

- **INV-1 (윈도우 복구)** — `now - recovery_window_days` 시점 T에 대해, T를 커버하는 base 체인과
  T까지 이어지는 증분/WAL이 존재해야 한다. 즉 **T보다 이전에 시작한 가장 최근 base**를 보존한다.
- **INV-2 (체인 완결)** — 보존되는 base의 PITR에 필요한 모든 증분/oplog/WAL 세그먼트를 함께 보존.
- **INV-3 (최소 이중화)** — 어떤 정책이든 최소 `min_redundancy`개의 완결 풀 체인을 남긴다.
- **INV-4 (기준 없으면 무삭제)** — 정책이 하나도 없으면 아무것도 지우지 않는다(현행 유지).

prune은 삭제 계획 확정 전 위 불변식을 검사하고, 위반하면 그 삭제 항목을 보존으로 되돌린다.

---

## 5. 기능 요구사항 (FR)

### FR-1. recovery_window_days
- `recovery_window_days = N`이면 보존 커트라인 `T = now - N일`.
- **T 이후에 시작한 base 체인은 전부 보존**(윈도우 내부).
- 추가로 **T를 커버하는(=T보다 앞서 시작한 가장 최근) base 체인 1개를 보존**(경계 규칙, INV-1). 이게 없으면 T 시점 복구 불가.
- 그보다 더 오래된 base 체인은 (다른 규칙이 보존하지 않는 한) 삭제 후보.

### FR-2. PITR 동반 보존
- 물리 PITR(PRD 01): 보존 base의 `start_lsn`부터 필요한 WAL 세그먼트 전부 보존. 어떤 보존 base보다도 앞선 WAL은 삭제 가능.
- 논리/oplog PITR: 보존 체인이 참조하는 증분 슬라이스 전부 보존(현행 체인 안전 규칙 확장).

### FR-3. min_redundancy
- 삭제 계획 적용 후 남는 완결 풀 체인이 M개 미만이면, 가장 최근 것부터 M개가 될 때까지 삭제를 취소.
- recovery-window와 병용 시 **합집합 보존**(더 많이 남기는 쪽).

### FR-4. 기존 규칙과의 결합
- `keep_full`/`keep_days`/`keep_last`/`recovery_window_days`/`min_redundancy`는 **합집합 보존**
  (어느 하나라도 보존하면 보존). 현행 합집합 시맨틱 유지.

### FR-5. 안전 거부 & 리포트
- 계획이 INV-1~3을 만족하지 못하면(예: WAL 구멍으로 윈도우 복구 불가) prune은 **삭제를 중단**하고
  무엇이 왜 위험한지 보고(exit code로 구분).
- `--dry-run`은 보존/삭제 분류와 **각 삭제가 어떤 불변식으로 검사됐는지** 표로 출력.

### FR-6. config / CLI 표면
```toml
[profiles.prod.retention]
recovery_window_days = 30     # 신규
min_redundancy       = 2      # 신규
keep_full = 3                 # 기존(선택)
keep_days = 14                # 기존(선택)
keep_last = 20                # 기존(선택)
```
```text
x-backup prune <profile> [--recovery-window-days N] [--min-redundancy M] \
                         [--keep-full N] [--keep-days D] [--keep-last N] \
                         [--dry-run] [--force]
```
CLI 플래그가 config보다 우선(현행 규칙 유지).

---

## 6. 알고리즘 (순수 함수 확장)

`plan_prune`(현행: I/O 없는 순수 판정)에 단계 추가:

```text
1. 체인 구성(base + 증분/WAL) 수집·정렬(base 시작 시각 내림차순).
2. 각 규칙별 보존 집합 계산:
   - keep_full/keep_days/keep_last: 현행 로직.
   - recovery_window: T 내부 체인 전부 + T를 커버하는 최오래된 경계 base 1개.
   - min_redundancy: 최신 M개 풀 체인.
3. 보존 = 규칙들의 합집합.
4. 삭제 후보 = 전체 − 보존.
5. 불변식 검사(INV-1..3): 위반 유발 항목은 보존으로 승격.
6. 삭제 확정 체인의 종속 WAL/증분 동반 삭제 목록 확정(INV-2 역방향).
7. 검사 실패(구멍 등)면 전체 삭제 거부(FR-5).
```

순수 함수라 단위 테스트로 불변식을 전수 검증한다.

---

## 7. 엣지 케이스

- **윈도우가 모든 백업보다 김** — 아무것도 안 지움(전부 윈도우 내부/경계).
- **경계 base의 WAL 구멍** — 그 base로는 T 복구 불가 → 더 오래된 완결 base로 경계 확장, 없으면 FR-5 거부.
- **incomplete/orphan** — 현행대로 `--force`에서만 삭제(윈도우 계산에서 제외).
- **논리+물리 혼재 프로파일** — 엔진별로 종속 산출물(증분 슬라이스 vs WAL) 분기 계산.
- **min_redundancy > 존재 체인 수** — 전부 보존.

---

## 8. 마이그레이션 / 호환

- 신규 키는 모두 선택. 미설정이면 현행 동작 그대로(무변경).
- `recovery_window_days`만 있고 base 주기가 너무 길면 경계 규칙이 오래된 base를 보존하므로,
  스토리지 증가를 `--dry-run`으로 먼저 확인하도록 문서에 경고.

---

## 9. 테스트 전략

- **단위(순수)** — 경계 base 보존, 합집합, min_redundancy, 불변식 위반→보존 승격, WAL 구멍→거부.
- **속성 기반** — 임의 체인/증분 타임라인 생성 후 "보존 집합으로 윈도우 임의 시점 복구 성립" 검증.
- **통합** — 실제 백업 세트로 prune 후 `restore --at <윈도우 경계>` 성공 확인.

## 10. 단계

1. **P1** — recovery_window_days + 경계 base 규칙(INV-1) + dry-run 리포트.
2. **P2** — min_redundancy(INV-3) + PITR 동반 보존(INV-2).
3. **P3** — 안전 거부(FR-5) + 물리/논리 엔진 분기.
