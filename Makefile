# x-backup 개발 Makefile — 빌드 / 테스트 / 테스트용 DB 컨테이너 / E2E 시나리오
#
# `make` 또는 `make help`로 타깃 목록을 본다.

SHELL := /bin/bash

COMPOSE_MONGO := docker compose -f docker/docker-compose.mongodb.yaml
COMPOSE_PG    := docker compose -f docker/docker-compose.postgres.yaml
COMPOSE_MYSQL := docker compose -f docker/docker-compose.mysql.yaml

# MongoDB Database Tools — 프로젝트 로컬 설치(.tools/, gitignore 대상).
# 버전·sha256은 docs/spike-oplog-archive.md §2와 동일하게 핀.
# 다른 플랫폼은 TOOLS_URL/TOOLS_SHA 오버라이드(linux x86_64는 .tgz — docs/ci.md 참고).
TOOLS_DIR     := .tools
TOOLS_VERSION := 100.16.1
TOOLS_URL     ?= https://fastdl.mongodb.org/tools/db/mongodb-database-tools-macos-arm64-$(TOOLS_VERSION).zip
TOOLS_SHA     ?= 5bef906f3d9b593e70155b01b8eff8de37f9717cfc1c77568853a9b122e6adbf
TOOLS_PATH    := $(abspath $(TOOLS_DIR))/bin

# 현재 패키지 버전(Cargo.toml [package].version) — bump/tag/release가 공유.
# release.sh와 동일한 추출식(행 시작 version만 매칭 — 의존성의 들여쓴 version은 제외).
VERSION  := $(shell grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')
# 디버그 실행 기본값 — `make run ARGS="backup --config ..."`, `make run RUST_LOG=trace`로 오버라이드.
RUST_LOG ?= debug
ARGS     ?=

# 로컬 설치 위치 — install.sh 기본과 동일($HOME/.local/bin, sudo 불필요).
# 시스템 전역은 `make install PREFIX=/usr/local`(쓰기 권한 필요할 수 있음).
PREFIX ?= $(HOME)/.local
BINDIR := $(PREFIX)/bin

.DEFAULT_GOAL := help

.PHONY: help build build-debug run debug lint fmt install uninstall \
        bump-patch bump-minor bump-major tag release release-dry release-skip-tap \
        test test-integration test-s3 test-pg test-pg-integration test-mysql test-mysql-integration \
        mongodb-up mongodb-down postgres-up postgres-down mysql-up mysql-down \
        tools scenario scenario-pg scenario-mysql \
        clean clean-all devenv devenv-down xbenv-mongo xbenv-pg xbenv-mysql xbenv-both xbenv-clean

help: ## 타깃 목록 출력
	@grep -E '^[a-zA-Z0-9_-]+:.*?## ' $(MAKEFILE_LIST) | awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-18s\033[0m %s\n", $$1, $$2}'

# ── 빌드 ──────────────────────────────────────────────────────────────

build: ## release 빌드 → target/release/x-backup
	cargo build --release

build-debug: ## 디버그 빌드(심볼 포함, 디버거 연결용) → target/debug/x-backup
	cargo build

run: build-debug ## 디버그 빌드 실행(RUST_LOG=debug 기본, 백트레이스 ON). 인자: ARGS="backup ..."
	RUST_LOG=$(RUST_LOG) RUST_BACKTRACE=1 target/debug/x-backup $(ARGS)

debug: build-debug ## rust-lldb로 디버거 기동(브레이크포인트·스텝). 인자: ARGS="..."
	@command -v rust-lldb >/dev/null 2>&1 || { echo "rust-lldb 없음 — rustup component add llvm-tools / Xcode CLT 확인"; exit 1; }
	RUST_BACKTRACE=1 rust-lldb -- target/debug/x-backup $(ARGS)

lint: ## cargo fmt --check + clippy -D warnings (CI lint job과 동일)
	cargo fmt --all -- --check
	cargo clippy --all-targets --all-features -- -D warnings

fmt: ## cargo fmt --all 적용
	cargo fmt --all

# ── 설치(로컬) ────────────────────────────────────────────────────────
# release 바이너리를 $(BINDIR)에 복사한다. brew 설치본($(brew --prefix)/bin/x-backup)과는
# 다른 디렉터리라 파일 충돌 없이 공존하며, 실제 실행 바이너리는 PATH 우선순위가 정한다
# (보통 ~/.local/bin이 앞 → 로컬 빌드가 brew 것을 가린다. 확인: which -a x-backup).

install: build ## release 바이너리를 로컬 설치 → $(BINDIR)/x-backup. 위치 변경: PREFIX=/usr/local
	@mkdir -p "$(BINDIR)"
	install -m 0755 target/release/x-backup "$(BINDIR)/x-backup"
	@printf '\033[1;32m✓\033[0m 설치: %s (%s)\n' "$(BINDIR)/x-backup" "$$("$(BINDIR)/x-backup" --version 2>/dev/null)"
	@case ":$$PATH:" in *":$(BINDIR):"*) ;; \
	  *) printf '\033[1;33m주의\033[0m %s 가 PATH에 없습니다 — 셸 rc에 추가: export PATH="%s:$$PATH"\n' "$(BINDIR)" "$(BINDIR)" ;; esac
	@if command -v brew >/dev/null 2>&1 && [ -x "$$(brew --prefix)/bin/x-backup" ]; then \
	  printf 'brew 설치본과 공존: %s/bin/x-backup — 실제 실행은 PATH 우선순위 기준(확인: which -a x-backup)\n' "$$(brew --prefix)"; \
	fi

