---
name: x-backup web console
description: 기록물 보존소의 목록처럼 읽히는 DB 백업 운영 콘솔
colors:
  board: "#33424f"
  board-deep: "#27333e"
  board-ink: "#e6ebef"
  board-muted: "#a9b7c3"
  field: "#e8ecef"
  sheet: "#fcfcfb"
  sheet-2: "#f2f4f5"
  rule: "#d3d9de"
  rule-strong: "#aab4bd"
  ink: "#1b232b"
  muted: "#56626d"
  link: "#1f4e79"
  focus: "#2463b0"
  manila: "#e6d5a9"
  manila-ink: "#3a2f12"
  ok: "#1e6b40"
  warn: "#8a5800"
  fail: "#b3261e"
  error: "#5b3d99"
typography:
  headline:
    fontFamily: "-apple-system, BlinkMacSystemFont, \"Segoe UI\", \"Apple SD Gothic Neo\", \"Noto Sans KR\", \"Malgun Gothic\", system-ui, sans-serif"
    fontSize: "24px"
    fontWeight: 700
    lineHeight: 1.2
    letterSpacing: "-0.02em"
  title:
    fontFamily: "-apple-system, BlinkMacSystemFont, \"Segoe UI\", \"Apple SD Gothic Neo\", \"Noto Sans KR\", \"Malgun Gothic\", system-ui, sans-serif"
    fontSize: "14.5px"
    fontWeight: 700
    letterSpacing: "-0.005em"
  body:
    fontFamily: "-apple-system, BlinkMacSystemFont, \"Segoe UI\", \"Apple SD Gothic Neo\", \"Noto Sans KR\", \"Malgun Gothic\", system-ui, sans-serif"
    fontSize: "14px"
    fontWeight: 400
    lineHeight: 1.55
  body-small:
    fontFamily: "-apple-system, BlinkMacSystemFont, \"Segoe UI\", \"Apple SD Gothic Neo\", \"Noto Sans KR\", \"Malgun Gothic\", system-ui, sans-serif"
    fontSize: "12.5px"
    fontWeight: 400
    lineHeight: 1.55
  label:
    fontFamily: "-apple-system, BlinkMacSystemFont, \"Segoe UI\", \"Apple SD Gothic Neo\", \"Noto Sans KR\", \"Malgun Gothic\", system-ui, sans-serif"
    fontSize: "10.5px"
    fontWeight: 700
    letterSpacing: "0.08em"
  stamp:
    fontFamily: "-apple-system, BlinkMacSystemFont, \"Segoe UI\", \"Apple SD Gothic Neo\", \"Noto Sans KR\", \"Malgun Gothic\", system-ui, sans-serif"
    fontSize: "11px"
    fontWeight: 750
    lineHeight: 1.2
    letterSpacing: "0.08em"
  mono:
    fontFamily: "ui-monospace, \"SF Mono\", SFMono-Regular, Menlo, Consolas, \"Liberation Mono\", monospace"
    fontSize: "13px"
    fontWeight: 400
rounded:
  radius: "4px"
  radius-sm: "3px"
spacing:
  s-1: "0.25rem"
  s-2: "0.5rem"
  s-3: "0.75rem"
  s-4: "1rem"
  s-5: "1.25rem"
  s-6: "1.75rem"
  s-8: "2.5rem"
components:
  button-primary:
    backgroundColor: "{colors.board}"
    textColor: "#ffffff"
    rounded: "{rounded.radius}"
    padding: "0.45rem 1rem"
    height: "2.15rem"
  button-primary-hover:
    backgroundColor: "{colors.board-deep}"
  button-cancel:
    backgroundColor: "{colors.sheet}"
    textColor: "{colors.fail}"
    rounded: "{rounded.radius}"
    padding: "0.45rem 1rem"
  input:
    backgroundColor: "#ffffff"
    textColor: "{colors.ink}"
    typography: "{typography.mono}"
    rounded: "{rounded.radius}"
    padding: "0.4rem 0.6rem"
    height: "2.15rem"
  badge-ok:
    textColor: "{colors.ok}"
    typography: "{typography.stamp}"
    rounded: "{rounded.radius-sm}"
    padding: "0.26rem 0.45rem 0.22rem"
    width: "4.6rem"
  tag:
    backgroundColor: "{colors.sheet-2}"
    textColor: "{colors.muted}"
    rounded: "{rounded.radius-sm}"
    padding: "0 0.4rem"
  nav-item:
    backgroundColor: "{colors.board}"
    textColor: "{colors.board-ink}"
    padding: "0.32rem 1.25rem"
  nav-item-current:
    backgroundColor: "{colors.manila}"
    textColor: "{colors.manila-ink}"
    rounded: "{rounded.radius-sm}"
  panel:
    backgroundColor: "{colors.sheet}"
    textColor: "{colors.ink}"
    rounded: "{rounded.radius}"
  panel-head:
    backgroundColor: "{colors.sheet-2}"
    typography: "{typography.title}"
    padding: "0.6rem 1rem"
  card:
    backgroundColor: "{colors.sheet}"
    rounded: "{rounded.radius}"
    padding: "1rem 1.25rem"
  meta-strip:
    backgroundColor: "{colors.sheet}"
    rounded: "{rounded.radius}"
    padding: "0.75rem 1rem"
