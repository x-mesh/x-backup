//! pgoutput 논리복제 메시지 디코더 — 외부 의존 없이 직접 파싱(PG 증분).
//!
//! PG 문서 "Logical Streaming Replication Protocol — Message Formats" 기준. 모든 정수는
//! big-endian, 문자열은 NUL 종단. `pg_logical_slot_peek_binary_changes`가 돌려주는 bytea
//! 메시지를 하나씩 [`Decoder::feed`]에 넣으면 I/U/D 변경([`Change`])을 돌려준다.
//!
//! 상태가 필요한 이유: 컬럼 이름·키 여부는 `R`(Relation) 메시지에, commit 시각은 `B`(Begin)
//! 메시지에 담겨 오므로, 디코더가 트랜잭션·릴레이션 메타를 들고 있다가 I/U/D에 붙인다.

use std::collections::HashMap;

use crate::error::{Result, XBackupError};

/// 변경 종류.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Insert,
    Update,
    Delete,
}

/// 디코드된 한 행 변경 — 복구 시 그대로 DML로 적용 가능한 자기 완결 레코드.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub op: Op,
    pub schema: String,
    pub table: String,
    /// 릴레이션 컬럼 이름(순서대로).
    pub colnames: Vec<String>,
    /// 각 컬럼이 복제 식별 키(replica identity)인지.
    pub keycols: Vec<bool>,
    /// 신규 튜플 값(Insert/Update). Delete면 빈 벡터.
    pub new_vals: Vec<Option<String>>,
    /// `new_vals[i]`가 **unchanged-TOAST(`'u'`)** 라 써서는 안 되는 컬럼인지(H3).
    ///
    /// pgoutput은 UPDATE에서 변경되지 않은 out-of-line TOAST 값을 `'u'`로 보낸다 — 실제
    /// 값을 싣지 않는다. 이를 `'n'`(SQL NULL)과 같은 `None`으로 뭉개면 UPDATE SET이 기존
    /// 값을 NULL로 덮어써 행을 파손한다. 이 마스크가 `true`인 컬럼은 SET에서 제외해
    /// 기존 값을 보존한다.
    pub new_unchanged: Vec<bool>,
    /// 키/old 튜플 값(Update/Delete의 WHERE 식별). Insert면 빈 벡터.
    pub key_vals: Vec<Option<String>>,
    /// 트랜잭션 commit 시각(unix epoch 마이크로초). `--at` PITR 필터에 사용.
    pub commit_unix_micros: i64,
}

/// 릴레이션 메타(컬럼 이름·키 여부).
struct RelMeta {
    schema: String,
    table: String,
    cols: Vec<(String, bool)>,
}

/// PG epoch(2000-01-01)와 unix epoch(1970-01-01)의 초 차이.
const PG_EPOCH_UNIX_SECS: i64 = 946_684_800;

