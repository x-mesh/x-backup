# MySQL 엔진

> 대상 독자: x-backup으로 MySQL을 백업/복구하려는 운영자·개발자.
> 핵심: **외부 `mysqldump`/`mysql`/`mysqlbinlog` 없이** 순수 Rust `mysql_async` 드라이버만으로
> 풀 백업·복구·증분(binlog ROW 스트리밍)·시점 복구(PITR)를 수행한다.

DB 종류는 source URI 스킴으로 자동 판별한다(`mysql://` / `mariadb://` → MySQL/MariaDB,
그 외 → MongoDB 또는 PostgreSQL). 별도 플래그가 필요 없다.

## 1. 기능 요약

| 기능 | 명령 | 비고 |
|---|---|---|
| 풀 백업 | `backup --type full` | DDL(`SHOW CREATE`) + `SELECT` 스트리밍(`xb-mysql-v1`) |
| 증분 백업 | `backup --type incr` | binlog ROW 이벤트 캡처(`xb-mysql-incr-v1`), opt-in |
| 풀 복구 | `restore` | DROP/CREATE + batched INSERT |
| 시점 복구(PITR) | `restore --at <RFC3339>\|latest` | 풀 복원 + binlog ROW 재생 |
| 상태 | `status` | 연결·버전·DB 크기·테이블/행 수·레플리카 상태 |
| 미리보기 | `peek` | 테이블/행 수·샘플 행 |
| 마이그레이션 | `migrate` | MySQL→MySQL 드라이버 직접(외부 도구 없음) |

압축(zstd)·암호화(age/aes-256-gcm)·로컬/S3 destination·멀티 destination 복제는 Mongo·PG
경로와 동일한 파이프라인을 그대로 탄다.

## 2. 스키마 충실도

**덤프/복구 대상:** 테이블(DDL `SHOW CREATE TABLE`)·데이터(`SELECT` 스트리밍)·뷰·트리거·
스토어드 프로시저·함수·이벤트. 바이트 타입 → `0x` hex 표현, JSON 컬럼 값 보존, STORED 생성
컬럼은 INSERT 목록에서 제외(서버가 재계산), 인비저블 컬럼 포함. 외래키 의존 순서(FK-ordered)로
테이블을 덤프하며 복구 세션은 `FOREIGN_KEY_CHECKS=0`으로 열린다. `AUTO_INCREMENT` 값 보존.
뷰·트리거·루틴·이벤트는 데이터 적재 후 적용하며, `DEFINER` 절은 이식성을 위해 제거한다.

복구 세션 preamble: `FOREIGN_KEY_CHECKS=0`, `UNIQUE_CHECKS=0`,
`SQL_MODE='NO_AUTO_VALUE_ON_ZERO'`, `time_zone='+00:00'`, `NAMES utf8mb4`.

**미지원(경고만 — 백업은 진행):**
- FLOAT/DOUBLE: 서버 텍스트 표현 사용 — `mysqldump`와 동등하며 10진 소수 완전 보존이 보장되지 않는다.
- 동시 DDL 격리 불가: `START TRANSACTION WITH CONSISTENT SNAPSHOT`은 InnoDB에만 유효하며
  백업 중 병행 DDL(ALTER TABLE 등)이 차단되지 않는다.
- 뷰·트리거별 `sql_mode` 미재현: 복구 시 단일 관대 모드를 사용한다.

## 3. 설정

```toml
[profiles.mysql.source]
uri_env = "MYSQL_URI"   # 예: mysql://root:pass@127.0.0.1:3306/mydb (데이터베이스 이름 필수)

[profiles.mysql.features.incremental]
mysql_binlog = true     # binlog 증분 사용 여부(기본 false, 명시 opt-in)
```

v2 flat 형식:

```toml
[profile.mysql]
uri_env      = "MYSQL_URI"
dest         = "local:/var/backups/mysql"
mysql_binlog = true     # MySQL 증분/PITR opt-in
```

**URI 주의:** 데이터베이스 이름이 URI에 반드시 포함돼야 한다
(`mysql://user:pass@host:3306/dbname`). 생략하면 연결 자체를 거부한다.

