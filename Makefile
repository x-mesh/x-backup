# x-backup 개발 Makefile — 빌드 / 테스트 / 테스트용 DB 컨테이너 / E2E 시나리오
#
# `make` 또는 `make help`로 타깃 목록을 본다.

SHELL := /bin/bash

COMPOSE_MONGO := docker compose -f docker/docker-compose.mongodb.yaml
COMPOSE_PG    := docker compose -f docker/docker-compose.postgres.yaml

# MongoDB Database Tools — 프로젝트 로컬 설치(.tools/, gitignore 대상).
# 버전·sha256은 docs/spike-oplog-archive.md §2와 동일하게 핀.
# 다른 플랫폼은 TOOLS_URL/TOOLS_SHA 오버라이드(linux x86_64는 .tgz — docs/ci.md 참고).
TOOLS_DIR     := .tools
TOOLS_VERSION := 100.16.1
TOOLS_URL     ?= https://fastdl.mongodb.org/tools/db/mongodb-database-tools-macos-arm64-$(TOOLS_VERSION).zip
TOOLS_SHA     ?= 5bef906f3d9b593e70155b01b8eff8de37f9717cfc1c77568853a9b122e6adbf
TOOLS_PATH    := $(abspath $(TOOLS_DIR))/bin

.DEFAULT_GOAL := help

.PHONY: help build build-debug lint fmt test test-integration test-s3 \
        mongodb-up mongodb-down postgres-up postgres-down tools scenario clean \
        devenv devenv-down

help: ## 타깃 목록 출력
	@grep -E '^[a-zA-Z0-9_-]+:.*?## ' $(MAKEFILE_LIST) | awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-18s\033[0m %s\n", $$1, $$2}'

# ── 빌드 ──────────────────────────────────────────────────────────────

build: ## release 빌드 → target/release/x-backup
	cargo build --release

build-debug: ## 디버그 빌드(심볼 포함, 디버거 연결용) → target/debug/x-backup
	cargo build

lint: ## cargo fmt --check + clippy -D warnings (CI lint job과 동일)
	cargo fmt --all -- --check
	cargo clippy --all-targets --all-features -- -D warnings

fmt: ## cargo fmt --all 적용
	cargo fmt --all

# ── 테스트 ────────────────────────────────────────────────────────────

test: ## 단위(lib) + exit code E2E — DB·Docker 불필요
	cargo test --lib
	cargo test --test exit_codes_e2e

test-integration: tools ## 통합 테스트 — 자체 fixture(:27017) 사용. compose mongodb와 동시 사용 금지
	tests/fixtures/replica-set.sh up
	PATH="$(TOOLS_PATH):$$PATH" cargo test --features integration-tests -- --include-ignored --test-threads=1; \
	  status=$$?; tests/fixtures/replica-set.sh down; exit $$status

test-s3: ## S3(MinIO) 통합 테스트 — 테스트가 MinIO 컨테이너를 자체 기동·정리
	cargo test --features s3-integration --test s3_minio

# ── 테스트용 DB 컨테이너 ──────────────────────────────────────────────

mongodb-up: ## MongoDB replica set 기동(소스 :27017 + 복구 타깃 :27117, healthy까지 대기)
	$(COMPOSE_MONGO) up -d --wait

mongodb-down: ## MongoDB 컨테이너·볼륨 정리
	$(COMPOSE_MONGO) down -v

postgres-up: ## PostgreSQL :5432 기동(PG 엔진 백업/복구/status 테스트용)
	$(COMPOSE_PG) up -d --wait

postgres-down: ## PostgreSQL 정리
	$(COMPOSE_PG) down -v

# ── 도구·시나리오 ─────────────────────────────────────────────────────

tools: $(TOOLS_DIR)/bin/mongodump ## mongodump/mongorestore 로컬 설치(.tools/, sha256 검증)

$(TOOLS_DIR)/bin/mongodump:
	mkdir -p $(TOOLS_DIR)
	curl -fsSL -o $(TOOLS_DIR)/tools.zip "$(TOOLS_URL)"
	echo "$(TOOLS_SHA)  $(TOOLS_DIR)/tools.zip" | shasum -a 256 -c -
	unzip -q -o $(TOOLS_DIR)/tools.zip -d $(TOOLS_DIR)
	mkdir -p $(TOOLS_DIR)/bin
	cp $(TOOLS_DIR)/mongodb-database-tools-*/bin/* $(TOOLS_DIR)/bin/
	rm -f $(TOOLS_DIR)/tools.zip

scenario: build tools mongodb-up ## E2E 시나리오 실행(docs/test-scenario.md) — 풀/증분/검증/복구/PITR
	PATH="$(TOOLS_PATH):$$PATH" scripts/scenario-e2e.sh

# ── 대화형 테스트 환경(scripts/xb 래퍼) ───────────────────────────────

devenv: build tools mongodb-up ## 손쉬운 테스트 환경 — 컨테이너+config+키 준비 후 scripts/xb 안내
	scripts/xb setup
	@printf '\n다음처럼 쓰세요:\n  scripts/xb seed\n  scripts/xb backup\n  scripts/xb list\n  scripts/xb verify-latest\n  scripts/xb restore-target\n  eval "$$(scripts/xb env)"   # 환경만 export\n'

devenv-down: ## 테스트 환경 정리(컨테이너 종료 + .devenv 삭제)
	scripts/xb down

clean: ## 빌드 산출물·도구 제거(컨테이너는 *-down 타깃으로)
	cargo clean
	rm -rf $(TOOLS_DIR)