---

# Design System: x-backup web console

## Overview

**Creative North Star: "The Finding Aid"**

콘솔은 기록물 보존소의 목록(finding aid)이다. 소장 목록은 왼쪽 보존 상자의 등(레일)에 붙고, 기록은 괘선 친 흰 기록지에 적히며, 판정은 고무 도장으로 찍힌다. 서늘한 회색 판지 바탕 위에 기록지가 한 장씩 놓이고, 현재 화면은 마닐라 폴더 탭으로 튀어나온다.

밀도는 운영 대장 수준이다. 본문 14px, 표 13px, 촘촘한 괘선 행으로 여러 프로파일을 한 화면에 싣는다. 색은 거의 무채색이고, 상태 잉크는 도장·글자·괘선에만 묻는다. 문장은 시스템 산세리프, 식별자·경로·수치는 등폭으로 적는다.

SaaS 대시보드와 터미널 흉내는 이 세계가 아니다. 이전 "터미널 미학" 아이덴티티는 폐기했다.

**Key Characteristics:**
- 청회색 레일 + 회색 판지 바탕 + 흰 기록지의 3층 재질
- 상태는 이중 테두리 고무 도장으로 말하고, 도장에는 늘 영문 글자가 찍힌다
- 대장의 이중 괘선 아래에 화면 제목이 앉는다
- 장식 모션은 판정 도장이 찍히는 순간 하나뿐이다
- 웹폰트·외부 리소스 없이 시스템 서체만 쓴다

## Colors

무채색 판지와 기록지 위에 청회색 레일, 마닐라 탭, 도장 잉크 네 가지만 색을 가진다.

### Primary
- **Archive Box Slate** (`board`): 레일 바탕, 기본 버튼, 체크박스·진행 막대. 기록지와 대비가 커서 현재 위치가 레일에서 바로 읽힌다.
- **Deep Box Slate** (`board-deep`): 버튼 hover·테두리, 레일 라벨 카드의 아래 모서리.
- **Board Label Ink** (`board-ink`) / **Board Muted** (`board-muted`): 레일 위의 링크 글자와 묶음 라벨.

### Secondary
- **Manila Folder** (`manila`) / **Manila Ink** (`manila-ink`): 현재 화면 탭과 텍스트 선택에만 쓴다.

### Status Inks
- **Stamp Green** (`ok`), **Stamp Ochre** (`warn`), **Stamp Red** (`fail`), **Stamp Violet** (`error`): 도장 잉크. 도장 안쪽 바탕은 잉크 8%를 기록지에 섞은 번짐, 안쪽 괘선은 잉크 38%를 섞는다(`color-mix(in oklab, …)`, 폴백은 `sheet`와 `rule-strong`).

### Neutral
- **Cool Board Field** (`field`): 페이지 바탕.
- **Record Sheet** (`sheet`) / **Sheet Shade** (`sheet-2`): 패널·카드·표의 흰 면과, 패널 제목줄·표 머리·로그·꼬리표의 옅은 면.
- **Ledger Rule** (`rule`) / **Strong Rule** (`rule-strong`): 행 괘선·테두리와, 입력칸 테두리·이중 괘선.
- **Record Ink** (`ink`) / **Faded Ink** (`muted`): 본문과 보조 문장.
- **Archive Blue** (`link`) / **Focus Blue** (`focus`): 링크와 키보드 초점 윤곽.

### Named Rules
**The One Mapping Rule.** 상태를 색으로 바꾸는 곳은 `app.css`의 `[data-level]` 규칙 한 곳이다. 마크업과 스크립트는 `data-level`(`ok`/`warn`/`fail`/`error`)만 싣는다.

**The Ink-Not-Fill Rule.** 상태 잉크는 도장, 글자(문제 있는 경과 시간·취소 버튼·입력 경고 `.field__warn`·Peek 생략 표시 `.peek-trunc`), 알림의 테두리 괘선에만 묻는다. config 문법 도장은 성공·실패가 아니어서 잉크 대신 `link` 색을 쓴다. 행, 배너, 패널의 면을 상태색으로 채우지 않는다. 면에 허용되는 것은 잉크 8% 번짐뿐이다.