**`mysql_binlog`** (기본 `false`): `true`면 풀 백업이 현재 binlog 좌표(`file:pos` +
`gtid_executed`)를 매니페스트에 기록하고, `backup --type incr`로 이후 ROW 이벤트를 캡처한다.
binlog가 퍼지(purge)되면 증분은 퍼지가 미치지 않는 범위까지만 유효하다.

**증분 서버 요건** (`mysql_binlog = true`일 때만 — 풀 백업·복구·status에는 불필요):

| 서버 설정 | 필요 값 |
|---|---|
| `log_bin` | `ON` |
| `binlog_format` | `ROW` |
| `binlog_row_image` | `FULL` |
| `binlog_row_metadata` | `FULL`(MySQL 8.0.1+ 필요) |
| `gtid_mode` | `ON`(권장) |
| `server_id` | 복제 토폴로지 내 유일한 정수 |
| 계정 권한 | `REPLICATION SLAVE`, `REPLICATION CLIENT` |

MySQL 8.4에서는 `SHOW MASTER STATUS`가 `SHOW BINARY LOG STATUS`로 이름이 바뀌었다 — 엔진이 양쪽 모두 처리한다.

## 4. 사용 예

```bash
# 풀 백업
x-backup backup --profile mysql --type full

# 증분 백업(mysql_binlog=true 필요) — 직전 캡처 이후 binlog ROW 변경만
x-backup backup --profile mysql --type incr

# 풀 복구(다른 DB로 분리 복구)
x-backup restore --profile mysql --target "mysql://u:p@host/restore_db" --force

# 시점 복구(PITR): 풀 + binlog ROW를 목표 시각까지 재생
x-backup restore --profile mysql --target "$URI" --at 2026-06-14T09:00:00Z --force
# 전체 재생(가용한 모든 증분):
x-backup restore --profile mysql --target "$URI" --at latest --force
```

`--at`은 RFC3339(UTC) 또는 `latest`. 경계 필터는 **트랜잭션 단위**로 적용된다 — 각 변경은 GTID의
`immediate_commit_timestamp`(마이크로초)로 스탬프되므로, 한 트랜잭션은 전부 적용되거나 전부
제외되어 **transaction-consistent**하다(GTID 미사용 서버는 이벤트 헤더 초 단위로 폴백). `--id`로
base 풀백업을 고정할 수 있다(미지정 시 최신 MySQL 풀백업).

## 5. 엔진 내부 (요약)

- **아카이브 포맷:** 풀 `xb-mysql-v1`(DDL 프레임 + 행 데이터 프레임 + 매니페스트),
  증분 `xb-mysql-incr-v1`(헤더/ROW 이벤트/끝 프레임). 변경이 없는 구간은 `data.bin`
  없이 매니페스트만 기록한다(empty slice).
- **풀 백업 흐름:** `START TRANSACTION WITH CONSISTENT SNAPSHOT`(InnoDB, REPEATABLE READ)
  → 스냅샷 시점 binlog 좌표(`file:pos`) + `gtid_executed` 기록 → `SHOW CREATE
  TABLE/VIEW/TRIGGER/PROCEDURE/FUNCTION/EVENT`로 DDL 수집 → FK 의존 순서 결정 →
  `SELECT` 스트리밍으로 행 데이터 덤프 → 아카이브 저장.
- **증분 캡처:** `mysql_async` 바이너리 로그 스트림으로 ROW 이벤트
  (WriteRows·UpdateRows·DeleteRows)를 캡처 → 아카이브 저장 → **저장 성공 후** 다음
  체인의 `file:pos`를 매니페스트에 기록(실패 시 다음에 재캡처 — 적용이 idempotent라 안전).
- **gap:** base 풀백업의 binlog 파일이 서버에서 퍼지됐으면 증분은 거부되고 풀 백업으로
  자동 승격한다(exit 4) — slot이 없으므로 binlog 보존 기간과 `incr` 주기를 맞춰야 한다.
