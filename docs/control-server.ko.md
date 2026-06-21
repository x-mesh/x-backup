# 중앙 control 서버 운영 가이드 — 다중 DB 백업·복구·마이그레이션

한 대의 control/백업 서버가 **여러 DB(MongoDB·PostgreSQL)를 하나의 config**로 운영하는
패턴을 설명한다. 예시 config: [`examples/control-server.toml`](../examples/control-server.toml).

---

## 1. 개념 — 프로파일의 3요소

```
profile = ① endpoint(source 접속)  +  ② storage(destination)  +  ③ policy(엔진·압축·암호화·증분·retention)
```

프로파일은 두 종류로 나눠 쓴다:

| 종류 | 구성 | 용도 |
|---|---|---|
| **백업 잡** | ① + ② + ③ 전부 | `backup`/`list`/`prune` 대상 |
| **endpoint 전용** | ①(source)만 | `restore --target-profile`·`migrate --target-profile`의 **목적지** |

endpoint 전용 프로파일 덕분에 복구/이관 대상을 **URI 직타 대신 이름**으로 가리킨다:

```bash
# 이전 — raw URI(프로파일에 이미 주소가 있는데 또 URI를 적어 혼란)
x-backup restore --profile mongo-prod --target "mongodb://user:pass@dr:27017/?replicaSet=rs1"

# 지금 — 프로파일 참조(명확)
x-backup restore --profile mongo-prod --target-profile mongo-dr
```

> `restore`와 `migrate` 둘 다 `--target`(URI 직접)·`--target-profile`(프로파일 이름)을
> 택일로 지원한다. 미지정 시 복구는 **프로파일 자신의 source로 in-place 복구**한다.

---

## 2. config 준비

```bash
sudo install -d /etc/x-backup
sudo cp examples/control-server.toml /etc/x-backup/config.toml
# 모든 명령이 이 파일을 보도록 환경에 고정(또는 매 명령에 --config 전달)
export XB_CONFIG=/etc/x-backup/config.toml
```

시크릿은 config에 평문으로 담지 않는다 — `uri_env`/`s3_creds`(v1: `credentials_env`)로
**환경변수 이름만** 보관하고 실제 값은 실행 환경에서 주입한다.