**The Heatmap Exception.** Dashboard의 백업 이력 격자만 칸 면을 잉크로 채운다. 히트맵은 면이 채워져야 읽히기 때문이다. 대신 레벨마다 모양이 다르다: ok 옅은 면, warn 대각선 반쪽, fail 진한 면 + 빗금, error 진한 면 + 점, 기록 없음 점선 빈 칸. 기록 없음은 실패와 같은 모양이 될 수 없다. 이 예외를 다른 컴포넌트로 넓히지 않는다.

**The Two Reds Rule.** `fail`(점검 결과가 안 된다)과 `error`(점검 자체를 못 했다)는 다른 색상이다. 운영자가 할 일이 반대이기 때문이다.

## Typography

**Body Font:** 시스템 산세리프 스택(`-apple-system`, `Apple SD Gothic Neo`, `Noto Sans KR` … `sans-serif`)
**Label/Mono Font:** 시스템 등폭 스택(`ui-monospace`, `SF Mono` … `monospace`)

**Character:** 타자기로 친 목록처럼 담백하다. 서체 개성 대신 굵기, 대문자 자간, 등폭의 대비로 위계를 만든다. 웹폰트를 두지 않는다는 제약에서 나온 선택이다.

### Hierarchy
- **Headline** (700, 24px, 1.2, -0.02em): 화면 제목. 48rem 이하에서 21px.
- **Title** (700, 14.5px): 패널 제목. 판정 헤드라인과 본문 소제목은 같은 층에서 15px/650을 쓴다.
- **Body** (400, 14px, 1.55): 문장. 부제는 13.5px, 최대 76ch.
- **Body Small** (400, 12.5px): 힌트, 카운트.
- **Section Caption** (650, 13.5px, `ink`): 기록지 안의 절 제목(표 캡션).
- **Label** (650–700, 10.5px, 0.08–0.1em, 대문자): 레일 묶음 라벨, 표 머리, 요약 띠의 키. 이 세 역할에만 쓴다.
- **Stamp** (750, 11px, 0.08em, 대문자): 도장 글자.
- **Mono** (400, 13px): 식별자, 경로, 수치, 텍스트 입력값, 요약 띠 값, 버전. 선택지(`select`)는 설명 문장이 섞이므로 산세리프다.

한국어는 어절 단위로 줄을 바꾼다(`word-break: keep-all`). 공백 없는 긴 토큰만 어디서든 끊는다.

### Named Rules
**The Mono-For-Data Rule.** 등폭은 기계가 만든 값(식별자·경로·수치·입력)에만 쓴다. 문장과 제목은 산세리프다.

**The Fixed Labels Rule.** 라벨과 기술용어(도장, 표 머리, 레일, 제품명 `x-backup`)는 영문 고정이다. 설명 문장만 ko/en을 고른다.

## Layout

두 칸 격자다. 왼쪽 13.5rem 레일은 sticky로 화면 높이를 채우고, 오른쪽 작업면은 `minmax(0, 1fr)`에 최대 82rem, 안쪽 여백은 1.75rem/2.5rem이다. 수직 리듬은 `s-5`(1.25rem) 간격으로 기록지를 쌓는다.

첫 화면 순서는 이중 괘선 아래 화면 제목, 판정 배너, 요약 띠, 대장형 표다. 요약 띠는 라벨 위·값 아래 쌍이 가로로 늘어서고, 패널 안에서는 세로 키-값 목록이 된다.

- 60rem 이하: 레일이 상단 띠로 접힌다. 묶음 라벨과 버전은 숨고, 화면 목록은 줄바꿈해 전부 보인다(현재 화면 탭이 띠 밖에 숨지 않게). 띠는 고정하지 않고 흘려 둔다. 묶음 사이는 세로 괘선으로 가른다.
- 48rem 이하: 입력 행이 한 칸으로 접히고, 요약 띠는 세로 키-값 목록이 되며, 감싸개 없는 표는 자신이 가로 스크롤을 맡는다.
- 표는 항상 자기 안에서만 가로로 스크롤한다. 페이지 자체는 옆으로 넘치지 않는다.

**The Stamp Column Rule.** 대장의 첫 열은 상태 도장 열이다. 모든 표에서 최소 폭으로 서서, 표를 넘나들어도 다음 열이 같은 자리에서 시작한다.

## Elevation & Depth

