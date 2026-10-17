//! Minimal Amazon/Kindle Ion binary walker — a faithful port of the proven Python
//! `ion_walk.walk_ion_document` semantics (not a general Ion implementation):
//!
//! * Kindle VarUInt: the high bit marks the **final** byte.
//! * `0x00` inside containers / at top level is skipped (NOP pad); an embedded `E0 01 00 EA`
//!   version marker is skipped.
//! * A malformed child ends its container (the container's declared length still bounds it);
//!   a malformed top-level value ends the document.

pub const IVM: [u8; 4] = [0xE0, 0x01, 0x00, 0xEA];

#[derive(Clone, Debug, PartialEq)]
pub enum Kind<'a> {
    Null,
    Bool(Option<bool>),
    /// Signed integer; `None` when it does not fit in i128.
    Int(Option<i128>),
    Float(Option<f64>),
    Symbol(u64),
    String(String),
    Blob(&'a [u8]),
    List,
    Struct,
    /// Annotation wrapper: annotation SIDs; the wrapped value is the single child.
    Annotation(Vec<u64>),
    /// decimal / timestamp / clob / reserved / typed null — not interpreted.
    Other,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Node<'a> {
    pub offset: usize,
    /// Field SID when this value is a struct field.
    pub sid: Option<u64>,
    pub kind: Kind<'a>,
    pub children: Vec<Node<'a>>,
}

impl<'a> Node<'a> {
    /// Strip annotation wrappers.
    pub fn unwrap(&self) -> &Node<'a> {
        let mut n = self;
        while let (Kind::Annotation(_), Some(c)) = (&n.kind, n.children.first()) {
            n = c;
        }
        n
    }

    pub fn is_struct(&self) -> bool {
        matches!(self.kind, Kind::Struct)
    }

    /// Struct fields (through annotation wrappers).
    pub fn fields(&self) -> &[Node<'a>] {
        let n = self.unwrap();
        if n.is_struct() {
            &n.children
        } else {
            &[]
        }
    }

    pub fn field(&self, sid: Option<u64>) -> Option<&Node<'a>> {
        let sid = sid?;
        self.fields().iter().find(|f| f.sid == Some(sid))
    }

    pub fn as_str(&self) -> Option<&str> {
        match &self.unwrap().kind {
            Kind::String(s) => Some(s),
            _ => None,
        }
    }

    /// Integer value; floats truncate toward zero (Python `int(float)`).
    pub fn as_int(&self) -> Option<i128> {
        match self.unwrap().kind {
            Kind::Int(v) => v,
            Kind::Float(Some(f)) if f.is_finite() => Some(f.trunc() as i128),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self.unwrap().kind {
            Kind::Float(v) => v,
            Kind::Int(Some(v)) => Some(v as f64),
            _ => None,
        }
    }

    pub fn as_blob(&self) -> Option<&'a [u8]> {
        match self.unwrap().kind {
            Kind::Blob(b) => Some(b),
            _ => None,
        }
    }

    pub fn as_symbol(&self) -> Option<u64> {
        match self.unwrap().kind {
            Kind::Symbol(s) => Some(s),
            _ => None,
        }
    }

    /// Pre-order visit of this node and all descendants.
    pub fn visit(&self, f: &mut impl FnMut(&Node<'a>)) {
        f(self);
        for c in &self.children {
            c.visit(f);
        }
    }
}

struct Bad;

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    fn read(&mut self, n: usize) -> Result<&'a [u8], Bad> {
        let end = self.pos.checked_add(n).ok_or(Bad)?;
        if end > self.data.len() {
            return Err(Bad);
        }
        let s = &self.data[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8, Bad> {
        Ok(self.read(1)?[0])
    }

    fn vluint(&mut self) -> Result<u64, Bad> {
        let mut v: u64 = 0;
        loop {
            let b = self.u8()?;
            v = v.checked_mul(128).ok_or(Bad)? | (b & 0x7F) as u64;
            if b & 0x80 != 0 {
                return Ok(v);
            }
        }
    }

    fn len_bytes(&mut self, n: u64) -> Result<usize, Bad> {
        usize::try_from(n).map_err(|_| Bad)
    }
}

fn be_uint(b: &[u8]) -> Option<u128> {
    if b.len() > 16 {
        return None;
    }
    Some(b.iter().fold(0u128, |v, x| (v << 8) | *x as u128))
}

const MAX_DEPTH: usize = 200;