/// 상태 보존 pgoutput 디코더 — 메시지를 순서대로 feed한다.
#[derive(Default)]
pub struct Decoder {
    rels: HashMap<i32, RelMeta>,
    cur_commit_micros: i64,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// pgoutput 메시지 하나를 처리한다. I/U/D면 [`Change`], 그 외(B/C/R/...)면 `None`.
    pub fn feed(&mut self, data: &[u8]) -> Result<Option<Change>> {
        let mut c = Cur::new(data);
        match c.u8()? {
            b'B' => {
                // Begin: Int64 final_lsn, Int64 commit_ts(PG micros), Int32 xid.
                c.skip(8)?;
                let pg_micros = c.i64()?;
                self.cur_commit_micros = pg_micros + PG_EPOCH_UNIX_SECS * 1_000_000;
                Ok(None)
            }
            b'C' => Ok(None), // Commit
            b'R' => {
                let rel_id = c.i32()?;
                let schema = c.cstr()?;
                let table = c.cstr()?;
                c.skip(1)?; // replica identity
                let ncol = c.i16()?;
                let mut cols = Vec::with_capacity(ncol.max(0) as usize);
                for _ in 0..ncol {
                    let flags = c.u8()?;
                    let name = c.cstr()?;
                    c.skip(8)?; // type oid(4) + type modifier(4)
                    cols.push((name, flags & 1 == 1));
                }
                self.rels.insert(
                    rel_id,
                    RelMeta {
                        schema,
                        table,
                        cols,
                    },
                );
                Ok(None)
            }
            b'I' => {
                let rel_id = c.i32()?;
                c.skip(1)?; // 'N'
                let (new_vals, new_unchanged) = c.tuple()?;
                let rel = self.rel(rel_id)?;
                Ok(Some(self.make(rel, Op::Insert, new_vals, new_unchanged, Vec::new())))
            }
            b'U' => {
                let rel_id = c.i32()?;
                // 선택적 K(키)/O(old) 튜플 뒤 'N' 신규 튜플.
                let mut key_vals = Vec::new();
                let tag = c.u8()?;
                let tag = if tag == b'K' || tag == b'O' {
                    // old/key 튜플의 unchanged 마스크는 버린다 — WHERE 식별엔 키 컬럼(보통 PK,
                    // 비 TOAST)만 쓰므로 영향이 없다. 신규 튜플의 'u'만 보존한다(H3).
                    let (kv, _) = c.tuple()?;
                    key_vals = kv;
                    c.u8()? // 'N'
                } else {
                    tag // 이미 'N'
                };
                if tag != b'N' {
                    return Err(XBackupError::Failure(format!(
                        "pgoutput UPDATE: 예기치 못한 튜플 태그 {}",
                        tag as char
                    )));
                }
                let (new_vals, new_unchanged) = c.tuple()?;
                let rel = self.rel(rel_id)?;
                Ok(Some(self.make(rel, Op::Update, new_vals, new_unchanged, key_vals)))
            }
            b'D' => {
                let rel_id = c.i32()?;
                c.skip(1)?; // 'K' or 'O'
                let (key_vals, _) = c.tuple()?;
                let rel = self.rel(rel_id)?;
                Ok(Some(self.make(rel, Op::Delete, Vec::new(), Vec::new(), key_vals)))
            }
            // 기타(Type 'Y', Origin 'O', Truncate 'T', Message 'M', stream 메시지 등)는 무시.
            _ => Ok(None),
        }
    }

    fn rel(&self, rel_id: i32) -> Result<&RelMeta> {
        self.rels.get(&rel_id).ok_or_else(|| {
            XBackupError::Failure(format!(
                "pgoutput: rel_id {rel_id} 메타 없음(Relation 누락)"
            ))
        })
    }

    fn make(
        &self,
        rel: &RelMeta,
        op: Op,
        new_vals: Vec<Option<String>>,
        new_unchanged: Vec<bool>,
        key_vals: Vec<Option<String>>,
    ) -> Change {
        Change {
            op,
            schema: rel.schema.clone(),
            table: rel.table.clone(),
            colnames: rel.cols.iter().map(|(n, _)| n.clone()).collect(),
            keycols: rel.cols.iter().map(|(_, k)| *k).collect(),
            new_vals,
            new_unchanged,
            key_vals,
            commit_unix_micros: self.cur_commit_micros,
        }
    }
}

