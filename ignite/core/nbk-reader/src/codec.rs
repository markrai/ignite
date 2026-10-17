//! `nmdl.stroke_points` channel codec (confirmed; port of Python `decode_stroke_values`):
//!
//! ```text
//! 01 01 <u32le count> <nibble opcodes, two per byte> <extra increment bytes…>
//! ```
//! opcode: `n = op & 3`; `op & 4` → inline increment `n`, else read 0/1/2/3 extra bytes
//! (u8 / u16le / u24le); `op & 8` → negate. Values are second-order deltas: the first value is
//! absolute, then `change += increment; value += change`.

pub fn decode_channel(data: &[u8], expected: Option<usize>) -> Result<Vec<i64>, String> {
    if data.len() < 6 || data[..2] != [1, 1] {
        return Err("bad stroke channel signature".into());
    }
    let count = u32::from_le_bytes([data[2], data[3], data[4], data[5]]) as usize;
    if let Some(e) = expected {
        if count != e {
            return Err(format!("count mismatch: blob={count} expected={e}"));
        }
    }
    // Every value needs at least half an opcode byte: bound the allocation by the input.
    if count > (data.len() - 6) * 2 {
        return Err("truncated while reading stroke opcodes".into());
    }
    let mut pos = 6;
    let mut ops = Vec::with_capacity(count + 1);
    while ops.len() < count {
        let b = *data
            .get(pos)
            .ok_or("truncated while reading stroke opcodes")?;
        pos += 1;
        ops.push(b >> 4);
        ops.push(b & 0x0F);
    }
    if ops.len() > count {
        if ops[count] != 0 {
            return Err(format!("nonzero opcode padding: {}", ops[count]));
        }
        ops.truncate(count);
    }
    let mut vals = Vec::with_capacity(count);
    let (mut change, mut value) = (0i64, 0i64);
    for (i, op) in ops.into_iter().enumerate() {
        let n = (op & 3) as usize;
        let mut inc: i64 = if op & 4 != 0 {
            n as i64
        } else {
            let bytes = data.get(pos..pos + n).ok_or("truncated increment")?;
            pos += n;
            bytes.iter().rev().fold(0i64, |v, b| (v << 8) | *b as i64)
        };
        if op & 8 != 0 {
            if inc == 0 {
                return Err("negative zero increment".into());
            }
            inc = -inc;
        }
        if i == 0 {
            change = 0;
            value = inc;
        } else {
            change = change.checked_add(inc).ok_or("channel overflow")?;
            value = value.checked_add(change).ok_or("channel overflow")?;
        }
        vals.push(value);
    }
    if pos != data.len() {
        return Err(format!("trailing channel bytes: {}", data.len() - pos));
    }
    Ok(vals)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_order_deltas_with_inline_and_extra_bytes() {
        // 3 values: op0 = 0x1 (u8 extra: 10) → 10; op1 = 0x5 (inline +1) → change 1 → 11;
        // op2 = 0x4 (inline 0) → change 1 → 12. Opcodes packed 0x15, 0x40 (pad 0).
        let blob = [1, 1, 3, 0, 0, 0, 0x15, 0x40, 10];
        assert_eq!(decode_channel(&blob, Some(3)).unwrap(), vec![10, 11, 12]);
        // Negative u16: op 0xA (negate, 2 bytes) = -(0x0102).
        let blob = [1, 1, 1, 0, 0, 0, 0xA0, 0x02, 0x01];
        assert_eq!(decode_channel(&blob, None).unwrap(), vec![-258]);
    }

    #[test]
    fn rejects_malformed_channels() {
        assert!(decode_channel(&[0, 1, 1, 0, 0, 0, 0], None).is_err());
        assert!(decode_channel(&[1, 1, 3, 0, 0, 0, 0x15, 0x40, 10], Some(4)).is_err());
        assert!(
            decode_channel(&[1, 1, 1, 0, 0, 0, 0x10], None).is_err(),
            "missing u8"
        );
        assert!(decode_channel(&[1, 1, 1, 0, 0, 0, 0x0F, 9], None).is_err());
        assert!(decode_channel(&[1, 1, 255, 255, 255, 255, 0], None).is_err());
    }
}
