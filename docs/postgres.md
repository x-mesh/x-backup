# PostgreSQL 엔진

> 대상 독자: x-backup으로 PostgreSQL을 백업/복구하려는 운영자·개발자.
> 핵심: **외부 `pg_dump`/`pg_restore` 없이** 드라이버(tokio-postgres) 프로토콜만으로
> 풀 백업·복구·증분(logical decoding)·시점 복구(PITR)를 수행한다.

DB 종류는 source URI 스킴으로 자동 판별한다(`postgres://` / `postgresql://` → PostgreSQL,
그 외 → MongoDB). 별도 플래그가 필요 없다.

## 1. 기능 요약

| 기능 | 명령 | 비고 |
|---|---|---|
| 풀 백업 | `backup --type full` | 스키마 DDL + `COPY` 데이터(`xb-pg-v1`) |
| 증분 백업 | `backup --type incr` | logical decoding/pgoutput(`xb-pg-incr-v1`), opt-in |
| 풀 복구 | `restore` | 스키마 재생성 + `COPY FROM STDIN` |
| 시점 복구(PITR) | `restore --at <RFC3339>\|latest` | 풀 복원 + 증분 DML 재생 |
| 상태 | `status` | 연결·버전·DB 크기·테이블/행 수 |
| 미리보기 | `peek` | 테이블/행 수·샘플 행 |
| 마이그레이션 | `migrate` | PG→PG 드라이버 직접(외부 도구 없음) |

압축(zstd)·암호화(age/aes-256-gcm)·로컬/S3 destination·멀티 destination 복제는 Mongo
경로와 동일한 파이프라인을 그대로 탄다.

## 2. 스키마 충실도

**덤프/복구 대상:** 테이블·제약(PK/FK/UNIQUE/CHECK)·인덱스·시퀀스(파라미터 포함)·행 데이터,
확장(extension), 타입(enum/domain/composite), 함수(`f`/`p`), 트리거, 뷰·머티리얼라이즈드 뷰,
파티셔닝(RANGE/LIST/DEFAULT — 부모 `PARTITION BY` + 자식 `PARTITION OF`, 다중 레벨; 자식
데이터를 직접 COPY), IDENTITY(GENERATED ALWAYS/BY DEFAULT)·생성(STORED) 컬럼.

**미지원(경고만 — 백업은 진행):**
- 집계(`a`)/윈도우(`w`) 함수 — `pg_get_functiondef`가 정의를 주지 않는다.
- 사용자 정의 range/base 타입.

## 3. 설정

```toml
[profiles.prod.source]
uri_env = "PROD_PG_URI"          # 예: postgres://user:pass@host:5432/db?sslmode=require

[profiles.prod.features.incremental]
pg_logical = true                # PG 증분 사용 여부(기본 false, 명시 opt-in)
```

- **`pg_logical`** (기본 `false`): `true`면 풀 백업이 replication slot + publication(FOR ALL
  TABLES)을 만들어 그 시점부터 WAL을 잡고, `backup --type incr`로 변경을 캡처한다. 미사용
  slot은 WAL을 무한 보존해 디스크를 채울 수 있으므로 **명시적 opt-in**이다.
- **`wal_level=logical`** (서버 전제, 증분에만): PG 증분은 logical decoding이 필요하다.
  서버에서 `ALTER SYSTEM SET wal_level=logical;` 후 재시작한다(풀 백업/복구만 쓰면 불필요).
  풀 백업 시 `pg_logical=true`면 `SHOW wal_level`로 사전 점검하고 아니면 거부한다(exit 3).
- **`sslmode`** (URI): `prefer`(기본)면 TLS 시도 후 미지원 서버엔 평문 폴백, `require`/
  `verify-*`면 TLS 강제(rustls + webpki 신뢰 루트).

## 4. 사용 예

