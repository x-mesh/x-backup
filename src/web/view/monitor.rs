//! 라이브 모니터 화면 마크업.
//!
//! ## 서버가 그리는 것이 거의 없다
//! 다른 화면과 달리 이 화면의 첫 렌더에는 **데이터가 없다.** 프레임은 SSE로 오고, 첫
//! 프레임이 도착하기 전까지는 "기다리는 중"이 정직한 상태다. 서버가 여기서 `status`를 한 번
//! 더 돌려 초기값을 채우면 자식이 둘이 되고, 그건 이 화면의 존재 이유(자식 하나)를 깬다.
//!
//! ## 표는 스크립트가 채운다 — 레벨은 그래도 서버가 정한다
//! [`crate::web::view::backup`] 헤더가 정한 규약을 따른다: 스크립트는 **판정하지 않고
//! 반영만** 한다. 다만 이 화면의 프레임에는 신호등이 없다(문서 수·크기·델타뿐) — 그래서
//! 스크립트가 정하는 것도 없다. 델타의 부호를 `data-delta` 토큰(`up`/`down`/`same`/`none`)
//! 으로만 표시하고, 그 토큰이 무슨 색인지는 `app.css`가 정한다.
//!
//! ## 왜 델타에 `none`이 따로 있는가
//! 첫 틱에는 비교할 이전 값이 없어 CLI가 `null`을 보낸다
//! (`cli::handlers::status::build_watch_json`). 그걸 `0`(변화 없음)으로 그리면 **"아무 일도
//! 없었다"와 "아직 모른다"가 같은 화면**이 된다 — 모니터에서 그 둘은 정반대의 뜻이다.

use maud::{html, Markup, PreEscaped};

use crate::i18n::Lang;
use crate::web::routes::monitor as route;
use crate::web::view::components;

/// 프레임을 받아 표를 갱신하는 스크립트 — **컴파일 타임 상수**다.
///
/// 사용자 입력을 한 글자도 보간하지 않으므로 `PreEscaped`가 안전하다
/// ([`components`] 헤더의 규약). 값은 전부 `textContent`로 넣는다 — `innerHTML`을 쓰면
/// 자식이 만든 문자열(프로파일명·네임스페이스)이 마크업으로 해석될 수 있다.
const MONITOR_SCRIPT: &str = r##"
(function () {
  var root = document.getElementById("monitor");
  if (!root || !window.EventSource) { return; }
  var body = document.getElementById("monitor-rows");
  var status = document.getElementById("monitor-status");
  var tickTag = document.getElementById("monitor-tick");

  function sign(value) {
    if (value === null || value === undefined) { return "none"; }
    if (value > 0) { return "up"; }
    if (value < 0) { return "down"; }
    return "same";
  }

  function text(value) {
    if (value === null || value === undefined) { return "—"; }
    return (value > 0 ? "+" : "") + value;
  }

  function cell(row, value, delta) {
    var td = document.createElement("td");
    var main = document.createElement("span");
    main.textContent = value;
    td.appendChild(main);
    var d = document.createElement("span");
    d.className = "monitor-delta";
    d.setAttribute("data-delta", sign(delta));
    d.textContent = " " + text(delta);
    td.appendChild(d);
    row.appendChild(td);
    return td;
  }

  var source = new EventSource(root.dataset.eventsHref);

  source.addEventListener("frame", function (e) {
    var frame;
    try { frame = JSON.parse(e.data); } catch (err) { return; }
    if (!frame || !frame.profiles) { return; }

    if (status) { status.setAttribute("data-level", "ok"); status.textContent = root.dataset.labelLive; }
    if (tickTag) { tickTag.textContent = "#" + frame.tick; }

    body.textContent = "";
    frame.profiles.forEach(function (p) {
      var row = document.createElement("tr");
      var name = document.createElement("td");
      name.className = "mono";
      name.textContent = p.profile;
      row.appendChild(name);

      var conn = document.createElement("td");
      conn.setAttribute("data-level", p.connected ? "ok" : "fail");
      conn.textContent = p.connected ? root.dataset.labelUp : root.dataset.labelDown;
      row.appendChild(conn);

      cell(row, p.total_docs, p.delta_docs);
      cell(row, p.data_size_bytes, p.delta_bytes);
      body.appendChild(row);
    });
  });

  source.addEventListener("error", function () {
    if (status) { status.setAttribute("data-level", "warn"); status.textContent = root.dataset.labelStalled; }
  });
})();
"##;

