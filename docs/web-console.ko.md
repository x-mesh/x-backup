# 웹 운영 콘솔 운영 가이드 (`x-backup serve`)

`x-backup serve`는 백업·복구·마이그레이션을 브라우저에서 운영하는 **상주 HTTP 서버**다.
CLI를 대체하지 않는다 — 콘솔은 화면에서 받은 요청을 `x-backup <명령> --json` **자식
프로세스로 넘기고 그 출력을 중계**한다. 그래서 화면에서 한 일과 터미널에서 한 일이 갈라질
수 없다.

CLI 중심 운영(cron·systemd timer로 백업 돌리기)은 [control 서버 운영
가이드](control-server.ko.md)를 보라. 이 문서는 **콘솔을 안전하게 띄우는 것**만 다룬다.

> 이 서버는 **복호화 개인키와 프로덕션 접속 정보를 상시 보유**한다. 아래 §2·§3·§4는
> 선택이 아니라 기동 요건이다 — 충족하지 못하면 서버가 뜨지 않는다(fail-closed).

---

## 1. 요약 — 최소 안전 구성

```bash
# 1) 토큰을 0600 파일로 만든다(값을 셸 히스토리·env 목록에 남기지 않는다)
umask 077
openssl rand -base64 24 > /etc/x-backup/web-token
chmod 600 /etc/x-backup/web-token

# 2) 루프백에만 띄운다(기본값)
XB_WEB_TOKEN_FILE=/etc/x-backup/web-token \
XB_CONFIG=/etc/x-backup/config.toml \
  x-backup serve

# 3) 브라우저는 SSH 터널로 붙는다 — 콘솔을 네트워크에 노출하지 않는 가장 싼 방법
ssh -N -L 8787:127.0.0.1:8787 ops@backup-host
# → http://127.0.0.1:8787
```

원격에서 직접 열어야 한다면 §3(바인딩)과 §6(TLS)을 먼저 읽어라.

---

## 2. 인증 — 없으면 뜨지 않는다

토큰을 주는 방법이 두 가지이고 **정확히 하나만** 설정해야 한다(둘 다 설정하면 어느 것이
유효한지 모호해지므로 거부한다).

| 환경변수 | 뜻 | 권장 |
|---|---|---|
| `XB_WEB_TOKEN` | 토큰 값을 직접 | 컨테이너 시크릿 주입 등 파일을 둘 수 없을 때만 |
| `XB_WEB_TOKEN_FILE` | 토큰이 담긴 **0600 파일**의 경로 | ✅ 이쪽 |

`XB_WEB_TOKEN_FILE`을 권하는 이유: env 값은 `/proc/<pid>/environ`·프로세스 목록·
systemd `Environment=` 줄·셸 히스토리에 남는다. 파일은 권한으로 막을 수 있고, 파일 권한이
0600이 아니면 **서버가 거부한다**(symlink를 통한 우회도 대상 파일의 실제 권한을 본다).

### 토큰 요건

- **최소 24바이트.** 짧으면 기동을 거부한다.
- **엔트로피 하한 64비트.** 같은 문자만 24번 반복한 값은 길이를 채워도 거부된다
  (`aaaa…` → 추정 엔트로피 0비트).
- `openssl rand -base64 24`(32자) 또는 `openssl rand -hex 16`으로 만들어라. 도구가 안내하는
  최소값은 `openssl rand -base64 18`(정확히 24자)이지만, 하한에 딱 맞추는 것보다 여유를
  두는 편이 낫다.

이 토큰 하나가 복호화 개인키·프로덕션 접속 정보·config 쓰기 권한 전부를 여는 문이다.
DB 비밀번호와 같은 급으로 취급하라.

### 로그인 후

토큰으로 한 번 로그인하면 **불투명 세션 ID**가 쿠키로 발급된다(`HttpOnly`,
`SameSite=Strict`). 토큰 자체는 쿠키에 담기지 않는다 — 쿠키가 유출되어도 토큰은 그대로다.
세션은 12시간 뒤 만료된다.

