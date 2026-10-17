//! Minimal **read-only** SQLite file reader over an in-memory image.
//!
//! Supports exactly what the `nbk` container needs: the schema table and ordinary rowid table
//! b-trees (interior 0x05 / leaf 0x0D pages, overflow chains, record format). It never writes,
//! never opens the file itself, and fails closed on anything malformed (bounded page visits,
//! bounds-checked reads). Format: <https://www.sqlite.org/fileformat.html>.

use std::collections::HashSet;

use crate::NbkError;

const MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// One decoded column value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    Float(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl Value {
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Value::Text(s) => Some(s),
            _ => None,
        }
    }

    pub fn into_blob(self) -> Option<Vec<u8>> {
        match self {
            Value::Blob(b) => Some(b),
            Value::Null => Some(Vec::new()),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(v) => Some(*v),
            _ => None,
        }
    }
}

pub struct Db<'a> {
    data: &'a [u8],
    page_size: usize,
    usable: usize,
    page_count: usize,
}

/// Schema row of a table.
#[derive(Clone, Debug)]
pub struct TableInfo {
    pub name: String,
    pub root_page: u32,
    pub sql: String,
}

impl TableInfo {
    /// Declared column names, lower-cased, from `CREATE TABLE name(col type, …)` (table
    /// constraints such as `primary key (…)` end the list).
    pub fn columns(&self) -> Vec<String> {
        let Some(open) = self.sql.find('(') else {
            return Vec::new();
        };
        let body = &self.sql[open + 1..self.sql.rfind(')').unwrap_or(self.sql.len())];
        body.split(',')
            .map(str::trim)
            .take_while(|c| {
                let l = c.to_ascii_lowercase();
                !(l.starts_with("primary key")
                    || l.starts_with("unique")
                    || l.starts_with("constraint"))
            })
            .filter_map(|c| c.split_whitespace().next())
            .map(|c| {
                c.trim_matches(|ch| ch == '"' || ch == '`' || ch == '[' || ch == ']')
                    .to_ascii_lowercase()
            })
            .collect()
    }
}

fn bad(msg: impl Into<String>) -> NbkError {
    NbkError::Container(msg.into())
}

impl<'a> Db<'a> {
    pub fn open(data: &'a [u8]) -> Result<Self, NbkError> {
        if data.len() < 100 || &data[..16] != MAGIC {
            return Err(bad("not a SQLite database"));
        }
        let raw_ps = u16::from_be_bytes([data[16], data[17]]) as usize;
        let page_size = if raw_ps == 1 { 65536 } else { raw_ps };
        if !(512..=65536).contains(&page_size) || !page_size.is_power_of_two() {
            return Err(bad(format!("bad page size {raw_ps}")));
        }
        let reserved = data[20] as usize;
        let usable = page_size
            .checked_sub(reserved)
            .filter(|u| *u >= 480)
            .ok_or_else(|| bad("bad reserved space"))?;
        if u32::from_be_bytes([data[56], data[57], data[58], data[59]]) > 1 {
            return Err(bad("only UTF-8 databases are supported"));
        }
        Ok(Self {
            data,
            page_size,
            usable,
            page_count: data.len() / page_size,
        })
    }