uninstall: ## 로컬 설치 제거($(BINDIR)/x-backup) — brew 설치본은 건드리지 않음
	@rm -f "$(BINDIR)/x-backup" && printf '\033[1;32m✓\033[0m 제거: %s\n' "$(BINDIR)/x-backup"

# ── 릴리스 · 버전 · 배포 ──────────────────────────────────────────────
# 표준 흐름: make bump-patch → (CHANGELOG 갱신·커밋) → make tag → make release
# bump은 cargo set-version(cargo-edit) 우선, 없으면 sed+cargo update로 폴백한다.

bump-patch: ## 패치 버전 +1 (Cargo.toml + Cargo.lock)
	$(bump-version)
bump-minor: ## 마이너 버전 +1, 패치 리셋 (Cargo.toml + Cargo.lock)
	$(bump-version)
bump-major: ## 메이저 버전 +1, 마이너·패치 리셋 (Cargo.toml + Cargo.lock)
	$(bump-version)

# $(@:bump-%=%) → patch|minor|major. cargo set-version이 있으면 Cargo.lock까지 갱신,
# 없으면 awk로 semver 증가 후 [package].version만 치환(BSD/GNU sed 공통 -i.bak) + lock 동기화.
define bump-version
	@part='$(@:bump-%=%)'; cur='$(VERSION)'; \
	if command -v cargo-set-version >/dev/null 2>&1; then \
	  cargo set-version --bump "$$part"; \
	else \
	  new=$$(printf '%s' "$$cur" | awk -F. -v p="$$part" -v OFS=. \
	    '{ if(p=="major"){$$1++;$$2=0;$$3=0} else if(p=="minor"){$$2++;$$3=0} else {$$3++} } 1'); \
	  sed -i.bak -E "s/^version = \"[^\"]+\"/version = \"$$new\"/" Cargo.toml && rm -f Cargo.toml.bak; \
	  cargo update -p x-backup --precise "$$new" >/dev/null 2>&1 || cargo update -p x-backup >/dev/null 2>&1 || true; \
	fi; \
	printf '\033[1;32m✓\033[0m 버전: %s → %s  (다음: CHANGELOG 갱신·커밋 → make tag → make release)\n' \
	  "$$cur" "$$(grep -m1 '^version' Cargo.toml | sed -E 's/.*\"([^\"]+)\".*/\1/')"
endef

tag: ## 현재 버전으로 git 태그 v$(VERSION) 생성 + origin push (release 전제조건)
	@v='v$(VERSION)'; \
	git rev-parse "$$v" >/dev/null 2>&1 && { echo "태그 $$v 이미 존재 — 중복 생성 안 함"; exit 1; }; \
	git diff --quiet && git diff --cached --quiet || { echo "워킹트리가 clean이 아님 — 커밋 후 태그하세요"; exit 1; }; \
	git tag "$$v" && git push origin "$$v" && printf '\033[1;32m✓\033[0m 태그 push: %s\n' "$$v"

release: ## 정식 릴리스 게시 — 4플랫폼 빌드·패키징 → GitHub 릴리스 → Homebrew tap 갱신
	scripts/release.sh

release-dry: ## 릴리스 빌드·패키징만 검증(게시/tap 갱신 안 함) → dist/
	scripts/release.sh --dry-run

release-skip-tap: ## 릴리스 게시하되 Homebrew tap 갱신은 생략
	scripts/release.sh --skip-tap

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

test-pg: ## PostgreSQL 엔진 단위 테스트(engine::postgres 모듈) — DB·Docker 불필요
	cargo test --lib engine::postgres

test-pg-integration: postgres-up ## PG cargo 통합 테스트(H3 TOAST·C2 슬롯 gap·풀→복구) — compose PG(wal_level=logical) 자체 기동·정리
	cargo test --features pg-integration --test pg_integration -- --include-ignored --test-threads=1; \
	  status=$$?; $(COMPOSE_PG) down -v; exit $$status

test-mysql: ## MySQL 엔진 단위 테스트(engine::mysql 모듈) — DB·Docker 불필요
	cargo test --lib engine::mysql

test-mysql-integration: mysql-up ## MySQL cargo 통합 테스트(풀→복구 라운드트립·binlog 증분) — compose MySQL(binlog/gtid ON) 자체 기동·정리
	cargo test --features mysql-integration --test mysql_integration -- --include-ignored --test-threads=1; \
	  status=$$?; $(COMPOSE_MYSQL) down -v; exit $$status

# ── 테스트용 DB 컨테이너 ──────────────────────────────────────────────

mongodb-up: ## MongoDB replica set 기동(소스 :27017 + 복구 타깃 :27117, healthy까지 대기)
	$(COMPOSE_MONGO) up -d --wait