```bash
# 풀 백업(증분 활성 프로파일이면 이때 slot+publication 생성)
x-backup backup --profile prod --type full

# 증분 백업(pg_logical=true 필요) — 직전 캡처 이후 변경만
x-backup backup --profile prod --type incr

# 풀 복구(다른 DB로 분리 복구)
x-backup restore --profile prod --target "postgres://u:p@host/restore_db" --force

# 시점 복구(PITR): 풀 + 증분을 목표 시각까지 재생
x-backup restore --profile prod --target "$URI" --at 2026-06-14T09:00:00Z --force
# 전체 재생(가용한 모든 증분):
x-backup restore --profile prod --target "$URI" --at latest --force
```

`--at`은 RFC3339(UTC) 또는 `latest`. 경계는 각 변경의 commit 타임스탬프(아카이브 내장)로
거르므로 마이크로초 정밀도다. `--id`로 base 풀백업을 고정할 수 있다(미지정 시 최신 PG 풀백업).

## 5. 엔진 내부 (요약)

- **아카이브 포맷:** 풀 `xb-pg-v1`(태그 프레임 H/R/O/Q/T/D/X/E), 증분 `xb-pg-incr-v1`
  (헤더/변경/끝 프레임, 변경당 BSON).
- **증분 캡처:** `pg_logical_slot_peek_binary_changes`(비소비)로 pgoutput을 읽어 디코드 →
  아카이브 저장 → **저장 성공 후** `pg_replication_slot_advance`로 slot 전진(실패 시 다음에
  재캡처 — 적용이 idempotent라 안전).
- **증분 적용(복구):** `session_replication_role=replica`로 FK/트리거 우회, Insert=키 충돌
  upsert(`OVERRIDING SYSTEM VALUE`), Update/Delete=키 기반. 값은 텍스트로 받아
  `$n::text::<타입>`로 캐스트(대상 카탈로그에서 컬럼명으로 타입 조회). 적용 후 IDENTITY/serial
  시퀀스를 `max(컬럼)`으로 재동기화한다(복구 후 PK 충돌 방지).
- **gap:** slot이 없으면(드롭 등) 증분은 거부된다(`PrecheckFailed`, exit 3) — 풀 백업을 다시
  수행해 slot을 재생성한다.

함정·근거는 코드 주석과 메모리(`pg-logical-decoding-apply`, `pg-identifier-quoting-and-regclass`)
참조.

## 6. 개발/CI

```bash
make postgres-up      # docker compose로 PG(wal_level=logical) 기동(증분 가능)
make test-pg          # PG 엔진 단위 테스트(engine::postgres, DB 불필요)
make scenario-pg      # PG E2E: 풀→증분×2→복구→PITR(전체/중간)→시퀀스 재동기화
make postgres-down    # 정리
```

CI(`postgres` 잡, push main + nightly)도 동일하게 docker compose로 PG를 띄워 `scenario-pg`를
돌린다(상세: [ci.md](ci.md)). GitHub 서비스 컨테이너는 `command`(`-c wal_level=logical`)
오버라이드를 지원하지 않아 docker compose를 쓴다.

### 격리 워크스페이스 (`scripts/xbenv`)

Python venv처럼 **디렉터리 하나에 config·키·store를 격리**해 테스트한다. 전역 설정을 건드리지
않고, `rm -rf`로 흔적 없이 지워진다(`activate`가 `XB_CONFIG`/`XB_PROFILE`/시크릿/PATH를 잡음).

```bash
scripts/xbenv new ./ws --engine pg     # 워크스페이스 + 격리 DB(xbenv_ws, xbenv_ws_restore) 생성
source ./ws/activate                    # 활성화 → 프롬프트 (xbenv:ws)
x-backup backup --type full             # --profile 불필요(XB_PROFILE 자동), pg_logical 켜짐
x-backup backup --type incr
x-backup restore --target "$XBENV_TARGET_URI" --at latest --force
deactivate                              # 환경 원복
scripts/xbenv destroy ./ws             # 워크스페이스 + 격리 DB + slot 제거
```

`--engine mongo`면 source=:27017 RS / target=:27117 컨테이너를 가리킨다. 자세한 사용법은
`scripts/xbenv help`.
