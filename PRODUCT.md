# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Users

- 주 사용자: MongoDB·PostgreSQL·MySQL을 운영하며 x-backup으로 백업·복구를 돌리는 운영자(DBA·SRE·백엔드 개발자). 한 사람이 여러 DB 프로파일을 관리한다(`docs/control-server.ko.md`의 control 서버 운영 형태). _저장소 근거로 추정했다._
- 사용 장면: 노트북·데스크톱 브라우저에서 연다. 보통 SSH 터널로 `127.0.0.1:8787`에 붙는다. 모바일은 가로로 넘치지 않고 읽을 수 있는 수준이면 된다(2026-09-28 사용자 확인).
- 하는 일: 전 프로파일 상태를 한눈에 확인하고, 백업·검증·복구·정리(prune)·마이그레이션을 실행하고, 잡 진행과 이력을 지켜본다.

## Product Purpose

x-backup은 외부 덤프 도구 없이 DB를 암호화·검증·복구 가능한 형태로 백업하는 Rust 단일 바이너리 CLI다. 웹 콘솔(`x-backup serve`)은 같은 작업을 브라우저에서 운영하게 한다. 성공의 기준은 운영자가 "지금 무엇이 위험한가"를 몇 초 안에 파악하고, 파괴적 작업을 실수 없이 실행하는 것이다.

## Positioning

웹 콘솔은 백업 로직을 따로 갖지 않는다. 모든 동작을 `x-backup <command> --json` 자식 프로세스로 실행하고 그 출력을 중계한다. 그래서 화면에서 한 일과 터미널에서 한 일이 갈라지지 않는다. 콘솔은 CLI의 exit code 규약(0~5)과 상태 어휘를 그대로 화면에 옮긴다.

## Operating Context

- 화면 15개: Console, Dashboard, Monitor, Catalog, Backup, Verify, Peek, Restore, Prune, Migrate, Schedule, Jobs, Lock, Config, Doctor.
- 운영 흐름: 개요(Dashboard·Monitor·Catalog) → 실행(Backup·Verify·Peek·Restore·Prune·Migrate·Schedule) → 사후(Jobs·Lock) → 설정(Config·Doctor).
- 라이브 화면(Monitor, 잡 진행)은 SSE로 갱신한다.
- restore·prune·migrate는 감사 로그 기록과 확인 절차를 거친 뒤 실행된다.

## Capabilities and Constraints

- 렌더링: Rust axum + maud 서버 렌더링. node 툴체인을 도입하지 않는다(R40).
- 에셋: `src/web/assets/app.css` 한 파일을 `include_str!`로 바이너리에 넣는다(R41). 외부 리소스(CDN, 웹폰트 URL)를 두지 않는다.
- 보안: 기본 루프백 바인드, 토큰 인증, 화면·로그에 시크릿 미노출. 마크업에 `PreEscaped`를 쓰지 않는다(저장형 XSS 차단).
- 상태 표현 계약: 상태는 `data-level`(`ok`/`warn`/`fail`/`error`)로만 싣고 색은 CSS 한 곳에서 정한다. `fail`(점검 결과가 안 된다)과 `error`(점검 자체를 못 했다)는 구분한다.
- 언어: 라벨·기술용어는 영문 고정, 설명 문장만 ko/en을 고른다(R42).

## Brand Commitments

- 제품명 `x-backup`은 소문자 그대로 쓴다.
- 이전 아이덴티티 "터미널 미학"(녹빛 검정, ANSI 색, 전부 등폭, 스캔라인)은 2026-09-28 사용자 결정으로 폐기한다. 교체 방향은 새로 정한다.

## Evidence on Hand

- 실제 화면 데이터: 로컬 `.xbackup/config.toml`의 프로파일 2개(MariaDB), 로컬 destination의 백업 5개.
- 데모 GIF와 테이프 스크립트: `docs/assets/`.
- 고객·도입 사례·벤치마크 수치는 화면에 만들어 넣지 않는다.

## Product Principles

1. 상태가 먼저다. 화면을 연 운영자가 가장 먼저 보는 것은 무엇이 정상이고 무엇이 문제인지다.
2. CLI와 같은 말을 한다. exit code, 상태 라벨, 옵션 이름은 CLI와 같다.
3. 파괴적 작업은 느리게, 조회는 빠르게. 확인 절차는 줄이지 않는다.
4. 색에만 의존하지 않는다. 상태는 항상 글자로도 말한다.

## Accessibility & Inclusion

- 상태는 색과 글자를 함께 쓴다(WCAG 1.4.1).
- 키보드 포커스 표시를 지우지 않는다.
- `prefers-reduced-motion`을 따른다.
