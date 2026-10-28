//! 백업 진행 중 행·단위(테이블/컬렉션) 상태 — 엔진의 덤프 task가 갱신하고 CLI가 폴링해 보여 준다.
//!
//! 엔진 중립이다. "단위"는 MySQL·PostgreSQL에서는 테이블, MongoDB에서는 컬렉션이다. 필드와
//! `--json` 키(`tables_done`, `tables_total`)는 MySQL 표시에서 이미 쓰고 있어 그대로 둔다.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// 백업 진행 중 행·테이블 상태 — 덤프 task가 갱신하고 CLI가 폴링해 보여 준다.
#[derive(Debug, Default)]
pub struct RowProgress {
    rows: AtomicU64,
    tables_done: AtomicU64,
    tables_total: AtomicU64,
    current: Mutex<String>,
}

/// [`RowProgress`]의 한 시점 값.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowProgressSnapshot {
    pub rows: u64,
    pub tables_done: u64,
    pub tables_total: u64,
    /// 지금 덤프 중인 테이블(시작 전이면 빈 문자열).
    pub current: String,
}

impl RowProgress {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn snapshot(&self) -> RowProgressSnapshot {
        RowProgressSnapshot {
            rows: self.rows.load(Ordering::Relaxed),
            tables_done: self.tables_done.load(Ordering::Relaxed),
            tables_total: self.tables_total.load(Ordering::Relaxed),
            current: self.current.lock().expect("row progress poisoned").clone(),
        }
    }

    pub(crate) fn set_total(&self, units: usize) {
        self.tables_total.store(units as u64, Ordering::Relaxed);
    }

    pub(crate) fn start_table(&self, table: &str) {
        *self.current.lock().expect("row progress poisoned") = table.to_string();
    }

    pub(crate) fn add_row(&self) {
        self.add_rows(1);
    }

    pub(crate) fn add_rows(&self, rows: u64) {
        self.rows.fetch_add(rows, Ordering::Relaxed);
    }

    pub(crate) fn finish_table(&self) {
        self.tables_done.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_reflects_started_tables_rows_and_totals() {
        let p = RowProgress::new();
        assert_eq!(
            p.snapshot(),
            RowProgressSnapshot {
                rows: 0,
                tables_done: 0,
                tables_total: 0,
                current: String::new()
            }
        );
        p.set_total(3);
        p.start_table("public.orders");
        p.add_row();
        p.add_rows(41);
        let snap = p.snapshot();
        assert_eq!((snap.rows, snap.tables_done, snap.tables_total), (42, 0, 3));
        assert_eq!(snap.current, "public.orders");
        p.finish_table();
        assert_eq!(p.snapshot().tables_done, 1);
    }
}