    fn page(&self, n: u32) -> Result<&'a [u8], NbkError> {
        let n = n as usize;
        if n == 0 || n > self.page_count {
            return Err(bad(format!("page {n} out of range")));
        }
        let start = (n - 1) * self.page_size;
        Ok(&self.data[start..start + self.page_size])
    }

    /// Tables from `sqlite_master` (type = 'table').
    pub fn tables(&self) -> Result<Vec<TableInfo>, NbkError> {
        let mut out = Vec::new();
        for row in self.scan(1)? {
            // type, name, tbl_name, rootpage, sql
            if row.len() < 5 || row[0].as_text() != Some("table") {
                continue;
            }
            let name = row[1].as_text().unwrap_or_default().to_string();
            let root = row[3].as_int().unwrap_or(0);
            let sql = row[4].as_text().unwrap_or_default().to_string();
            if root <= 0 || root > u32::MAX as i64 {
                return Err(bad(format!("table {name}: bad root page")));
            }
            out.push(TableInfo {
                name,
                root_page: root as u32,
                sql,
            });
        }
        Ok(out)
    }

    /// Every row of the rowid table rooted at `root`, in rowid order.
    pub fn scan(&self, root: u32) -> Result<Vec<Vec<Value>>, NbkError> {
        let mut rows = Vec::new();
        let mut seen = HashSet::new();
        self.walk(root, 0, &mut seen, &mut rows)?;
        Ok(rows)
    }

    fn walk(
        &self,
        n: u32,
        depth: usize,
        seen: &mut HashSet<u32>,
        rows: &mut Vec<Vec<Value>>,
    ) -> Result<(), NbkError> {
        if depth > 64 || !seen.insert(n) {
            return Err(bad("b-tree cycle or excessive depth"));
        }
        let page = self.page(n)?;
        let hdr = if n == 1 { 100 } else { 0 };
        let kind = *page.get(hdr).ok_or_else(|| bad("short page"))?;
        let cells = u16::from_be_bytes([page[hdr + 3], page[hdr + 4]]) as usize;
        let ptrs = hdr + if kind == 0x05 { 12 } else { 8 };
        let cell_offset = |i: usize| -> Result<usize, NbkError> {
            let p = ptrs + 2 * i;
            let b = page
                .get(p..p + 2)
                .ok_or_else(|| bad("cell pointer out of page"))?;
            Ok(u16::from_be_bytes([b[0], b[1]]) as usize)
        };
        match kind {
            0x05 => {
                for i in 0..cells {
                    let off = cell_offset(i)?;
                    let child = page
                        .get(off..off + 4)
                        .ok_or_else(|| bad("bad interior cell"))?;
                    self.walk(
                        u32::from_be_bytes([child[0], child[1], child[2], child[3]]),
                        depth + 1,
                        seen,
                        rows,
                    )?;
                }
                let r = &page[hdr + 8..hdr + 12];
                self.walk(
                    u32::from_be_bytes([r[0], r[1], r[2], r[3]]),
                    depth + 1,
                    seen,
                    rows,
                )
            }
            0x0D => {
                for i in 0..cells {
                    let off = cell_offset(i)?;
                    let payload = self.leaf_payload(page, off)?;
                    rows.push(decode_record(&payload)?);
                }
                Ok(())
            }
            other => Err(bad(format!(
                "unsupported b-tree page type {other:#x} (page {n})"
            ))),
        }
    }

    fn leaf_payload(&self, page: &[u8], off: usize) -> Result<Vec<u8>, NbkError> {
        let (p_len, a) = varint(page, off)?;
        let (_rowid, b) = varint(page, off + a)?;
        let start = off + a + b;
        let p_len = usize::try_from(p_len).map_err(|_| bad("payload too large"))?;
        if p_len > 64 * 1024 * 1024 {
            return Err(bad("payload too large"));
        }
        let u = self.usable;
        let x = u - 35;
        let local = if p_len <= x {
            p_len
        } else {
            let m = ((u - 12) * 32 / 255) - 23;
            let k = m + ((p_len - m) % (u - 4));
            if k <= x {
                k
            } else {
                m
            }
        };
        let mut out = page
            .get(start..start + local)
            .ok_or_else(|| bad("cell payload out of page"))?
            .to_vec();
        if local < p_len {
            let o = start + local;
            let ptr = page
                .get(o..o + 4)
                .ok_or_else(|| bad("overflow pointer out of page"))?;
            let mut next = u32::from_be_bytes([ptr[0], ptr[1], ptr[2], ptr[3]]);
            let mut hops = 0usize;
            while out.len() < p_len {
                hops += 1;
                if next == 0 || hops > self.page_count {
                    return Err(bad("broken overflow chain"));
                }
                let op = self.page(next)?;
                next = u32::from_be_bytes([op[0], op[1], op[2], op[3]]);
                let take = (p_len - out.len()).min(u - 4);
                out.extend_from_slice(&op[4..4 + take]);
            }
        }
        Ok(out)
    }
}

