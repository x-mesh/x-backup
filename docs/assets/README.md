# README 데모 GIF

README 상단과 본문에 들어가는 터미널 녹화본이다. [VHS](https://github.com/charmbracelet/vhs)로
만들며, `.tape` 스크립트를 함께 두므로 언제든 같은 화면을 다시 뽑을 수 있다.

| GIF | 테이프 | 보여주는 것 |
|-----|--------|------------|
| `demo-backup.gif` | `backup.tape` | 풀 백업 → 증분 백업 → `list`의 체인(BASE 컬럼) |
| `demo-status.gif` | `status.tape` | `status` 사전 점검 신호등 16항목 |
| `demo-verify.gif` | `verify.tape` | `verify --deep --chain` — 복구 없이 복구 가능성 검증 |

## 다시 만들기

```bash
brew install vhs          # ttyd·ffmpeg 포함
make build                # 릴리스 바이너리 필요
make mongodb-up           # 로컬 테스트 컨테이너(:27017)

docs/assets/demo-setup.sh # 데모 워크스페이스 + 시드 데이터

vhs docs/assets/backup.tape   # 이 순서로 — status/verify가 backup의 결과를 읽는다
vhs docs/assets/status.tape
vhs docs/assets/verify.tape
```

정리:

```bash
rm -rf /tmp/xb-demo && make mongodb-down
```

## 녹화본에 데이터베이스를 쓰지 않는 이유

세 테이프가 부르는 명령은 **소스 DB를 바꾸지 않는다**. `backup`은 읽기만 하고 백업
파일만 쓰며, `status`·`list`·`verify`는 전부 읽기 전용이다. `restore`는 대상 DB를
덮어쓰므로 일부러 넣지 않았다 — 문서용 녹화를 돌리다 실서비스를 가리키는 사고가
가장 나기 쉬운 지점이기 때문이다. 백업이 실제로 복구 가능한지는 `verify --deep`이
복호화·디코드까지 해서 증명하므로, 복구를 실행하지 않고도 같은 이야기를 할 수 있다.

증분 백업이 잡을 변경만은 만들어야 해서 `demo-churn.sh`가 데이터를 쓴다. 이것과
`demo-setup.sh`는 둘 다 시작할 때 대상을 검사하고, 아래 조건을 하나라도 어기면
아무것도 하지 않고 멈춘다.

- 컨테이너가 이 리포의 `docker compose`가 만든 것인가 (`com.docker.compose.project=docker`)
- 그 컨테이너가 해당 포트를 게시하고 있는가
- 시스템 DB(`admin`/`config`/`local`)와 데모 DB(`shop`) 말고 다른 게 들어 있지 않은가

쓰기는 `shop` 한 곳에만 일어난다.

## 만들 때 걸렸던 것들

- **`Hide`는 프레임 캡처만 멈춘다.** 터미널 버퍼는 그대로라, 숨긴 채로 명령을 치면
  `Show` 뒤에 그 줄이 화면에 남는다. `backup.tape`가 churn을 백그라운드로 예약해 두고
  `clear`로 흔적을 지우는 이유다.
- **경로가 화면에 찍힌다.** `status`와 `doctor`는 config 경로를 그대로 출력하므로,
  워크스페이스를 홈 디렉터리가 아니라 `/tmp/xb-demo`에 만든다. 녹화본에 사용자 이름이
  들어가지 않게 하려는 것이다.
- **시드 데이터는 압축이 잘 되면 안 된다.** 같은 문자를 반복해 채우면 zstd가 수백 KiB로
  눌러 버려서 진행률이 한 프레임에 끝나고 압축률 수치도 현실과 멀어진다. 그래서
  `demo-setup.sh`는 무작위 문자열을 섞어 176 MiB쯤을 만든다.
