//! 병리적으로 깊은 JSON을 **파싱 전에** 걸러 운영자에게 정확한 진단을 주는 관문.
//!
//! ## 먼저: 이 모듈은 크래시 방어가 **아니다**
//! 이 코드는 원래 "깊은 JSON을 `serde_json`에 넘기면 스택 오버플로로 프로세스가 하드
//! abort한다"는 전제로 쓰였다. **그 전제는 사실이 아니다.** `serde_json`은 기본으로
//! 재귀 깊이 상한(128)을 켜고 들어오며, 그 이상 중첩된 입력은 스택을 건드리기 전에
//! 평범한 `Err`로 접는다:
//!
//! ```text
//! depth    127: OK
//! depth    128: ERR = recursion limit exceeded at line 1 column 128
//! depth 200000: ERR = recursion limit exceeded at line 1 column 128
//! ```
//!
//! debug·release 양쪽에서 확인했다. 상한을 끄려면 `unbounded_depth` feature와
//! `Deserializer::disable_recursion_limit`이 필요한데 이 크레이트는 둘 다 쓰지 않는다.
//! 따라서 **깊은 입력으로 이 서버를 죽일 수 있는 경로는 없다** — 그 방어는 `serde_json`이
//! 이미 하고 있다.
//!
//! ## 그러면 이 모듈은 무엇을 하는가 — 진단의 질
//! `serde_json`의 거부는 안전하지만 운영자에게 쓸모없는 문장을 남긴다:
//! `recursion limit exceeded at line 1 column 128`. 여기서 "column 128"은 실제 열 번호가
//! 아니라 재귀 카운터라 오해를 부르고, **입력이 실제로 얼마나 깊었는지**를 말해주지
//! 않는다. 그런데 운영자에게는 그 숫자가 곧 원인 분류다:
//!
//! - 130겹 → 스키마가 한 겹 깊어졌나? 콘솔과 CLI 버전이 어긋났나?
//! - 50,000겹 → 데이터 자체가 병리적이다. 누가 심었거나 생성기가 망가졌다.
//!
//! 그래서 이 관문은 재귀 없이 O(n) 단일 패스로 실제 깊이를 재고([`max_depth`]), 화면이
//! `TooDeep { found, max }`로 그 숫자를 그대로 보여줄 수 있게 한다. "JSON이 깨졌다"
//! (`Malformed`)와 "데이터가 병리적이다"(`TooDeep`)를 다른 오류로 가르는 것도 같은
//! 목적이다 — 운영자가 의심할 곳이 다르다.
//!
//! ## 상한을 `serde_json`의 실효 한계에 맞춘다
//! [`MAX_JSON_DEPTH`]는 127이다 — `serde_json`이 실제로 받아주는 최대 깊이. 그래서 이
//! 관문이 거부하는 입력 집합과 `serde_json`이 거부하는 집합이 **정확히 같다**. 이 정렬이
//! 중요한 이유:
//!
//! - 우리 상한이 더 높으면 그 사이 구간은 관문을 통과했다가 `serde_json`에게 거부되어
//!   결국 나쁜 메시지를 보게 된다(관문이 하는 일이 없어진다).
//! - 우리 상한이 더 낮으면 `serde_json`이 멀쩡히 파싱할 문서를 우리가 막는다.
//!
//! MongoDB는 BSON 중첩 깊이 100을 서버가 강제하므로 정상 mongo 문서는 이 상한에 걸리지
//! 않는다. PostgreSQL·MySQL의 JSON 컬럼에는 그런 상한이 없어 이론상 더 깊은 값이 올 수
//! 있지만, 그 경우도 결과는 크래시가 아니라 "이 화면을 못 그린다"이다.
//!
//! ## 적용 지점
//! 자식 프로세스 stdout을 파싱하는 모든 라우트([`crate::web::routes::doctor`]·
//! `dashboard`·`catalog`·`verify`·`peek`), 그 stdout을 줄 단위로 흘리는
//! [`crate::web::job::stream`], 그리고 디스크의 잡 인덱스를 읽는
//! [`crate::web::state::jobs`]. 전부 "메시지를 낫게 한다"는 같은 이유이며, 어느 지점도
//! 안전을 이 관문에 의존하지 않는다.