거의 평평하다. 기록지(패널·카드·판정 배너)는 판지 위에 얇게 뜬 종이 한 장 높이의 그림자 하나(`--shadow-sheet`)만 받는다. 레일 라벨 카드는 상자 등에 붙은 종이라서 조금 더 짙은 그림자를 가진다. 현재 화면 탭은 그림자 없이 아래쪽 안쪽 괘선 하나만 가진다. 제목 아래 이중 괘선과 도장의 안쪽 괘선은 테두리이고 높이가 아니다. 
### Shadow Vocabulary
- **Sheet** (`box-shadow: 0 1px 2px rgba(27, 35, 43, 0.06), 0 2px 6px rgba(27, 35, 43, 0.04)`): 패널, 카드, 판정 배너.
- **Rail Label Card** (`box-shadow: 0 1px 0 #27333e, 0 2px 5px rgba(0, 0, 0, 0.25)`): 레일 맨 위 타자 라벨 카드.
- **Folder Tab** (`box-shadow: inset 0 -1px 0 rgba(58, 47, 18, 0.22)`): 현재 화면 마닐라 탭의 아래 괘선. 높이가 아니라 테두리다.
- **Button** (`box-shadow: 0 1px 2px rgba(27, 35, 43, 0.18)`): 버튼. 누르면 사라진다.

**The One Sheet Rule.** 기록지는 한 장 높이만 뜬다. 높이를 쌓아 위계를 만들지 않는다.

## Shapes

모서리는 거의 직각이다. 기록지·입력·버튼은 4px, 도장·꼬리표는 3px, 레일 라벨 카드는 2px이다. 현재 탭은 앞쪽 모서리를 비스듬히 깎고(`clip-path`) 작업면 쪽은 레일 끝에 그대로 붙여 폴더 탭 모양이 된다. 둥근 알약 형태는 진행 막대 하나뿐이다.

괘선이 형태 언어의 중심이다. 행 괘선 1px, 화면 제목과 표 머리 아래의 3px 이중 괘선(대장의 표식), 중첩 문서의 들여 쓴 왼쪽 괘선이 있다.

## Components

### Buttons
- **Shape:** 4px 모서리, 높이 2.15rem.
- **Primary:** 레일과 같은 청회색 바탕, 흰 글자 13.5px/650, 얕은 그림자.
- **Hover / Active:** hover에서 짙은 청회색, 누르면 1px 내려앉고 그림자가 사라진다(120ms). 비활성은 불투명도 0.5.
- **Cancel:** 실행 중인 잡 취소는 되돌릴 수 없어서 흰 기록지 바탕에 `fail` 글자·테두리로 모양을 가른다.

### Stamps (Badge)
- **Style:** 1.5px 잉크 테두리 안에 기록지색 1.5px 틈과 옅은 잉크 괘선이 한 번 더 도는 이중 테두리. 바탕은 잉크 번짐, 글자는 잉크, 최소 폭 4.6rem, 가운데 정렬.
- **Variants:** `.badge`, 판정 배너 도장, `data-level`을 가진 꼬리표, 모니터 상태가 같은 모양을 공유한다. config 문법 배너는 성공·실패가 아니어서 `link` 색 도장을 쓰고 모션이 없다.
- **Placement:** 패널 제목줄에서는 오른쪽 끝에 선다.

### Tags
- **Style:** `sheet-2` 바탕, `rule-strong` 1px 테두리, 3px 모서리, 등폭 11.5px `muted` 글자. 엔진 같은 분류 꼬리표이고 상태가 아니어서 잉크를 쓰지 않는다.

### Cards / Containers
- **Corner Style:** 4px.
- **Background:** `sheet`, 패널 제목줄은 `sheet-2`에 아래 괘선.
- **Shadow Strategy:** `--shadow-sheet` 한 단계.
- **Border:** `rule` 1px.
- **Internal Padding:** 패널 본문 1rem, 카드 1rem × 1.25rem. 패널 안 표는 본문 여백을 되돌려 기록지 가장자리까지 닿는다.

### Verdict Banner
기록지 한 장에 도장 · 헤드라인 · (선택)근거의 3층을 담는다. 테두리는 `rule`이고, 상태색은 도장에만 있다. 도장은 나타날 때 1.14배·-3도에서 눌려 자리를 잡는다(260ms). 이것이 콘솔의 유일한 장식 모션이고, `prefers-reduced-motion`에서 꺼진다.

### Notice
판정과 별개인 안내·오류 설명이다. 바탕은 `sheet`, 제목은 `ink` 650이고, 테두리에만 잉크 38% 괘선을 남긴다.

