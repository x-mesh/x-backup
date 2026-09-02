---
name: release
description: >-
  x-backup 릴리스 게시 자동화. 새 버전을 빌드·패키징해 GitHub 릴리스를 만들고
  Homebrew tap formula를 갱신한다. "릴리스 해줘", "0.x.y 배포/게시", "새 버전 내보내",
  "릴리스 자산 올려", "tap 갱신", "x-backup update가 새 버전을 못 받는다" 같은 요청에서
  사용한다. 4개 플랫폼(darwin/linux × arm64/amd64) 정적 바이너리 + checksums.txt를
  GitHub 릴리스에 올리고 tap의 version/url/sha256까지 한 번에 처리한다.
---

# x-backup 릴리스

`x-backup update`와 `brew upgrade`, `install.sh`가 동작하려면 **GitHub 릴리스에 4개
플랫폼 자산 + `checksums.txt`가 게시**되고 **Homebrew tap formula가 같은 버전·해시로
갱신**돼야 한다. 이 두 가지가 릴리스의 핵심이며, 둘 중 하나라도 빠지면 사용자에게
"0.x.y가 설치되지 않는다"로 나타난다.

> **왜 이게 필요한가**: CI(`.github/workflows/ci.yml`)에는 릴리스 생성 잡이 없다.
> 태그를 push해도 GitHub 릴리스는 자동으로 만들어지지 않는다. 그래서 게시는
> 이 스킬(= `scripts/release.sh`)로 수행한다. (CI에 release 잡을 추가하면 이 스킬을
> 트리거 래퍼로 축소할 수 있다 — 아래 "후속" 참고.)

## 한 줄 실행

```bash
scripts/release.sh                 # Cargo.toml 버전으로 게시 + tap 갱신
scripts/release.sh --version 0.3.0 # 버전 명시
scripts/release.sh --dry-run       # 빌드·패키징까지만(게시 안 함) — 사전 점검용
scripts/release.sh --skip-tap      # GitHub 릴리스만, tap 갱신 생략
```

스크립트가 빌드 → 패키징 → GitHub 릴리스 → tap 갱신 → 검증을 순서대로 수행한다.

## 릴리스 전 체크리스트 (스크립트 실행 *전*에 사람이 한다)

1. `Cargo.toml`의 `version`을 새 버전으로 올린다.
2. `CHANGELOG.md`에 `## [X.Y.Z] - YYYY-MM-DD` 섹션을 추가한다(릴리스 노트로 쓰임).
3. 커밋하고 main에 push한다.
4. **태그를 만들고 push한다** — 스크립트는 `--verify-tag`로 원격 태그를 요구한다:
   ```bash
   git tag vX.Y.Z && git push origin vX.Y.Z
   ```
5. 워킹트리가 clean인지 확인한다(스크립트가 막지만 미리 정리).

## 전제조건(도구)

- `cargo`, `cross`(linux musl 빌드), **실행 중인 `docker`**(cross 백엔드)
- `gh` 인증(`gh auth status`) — 릴리스 게시 + tap clone/push 권한
- `shasum`, `tar`, `ruby`(formula 문법 검증), `rustup`
- **macOS arm64**: cross가 컨테이너에 마운트할 linux 툴체인이 필요하다. 한 번만:
  `rustup toolchain install stable-x86_64-unknown-linux-gnu --force-non-host --profile minimal`
  (없으면 스크립트가 이 명령을 알려주고 중단한다). cross 이미지는 amd64 전용이라
  linux 빌드는 에뮬레이션으로 돌아 느리다 — 스크립트가 자동 설정한다.
- private 저장소 단계: 다운로드 검증 시 `HOMEBREW_GITHUB_API_TOKEN=$(gh auth token)`

## 스크립트가 하는 일(단계별)

