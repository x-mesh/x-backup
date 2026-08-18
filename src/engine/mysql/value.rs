//! MySQL 컬럼 값 → SQL 리터럴 렌더링(백업 시점). mysqldump가 텍스트 INSERT를 만드는 것과 같은
//! 전략이되, 무손실을 위해 타입별 규칙을 명시한다.
//!
//! - 바이너리(BINARY/BLOB/BIT/GEOMETRY) → `0x<hex>`(빈 값은 `''`). escaping·charset 문제 회피.
//! - JSON → `CAST('<정규화 텍스트>' AS JSON)`.
//! - 숫자(INT/DECIMAL/FLOAT/DOUBLE/YEAR) → unquoted. DECIMAL은 서버 텍스트를 그대로 보존.
//! - 그 외(문자/날짜/시간/ENUM/SET) → 작은따옴표 + 백슬래시 이스케이프(기본 sql_mode 전제).
//!
//! 드라이버는 **text protocol**(`query_iter`)로 읽으므로 NULL을 제외한 모든 값이
//! [`Value::Bytes`]로 도착한다(서버가 이미 텍스트로 렌더링). 따라서 카테고리는 그 바이트를
//! 어떻게 감쌀지를 정한다. (typed 변형 Int/Float/Date/Time은 binary protocol 대비 방어적 처리.)
//!
//! **알려진 한계:** FLOAT/DOUBLE은 서버 텍스트 표현을 보존하므로 mysqldump와 동일하게 10진
//! 왕복이 보장되지 않을 수 있다(docs/mysql.md에 명시).

use mysql_async::consts::ColumnType;
use mysql_async::Value;

/// 컬럼 렌더링 카테고리 — information_schema.COLUMNS.DATA_TYPE으로 결정.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColCategory {
    /// 숫자 — unquoted.
    Numeric,
    /// 바이너리/공간 — `0x<hex>`(빈 값 `''`).
    Binary,
    /// JSON — `CAST('...' AS JSON)`.
    Json,
    /// 문자/날짜/시간/ENUM/SET — quote + escape.
    Text,
}

impl ColCategory {
    /// `DATA_TYPE`(소문자, 길이·unsigned 제외) → 카테고리.
    pub fn from_data_type(data_type: &str) -> Self {
        match data_type.trim().to_ascii_lowercase().as_str() {
            "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" | "decimal"
            | "dec" | "numeric" | "fixed" | "float" | "double" | "real" | "double precision"
            | "year" | "bool" | "boolean" => ColCategory::Numeric,
            "binary" | "varbinary" | "tinyblob" | "blob" | "mediumblob" | "longblob" | "bit"
            | "geometry" | "point" | "linestring" | "polygon" | "multipoint"
            | "multilinestring" | "multipolygon" | "geometrycollection" | "geomcollection" => {
                ColCategory::Binary
            }
            "json" => ColCategory::Json,
            // char/varchar/text*/enum/set/date/time/datetime/timestamp 및 미지의 타입은 안전하게 quote.
            _ => ColCategory::Text,
        }
    }
}

/// `mysql_async::Value`를 SQL 리터럴로 렌더링한다.
pub fn render_value(v: &Value, cat: ColCategory) -> String {
    match v {
        Value::NULL => "NULL".to_string(),
        Value::Bytes(b) => render_bytes(b, cat),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        // Rust의 `{}`는 f32/f64를 왕복 가능한 최단 표현으로 출력한다(binary protocol 경로).
        Value::Float(f) => format!("{f}"),
        Value::Double(f) => format!("{f}"),
        Value::Date(y, mo, d, h, mi, s, us) => render_datetime(*y, *mo, *d, *h, *mi, *s, *us),
        Value::Time(neg, days, h, mi, s, us) => render_time(*neg, *days, *h, *mi, *s, *us),
    }
}

/// text protocol의 raw 바이트를 카테고리에 맞춰 리터럴로.
fn render_bytes(b: &[u8], cat: ColCategory) -> String {
    match cat {
        ColCategory::Numeric => String::from_utf8_lossy(b).into_owned(),
        ColCategory::Binary => {
            if b.is_empty() {
                "''".to_string()
            } else {
                format!("0x{}", hex::encode(b))
            }
        }
        ColCategory::Json => format!("CAST('{}' AS JSON)", escape_str(b)),
        ColCategory::Text => format!("'{}'", escape_str(b)),
    }
}

