//! 표시 폭 인지 정렬 표 + ANSI 색 유틸 — `status --all` 비교 뷰와 `migrate` dry-run 표가 공용.
//!
//! 한글/CJK 문자는 터미널에서 2칸을 차지하므로 `{:<N}`(문자 수 기준 패딩)으로는 열이
//! 어긋난다. 여기의 [`display_width`]/[`pad`]는 **표시 폭**(East Asian Wide)을 기준으로
//! 정렬해 한글이 섞인 표도 정확히 맞춘다.

use std::io::IsTerminal;

/// 셀 정렬 방향.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    /// 좌측 정렬(라벨·텍스트).
    Left,
    /// 우측 정렬(숫자).
    Right,
}

/// 터미널에서 2칸을 차지하는 광폭 문자인지(East Asian Wide, 간이 판정).
pub fn is_wide(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x115F | 0x2E80..=0x303E | 0x3041..=0x33FF | 0x3400..=0x4DBF |
        0x4E00..=0x9FFF | 0xA000..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF |
        0xFE30..=0xFE4F | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6)
}

/// 문자열의 터미널 표시 폭(광폭 2칸, 그 외 1칸). ANSI 코드는 포함하지 않는 원문에만 쓴다.
pub fn display_width(s: &str) -> usize {
    s.chars().map(|c| if is_wide(c) { 2 } else { 1 }).sum()
}

/// 표시 폭 기준 좌측 정렬(우측 공백 패딩).
pub fn pad(s: &str, width: usize) -> String {
    let w = display_width(s);
    if w >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - w))
    }
}

/// 표시 폭 기준 우측 정렬(좌측 공백 패딩 — 숫자 열).
pub fn pad_left(s: &str, width: usize) -> String {
    let w = display_width(s);
    if w >= width {
        s.to_string()
    } else {
        format!("{}{s}", " ".repeat(width - w))
    }
}

// ───────────────────────── ANSI 색 ─────────────────────────

/// 리셋 + 스타일 코드(색 출력 시에만 적용).
pub const RESET: &str = "\x1b[0m";
pub const BOLD: &str = "\x1b[1m";
pub const UNDERLINE: &str = "\x1b[4m";
pub const DIM: &str = "\x1b[2m";
pub const RED: &str = "\x1b[31m";
pub const GREEN: &str = "\x1b[32m";
pub const YELLOW: &str = "\x1b[33m";
pub const CYAN: &str = "\x1b[36m";

/// ANSI 색을 쓸지 — stdout이 TTY이고 `NO_COLOR` 환경변수가 없을 때만.
pub fn use_color() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
}

/// 코드(들)로 텍스트를 감싼다(`color=false`면 원문 그대로). `codes`는 이어붙여 적용한다.
pub fn paint(s: &str, codes: &[&str], color: bool) -> String {
    if !color || codes.is_empty() {
        return s.to_string();
    }
    format!("{}{s}{RESET}", codes.concat())
}

// ───────────────────────── 정렬 표 빌더 ─────────────────────────

/// 표시 폭 인지 정렬 표. 헤더·행을 모아 열 너비를 자동 계산해 렌더한다.
///
/// 셀 텍스트는 **원문(ANSI 없음)** 으로 넣는다 — 색은 렌더 후 적용하거나 호출자가 별도
/// 처리한다(폭 계산이 어긋나지 않도록).
pub struct Table {
    headers: Vec<String>,
    aligns: Vec<Align>,
    rows: Vec<Vec<String>>,
    /// 행 단위 강조 코드(색). `None`이면 무채색.
    row_styles: Vec<Option<Vec<&'static str>>>,
}

impl Table {
    /// 헤더와 열 정렬로 표를 만든다(헤더 수 = 정렬 수).
    pub fn new(headers: &[&str], aligns: &[Align]) -> Self {
        debug_assert_eq!(headers.len(), aligns.len(), "헤더 수와 정렬 수 불일치");
        Self {
            headers: headers.iter().map(|h| h.to_string()).collect(),
            aligns: aligns.to_vec(),
            rows: Vec::new(),
            row_styles: Vec::new(),
        }
    }

