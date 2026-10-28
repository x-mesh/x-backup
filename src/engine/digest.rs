//! 엔진 공통 내용 다이제스트 누적기.
//!
//! 행(문서)마다 직렬화된 바이트의 SHA-256 앞 128비트를 덧셈(wrapping)으로 합친다. 덧셈은
//! 교환법칙이 성립하므로 행 순서와 무관하게 같은 값이 나온다. 무엇을 직렬화한 바이트로 보는지는
//! 엔진마다 다르며, 그 차이는 manifest의 알고리즘 태그가 구분한다.

use sha2::{Digest, Sha256};

/// 행 다이제스트 누적기.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RowDigest(u128);

impl RowDigest {
    /// 한 행의 렌더링된 튜플 바이트를 더한다.
    pub fn add(&mut self, tuple: &[u8]) {
        let hash = Sha256::digest(tuple);
        let head: [u8; 16] = hash[..16].try_into().expect("SHA-256은 32바이트다");
        self.0 = self.0.wrapping_add(u128::from_le_bytes(head));
    }

    /// 32자리 소문자 hex.
    pub fn finish(self) -> String {
        format!("{:032x}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest_of(rows: &[&str]) -> String {
        let mut d = RowDigest::default();
        for row in rows {
            d.add(row.as_bytes());
        }
        d.finish()
    }

    #[test]
    fn empty_table_has_the_zero_digest() {
        assert_eq!(digest_of(&[]), "0".repeat(32));
    }

    #[test]
    fn digest_does_not_depend_on_row_order() {
        let forward = digest_of(&["(1,'a')", "(2,'b')", "(3,'c')"]);
        let shuffled = digest_of(&["(3,'c')", "(1,'a')", "(2,'b')"]);
        assert_eq!(forward, shuffled);
    }

    #[test]
    fn changing_one_value_changes_the_digest() {
        assert_ne!(
            digest_of(&["(1,'a')", "(2,'b')"]),
            digest_of(&["(1,'a')", "(2,'B')"])
        );
    }

    #[test]
    fn duplicate_rows_are_counted_as_a_multiset() {
        let once = digest_of(&["(1,'a')"]);
        assert_ne!(once, digest_of(&["(1,'a')", "(1,'a')"]));
        assert_ne!(
            digest_of(&["(1,'a')", "(1,'a')"]),
            digest_of(&["(2,'b')", "(2,'b')"])
        );
    }

    #[test]
    fn row_boundaries_matter() {
        assert_ne!(digest_of(&["ab", "c"]), digest_of(&["a", "bc"]));
    }

    #[test]
    fn digest_is_32_lowercase_hex_chars() {
        let digest = digest_of(&["(1,'a')"]);
        assert_eq!(digest.len(), 32);
        assert!(digest
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }
}