> **쿠키는 포트로 격리되지 않는다**(RFC 6265 §8.5). 같은 호스트의 다른 포트에서 도는
> 서비스가 이 콘솔의 세션 쿠키를 함께 받는다. 그래서 콘솔은 쿠키 외에 **동일 출처
> 검사**를 POST마다 한 겹 더 한다. 그래도 신뢰할 수 없는 서비스와 호스트를 공유하지 마라.

---

## 3. 바인딩 — 기본은 루프백, 넘어가려면 명시해야 한다

```
기본값: 127.0.0.1:8787
```

- 루프백이 **아닌** 주소로 띄우려면 `--allow-remote`를 함께 줘야 한다. 없으면 거부한다.
- **IP 리터럴 또는 `localhost`만** 받는다. 호스트명(DNS)은 받지 않는다 — 같은 문자열이
  환경에 따라 다른 주소로 풀리면 "무엇에 바인딩되는가"가 흔들리고 루프백 가드도 우회된다.
- **특권 포트(<1024)와 포트 0은 거부한다.** 백업 콘솔이 root로 돌 이유가 없고, 포트 0은
  매 기동마다 주소가 바뀌어 방화벽 규칙을 쓸 수 없다.

`--allow-remote`는 "이 결정을 내가 알고 한다"는 서명이다. 그 뒤에는 **반드시** 앞단
리버스 프록시가 있어야 한다(§6).

---

## 4. age 개인키 권한

백업이 age로 암호화되어 있으면 복구·검증에 개인키가 필요하고, 콘솔은 그 키를
`XB_AGE_IDENTITY_FILE`로 받는다. 이 파일의 권한이 느슨하면 **기동을 거부한다.**

```bash
chmod 600 /etc/x-backup/age-identity.txt
```

symlink로 우회할 수 없다 — 링크 자체가 아니라 **최종 대상 파일의 실제 권한**을 본다.

키를 두는 위치에 대한 판단은 하나 더 있다. `verify --deep`처럼 복호화가 필요한 작업은
**개인키를 가진 호스트에서** 돌아야 한다. 콘솔 호스트에 키를 두지 않는 운영을 택했다면
그 작업은 콘솔에서 실패하고, 그건 설계된 결과다.

---

## 5. 감사 로그와 상태 파일

`--state-dir`가 정하는 디렉터리 아래에 쌓인다. 미지정 시
`$XDG_STATE_HOME/x-backup` → 없으면 `~/.local/state/x-backup`.

```
<state_dir>/
├── audit.ndjson     ← 감사 로그(append-only)
├── jobs/            ← 잡 이력과 자식 프로세스 로그
└── schedules.json   ← 스케줄 정의(+ 직전본 한 세대)
```

### `audit.ndjson`

한 줄에 항목 하나(NDJSON). **기존 줄은 절대 수정·삭제되지 않는다.** 필드:

| 필드 | 내용 |
|---|---|
| `timestamp` | RFC3339 UTC. **기록 시점에 서버가 직접 채운다** — 호출자가 백데이팅할 수 없다 |
| `actor` | 이 동작을 수행한 주체 |
| `action` | `backup.run` · `restore.run` · `prune.run` · `migrate.run` · `verify.run` 등 고정 어휘 |
| `target` | 프로파일명 등 식별자 |
| `args_masked` | 자식에게 넘긴 인자(시크릿 마스킹 완료) |
| `outcome` | 요청·성공·실패 |
| `exit_code` | 끝난 작업의 종료 코드(진행 중이면 없음) |

`action`이 고정 어휘인 이유: 이 파일은 사람이 아니라 `grep`/스크립트가 훑는 것을
전제한다.

### 핵심 불변식

> **파괴적 작업(restore · prune · migrate)은 감사 로그 append가 성공한 뒤에만 실행된다.
> append 실패 = 작업 거부.**

이것은 관례가 아니라 타입으로 강제된다. 그래서 감사 로그를 쓸 수 없는 상태
(디스크 꽉 참, 권한 오류)에서는 파괴적 작업이 **실행되지 않는다.** state 디렉터리의
여유 공간과 권한을 모니터링하라 — 그게 막히면 복구도 막힌다.

### 로그 로테이션

`audit.ndjson`은 스스로 줄어들지 않는다(append-only가 목적이다). 보존 기간이 정해져
있다면 외부에서 회전시켜라. 단 **회전은 복사 후 절단(copytruncate)이 아니라 이름 변경**
방식을 써야 안전하다 — 서버가 열어 둔 파일 핸들을 존중한다.