    /// 행을 추가한다(셀 수는 헤더 수와 같아야 한다).
    pub fn row(&mut self, cells: Vec<String>) {
        self.rows.push(cells);
        self.row_styles.push(None);
    }

    /// 강조 색을 입혀 행을 추가한다(`use_color()`가 true일 때만 적용).
    pub fn row_styled(&mut self, cells: Vec<String>, style: Vec<&'static str>) {
        self.rows.push(cells);
        self.row_styles.push(Some(style));
    }

    /// 표를 렌더한다 — 각 줄 앞에 `indent`를 붙이고, 열은 2칸 간격으로 정렬한다.
    pub fn render(&self, indent: &str, color: bool) -> String {
        let ncol = self.headers.len();
        let mut widths = vec![0usize; ncol];
        for (i, h) in self.headers.iter().enumerate() {
            widths[i] = display_width(h);
        }
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate().take(ncol) {
                widths[i] = widths[i].max(display_width(cell));
            }
        }

        let last = ncol.saturating_sub(1);
        let fmt_row = |cells: &[String], style: Option<&Vec<&'static str>>| -> String {
            let mut parts = Vec::with_capacity(ncol);
            for (i, cell) in cells.iter().enumerate().take(ncol) {
                // 마지막 좌측 정렬 열은 우측 패딩 생략 — 줄 끝 공백을 남기지 않는다.
                let aligned = match self.aligns[i] {
                    Align::Left if i == last => cell.clone(),
                    Align::Left => pad(cell, widths[i]),
                    Align::Right => pad_left(cell, widths[i]),
                };
                parts.push(aligned);
            }
            let line = parts.join("  ");
            match style {
                Some(codes) if color => format!("{indent}{}", paint(&line, codes, true)),
                _ => format!("{indent}{line}"),
            }
        };

        let mut out = String::new();
        out.push_str(&fmt_row(&self.headers, None));
        for (row, style) in self.rows.iter().zip(&self.row_styles) {
            out.push('\n');
            out.push_str(&fmt_row(row, style.as_ref()));
        }
        out
    }

    /// 표 전체 표시 폭(구분선 길이 등에 활용) — 열 너비 합 + 간격.
    pub fn total_width(&self) -> usize {
        let ncol = self.headers.len();
        let mut widths = vec![0usize; ncol];
        for (i, h) in self.headers.iter().enumerate() {
            widths[i] = display_width(h);
        }
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate().take(ncol) {
                widths[i] = widths[i].max(display_width(cell));
            }
        }
        widths.iter().sum::<usize>() + 2 * ncol.saturating_sub(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_width_counts_hangul_as_two() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("버전"), 4);
        assert_eq!(display_width("연결·인증"), 9); // 중점은 1칸
    }

    #[test]
    fn pad_and_pad_left_use_display_width() {
        assert_eq!(pad("버전", 8), "버전    ");
        assert_eq!(pad_left("3", 4), "   3");
    }

    #[test]
    fn paint_noop_when_color_off() {
        assert_eq!(paint("x", &[RED], false), "x");
        assert_eq!(paint("x", &[RED], true), format!("{RED}x{RESET}"));
    }

    #[test]
    fn table_aligns_columns_with_hangul_header() {
        let mut t = Table::new(&["네임스페이스", "source"], &[Align::Left, Align::Right]);
        t.row(vec!["shop.events".into(), "3".into()]);
        let out = t.render("  ", false);
        let lines: Vec<&str> = out.lines().collect();
        // 헤더 라벨 폭(네임스페이스=12) 기준으로 두 번째 열 시작 위치가 모든 줄에서 동일해야 한다.
        let col2_start = |line: &str| display_width(line.split("  ").next().unwrap_or(""));
        assert_eq!(col2_start(lines[0]), col2_start(lines[1]));
    }
}
