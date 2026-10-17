//! The `nbk` container: a KDF SQLite image with periodic fingerprint records, whose effective
//! fragment set is `fragments` overlaid by `local_delta_fragments` (a deleted delta row
//! tombstones the id). Port of the proven Python `nbk_store` / `fragment_store` rules.

use std::collections::BTreeMap;

use crate::sqlite::{Db, TableInfo, Value};
use crate::NbkError;

const FINGERPRINT_OFFSET: usize = 1024;
const FINGERPRINT_RECORD_LEN: usize = 1024;
const DATA_RECORD_LEN: usize = 1024;
const DATA_RECORD_COUNT: usize = 1024;
const FINGERPRINT_SIGNATURE: [u8; 4] = [0xFA, 0x50, 0x0A, 0x5F];

/// Remove the periodic 1 KiB fingerprint records (KDF). Returns (clean image, records removed).
pub fn unwrap_fingerprints(data: &[u8]) -> (Vec<u8>, usize) {
    if data.len() < FINGERPRINT_OFFSET + FINGERPRINT_RECORD_LEN
        || data[FINGERPRINT_OFFSET..FINGERPRINT_OFFSET + 4] != FINGERPRINT_SIGNATURE
    {
        return (data.to_vec(), 0);
    }
    let mut buf = data.to_vec();
    let mut count = 0;
    let mut offset = FINGERPRINT_OFFSET;
    while buf.len() >= offset + FINGERPRINT_RECORD_LEN {
        if buf[offset..offset + 4] != FINGERPRINT_SIGNATURE {
            break;
        }
        buf.drain(offset..offset + FINGERPRINT_RECORD_LEN);
        count += 1;
        offset += DATA_RECORD_LEN * DATA_RECORD_COUNT;
    }
    (buf, count)
}

/// Effective (live) fragments by id, plus counts for diagnostics.
#[derive(Debug, Default)]
pub struct FragmentStore {
    pub fragments: BTreeMap<String, Vec<u8>>,
    pub fingerprints_removed: usize,
    pub base_count: usize,
    pub delta_count: usize,
    pub delta_deleted_count: usize,
}

impl FragmentStore {
    pub fn get(&self, id: &str) -> Option<&[u8]> {
        self.fragments.get(id).map(Vec::as_slice)
    }
}

fn require_columns(t: &TableInfo, want: &[&str]) -> Result<(), NbkError> {
    let cols = t.columns();
    if cols.len() < want.len() || cols.iter().zip(want).any(|(c, w)| c != w) {
        return Err(NbkError::Container(format!(
            "unexpected {} columns {cols:?} (expected {want:?})",
            t.name
        )));
    }
    Ok(())
}

/// Build the effective fragment store from raw `nbk` bytes.
pub fn read_fragments(nbk: &[u8]) -> Result<FragmentStore, NbkError> {
    let (clean, fingerprints_removed) = unwrap_fingerprints(nbk);
    let db = Db::open(&clean)?;
    let tables = db.tables()?;
    let find = |name: &str| tables.iter().find(|t| t.name == name);
    let base = find("fragments").ok_or_else(|| NbkError::Container("no fragments table".into()))?;
    require_columns(base, &["id", "payload_type", "payload_value"])?;

    let mut store = FragmentStore {
        fingerprints_removed,
        ..Default::default()
    };
    for row in db.scan(base.root_page)? {
        let mut row = row.into_iter();
        let (Some(id), Some(_ptype), Some(payload)) = (row.next(), row.next(), row.next()) else {
            return Err(NbkError::Container("short fragments row".into()));
        };
        let id = value_text(id)?;
        let payload = payload
            .into_blob()
            .ok_or_else(|| NbkError::Container(format!("fragment {id}: payload is not a blob")))?;
        store.fragments.insert(id, payload);
    }
    store.base_count = store.fragments.len();

    if let Some(delta) = find("local_delta_fragments") {
        require_columns(delta, &["id", "payload_type", "payload_value", "deleted"])?;
        for row in db.scan(delta.root_page)? {
            let mut row = row.into_iter();
            let (Some(id), Some(_ptype), Some(payload), Some(deleted)) =
                (row.next(), row.next(), row.next(), row.next())
            else {
                return Err(NbkError::Container(
                    "short local_delta_fragments row".into(),
                ));
            };
            let id = value_text(id)?;
            store.delta_count += 1;
            let is_deleted = match deleted {
                Value::Null => false,
                Value::Int(v) => v != 0,
                other => {
                    return Err(NbkError::Container(format!(
                        "delta {id}: deleted={other:?}"
                    )))
                }
            };
            if is_deleted {
                store.delta_deleted_count += 1;
                store.fragments.remove(&id);
                continue;
            }
            let payload = payload
                .into_blob()
                .ok_or_else(|| NbkError::Container(format!("delta {id}: payload is not a blob")))?;
            store.fragments.insert(id, payload);
        }
    }
    Ok(store)
}

fn value_text(v: Value) -> Result<String, NbkError> {
    match v {
        Value::Text(s) => Ok(s),
        Value::Int(i) => Ok(i.to_string()),
        other => Err(NbkError::Container(format!("fragment id {other:?}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_records_are_removed_periodically() {
        let mut data = vec![7u8; FINGERPRINT_OFFSET];
        data.extend_from_slice(&FINGERPRINT_SIGNATURE);
        data.extend(vec![0u8; FINGERPRINT_RECORD_LEN - 4]);
        data.extend(vec![9u8; 10]);
        let (clean, n) = unwrap_fingerprints(&data);
        assert_eq!(n, 1);
        assert_eq!(clean.len(), FINGERPRINT_OFFSET + 10);
        assert!(clean[FINGERPRINT_OFFSET..].iter().all(|b| *b == 9));
        let (same, n) = unwrap_fingerprints(b"no fingerprints");
        assert_eq!((same.as_slice(), n), (&b"no fingerprints"[..], 0));
    }

    #[test]
    fn schema_columns_are_parsed() {
        let t = TableInfo {
            name: "local_delta_fragments".into(),
            root_page: 6,
            sql: "CREATE TABLE local_delta_fragments(id char(40), payload_type char(10), payload_value blob, deleted smallint, primary key (id))".into(),
        };
        assert_eq!(
            t.columns(),
            ["id", "payload_type", "payload_value", "deleted"]
        );
    }
}