/// 바이트 커서 — big-endian 정수·NUL 문자열·TupleData. 경계 초과는 에러(손상 스트림 방어).
struct Cur<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Cur<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, i: 0 }
    }
    fn need(&self, n: usize) -> Result<()> {
        if self.i + n > self.b.len() {
            return Err(XBackupError::Failure("pgoutput 메시지가 잘렸습니다".into()));
        }
        Ok(())
    }
    fn u8(&mut self) -> Result<u8> {
        self.need(1)?;
        let v = self.b[self.i];
        self.i += 1;
        Ok(v)
    }
    fn i16(&mut self) -> Result<i16> {
        self.need(2)?;
        let v = i16::from_be_bytes([self.b[self.i], self.b[self.i + 1]]);
        self.i += 2;
        Ok(v)
    }
    fn i32(&mut self) -> Result<i32> {
        self.need(4)?;
        let v = i32::from_be_bytes(self.b[self.i..self.i + 4].try_into().unwrap());
        self.i += 4;
        Ok(v)
    }
    fn i64(&mut self) -> Result<i64> {
        self.need(8)?;
        let v = i64::from_be_bytes(self.b[self.i..self.i + 8].try_into().unwrap());
        self.i += 8;
        Ok(v)
    }
    fn skip(&mut self, n: usize) -> Result<()> {
        self.need(n)?;
        self.i += n;
        Ok(())
    }
    fn cstr(&mut self) -> Result<String> {
        let start = self.i;
        while self.i < self.b.len() && self.b[self.i] != 0 {
            self.i += 1;
        }
        self.need(1)?; // NUL
        let s = String::from_utf8_lossy(&self.b[start..self.i]).into_owned();
        self.i += 1;
        Ok(s)
    }
    /// TupleData: Int16 ncols, 컬럼별 'n'(null)/'u'(toast 미변경)/'t'(텍스트 len+bytes).
    ///
    /// 반환 = (값, unchanged 마스크). `'u'`는 값이 없으므로 `None`을 넣되 마스크를 `true`로
    /// 표시해 `'n'`(SQL NULL)과 구분한다(H3 — UPDATE SET에서 제외해 기존 값 보존).
    fn tuple(&mut self) -> Result<(Vec<Option<String>>, Vec<bool>)> {
        let n = self.i16()?;
        let cap = n.max(0) as usize;
        let mut out = Vec::with_capacity(cap);
        let mut unchanged = Vec::with_capacity(cap);
        for _ in 0..n {
            match self.u8()? {
                b'n' => {
                    out.push(None);
                    unchanged.push(false);
                }
                b'u' => {
                    out.push(None);
                    unchanged.push(true);
                }
                b't' => {
                    let len = self.i32()? as usize;
                    self.need(len)?;
                    let s = String::from_utf8_lossy(&self.b[self.i..self.i + len]).into_owned();
                    self.i += len;
                    out.push(Some(s));
                    unchanged.push(false);
                }
                other => {
                    return Err(XBackupError::Failure(format!(
                        "pgoutput tuple: 알 수 없는 컬럼 종류 {}",
                        other as char
                    )))
                }
            }
        }
        Ok((out, unchanged))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 합성 메시지 빌더(테스트용).
    fn relation(rel_id: i32, schema: &str, table: &str, cols: &[(&str, bool)]) -> Vec<u8> {
        let mut m = vec![b'R'];
        m.extend_from_slice(&rel_id.to_be_bytes());
        m.extend_from_slice(schema.as_bytes());
        m.push(0);
        m.extend_from_slice(table.as_bytes());
        m.push(0);
        m.push(b'd'); // replica identity default
        m.extend_from_slice(&(cols.len() as i16).to_be_bytes());
        for (name, key) in cols {
            m.push(if *key { 1 } else { 0 });
            m.extend_from_slice(name.as_bytes());
            m.push(0);
            m.extend_from_slice(&23i32.to_be_bytes()); // type oid
            m.extend_from_slice(&(-1i32).to_be_bytes()); // type mod
        }
        m
    }
    fn tuple(vals: &[Option<&str>]) -> Vec<u8> {
        let mut t = Vec::new();
        t.extend_from_slice(&(vals.len() as i16).to_be_bytes());
        for v in vals {
            match v {
                None => t.push(b'n'),
                Some(s) => {
                    t.push(b't');
                    t.extend_from_slice(&(s.len() as i32).to_be_bytes());
                    t.extend_from_slice(s.as_bytes());
                }
            }
        }
        t
    }

    #[test]
    fn decodes_insert_after_relation() {
        let mut d = Decoder::new();
        assert!(d
            .feed(&relation(
                1,
                "public",
                "t",
                &[("id", true), ("name", false)]
            ))
            .unwrap()
            .is_none());
        let mut ins = vec![b'I'];
        ins.extend_from_slice(&1i32.to_be_bytes());
        ins.push(b'N');
        ins.extend_from_slice(&tuple(&[Some("7"), Some("alice")]));
        let ch = d.feed(&ins).unwrap().unwrap();
        assert_eq!(ch.op, Op::Insert);
        assert_eq!(ch.schema, "public");
        assert_eq!(ch.table, "t");
        assert_eq!(ch.colnames, vec!["id", "name"]);
        assert_eq!(ch.keycols, vec![true, false]);
        assert_eq!(ch.new_vals, vec![Some("7".into()), Some("alice".into())]);
    }

    #[test]
    fn decodes_delete_with_key() {
        let mut d = Decoder::new();
        d.feed(&relation(5, "s", "u", &[("id", true), ("x", false)]))
            .unwrap();
        let mut del = vec![b'D'];
        del.extend_from_slice(&5i32.to_be_bytes());
        del.push(b'K');
        del.extend_from_slice(&tuple(&[Some("42"), None]));
        let ch = d.feed(&del).unwrap().unwrap();
        assert_eq!(ch.op, Op::Delete);
        assert_eq!(ch.key_vals, vec![Some("42".into()), None]);
        assert!(ch.new_vals.is_empty());
    }

    #[test]
    fn begin_sets_commit_ts() {
        let mut d = Decoder::new();
        let mut begin = vec![b'B'];
        begin.extend_from_slice(&0i64.to_be_bytes()); // final lsn
        begin.extend_from_slice(&0i64.to_be_bytes()); // commit ts = PG epoch
        begin.extend_from_slice(&1i32.to_be_bytes()); // xid
        assert!(d.feed(&begin).unwrap().is_none());
        assert_eq!(d.cur_commit_micros, PG_EPOCH_UNIX_SECS * 1_000_000);
    }

    #[test]
    fn truncated_message_errors() {
        let mut d = Decoder::new();
        assert!(d.feed(&[b'I', 0, 0]).is_err()); // rel_id 잘림
    }

    /// H3 — UPDATE의 unchanged-TOAST('u') 컬럼은 `new_unchanged=true`로 표시되고
    /// `'n'`(NULL)과 구분된다. 값이 바뀐 't' 컬럼만 false.
    #[test]
    fn decodes_update_marks_unchanged_toast_distinct_from_null() {
        let mut d = Decoder::new();
        d.feed(&relation(
            9,
            "public",
            "docs",
            &[("id", true), ("body", false), ("note", false)],
        ))
        .unwrap();
        // UPDATE: 'N' 신규 튜플 = [ id='7'(t), body='u'(unchanged TOAST), note='n'(NULL) ].
        let mut upd = vec![b'U'];
        upd.extend_from_slice(&9i32.to_be_bytes());
        upd.push(b'N');
        upd.extend_from_slice(&3i16.to_be_bytes()); // ncols
        upd.push(b't'); // id
        upd.extend_from_slice(&1i32.to_be_bytes());
        upd.push(b'7');
        upd.push(b'u'); // body — unchanged TOAST
        upd.push(b'n'); // note — SQL NULL

        let ch = d.feed(&upd).unwrap().unwrap();
        assert_eq!(ch.op, Op::Update);
        assert_eq!(ch.new_vals, vec![Some("7".into()), None, None]);
        // body는 unchanged(true), id·note는 false — NULL과 unchanged가 구분된다.
        assert_eq!(ch.new_unchanged, vec![false, true, false]);
    }
}