/// 모니터 화면 본문.
pub fn body(lang: Lang, interval_secs: u32) -> Markup {
    // 숫자를 사이에 끼우는 문장이라 앞·뒤 조각을 따로 고른다
    // (`view::dashboard`의 TTL 문장과 같은 방식 — 언어마다 숫자 위치가 다르다).
    let subtitle = format!(
        "{}{interval_secs}{}",
        lang.sel(
            "Live counts across every profile, refreshed every ",
            "전 프로파일의 실시간 수치입니다. 백그라운드 리더 하나가 ",
        ),
        lang.sel("s by a single background reader.", "초마다 갱신합니다.",),
    );
    html! {
        (components::page_head(route::MONITOR_TITLE, Some(&subtitle)))

        (components::notice(
            components::Level::Ok,
            lang.sel("One reader for everyone", "모두가 리더 하나를 공유합니다"),
            html! {
                p { (lang.sel(
                    "However many people have this screen open, the console runs exactly one live reader against your databases. When the last viewer leaves, it stops.",
                    "이 화면을 몇 명이 열어 두든 콘솔은 데이터베이스에 라이브 리더를 정확히 하나만 돌립니다. 마지막 뷰어가 떠나면 멈춥니다.",
                )) }
            },
        ))

        div id="monitor"
            data-events-href=(route::MONITOR_EVENTS_PATH)
            data-label-live=(lang.sel("live", "실시간"))
            data-label-stalled=(lang.sel("reconnecting", "재연결 중"))
            data-label-up=(lang.sel("connected", "연결됨"))
            data-label-down=(lang.sel("unreachable", "연결 실패")) {

            p class="actions" {
                span id="monitor-status" data-level="warn" { (lang.sel("waiting for the first frame", "첫 프레임 대기 중")) }
                " "
                span id="monitor-tick" class="mono" {}
            }

            table class="dtable" {
                thead {
                    tr {
                        th { "profile" }
                        th { (lang.sel("connection", "연결")) }
                        th { (lang.sel("documents", "문서 수")) }
                        th { (lang.sel("data size (bytes)", "데이터 크기(바이트)")) }
                    }
                }
                tbody id="monitor-rows" {}
            }

            p class="field__hint" { (lang.sel(
                "The number after each value is the change since the previous refresh. A dash means there is no previous refresh to compare with yet — not that nothing changed.",
                "각 값 뒤의 숫자는 직전 갱신 대비 변화량입니다. 대시(—)는 비교할 이전 갱신이 아직 없다는 뜻이지, 변화가 없었다는 뜻이 아닙니다.",
            )) }
        }

        script { (PreEscaped(MONITOR_SCRIPT)) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_screen_carries_the_sse_href_and_an_empty_table() {
        let out = body(Lang::En, 2).into_string();
        assert!(
            out.contains(&format!(
                r#"data-events-href="{}""#,
                route::MONITOR_EVENTS_PATH
            )),
            "SSE 경로가 없다"
        );
        assert!(
            out.contains(r#"<tbody id="monitor-rows"></tbody>"#),
            "빈 표가 없다"
        );
        assert!(out.contains("2s"), "갱신 주기가 화면에 없다");
    }

    /// **첫 렌더에 데이터가 없다** — 서버가 초기값을 채우려고 자식을 하나 더 띄우지 않는다.
    #[test]
    fn the_first_render_contains_no_measurements() {
        let out = body(Lang::En, 2).into_string();
        assert!(
            out.contains("waiting for the first frame"),
            "대기 상태를 말하지 않는다"
        );
    }

    /// 자식 하나를 공유한다는 사실을 화면이 직접 말한다 — 운영자가 "여러 명이 열면
    /// DB가 더 바쁜가?"를 묻지 않아도 되게.
    #[test]
    fn the_screen_states_that_one_reader_is_shared() {
        let ko = body(Lang::Ko, 2).into_string();
        assert!(ko.contains("정확히 하나만"), "공유 사실을 말하지 않는다");
        assert!(
            ko.contains("마지막 뷰어가 떠나면 멈춥니다"),
            "종료 조건을 말하지 않는다"
        );
    }

    /// 스크립트는 값을 `textContent`로만 넣는다 — 자식이 만든 문자열이 마크업으로
    /// 해석될 자리를 만들지 않는다.
    #[test]
    fn the_script_never_uses_inner_html() {
        assert!(
            !MONITOR_SCRIPT.contains("innerHTML"),
            "innerHTML을 쓰면 프로파일명·네임스페이스가 마크업이 된다"
        );
        assert!(MONITOR_SCRIPT.contains("textContent"));
    }

    /// **델타 `null`은 `0`과 다르게 그린다** — "아직 모른다"와 "변화 없음"은 정반대다.
    #[test]
    fn a_null_delta_is_not_drawn_as_zero() {
        assert!(
            MONITOR_SCRIPT.contains(r#"return "none";"#),
            "null 델타 전용 토큰이 없다"
        );
        assert!(
            MONITOR_SCRIPT.contains(r#"return "—";"#),
            "null 델타를 대시로 그리지 않는다"
        );
    }

    /// 마크업에 색·인라인 스타일 리터럴이 없다(모양은 CSS 몫).
    #[test]
    fn markup_carries_no_presentation() {
        let out = body(Lang::Ko, 2).into_string();
        let without_script = out.replace(MONITOR_SCRIPT, "");
        assert!(!without_script.contains("style="), "인라인 스타일이 있다");
        assert!(
            !without_script.contains('#'),
            "색 리터럴로 보이는 값이 있다"
        );
    }
}