fn walk_value<'a>(cur: &mut Cursor<'a>, depth: usize) -> Result<Node<'a>, Bad> {
    if cur.remaining() == 0 || depth > MAX_DEPTH {
        return Err(Bad);
    }
    let start = cur.pos;
    let desc = cur.u8()?;
    if desc == 0xE0 && cur.remaining() >= 3 {
        cur.read(3)?;
        return walk_value(cur, depth);
    }
    let t = desc >> 4;
    let flag = desc & 0x0F;
    let leaf = |kind| Node {
        offset: start,
        sid: None,
        kind,
        children: Vec::new(),
    };
    if t == 1 {
        return Ok(leaf(Kind::Bool(match flag {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        })));
    }
    if t == 0 && flag == 15 {
        return Ok(leaf(Kind::Null));
    }
    let len = match flag {
        14 => {
            let v = cur.vluint()?;
            cur.len_bytes(v)?
        }
        15 => return Ok(leaf(if t == 0 { Kind::Null } else { Kind::Other })),
        n => n as usize,
    };
    match t {
        2 | 3 => {
            let raw = cur.read(len)?;
            let v = be_uint(raw).and_then(|u| i128::try_from(u).ok());
            Ok(leaf(Kind::Int(if t == 3 { v.map(|x| -x) } else { v })))
        }
        4 => {
            let raw = cur.read(len)?;
            Ok(leaf(Kind::Float(match len {
                8 => Some(f64::from_be_bytes(raw.try_into().unwrap())),
                4 => Some(f32::from_be_bytes(raw.try_into().unwrap()) as f64),
                _ => None,
            })))
        }
        7 => {
            let raw = cur.read(len)?;
            let sid = be_uint(raw)
                .and_then(|u| u64::try_from(u).ok())
                .ok_or(Bad)?;
            Ok(leaf(Kind::Symbol(sid)))
        }
        8 => {
            let raw = cur.read(len)?;
            Ok(leaf(Kind::String(
                String::from_utf8_lossy(raw).into_owned(),
            )))
        }
        10 => Ok(leaf(Kind::Blob(cur.read(len)?))),
        11..=13 => {
            // Python parity: children share the cursor over the whole buffer (a child may run
            // past `end`); the first malformed child ends the container; then pos = end, which
            // may lie beyond the buffer (the caller then stops).
            let end = cur.pos.checked_add(len).ok_or(Bad)?;
            let is_struct = t == 13;
            let mut children = Vec::new();
            while cur.pos < end && cur.remaining() > 0 {
                if cur.data[cur.pos] == 0x00 {
                    cur.pos += 1;
                    continue;
                }
                let mut field_sid = None;
                if is_struct {
                    match cur.vluint() {
                        Ok(s) => field_sid = Some(s),
                        Err(_) => break,
                    }
                }
                match walk_value(cur, depth + 1) {
                    Ok(mut child) => {
                        child.sid = field_sid;
                        children.push(child);
                    }
                    Err(_) => break,
                }
            }
            cur.pos = end;
            Ok(Node {
                offset: start,
                sid: None,
                kind: if is_struct { Kind::Struct } else { Kind::List },
                children,
            })
        }
        14 => {
            let end = cur.pos.checked_add(len).ok_or(Bad)?;
            let parsed = (|| -> Result<(Vec<u64>, Node<'a>), Bad> {
                let alen = cur.vluint()?;
                let aend = cur.pos.checked_add(cur.len_bytes(alen)?).ok_or(Bad)?;
                let mut anns = Vec::new();
                while cur.pos < aend {
                    anns.push(cur.vluint()?);
                }
                let child = walk_value(cur, depth + 1)?;
                Ok((anns, child))
            })();
            cur.pos = end;
            Ok(match parsed {
                Ok((anns, child)) => Node {
                    offset: start,
                    sid: None,
                    kind: Kind::Annotation(anns),
                    children: vec![child],
                },
                Err(_) => leaf(Kind::Other),
            })
        }
        _ => {
            cur.read(len)?;
            Ok(leaf(Kind::Other))
        }
    }
}

/// Walk every top-level value of an Ion document (a leading IVM is skipped).
pub fn walk_document(data: &[u8]) -> Vec<Node<'_>> {
    let mut cur = Cursor { data, pos: 0 };
    if data.starts_with(&IVM) {
        cur.pos = 4;
    }
    let mut roots = Vec::new();
    while cur.remaining() > 0 {
        let b = cur.data[cur.pos];
        if b == 0x00 {
            cur.pos += 1;
            continue;
        }
        if b == 0xE0 && cur.remaining() >= 4 {
            cur.pos += 4;
            continue;
        }
        match walk_value(&mut cur, 0) {
            Ok(n) => roots.push(n),
            Err(_) => break,
        }
    }
    roots
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walks_struct_with_annotation_list_blob_and_kindle_lengths() {
        // $ion IVM, annotation($260) { 10: "abc", 11: [1, -2], 12: blob(2) }
        let doc = [
            0xE0,
            0x01,
            0x00,
            0xEA, // IVM
            0xE0 | 0x0E,
            0x94, // annotation, length 20 (VarUInt with terminator bit)
            0x82,
            0x02,
            0x84, // annot_len 2, annotation SID 260 = 0x02 0x04 → "02 84"
            0xDE,
            0x8F, // struct, length 15
            0x8A,
            0x83,
            b'a',
            b'b',
            b'c', // field 10: string "abc"
            0x8B,
            0xB4,
            0x21,
            0x01,
            0x31,
            0x02, // field 11: list [1, -2]
            0x8C,
            0xA2,
            0x01,
            0x02, // field 12: blob [1,2]
        ];
        let roots = walk_document(&doc);
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].kind, Kind::Annotation(vec![260]));
        let s = roots[0].unwrap();
        assert!(s.is_struct());
        assert_eq!(s.field(Some(10)).unwrap().as_str(), Some("abc"));
        let l = s.field(Some(11)).unwrap();
        let ints: Vec<_> = l.children.iter().map(|c| c.as_int().unwrap()).collect();
        assert_eq!(ints, [1, -2]);
        assert_eq!(s.field(Some(12)).unwrap().as_blob(), Some(&[1u8, 2][..]));
    }

    #[test]
    fn malformed_values_end_the_document_without_panicking() {
        assert!(walk_document(&[0x2E]).is_empty());
        // Truncated struct: Python keeps the (empty) struct, then the document ends.
        let r = walk_document(&[0xD8, 0x81]);
        assert_eq!(r.len(), 1);
        assert!(r[0].is_struct() && r[0].children.is_empty());
        let _ = walk_document(&[0xEE, 0xFF, 0xFF, 0xFF]);
    }
}