/// 파싱을 시도하기 전에 거부할 최대 중첩 깊이.
///
/// `serde_json`이 실제로 받아주는 최대 깊이와 같다(그 값 128은 "이 깊이부터 거부"라
/// 실효 허용치는 127). 근거는 모듈 헤더 "상한을 `serde_json`의 실효 한계에 맞춘다".
pub const MAX_JSON_DEPTH: usize = 127;

/// 상한이 mongo의 서버 강제 깊이(100)보다 위에 있어야 정상 문서를 막지 않는다.
///
/// 런타임 `assert!`가 아니라 **컴파일 타임** 단정이다. 두 값 다 상수라 런타임에 검사할
/// 이유가 없고, 컴파일 타임이면 상한을 잘못 낮춘 순간 **빌드가 깨진다** — 테스트를 돌리기도
/// 전에.
///
/// `#[cfg(test)]` 모듈 밖에 두는 것이 핵심이다. 테스트 모듈 안에 두면 `cargo check`(테스트
/// 없는 빌드)에서는 평가되지 않아, 릴리스 빌드가 잘못된 상한을 그대로 통과시킨다 —
/// 실제로 그렇게 만들었다가 상한을 50으로 낮춰도 `cargo check --lib`가 통과하는 것을 보고
/// 옮겼다.
const _: () = {
    const MONGO_BSON_MAX_DEPTH: usize = 100;
    assert!(
        MAX_JSON_DEPTH > MONGO_BSON_MAX_DEPTH,
        "상한이 mongo가 실제로 허용하는 깊이보다 낮으면 정상 문서가 거부된다"
    );
};

/// 입력이 [`MAX_JSON_DEPTH`]보다 깊어 **파싱을 시도하지 않았다**는 사실.
///
/// `found`를 함께 싣는 것이 이 타입의 존재 이유다 — `serde_json`의 오류 문자열에는 그
/// 숫자가 없고, 운영자가 원인을 좁히는 데 필요한 것이 바로 그 숫자다(모듈 헤더).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TooDeep {
    /// 실제로 측정된 최대 중첩 깊이.
    pub found: usize,
    /// 이 빌드가 허용하는 상한([`MAX_JSON_DEPTH`]).
    pub max: usize,
}