```
# /etc/logrotate.d/x-backup-audit
/var/lib/x-backup/audit.ndjson {
    monthly
    rotate 24
    missingok
    notifempty
    create 0600 x-backup x-backup
}
```

---

## 6. TLS — 이 바이너리는 종단하지 않는다

`x-backup serve`는 **평문 HTTP만** 낸다. TLS 종단·인증서 갱신·접근 제한은 앞단
리버스 프록시의 몫이다. 이유는 단순하다: TLS를 직접 하면 인증서 갱신·프로토콜 버전·
암호 스위트 관리가 백업 도구의 릴리스 주기에 묶인다.

루프백 밖으로 바인딩하면 기동 시 이 사실을 stderr로 다시 알린다.

### nginx 예시 — SSE 때문에 기본값을 바꿔야 한다

콘솔에는 **끝나지 않는 응답 스트림**이 두 개 있다:

- `/monitor/events` — 라이브 모니터
- `/backup/{job_id}/events` — 잡 진행률

프록시의 기본 버퍼링·타임아웃은 이런 스트림을 끊거나 지연시킨다. 그러면 화면이 "멈춘 것
처럼" 보인다.

```nginx
server {
    listen 443 ssl;
    server_name backup-console.internal;

    ssl_certificate     /etc/ssl/certs/backup-console.pem;
    ssl_certificate_key /etc/ssl/private/backup-console.key;

    # 접근 제한은 여기서 한다 — 콘솔에는 사용자 개념이 없고 토큰 하나뿐이다.
    allow 10.0.0.0/8;
    deny  all;

    location / {
        proxy_pass http://127.0.0.1:8787;
        proxy_set_header Host              $host;
        proxy_set_header X-Forwarded-Proto $scheme;

        # SSE — 이 세 줄이 없으면 진행률·모니터 화면이 멈춘 것처럼 보인다.
        proxy_buffering    off;   # 응답을 모아 두지 않고 그대로 흘린다
        proxy_read_timeout 1h;    # 유휴 스트림을 끊지 않는다(콘솔은 15초마다 keep-alive를 보낸다)
        proxy_http_version 1.1;
    }
}
```

> 프록시를 두면 콘솔이 보는 요청 출처가 바뀐다. 동일 출처 검사가 프록시 뒤에서도
> 성립하도록 `Host` 헤더를 원본 그대로 넘겨라(위 예시의 `proxy_set_header Host $host`).

---

## 7. systemd 유닛

```ini
# /etc/systemd/system/x-backup-console.service
[Unit]
Description=x-backup web operations console
Documentation=https://github.com/x-mesh/x-backup/blob/main/docs/web-console.ko.md
After=network-online.target
Wants=network-online.target

[Service]
Type=exec
User=x-backup
Group=x-backup

# 토큰은 값이 아니라 0600 파일로 준다(§2) — Environment= 줄에 값을 적지 않는다.
Environment=XB_WEB_TOKEN_FILE=/etc/x-backup/web-token
Environment=XB_CONFIG=/etc/x-backup/config.toml
Environment=XB_AGE_IDENTITY_FILE=/etc/x-backup/age-identity.txt
# 프로덕션 접속 정보는 config가 env 이름으로만 참조한다. 값은 여기서 주입한다.
EnvironmentFile=/etc/x-backup/secrets.env

ExecStart=/usr/local/bin/x-backup serve \
    --bind 127.0.0.1:8787 \
    --state-dir /var/lib/x-backup

Restart=on-failure
RestartSec=5s

# ── 강화 ─────────────────────────────────────────────────────────────
# 이 프로세스는 개인키와 프로덕션 접속 정보를 상시 들고 있다. 권한을 최소로 깎는다.
NoNewPrivileges=yes
PrivateTmp=yes
ProtectSystem=strict
ProtectHome=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectControlGroups=yes
RestrictSUIDSGID=yes
RestrictNamespaces=yes
LockPersonality=yes
MemoryDenyWriteExecute=yes

# 쓸 수 있는 곳은 state 디렉터리뿐이다.
ReadWritePaths=/var/lib/x-backup
# 토큰·키·config는 읽기만.
ReadOnlyPaths=/etc/x-backup

# 루프백에만 띄우므로 그 밖으로 나갈 이유가 없다 — 원격 바인딩을 쓴다면 이 줄을 지워라.
# (백업 대상 DB와 S3가 원격이면 이 제한을 쓸 수 없다. 그때는 대신 방화벽으로 막아라.)
# IPAddressAllow=localhost
# IPAddressDeny=any

[Install]
WantedBy=multi-user.target
```