mongodb-down: ## MongoDB 컨테이너·볼륨 정리
	$(COMPOSE_MONGO) down -v

postgres-up: ## PostgreSQL 소스 :5432 + 타깃 :5433 기동(PG 엔진 백업/복구/migrate 테스트용)
	$(COMPOSE_PG) up -d --wait

postgres-down: ## PostgreSQL 정리(소스+타깃)
	$(COMPOSE_PG) down -v

mysql-up: ## MySQL 소스 :3306 + 타깃 :3307 기동(binlog/gtid ON — 백업/복구/migrate·증분 테스트용)
	$(COMPOSE_MYSQL) up -d --wait

mysql-down: ## MySQL 정리(소스+타깃)
	$(COMPOSE_MYSQL) down -v

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

scenario-pg: build postgres-up ## PostgreSQL E2E — 풀/증분(pgoutput)/복구/PITR(전체·중간)/시퀀스 재동기화
	scripts/scenario-pg-e2e.sh

scenario-mysql: build mysql-up ## MySQL E2E — 풀/증분(binlog ROW)/복구/PITR(전체·중간)
	scripts/scenario-mysql-e2e.sh

# ── 대화형 테스트 환경(scripts/xb 래퍼) ───────────────────────────────

devenv: build tools mongodb-up ## 손쉬운 테스트 환경 — 컨테이너+config+키 준비 후 scripts/xb 안내
	scripts/xb setup
	@printf '\n다음처럼 쓰세요:\n  scripts/xb seed\n  scripts/xb backup\n  scripts/xb list\n  scripts/xb verify-latest\n  scripts/xb restore-target\n  eval "$$(scripts/xb env)"   # 환경만 export\n'

devenv-down: ## 테스트 환경 정리(컨테이너 종료 + .devenv 삭제)
	scripts/xb down

# ── 격리 워크스페이스(scripts/xbenv, venv 모델) ───────────────────────
# 빌드 + 컨테이너 기동 + 워크스페이스 생성까지 한 번에. 활성화(source)는 부모 셸 환경을
# 바꿔야 해서 make로 불가하므로(서브셸 한계), 마지막에 실행할 source 한 줄을 출력한다.
# 경로는 기본 .xbenv-mongo/.xbenv-pg, `DIR=경로`로 변경.

xbenv-mongo: build mongodb-up ## Mongo 격리 워크스페이스 준비 + activate 안내(DIR=로 경로 변경)
	@d='$(or $(DIR),.xbenv-mongo)'; [ -f "$$d/activate" ] || scripts/xbenv new "$$d" --engine mongo --name mongo; \
	  printf '\n  활성화: \033[36msource %s/activate\033[0m   (해제: deactivate · 제거: make xbenv-clean)\n' "$$d"

xbenv-pg: build postgres-up ## PG 격리 워크스페이스 준비 + activate 안내(DIR=로 경로 변경)
	@d='$(or $(DIR),.xbenv-pg)'; [ -f "$$d/activate" ] || scripts/xbenv new "$$d" --engine pg --name pg; \
	  printf '\n  활성화: \033[36msource %s/activate\033[0m   (해제: deactivate · 제거: make xbenv-clean)\n' "$$d"

xbenv-mysql: build mysql-up ## MySQL 격리 워크스페이스 준비 + activate 안내(DIR=로 경로 변경)
	@d='$(or $(DIR),.xbenv-mysql)'; [ -f "$$d/activate" ] || scripts/xbenv new "$$d" --engine mysql --name mysql; \
	  printf '\n  활성화: \033[36msource %s/activate\033[0m   (해제: deactivate · 제거: make xbenv-clean)\n' "$$d"

xbenv-both: build mongodb-up postgres-up ## mongo+pg 결합 워크스페이스(프로파일 mongo/pg + *-target) + 안내
	@d='$(or $(DIR),.xbenv-both)'; [ -f "$$d/activate" ] || scripts/xbenv new "$$d" --engine both --name both; \
	  printf '\n  활성화: \033[36msource %s/activate\033[0m   (해제: deactivate · 제거: make xbenv-clean)\n' "$$d"

xbenv-clean: ## 격리 워크스페이스 제거(.xbenv-mongo/.xbenv-pg/.xbenv-both, 또는 DIR=)
	@for d in $(or $(DIR),.xbenv-mongo .xbenv-pg .xbenv-both); do \
	  if [ -e "$$d" ]; then scripts/xbenv destroy "$$d" --yes; fi; \
	done

clean: ## 빌드·릴리스 산출물·도구 제거(target/dist/.tools, 컨테이너는 *-down 타깃으로)
	cargo clean
	rm -rf $(TOOLS_DIR) dist

# 컨테이너는 유지 — devenv-down은 mongodb-down까지 호출하므로 의존 대신 .devenv만 직접 제거.
# xbenv-clean은 컨테이너를 내리지 않고 워크스페이스+격리 DB만 정리하므로 그대로 위임한다.
clean-all: clean xbenv-clean ## clean + 격리 워크스페이스(.xbenv-*)·.devenv 메타 제거(컨테이너는 유지)
	rm -rf .devenv