/// 텍스트의 `{`/`[` 최대 중첩 깊이를 **재귀 없이** O(n) 단일 패스로 잰다.
///
/// 재귀하지 않는 것은 이 함수가 "파서보다 먼저 도는 검사기"이기 때문이다 — 파서와 같은
/// 방식으로 재면 파서가 감당하지 못하는 입력은 검사기도 감당하지 못한다. (`serde_json`이
/// 이미 상한을 갖고 있어 실제 위험은 없지만, 검사기가 파서보다 약한 구조는 그 자체로
/// 잘못된 설계다.)
///
/// 문자열 리터럴 안의 `{`/`[`는 구조가 아니라 내용이므로 세지 않는다(역슬래시 이스케이프도
/// 추적한다). 그래서 `{"a":"[[[["}`의 깊이는 4가 아니라 1이다.
///
/// 문법 검증은 하지 않는다 — 닫는 괄호가 남아돌아도(`}}}`) 0으로 포화시킨다. 이 함수의
/// 책임은 깊이 하나뿐이고, 깨진 JSON은 그 뒤 파서가 정상적으로 오류로 접는다.
pub fn max_depth(text: &str) -> usize {
    let mut depth: usize = 0;
    let mut max_depth: usize = 0;
    let mut in_string = false;
    let mut escaped = false;
    for &b in text.as_bytes() {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth += 1;
                max_depth = max_depth.max(depth);
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    max_depth
}

/// `serde_json`이 받아줄 깊이인지 미리 판정한다.
///
/// `Err`면 호출자는 파싱을 건너뛰고 [`TooDeep`]을 자기 오류 타입으로 옮겨 담는다. 그냥
/// 파싱해도 안전하지만(모듈 헤더), 그러면 운영자가 깊이 숫자를 잃는다.
///
/// 반환을 `Result<(), TooDeep>`으로 둔 이유: 각 화면이 자기 오류 타입(`ReportError`·
/// `StatusError`·`ParseProblem` 등)으로 옮겨 담아 자기 문맥에 맞는 안내 문장을 쓰기
/// 때문이다. 공용 오류 하나를 강요하면 "무엇을 의심해야 하는지"가 화면마다 다르다는
/// 성질을 잃는다.
pub fn check_depth(text: &str) -> Result<(), TooDeep> {
    let found = max_depth(text);
    if found > MAX_JSON_DEPTH {
        return Err(TooDeep {
            found,
            max: MAX_JSON_DEPTH,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depth_counts_nesting_but_not_string_contents() {
        assert_eq!(max_depth(r#"{"a":1}"#), 1);
        assert_eq!(max_depth(r#"{"a":{"b":[1,2]}}"#), 3);
        assert_eq!(max_depth("[]"), 1);
        assert_eq!(max_depth("hello"), 0);
        assert_eq!(max_depth(""), 0);

        // 문자열 리터럴 안의 구조 문자는 내용이지 구조가 아니다.
        assert_eq!(max_depth(r#"{"a":"[[[["}"#), 1);
        // 이스케이프된 따옴표가 문자열을 닫지 않는다 — 닫힌 것으로 오인하면 그 뒤의
        // 내용을 구조로 세어 깊이를 부풀린다.
        assert_eq!(max_depth(r#"{"a":"x\"[[[[y"}"#), 1);
        // 이스케이프된 역슬래시는 그 자체로 소비된다 — 뒤따르는 따옴표는 진짜 종료다.
        assert_eq!(max_depth(r#"{"a":"x\\"}"#), 1);
    }

    #[test]
    fn unbalanced_closers_do_not_underflow() {
        // saturating_sub이 없으면 여기서 패닉한다(디버그 빌드).
        assert_eq!(max_depth("}}}]]]"), 0);
        assert_eq!(max_depth("[]}}}}[]"), 1);
    }

    /// 깊이 측정은 재귀하지 않으므로 수만 겹 입력도 상수 스택으로 끝난다.
    #[test]
    fn pathological_depth_is_measured_without_recursion() {
        let bomb = "[".repeat(200_000);
        assert_eq!(max_depth(&bomb), 200_000);
    }

    #[test]
    fn check_depth_admits_normal_documents_and_rejects_bombs() {
        assert!(check_depth(r#"{"schema":1,"store":{"backups":[]}}"#).is_ok());

        let at_limit = "[".repeat(MAX_JSON_DEPTH);
        assert!(check_depth(&at_limit).is_ok());

        let over_limit = "[".repeat(MAX_JSON_DEPTH + 1);
        let err = check_depth(&over_limit).expect_err("상한을 넘겼는데 통과했다");
        assert_eq!(
            err,
            TooDeep {
                found: MAX_JSON_DEPTH + 1,
                max: MAX_JSON_DEPTH,
            }
        );
    }

    /// **이 관문의 상한이 `serde_json`의 실효 한계와 정확히 같다**는 것을 고정한다.
    ///
    /// 이것이 깨지면 관문은 조용히 쓸모를 잃는다(우리가 더 관대하면 그 구간은 결국
    /// `serde_json`의 나쁜 메시지를 보고, 더 엄격하면 멀쩡한 문서를 우리가 막는다).
    /// `serde_json`을 올릴 때 이 테스트가 드리프트를 잡는다.
    #[test]
    fn limit_matches_what_serde_json_actually_accepts() {
        let accepted = "[".repeat(MAX_JSON_DEPTH) + &"]".repeat(MAX_JSON_DEPTH);
        assert!(
            serde_json::from_str::<serde_json::Value>(&accepted).is_ok(),
            "우리가 통과시킨 깊이를 serde_json이 거부한다 — 상한이 너무 높다"
        );

        let rejected = "[".repeat(MAX_JSON_DEPTH + 1) + &"]".repeat(MAX_JSON_DEPTH + 1);
        assert!(
            serde_json::from_str::<serde_json::Value>(&rejected).is_err(),
            "우리가 막는 깊이를 serde_json은 받아준다 — 상한이 너무 낮다"
        );
    }

    /// 깊은 입력이 **크래시가 아니라 평범한 오류**로 접힌다는 것 — 이 모듈이 크래시
    /// 방어가 아니라는 헤더 서술의 근거다. 이 테스트가 프로세스를 죽이면 전제가 바뀐 것이고,
    /// 그때는 이 모듈의 역할도 다시 정의해야 한다.
    #[test]
    fn deep_input_is_an_error_not_a_crash() {
        let bomb = "[".repeat(200_000);
        let err =
            serde_json::from_str::<serde_json::Value>(&bomb).expect_err("200,000겹이 파싱됐다");
        assert!(
            err.to_string().contains("recursion limit"),
            "예상과 다른 이유로 실패했다: {err}"
        );
    }
}