```bash
# state 디렉터리와 전용 계정
useradd --system --home /var/lib/x-backup --shell /usr/sbin/nologin x-backup
install -d -o x-backup -g x-backup -m 700 /var/lib/x-backup

systemctl daemon-reload
systemctl enable --now x-backup-console
systemctl status x-backup-console
journalctl -u x-backup-console -f
```

### 기동 실패를 읽는 방법

콘솔은 **설정이 부족하면 포트를 열기 전에** 죽는다. `journalctl`의 마지막 줄이 이유를
정확히 말한다.

아래는 **실제 출력 문구**다(발췌).

| 메시지 앞부분 | 원인 | 절 |
|---|---|---|
| `설정 오류: 웹 콘솔 인증이 설정되지 않았습니다` | `XB_WEB_TOKEN`/`XB_WEB_TOKEN_FILE` 둘 다 없음 | §2 |
| `설정 오류: XB_WEB_TOKEN과 XB_WEB_TOKEN_FILE이 동시에 설정되어 있습니다` | 둘 다 설정 — 우선순위를 추측하지 않는다 | §2 |
| `설정 오류: 웹 콘솔 토큰이 너무 짧습니다(N바이트, 최소 24바이트)` | 길이 미달 | §2 |
| `설정 오류: 웹 콘솔 토큰이 너무 단조롭습니다(추정 엔트로피 N비트, 최소 64비트)` | 길이는 채웠지만 문자 종류가 적음 | §2 |
| `설정 오류: 웹 콘솔 토큰 파일 권한이 너무 느슨합니다(현재 644, 0600이어야 함)` | 토큰 파일 권한 | §2 |
| `설정 오류: age 개인키 파일 권한이 너무 느슨합니다(현재 644, 0600이어야 함)` | age 키 권한 | §4 |
| `사용법 오류: --bind …는 루프백 주소가 아닙니다` | `--allow-remote` 없이 외부 바인딩 | §3 |
| `사용법 오류: --bind …의 포트 80는 특권 포트입니다(<1024)` | 1024 미만 포트 | §3 |

권한 오류 메시지는 **어떤 `chmod` 명령을 쳐야 하는지까지** 알려주고, symlink라면 가리키는
실제 파일에 적용하라는 안내를 함께 낸다.

---

## 8. 안전 수칙

1. **루프백 + SSH 터널을 먼저 고려하라.** 콘솔을 네트워크에 올리지 않는 것이 가장 확실한
   접근 제한이다.
2. **`--allow-remote`는 프록시와 짝으로만.** TLS 없이 열면 토큰이 평문으로 흐른다.
3. **파괴적 작업은 감사 로그가 살아 있어야 실행된다**(§5). state 디렉터리의 여유 공간이
   막히면 복구도 막힌다 — 모니터링 대상이다.
4. **토큰 교체는 재기동이 필요하다.** 토큰은 기동 시 한 번 읽는다. 교체하면 기존 세션도
   함께 무효화하려면 재기동하라.
5. **콘솔에는 사용자 개념이 없다.** 토큰을 가진 사람은 전부 같은 권한이고, 감사 로그의
   `actor`도 그들을 구분하지 못한다. 여러 사람이 쓴다면 프록시에서 접근 제한과 접근
   로그를 함께 걸어라.
6. **자식 프로세스는 콘솔보다 오래 살 수 있다.** 서버를 재기동해도 진행 중이던 백업은
   계속 돌고, 다음 기동에서 콘솔이 그 잡에 다시 붙는다. `systemctl restart`가 백업을
   중단시키지 않는다는 뜻이고, 반대로 **중단하려면 콘솔의 취소 버튼을 쓰라는 뜻**이다.