/// SQLite big-endian varint (1–9 bytes). Returns (value, bytes consumed).
fn varint(buf: &[u8], at: usize) -> Result<(u64, usize), NbkError> {
    let mut v: u64 = 0;
    for i in 0..9 {
        let b = *buf.get(at + i).ok_or_else(|| bad("truncated varint"))?;
        if i == 8 {
            return Ok(((v << 8) | b as u64, 9));
        }
        v = (v << 7) | (b & 0x7F) as u64;
        if b & 0x80 == 0 {
            return Ok((v, i + 1));
        }
    }
    unreachable!()
}

fn decode_record(p: &[u8]) -> Result<Vec<Value>, NbkError> {
    let (hsize, mut h) = varint(p, 0)?;
    let hsize = hsize as usize;
    if hsize > p.len() {
        return Err(bad("record header out of range"));
    }
    let mut types = Vec::new();
    while h < hsize {
        let (t, n) = varint(p, h)?;
        types.push(t);
        h += n;
    }
    let mut at = hsize;
    let mut vals = Vec::with_capacity(types.len());
    for t in types {
        let take = |at: &mut usize, n: usize| -> Result<&[u8], NbkError> {
            let s = p
                .get(*at..*at + n)
                .ok_or_else(|| bad("record body out of range"))?;
            *at += n;
            Ok(s)
        };
        let int = |b: &[u8]| -> i64 {
            let mut v: i64 = if b[0] & 0x80 != 0 { -1 } else { 0 };
            for x in b {
                v = (v << 8) | *x as i64;
            }
            v
        };
        vals.push(match t {
            0 => Value::Null,
            1 => Value::Int(int(take(&mut at, 1)?)),
            2 => Value::Int(int(take(&mut at, 2)?)),
            3 => Value::Int(int(take(&mut at, 3)?)),
            4 => Value::Int(int(take(&mut at, 4)?)),
            5 => Value::Int(int(take(&mut at, 6)?)),
            6 => Value::Int(int(take(&mut at, 8)?)),
            7 => {
                let b = take(&mut at, 8)?;
                Value::Float(f64::from_be_bytes(b.try_into().unwrap()))
            }
            8 => Value::Int(0),
            9 => Value::Int(1),
            t if t >= 12 && t % 2 == 0 => {
                Value::Blob(take(&mut at, ((t - 12) / 2) as usize)?.to_vec())
            }
            t if t >= 13 => Value::Text(
                String::from_utf8_lossy(take(&mut at, ((t - 13) / 2) as usize)?).into_owned(),
            ),
            t => return Err(bad(format!("reserved serial type {t}"))),
        });
    }
    Ok(vals)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_and_records_decode() {
        assert_eq!(varint(&[0x05], 0).unwrap(), (5, 1));
        assert_eq!(varint(&[0x81, 0x00], 0).unwrap(), (128, 2));
        // header len 4: text(len 2)=17, int8=1, blob(len 1)=14 ; body "hi", -1, 0xAB
        let rec = [4u8, 17, 1, 14, b'h', b'i', 0xFF, 0xAB];
        assert_eq!(
            decode_record(&rec).unwrap(),
            vec![
                Value::Text("hi".into()),
                Value::Int(-1),
                Value::Blob(vec![0xAB])
            ]
        );
    }

    #[test]
    fn rejects_non_sqlite_and_truncation() {
        assert!(Db::open(b"nope").is_err());
        assert!(decode_record(&[3, 17, 1]).is_err());
    }
}