1. **사전 점검** — 도구·docker·gh 인증·clean 트리·원격 태그 존재/HEAD 정합성.
2. **빌드 4종** — darwin arm64/amd64는 네이티브 `cargo`, linux amd64/arm64는 `cross`
   (musl 정적). 빌드된 darwin 바이너리의 `--version`이 대상 버전과 일치하는지 확인.
3. **패키징** — tarball 루트에 `x-backup`(0755) **단일 엔트리**. macOS는
   `COPYFILE_DISABLE=1`로 AppleDouble 혼입을 막는다. `checksums.txt`는
   `shasum -a 256` 출력 형식(`<hash>  <name>`) 그대로.
4. **GitHub 릴리스** — 없으면 `gh release create --verify-tag`, 있으면
   `gh release upload --clobber`로 자산 갱신. 노트는 CHANGELOG의 해당 버전 섹션.
5. **Homebrew tap 갱신** — `x-mesh/homebrew-tap`을 clone해 `Formula/x-backup.rb`의
   `version`·4개 `url`·4개 `sha256`을 교체, `ruby -c`로 검증 후 commit/push.
6. **검증** — `releases/latest`가 새 태그인지, 자산 목록 출력.

## 게시 후 검증(설치측, 실제 동작 확인)

```bash
# manual 설치(~/.local/bin) — 다운로드+sha256+self-replace
x-backup update

# Homebrew 설치 — tap formula 경유(private면 토큰 필요)
export HOMEBREW_GITHUB_API_TOKEN=$(gh auth token)
brew update && brew upgrade x-mesh/tap/x-backup
```

둘 다 새 버전으로 올라오면 릴리스 성공. `gh api repos/x-mesh/x-backup/releases/latest
--jq .tag_name`이 새 태그여야 `update`가 갱신을 인식한다.

## 함정 / 트러블슈팅

- **`update`가 "이미 최신 버전입니다"만 출력** → 릴리스가 게시 안 됐거나 latest가
  옛 태그다. `gh release list`와 `releases/latest`를 확인. 태그만 push하고 릴리스를
  안 만든 게 가장 흔한 원인.
- **tarball에 `x-backup`이 없다** → update의 `extract_binary`는 파일명 `x-backup`
  엔트리만 찾는다. 디렉터리째 묶거나 이름이 다르면 추출 실패. 스테이지에 `x-backup`만
  복사해 묶는다(스크립트가 처리).
- **sha256 불일치** → `checksums.txt`의 해시와 자산이 어긋남(재빌드 후 checksums
  갱신 누락). 스크립트는 패키징 직후 한 번에 생성하므로 항상 일치한다.
- **brew가 옛 버전 설치** → `brew update`로 tap을 먼저 최신화해야 새 formula가 보인다.
  `update`의 brew 위임(`brew upgrade`)도 tap이 최신이어야 동작.
- **`scripts/release.sh`를 파이프로 넘기지 않는다** → `| tee`를 붙이면 파이프라인
  종료 코드가 `tee`의 것이 되어 `set -e` 실패가 exit 0으로 보인다. 로그가 필요하면
  `scripts/release.sh > release.log 2>&1`처럼 리다이렉트한다.
- **macOS `sed -i`** → BSD sed는 `-i` 뒤 백업 접미사가 필수다(`-i.bak`). 스크립트는
  이를 지킨다.
- **GitHub API contents 캐시** → push 직후 `contents` API가 옛 내용을 줄 수 있다.
  `?ref=<커밋>`을 붙이거나 `commits/main`으로 확인.

## 후속(근본 자동화)

태그 push 시 darwin/linux 매트릭스 빌드 → checksums → `gh release`를 수행하는
`.github/workflows/release.yml`을 추가하면 GitHub 릴리스가 자동 생성된다. 그 경우
이 스킬은 (a) tap 갱신과 (b) 게시 후 설치측 검증만 담당하도록 줄이면 된다.
darwin 자산은 GitHub Actions의 `macos-*` 러너에서 빌드해야 한다(현재 CI의 musl 잡은
linux만 빌드한다).
