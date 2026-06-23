//! MySQL 엔진 공용 헬퍼.

/// 식별자를 backtick으로 안전하게 quote한다(내부 backtick은 이중화).
pub fn quote_ident(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

/// `db`.`table` 형태의 정규화된 식별자.
pub fn quote_qualified(db: &str, name: &str) -> String {
    format!("{}.{}", quote_ident(db), quote_ident(name))
}

/// `CREATE DEFINER=`user`@`host` ...`의 DEFINER 절을 제거한다(복구 이식성 — definer 부재/권한
/// 부족으로 인한 실패 회피). `SQL SECURITY DEFINER`(`=` 없음)는 건드리지 않는다.
///
/// SHOW CREATE는 user/host를 backtick으로 quote하며 계정명에 공백이 들어갈 수 있으므로
/// (`'my user'@'host'`), 단순히 첫 공백에서 끊지 않고 `` `user`@`host` `` 토큰을 정확히 파싱한다.
pub fn strip_definer(sql: &str) -> String {
    let Some(pos) = sql.find("DEFINER=") else {
        return sql.to_string();
    };
    let bytes = sql.as_bytes();
    // backtick-quoted(내부 `` 이스케이프) 또는 bareword 토큰의 끝 인덱스를 찾는다.
    let skip_token = |mut i: usize| -> usize {
        if i < bytes.len() && bytes[i] == b'`' {
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'`' {
                    if i + 1 < bytes.len() && bytes[i + 1] == b'`' {
                        i += 2; // 이스케이프된 backtick(``)
                        continue;
                    }
                    return i + 1; // 닫는 backtick
                }
                i += 1;
            }
            i
        } else {
            while i < bytes.len() && bytes[i] != b'@' && bytes[i] != b' ' {
                i += 1;
            }
            i
        }
    };
    let mut i = pos + "DEFINER=".len();
    i = skip_token(i); // user
    if i < bytes.len() && bytes[i] == b'@' {
        i += 1;
        i = skip_token(i); // host
    }
    // host 토큰 뒤 공백 하나를 함께 흡수한다(아래에서 단일 공백으로 대체하므로 중복 방지).
    let mut end = i;
    if end < bytes.len() && bytes[end] == b' ' {
        end += 1;
    }
    // 앞에 공백이 있으면 함께 제거하고 단일 공백으로 대체(토큰 분리 유지).
    let start = if pos > 0 && bytes[pos - 1] == b' ' {
        pos - 1
    } else {
        pos
    };
    let mut out = String::with_capacity(sql.len());
    out.push_str(&sql[..start]);
    out.push(' ');
    out.push_str(&sql[end..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_idents() {
        assert_eq!(quote_ident("tbl"), "`tbl`");
        assert_eq!(quote_ident("we`ird"), "`we``ird`");
        assert_eq!(quote_qualified("app", "t"), "`app`.`t`");
    }

    #[test]
    fn strips_definer_view() {
        let sql = "CREATE ALGORITHM=UNDEFINED DEFINER=`root`@`localhost` SQL SECURITY DEFINER VIEW `v` AS SELECT 1";
        let out = strip_definer(sql);
        assert!(!out.contains("DEFINER=`root`"));
        assert!(out.contains("SQL SECURITY DEFINER VIEW `v`"));
        assert!(out.starts_with("CREATE ALGORITHM=UNDEFINED SQL SECURITY DEFINER VIEW"));
    }

    #[test]
    fn strips_definer_trigger() {
        let sql = "CREATE DEFINER=`u`@`h` TRIGGER `t` BEFORE INSERT ON `x` FOR EACH ROW SET @a=1";
        let out = strip_definer(sql);
        assert_eq!(
            out,
            "CREATE TRIGGER `t` BEFORE INSERT ON `x` FOR EACH ROW SET @a=1"
        );
    }

    #[test]
    fn leaves_non_definer_untouched() {
        let sql = "CREATE VIEW `v` AS SELECT 1";
        assert_eq!(strip_definer(sql), sql);
    }

    #[test]
    fn strips_definer_with_spaces_in_account() {
        // 계정명에 공백이 있어도 backtick 토큰 경계로 정확히 끊는다.
        let sql = "CREATE ALGORITHM=UNDEFINED DEFINER=`my user`@`local host` SQL SECURITY DEFINER VIEW `v` AS SELECT 1";
        let out = strip_definer(sql);
        assert_eq!(
            out,
            "CREATE ALGORITHM=UNDEFINED SQL SECURITY DEFINER VIEW `v` AS SELECT 1"
        );
        assert!(!out.contains("my user"));
        assert!(!out.contains("local host"));
    }

    #[test]
    fn strips_definer_with_escaped_backtick() {
        let sql = "CREATE DEFINER=`we``ird`@`localhost` TRIGGER `t` BEFORE INSERT ON `x` FOR EACH ROW SET @a=1";
        let out = strip_definer(sql);
        assert_eq!(
            out,
            "CREATE TRIGGER `t` BEFORE INSERT ON `x` FOR EACH ROW SET @a=1"
        );
    }
}