### Ledger Table
머리는 `sheet-2` 위에 대문자 라벨, 아래 3px 이중 `rule-strong` 괘선. 행은 1px `rule` 괘선, hover에서 `sheet-2`. 숫자는 오른쪽 정렬 등폭 tabular 숫자.

### Backup History Heatmap
프로파일 × UTC 날짜 30칸 격자다(Dashboard 화면 제목 바로 아래, 판정 배너보다 위). 칸은 1.25rem 도장 칸이고 칸 사이 3px, 오늘 칸은 먹선 윤곽(`outline`)으로 가른다. 칸에 마우스를 올리면 날짜(첫 줄)와 레벨·백업 수·실패한 실행 수(둘째 줄)가 레일 색 툴팁으로 지연 없이 뜬다. 격자 양 끝 칸의 툴팁은 안쪽으로 붙는다. 같은 내용을 스크린리더용 문구로도 싣는다. 목록을 읽지 못한 프로파일(백업 저장소 없음 등)은 칸 대신 이유를 한 줄로 적는다 — 빈 칸으로 그리면 "기록 없음"으로 오해된다. 범례와 "실패는 이 콘솔에서 시작한 백업만 표시된다"는 고정 문구가 따라온다. 48rem 이하에서는 가로 스크롤이 오늘에서 시작하고 프로파일 이름 열이 왼쪽에 붙는다.

### Inputs / Fields
- **Style:** 흰 바탕, `rule-strong` 1px, 4px 모서리, 13px(텍스트 입력은 등폭, 선택지는 산세리프), 안쪽 1px 음영, 최대 폭 32rem.
- **Hover / Focus:** hover에서 테두리가 `muted`, 초점에서 테두리가 `focus`로 바뀌고 35% `focus` 윤곽 2px이 붙는다.
- **Disabled:** `sheet-2` 바탕, `muted` 글자.
- **Label:** 13px/600. 옵션 이름을 그대로 쓰는 라벨은 등폭이다.
- **Row:** 라벨 11rem · 컨트롤 칸의 격자, 힌트는 컨트롤 아래, 행 사이 `rule` 괘선. config 행은 키 · 출처 · 실효값 · 입력의 4칸이다. 조회용 GET 폼은 기록지 한 장 위에 가로로 늘어서고 라벨이 대문자 라벨 층으로 바뀐다.

### Navigation
- **Style:** 레일 맨 위에 흰 타자 라벨 카드(제품명 16px/750, 괘선 아래 등폭 버전). 그 아래 Overview · Operate · Records · Setup 네 묶음, 묶음마다 대문자 라벨.
- **States:** 링크는 `board-ink` 13.5px. hover는 8% 밝은 막. 현재 화면은 마닐라 탭(650 굵기, 앞쪽 모서리를 깎고 작업면 쪽으로 붙음)이다. 색만이 아니라 모양과 굵기로도 표시한다.
- **Mobile:** 60rem 이하에서 상단 띠로 접힌다(Layout 참고).

## Do's and Don'ts

### Do:
- **Do** 상태는 `data-level` 토큰으로만 싣고, 도장에는 늘 `OK`/`WARN`/`FAIL`/`ERROR` 글자를 함께 찍는다.
- **Do** 새 화면을 레일의 네 묶음 중 한 곳에만 등록하고, 화면 제목과 레일 라벨을 같은 상수로 맞춘다.
- **Do** 색·간격·모서리는 `:root` 변수(`--c-*`, `--s-*`, `--radius*`)를 거쳐 쓴다.
- **Do** 식별자·경로·수치는 등폭 tabular 숫자로 적는다.
- **Do** 키보드 초점 윤곽(2px `focus`)을 유지하고 `prefers-reduced-motion`을 따른다.

### Don't:
- **Don't** 웹폰트, CDN, 외부 이미지 같은 외부 리소스를 들이지 않는다.
- **Don't** 마크업에 `PreEscaped`, 인라인 `style=`, 색 리터럴을 두지 않는다.
- **Don't** 아이콘을 더하지 않는다. 현재 콘솔에는 아이콘이 없고, 상태는 글자 도장으로 말한다.
- **Don't** 행, 배너, 패널의 면을 상태색으로 채우지 않는다.
- **Don't** `fail`과 `error`를 같은 색으로 합치지 않는다.
- **Don't** 라벨과 기술용어를 번역하지 않는다.
- **Don't** 대문자 라벨 층을 레일 묶음·표 머리·요약 띠 키 밖으로 가져가지 않는다. 화면 제목이나 패널 제목 위에 얹는 머리글로 쓰지 않는다.
- **Don't** 판정 도장 외에 장식 모션을 더하지 않는다.
