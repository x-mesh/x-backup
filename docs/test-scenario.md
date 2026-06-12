# E2E 테스트 시나리오

`make scenario` 한 번으로 운영 흐름 전체(풀 → 증분 체인 → 검증 → 타깃 분리 복구 → PITR)를
컨테이너 기반으로 실행·판정한다. 실행체는 `scripts/scenario-e2e.sh`, 환경은
`docker/docker-compose.mongodb.yaml`(소스 `:27017` + 복구 타깃 `:27117`, 각각 single-node
replica set, healthcheck 안에서 `rs.initiate` 멱등 수행).

## 전제

| 항목 | 준비 방법 |
|------|----------|
| release 바이너리 | `make build` |
| mongodump/mongorestore 100.16.1 | `make tools` (`.tools/`에 sha256 검증 설치) |
| MongoDB 소스·타깃 컨테이너 | `make mongodb-up` (`--wait`로 PRIMARY 선출까지 대기) |

`make scenario`는 위 3개를 의존 타깃으로 자동 수행한다.
주의: 통합 테스트 fixture(`tests/fixtures/replica-set.sh`, `:27017`)와 compose 소스가
같은 포트를 쓰므로 동시에 띄우지 않는다.

## 단계와 기대 결과

| # | 단계 | 검증(assertion) |
|---|------|----------------|
| 1 | age 키쌍(`cargo run --example gen_age_key`) + config.toml 생성 | 시크릿은 env 참조만(`uri_env`), 평문 없음 |
| 2 | 시드(events 1000 + users 200, unique index) 후 `status --json` | exit 0 또는 4(경고 동반 — 작은 oplog 윈도우 등) |
| 3 | `backup --type full` | exit 0, 산출물 3종(data.bin/manifest/사이드카), **암호문**(archive magic `6de29981` 부재 — SC3) |
| 4 | 쓰기 A → `incr` → **PITR 기준점 기록** → 쓰기 B → `incr` | 두 증분 모두 exit 0, 체인 full+incr×2 |
| 5 | `verify`(구조) / `--deep` / `--chain` | 모두 exit 0. **키 env 없이 `--deep`은 exit 1 거부**(§8.5) |
| 6 | `restore --dry-run` → `--target <타깃> --force` | dry-run 무변경. 풀 복구는 **base 스냅샷만**(events 1000, A·B 없음 — PRD FR-3) |
| 7-1 | PITR `--at <B 이후>` | 체인 전체 재생 — 소스와 동일(A=50, B=50). `--only` 병용은 exit 2 즉시 거부 |
| 7-2 | PITR `--at <A·B 사이>` | **A=50 존재, B=0 부재 — SC4** |
| 8 | `prune --keep-full 1 --dry-run` | exit 0, 무변경(유일 체인 보존) |

쓰기 A/B와 기준점 사이에 `sleep 1.2`를 두는 이유: `--at`은 초 단위 RFC3339이고
oplog `ts` 내림 매핑이 초 경계에서 결정되므로(PRD FR-3), 같은 초에 A·기준점·B가
몰리면 판정이 비결정적이 된다.

## 수동 실행

```bash
make mongodb-up
PATH=".tools/bin:$PATH" scripts/scenario-e2e.sh
make mongodb-down          # 정리(데이터 비영속)
```

PostgreSQL 컨테이너(`make postgres-up`)는 2차 PostgreSQL 어댑터(PRD §12) 개발 대비용이며
현재 시나리오에서는 사용하지 않는다.