- **증분 캡처(상세):** GTID~XID 사이의 변경을 트랜잭션 단위로 버퍼링해 XID(커밋)에서 한꺼번에
  아카이브에 쓴다 — 좌표는 트랜잭션 경계에서만 전진하므로(이벤트/트랜잭션 중간 위치 미기록)
  큰 ROW 이벤트나 한도 도달 시에도 행 누락이 없다. 값은 `TableMapEvent`의 컬럼 타입을 반영해
  BIT→`0x`hex, SET→정수 비트마스크, TIMESTAMP→`FROM_UNIXTIME(...)`으로 정확히 렌더한다.
- **증분 적용(복구/PITR):** 풀 복구 후 binlog ROW 이벤트를 idempotent하게 재생한다.
  WriteRows → `INSERT … ON DUPLICATE KEY UPDATE`, UpdateRows → PK 포함 SET + before-PK WHERE
  (PK 변경 반영), DeleteRows → PK·UK 기반. PK·UK가 없으면 전행 매칭 + `LIMIT 1`로 RBR 1행
  의미를 지킨다. 시점 필터는 트랜잭션 커밋 시각(GTID)으로 적용한다.

함정·근거는 코드 주석(`engine/mysql`) 참조.

## 6. 개발/CI

```bash
make mysql-up                  # docker compose로 MySQL 소스(:3306) + 타깃(:3307) 기동
make test-mysql                # MySQL 엔진 단위 테스트(engine::mysql, DB 불필요)
make test-mysql-integration    # 통합 테스트
make scenario-mysql            # MySQL E2E: 풀→증분→복구→PITR
make mysql-down                # 정리
```

CI(`mysql` 잡, push main + nightly)도 docker compose로 MySQL을 띄워 `scenario-mysql`을
돌린다(상세: [ci.md](ci.md)).

### 격리 워크스페이스 (`scripts/xbenv`)

PostgreSQL/MongoDB와 같은 모델 — 디렉터리 하나에 config·키·store를 격리해 테스트하고,
`rm -rf`로 흔적 없이 지워진다.

```bash
scripts/xbenv new ./ws --engine mysql   # 워크스페이스 + 격리 DB(xbenv_ws, xbenv_ws_restore) 생성
source ./ws/activate                    # 활성화 → 프롬프트 (xbenv:ws)
x-backup backup --type full             # --profile 불필요(XB_PROFILE 자동), mysql_binlog 켜짐
x-backup backup --type incr
x-backup restore --target "$XBENV_TARGET_URI" --at latest --force
deactivate                              # 환경 원복
scripts/xbenv destroy ./ws             # 워크스페이스 + 격리 DB 제거
```

**make 단축** — 빌드 + 컨테이너 기동 + 워크스페이스 생성을 한 번에:

```bash
make xbenv-mysql     # build + mysql-up + .xbenv-mysql 생성 → activate 한 줄 출력
make xbenv-clean     # 워크스페이스(+격리 DB) 제거
```

활성화(`source`)는 부모 셸 환경을 바꿔야 해서 make 타깃으로는 불가하다. make는 마지막에
실행할 `source ...` 한 줄을 출력한다. 셸 rc 래퍼:

```bash
# ~/.zshrc 또는 ~/.bashrc
xbenv() { make -s "xbenv-$1" && source ".xbenv-$1/activate"; }
# 사용: xbenv mysql
```

## 7. 알려진 제한 사항

| 항목 | 내용 |
|---|---|
| FLOAT/DOUBLE 정밀도 | 서버 텍스트 표현 사용 — `mysqldump`와 동등하며 10진 소수 완전 보존 미보장 |
| DDL 격리 | 일관 스냅샷이 InnoDB에만 유효; 백업 중 병행 `ALTER TABLE` 등 차단 불가 |
| PITR 정밀도 | GTID 커밋 타임스탬프(마이크로초)로 트랜잭션 단위 컷; GTID 미사용 서버는 이벤트 헤더 초 단위로 폴백 |
| 선택 백업(`--collection`) | 단일 테이블 데이터 전용 — 뷰/트리거/루틴/이벤트는 덤프하지 않는다(누락 테이블 참조로 인한 복구 실패 방지) |
| View/Trigger sql_mode | 객체별 `sql_mode` 미재현; 복구 시 단일 관대 모드 사용 |
| MariaDB | `mariadb://` URI 지원, best-effort(MySQL 8.0 기준 실증) |
| migrate | MySQL → MySQL만 가능(엔진 간 이전 불가) |