> 예시 config는 **config v2** 표면 문법으로 작성돼 있다 — 프로파일마다 flat한
> `[profile.<name>]` 테이블, 공통 정책은 `[defaults]`·`[base.<name>]` + `extends`로 한 번만
> 정의한다(키 전체 표·상속 규칙은 [README](../README.ko.md#configtoml) 참조). 로더가 형식을
> 자동 판별하므로(단수 `profile`/`defaults`/`base` → v2, 복수 `profiles` → v1) 기존 v1 중첩
> config도 무변경으로 그대로 쓸 수 있다 — 한 파일에 둘을 섞지만 않으면 된다.

### 필요한 환경변수

| 변수 | 용도 | 예시 |
|---|---|---|
| `MONGO_PROD_URI` | mongo-prod source | `mongodb://user:pass@prod:27017/?replicaSet=rs0` |
| `MONGO_DR_URI` | mongo-dr 목적지 | `mongodb://user:pass@dr:27017/?replicaSet=rs1` |
| `PG_PROD_URI` | pg-prod source | `postgresql://user:pass@prod:5432/maindb` |
| `PG_DR_URI` | pg-dr 목적지 | `postgresql://user:pass@dr:5432/maindb` |
| `S3_CREDS` | S3 자격증명(단일 env, 콜론 구분; `s3_creds`가 가리키는 이름) | `AKIA...:wJalr...` |
| `XB_AGE_IDENTITY_FILE` | **복구 시** age 복호화 개인키 경로 | `/etc/x-backup/age.key` |

암호화 공개키(recipient)는 config의 `recipient_file`(예: `/etc/x-backup/age.pub`)로 지정한다.
복호화 개인키는 별도 격리하고 **복구할 때만** `XB_AGE_IDENTITY_FILE`로 준다.

설정을 바꾼 뒤에는 항상 정적 점검:

```bash
x-backup doctor --config /etc/x-backup/config.toml
```

---

## 3. 일상 운영

### 백업 (server → S3)

```bash
x-backup backup --profile mongo-prod              # 풀 백업
x-backup backup --profile mongo-prod --type incr  # 증분(oplog)
x-backup backup --profile pg-prod                 # PostgreSQL
x-backup backup --profile pg-prod    --type incr  # PG 증분(logical decoding, pg_logical=true 필요)
x-backup backup --profile mongo-archive           # 로컬+S3 이중화(멀티 destination)
```

### 상태·카탈로그

```bash
x-backup status --all          # 전 프로파일 연결·지연 한 줄 요약 + 최악 exit code
x-backup list --profile mongo-prod
x-backup list --profile pg-prod --type full --limit 10
```

### 복구 (DR로 — 프로파일 참조)

```bash
# DR 엔드포인트로 복구(복호화 개인키 필요)
XB_AGE_IDENTITY_FILE=/etc/x-backup/age.key \
  x-backup restore --profile mongo-prod --target-profile mongo-dr

# 시점 복구(PITR) — DR로
XB_AGE_IDENTITY_FILE=/etc/x-backup/age.key \
  x-backup restore --profile pg-prod --target-profile pg-dr --at "2026-06-21T03:00:00Z"

# 먼저 계획만 확인(무변경) — 어디로 가는지·충돌을 보여준다
x-backup restore --profile mongo-prod --target-profile mongo-dr --dry-run
```

복구 실행 시 stderr에 **대상이 명확히 표시**된다(자격증명은 마스킹):

```
→ restore target mongodb://***@dr:27017/?replicaSet=rs1 (--target-profile: mongo-dr)
```

### 마이그레이션 (파일 없이 server → server)

백업 파일/암호화/manifest 없이 source를 target으로 직접 복사한다(같은 엔진끼리만).

```bash
x-backup migrate --profile mongo-prod --target-profile mongo-dr
x-backup migrate --profile pg-prod    --target-profile pg-dr
x-backup migrate --profile mongo-prod --target-profile mongo-dr --db app --dry-run
```

### 보존 정리 (prune)

```bash
x-backup prune --profile mongo-prod --dry-run     # 삭제 대상만 출력
x-backup prune --profile mongo-prod               # config retention(keep_last/keep_days) 기준
x-backup prune --profile pg-prod --keep-last 60   # CLI로 기준 덮어쓰기
```

---

## 4. 자동화

### cron (control 서버)

```cron
# 매일 02:00 풀 백업, 15분마다 증분, 주간 prune. 시크릿은 cron 환경에 별도 주입.
0  2  * * *  XB_CONFIG=/etc/x-backup/config.toml x-backup backup --profile mongo-prod --quiet
*/15 * * * * XB_CONFIG=/etc/x-backup/config.toml x-backup backup --profile mongo-prod --type incr --quiet
0  3  * * *  XB_CONFIG=/etc/x-backup/config.toml x-backup backup --profile pg-prod --quiet
30 4  * * 0  XB_CONFIG=/etc/x-backup/config.toml x-backup prune  --profile mongo-prod --force
```

`--quiet`는 진행 표시를 끄고 요약·경고·에러만 남긴다(cron/CI 적합). 기계 연동은 `--json`.

### systemd timer (개요)

`x-backup-mongo-prod.service`(`Type=oneshot`, `EnvironmentFile=/etc/x-backup/env`)와
`x-backup-mongo-prod.timer`(`OnCalendar=*-*-* 02:00`)로 cron을 대체할 수 있다.
`EnvironmentFile`에 위 환경변수를 두면 시크릿을 cron 라인에 노출하지 않아도 된다.

---

## 5. doctor / status — endpoint 전용 프로파일 인식

`doctor`·`status`는 destination이 없는 프로파일을 **endpoint 전용(복구/이관 대상)** 으로
인식해 backup 잡 점검(destination·last-backup·encryption)을 건너뛴다. 그래서 `mongo-dr`/`pg-dr`
같은 endpoint 전용 프로파일은 오탐 없이 표시된다:

- **doctor**: `[OK] role  endpoint-only profile (restore/migrate target) — no backup storage`
- **status**: `destination: endpoint only`, `last backup: n/a`

즉 endpoint 전용 프로파일만 있는 한 `doctor`는 깨끗하게 통과한다(exit 0). source URI 점검은
그대로 수행하므로 대상 endpoint 접속 정보가 유효한지는 계속 검증된다.

> 참고: `status --all`의 나머지 경고(예: 로컬 Mongo의 "인증 비활성", 아직 백업 안 한 프로파일의
> "no backup history")는 endpoint 이슈가 아니라 환경 특성이다 — 백업을 한 번 뜨거나 운영에서
> 인증을 켜면 사라진다.

---

## 6. 안전 수칙

- **복구 대상을 항상 확인**한다. restore는 대상 미지정 시 **프로파일 source로 in-place 복구**
  (= 운영 DB 덮어쓰기)다. 실행 직전 stderr의 `→ restore target ...` 줄과 `(출처)`를 확인하라.
- 기존 데이터가 있는 대상은 `--force` 또는 대화형 확인이 필요하다(가드레일). `--dry-run`으로
  충돌 네임스페이스를 먼저 점검하라.
- 복호화 개인키(`XB_AGE_IDENTITY_FILE`)는 백업 서버와 **분리 보관**하고 복구 시에만 주입한다.
- `migrate`는 검증 가능한 백업본을 만들지 않는다(직접 복사). 시점 일관 백업이 필요하면
  `backup` → `restore`를 쓴다.