/// 기본 sql_mode(NO_BACKSLASH_ESCAPES 미설정)에서 안전한 문자열 이스케이프.
/// `'` `\` NUL 개행 CR Ctrl-Z를 백슬래시 이스케이프한다. 멀티바이트 UTF-8(>=0x80)은 그대로 통과.
fn escape_str(b: &[u8]) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(b.len() + 2);
    for &c in b {
        match c {
            b'\'' => out.extend_from_slice(b"\\'"),
            b'\\' => out.extend_from_slice(b"\\\\"),
            0 => out.extend_from_slice(b"\\0"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            0x1a => out.extend_from_slice(b"\\Z"),
            other => out.push(other),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// binlog ROW 이벤트 값(typed `Value`)을 SQL 리터럴로 렌더링한다 — 컬럼 카테고리 정보 없이
/// 값 변형으로 판단한다. `Bytes`는 유효 UTF-8이면 따옴표 문자열, 아니면 `0x<hex>`로 낸다.
///
/// 이 휴리스틱은 라운드트립에 안전하다: UTF-8 데이터는 텍스트/바이너리 컬럼 어느 쪽에
/// INSERT해도 같은 바이트가 저장되고(따옴표 문자열 → 바이트), 숫자/JSON 텍스트는 대상 컬럼
/// 타입으로 암묵 캐스트된다. 진짜 바이너리(non-UTF-8)만 hex가 필수다.
pub fn render_value_auto(v: &Value) -> String {
    match v {
        Value::NULL => "NULL".to_string(),
        Value::Bytes(b) => {
            if std::str::from_utf8(b).is_ok() {
                format!("'{}'", escape_str(b))
            } else if b.is_empty() {
                "''".to_string()
            } else {
                format!("0x{}", hex::encode(b))
            }
        }
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) => format!("{f}"),
        Value::Double(f) => format!("{f}"),
        Value::Date(y, mo, d, h, mi, s, us) => render_datetime(*y, *mo, *d, *h, *mi, *s, *us),
        Value::Time(neg, days, h, mi, s, us) => render_time(*neg, *days, *h, *mi, *s, *us),
    }
}

/// binlog ROW 값을 **컬럼 타입까지 고려**해 SQL 리터럴로 렌더링한다.
///
/// mysql_common이 일부 타입을 [`render_value_auto`]의 바이트 휴리스틱으로는 잘못 처리되는
/// 형태로 디코드하므로 타입별로 교정한다:
/// - `BIT` → `Bytes(raw)` (휴리스틱은 `0x41`을 `'A'`로 오인) → `0x<hex>`.
/// - `SET` → `Bytes(LE 비트마스크)` (휴리스틱은 따옴표 문자열로 오인) → 정수(비트마스크).
/// - `TIMESTAMP` → `Bytes("unix초[.usec]")` (휴리스틱은 `'1718…'`로 오인) → `FROM_UNIXTIME(...)`.
///
/// 그 외(ENUM→Int, DATETIME/TIME→Date/Time, DECIMAL/BLOB/TEXT→Bytes)는 [`render_value_auto`]가
/// 이미 올바르게 처리한다.
pub fn render_binlog_value(v: &Value, col_type: Option<ColumnType>) -> String {
    use ColumnType::*;
    match (v, col_type) {
        (Value::Bytes(b), Some(MYSQL_TYPE_BIT)) => {
            if b.is_empty() {
                "0".to_string()
            } else {
                format!("0x{}", hex::encode(b))
            }
        }
        (Value::Bytes(b), Some(MYSQL_TYPE_SET)) => {
            // SET은 LE 비트마스크 — 정수로 넣으면 MySQL이 멤버 비트로 해석한다.
            let mut n: u64 = 0;
            for (i, &byte) in b.iter().enumerate().take(8) {
                n |= (byte as u64) << (8 * i);
            }
            n.to_string()
        }
        (Value::Bytes(b), Some(MYSQL_TYPE_TIMESTAMP2 | MYSQL_TYPE_TIMESTAMP)) => {
            let s = String::from_utf8_lossy(b);
            if s == "0" {
                // zero-timestamp 특수값.
                "'0000-00-00 00:00:00'".to_string()
            } else {
                // unix epoch 초(UTC). 복구 세션 time_zone='+00:00'이라 왕복 일치.
                format!("FROM_UNIXTIME({s})")
            }
        }
        _ => render_value_auto(v),
    }
}

/// binary protocol DATETIME/DATE 렌더링(방어적 — text protocol에선 미사용).
fn render_datetime(y: u16, mo: u8, d: u8, h: u8, mi: u8, s: u8, us: u32) -> String {
    if us > 0 {
        format!("'{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}.{us:06}'")
    } else {
        format!("'{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}'")
    }
}

/// binary protocol TIME 렌더링(방어적). days를 시간으로 펼친다.
fn render_time(neg: bool, days: u32, h: u8, mi: u8, s: u8, us: u32) -> String {
    let sign = if neg { "-" } else { "" };
    let hours = days * 24 + h as u32;
    if us > 0 {
        format!("'{sign}{hours:02}:{mi:02}:{s:02}.{us:06}'")
    } else {
        format!("'{sign}{hours:02}:{mi:02}:{s:02}'")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn category_mapping() {
        assert_eq!(ColCategory::from_data_type("int"), ColCategory::Numeric);
        assert_eq!(ColCategory::from_data_type("DECIMAL"), ColCategory::Numeric);
        assert_eq!(ColCategory::from_data_type("blob"), ColCategory::Binary);
        assert_eq!(ColCategory::from_data_type("bit"), ColCategory::Binary);
        assert_eq!(ColCategory::from_data_type("json"), ColCategory::Json);
        assert_eq!(ColCategory::from_data_type("varchar"), ColCategory::Text);
        assert_eq!(ColCategory::from_data_type("datetime"), ColCategory::Text);
        assert_eq!(ColCategory::from_data_type("weirdtype"), ColCategory::Text);
    }

    #[test]
    fn renders_null() {
        assert_eq!(render_value(&Value::NULL, ColCategory::Text), "NULL");
        assert_eq!(render_value(&Value::NULL, ColCategory::Numeric), "NULL");
    }

    #[test]
    fn renders_numeric_unquoted() {
        let v = Value::Bytes(b"123.45".to_vec());
        assert_eq!(render_value(&v, ColCategory::Numeric), "123.45");
    }

    #[test]
    fn renders_binary_hex() {
        let v = Value::Bytes(vec![0x00, 0xff, 0x10]);
        assert_eq!(render_value(&v, ColCategory::Binary), "0x00ff10");
        let empty = Value::Bytes(vec![]);
        assert_eq!(render_value(&empty, ColCategory::Binary), "''");
    }

    #[test]
    fn renders_json_cast() {
        let v = Value::Bytes(br#"{"a": 1}"#.to_vec());
        assert_eq!(
            render_value(&v, ColCategory::Json),
            "CAST('{\"a\": 1}' AS JSON)"
        );
    }

    #[test]
    fn escapes_text() {
        let v = Value::Bytes(b"o'reilly\\path\nline".to_vec());
        assert_eq!(
            render_value(&v, ColCategory::Text),
            "'o\\'reilly\\\\path\\nline'"
        );
    }

    #[test]
    fn preserves_utf8_multibyte() {
        let v = Value::Bytes("한글".as_bytes().to_vec());
        assert_eq!(render_value(&v, ColCategory::Text), "'한글'");
    }

    #[test]
    fn renders_typed_float_roundtrip() {
        assert_eq!(
            render_value(&Value::Double(1.5), ColCategory::Numeric),
            "1.5"
        );
        assert_eq!(render_value(&Value::Int(-42), ColCategory::Numeric), "-42");
    }

    #[test]
    fn binlog_bit_renders_as_hex() {
        // BIT(8)=65 → Bytes([0x41]); 휴리스틱은 'A'로 오인하므로 타입 인지로 0x41.
        let v = Value::Bytes(vec![0x41]);
        assert_eq!(
            render_binlog_value(&v, Some(ColumnType::MYSQL_TYPE_BIT)),
            "0x41"
        );
    }

    #[test]
    fn binlog_set_renders_as_integer_bitmask() {
        // SET('a','b','c')에서 a,c → LE 비트마스크 [0x05] → 정수 5.
        let v = Value::Bytes(vec![0x05]);
        assert_eq!(
            render_binlog_value(&v, Some(ColumnType::MYSQL_TYPE_SET)),
            "5"
        );
    }

    #[test]
    fn binlog_timestamp_renders_from_unixtime() {
        let v = Value::Bytes(b"1718524800".to_vec());
        assert_eq!(
            render_binlog_value(&v, Some(ColumnType::MYSQL_TYPE_TIMESTAMP2)),
            "FROM_UNIXTIME(1718524800)"
        );
        let frac = Value::Bytes(b"1718524800.123456".to_vec());
        assert_eq!(
            render_binlog_value(&frac, Some(ColumnType::MYSQL_TYPE_TIMESTAMP2)),
            "FROM_UNIXTIME(1718524800.123456)"
        );
        // zero timestamp 특수값.
        assert_eq!(
            render_binlog_value(
                &Value::Bytes(b"0".to_vec()),
                Some(ColumnType::MYSQL_TYPE_TIMESTAMP2)
            ),
            "'0000-00-00 00:00:00'"
        );
    }

    #[test]
    fn binlog_other_types_delegate() {
        // ENUM → Int(index), 그대로 숫자.
        assert_eq!(
            render_binlog_value(&Value::Int(2), Some(ColumnType::MYSQL_TYPE_ENUM)),
            "2"
        );
        // 텍스트는 따옴표.
        assert_eq!(
            render_binlog_value(
                &Value::Bytes(b"hi".to_vec()),
                Some(ColumnType::MYSQL_TYPE_VARCHAR)
            ),
            "'hi'"
        );
        // NULL.
        assert_eq!(
            render_binlog_value(&Value::NULL, Some(ColumnType::MYSQL_TYPE_BIT)),
            "NULL"
        );
    }
}
