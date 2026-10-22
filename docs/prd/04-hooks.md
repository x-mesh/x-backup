# PRD — Hook 스크립트 (pre/post 통합 지점)

> Barman은 backup/archive 전후로 훅을 걸어 알림·스냅샷·외부 카탈로그 연동을 붙인다.
> x-backup은 스케줄러를 내장하지 않고 cron/CI/컨테이너에 위임하는 headless 도구다(현행 설계 원칙).
> 훅은 이 위임 모델에 **저비용·고효율의 통합 표면**을 더한다 — 알림, 스냅샷 트리거, 검증 게이트.

---

## 1. 개요

### 1.1 문제
백업 성공/실패를 Slack에 알리거나, 백업 후 `verify`를 자동 실행하거나, 백업 전 앱을 정지시키는
등의 연동을 지금은 사용자가 x-backup **바깥에서** 종료코드를 보고 직접 엮어야 한다. exit code 계약
(0–5)은 잘 정의돼 있지만, "언제·무슨 컨텍스트로" 외부 명령을 부를지는 표준화돼 있지 않다.

### 1.2 목표
백업/복구/prune/wal-archive의 **정해진 생명주기 지점**에서 사용자 명령을 실행하는 훅을 제공한다.
컨텍스트(프로파일·백업 ID·유형·결과·경로)를 환경변수로 전달한다.

---

## 2. 목표 / 비목표

### 2.1 목표
1. **생명주기 훅** — `pre_backup`, `post_backup`, `pre_restore`, `post_restore`, `pre_prune`, `post_prune`, `on_error`.
2. **컨텍스트 전달** — 훅 프로세스에 표준 환경변수(`XB_EVENT`, `XB_PROFILE`, `XB_BACKUP_ID`, `XB_STATUS`, …).
3. **게이팅 vs 관측** — `pre_*` 실패는 작업을 **중단**(게이트), `post_*`/`on_error` 실패는 **경고만**(관측).
4. **headless 안전** — 훅 실패가 백업 자체 성공/실패 판정을 왜곡하지 않도록 exit 계약과 정합.

### 2.2 비목표
- 내장 알림 채널(Slack/webhook) — 훅으로 사용자가 붙인다(핵심은 얇은 실행 지점).
- 스케줄링 — 여전히 cron/CI 위임.
- 임의 플러그인/동적 로딩 — 셸 명령 실행만(단순·감사 가능).

---

## 3. 유스케이스

| UC | 시나리오 |
|----|----------|
| UC-1 | `post_backup`으로 성공 시 Slack 알림, `on_error`로 실패 시 PagerDuty |
| UC-2 | `post_backup`에서 `x-backup verify --deep`를 자동 실행해 백업 직후 검증 |
| UC-3 | `pre_backup`으로 애플리케이션 quiesce, `post_backup`으로 재개 |
| UC-4 | `post_prune`에서 스토리지 사용량을 메트릭 수집기로 전송 |
| UC-5 | `pre_restore`에서 대상이 프로덕션이 아님을 확인하는 가드 스크립트(실패 시 복구 차단) |

---

## 4. 기능 요구사항 (FR)

### FR-1. 훅 지점
- `pre_backup` / `post_backup`
- `pre_restore` / `post_restore`
- `pre_prune` / `post_prune`
- `pre_wal_archive` / `post_wal_archive`(PRD 01 연계, 선택)
- `on_error`(어느 단계든 실패 시, 마지막에 1회)

### FR-2. 실행 모델
- 각 훅은 셸 명령(문자열) 또는 명령 배열. 기본 셸: `/bin/sh -c`(플랫폼별).
- 타임아웃(`hook_timeout_secs`, 기본값 지정) 초과 시 종료 + 경고.
- **작업 디렉토리·환경 상속** 최소화(명시 env만 전달, 민감정보 노출 방지 — §NFR-2).

### FR-3. 게이팅 규칙
- `pre_*` 훅이 비-0으로 끝나면 해당 작업을 **시작하지 않음**(exit 계약상 Usage/Failure로 매핑).
- `post_*`/`on_error` 훅 실패는 **경고 로그**만 남기고 작업 판정에 영향 없음(백업은 이미 성공).
- `--no-hooks`로 전 훅 비활성(디버깅/일회 실행).

### FR-4. 컨텍스트(환경변수)
훅 프로세스에 주입:
- `XB_EVENT` — 예: `post_backup`.
- `XB_PROFILE`, `XB_ENGINE`(mongo/postgres/mysql), `XB_BACKUP_TYPE`(full/incremental).
- `XB_BACKUP_ID`(생성/대상 ID), `XB_STATUS`(ok/failed), `XB_EXIT_CODE`.
- `XB_STORAGE_URI`(마스킹된 목적지), `XB_STARTED_AT`/`XB_ENDED_AT`(RFC3339).
- `on_error`에는 `XB_ERROR`(요약 메시지, 민감정보 제거).

### FR-5. config 표면
```toml
[profiles.prod.hooks]
pre_backup   = "/opt/xb/quiesce.sh"
post_backup  = "x-backup verify --deep && notify-slack.sh"
on_error     = "pager.sh"
hook_timeout_secs = 60
```
- 전역 기본 훅(`[hooks]`)과 프로파일별 훅 병합(프로파일 우선).

---

## 5. 비기능 요구사항 (NFR)

- **NFR-1 결정성** — 훅 실행 순서·시점을 문서로 고정(pre → 작업 → post, 실패 시 on_error).
- **NFR-2 비밀 보호** — DB 비밀번호·암호화 키·전체 URI 자격증명은 훅 env에 넣지 않는다(마스킹). 감사 로그에도 미노출.
- **NFR-3 headless** — 비-TTY에서 훅이 stdin을 기대하지 않도록 stdin 미연결. 출력은 캡처해 로그로.
- **NFR-4 이식성** — 셸 부재/권한 문제는 명확한 오류로(조용한 무시 금지).

---

## 6. 엣지 케이스

- **훅 자체가 x-backup 재귀 호출** — lock(현행 동시 실행 잠금)으로 데드락 방지, 문서에 경고.
- **`pre_backup` 게이트 실패** — 작업 미시작 + `on_error` 실행 여부 정의(게이트 실패도 error 이벤트로 볼지 결정: 기본 실행).
- **긴 훅** — 타임아웃 초과 시 SIGTERM→SIGKILL 순서, 경고.
- **다중 명령** — 하나라도 실패 시 훅 전체 실패로 간주(`sh -c`의 파이프라인/`&&` 시맨틱 존중).

---

## 7. 테스트 전략

- **단위** — env 구성·마스킹, 게이팅(pre 실패→중단), 타임아웃, `--no-hooks`.
- **통합** — 실제 백업 흐름에 훅을 걸어 순서·컨텍스트·경고전파 검증.
- **보안** — 비밀이 훅 env/로그에 새지 않음을 스냅샷으로 고정.

## 8. 단계

1. **P1** — `pre/post_backup` + `on_error` + env 컨텍스트 + 게이팅.
2. **P2** — restore/prune/wal-archive 훅 + 전역/프로파일 병합 + 타임아웃.
