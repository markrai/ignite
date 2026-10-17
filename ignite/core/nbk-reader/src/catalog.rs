//! The Kindle's notebook catalog (`/var/local/ksdk.content.db`, table `Nodes`) — read-only.
//!
//! FW 5.18.6 evidence: `Nodes.uuid` (primary key) of a `STANDALONE_NOTEBOOK` equals the
//! `/mnt/us/.notebooks/<uuid>/` directory name; `title_collation` is the user-visible title;
//! `additional_data` is JSON with `title`, `notebook_template`, `total_page`, `delete_state`.
//! Titles are display data only — never import identity.

use crate::sqlite::{Db, Value};
use crate::NbkError;

pub const STANDALONE_NOTEBOOK: &str = "STANDALONE_NOTEBOOK";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CatalogEntry {
    pub uuid: String,
    pub node_type: Option<String>,
    pub title: Option<String>,
    pub template: Option<String>,
    pub total_pages: Option<i64>,
    /// `delete_state != 0`: the Kindle considers the notebook deleted.
    pub deleted: bool,
}

/// Every `Nodes` row. Missing trailing columns (ALTER TABLE) read as NULL.
pub fn read_catalog(db_bytes: &[u8]) -> Result<Vec<CatalogEntry>, NbkError> {
    let db = Db::open(db_bytes)?;
    let tables = db.tables()?;
    let nodes = tables
        .iter()
        .find(|t| t.name == "Nodes")
        .ok_or_else(|| NbkError::Container("catalog has no Nodes table".into()))?;
    let cols = nodes.columns();
    let idx = |name: &str| cols.iter().position(|c| c == name);
    let (Some(i_uuid), Some(i_title), Some(i_type)) =
        (idx("uuid"), idx("title_collation"), idx("node_type"))
    else {
        return Err(NbkError::Container(format!(
            "unexpected Nodes columns {cols:?}"
        )));
    };
    let i_extra = idx("additional_data");
    let mut out = Vec::new();
    for row in db.scan(nodes.root_page)? {
        let text = |i: usize| match row.get(i) {
            Some(Value::Text(s)) => Some(s.clone()),
            _ => None,
        };
        let Some(uuid) = text(i_uuid) else { continue };
        let extra = i_extra.and_then(text).unwrap_or_default();
        out.push(CatalogEntry {
            uuid,
            node_type: text(i_type),
            title: text(i_title)
                .filter(|t| !t.trim().is_empty())
                .or_else(|| json_string(&extra, "title")),
            template: json_string(&extra, "notebook_template"),
            total_pages: json_number(&extra, "total_page"),
            deleted: json_number(&extra, "delete_state").is_some_and(|d| d != 0),
        });
    }
    Ok(out)
}

/// Value of a top-level `"key": "…"` string in a flat JSON object (escapes decoded).
fn json_string(json: &str, key: &str) -> Option<String> {
    let rest = value_after_key(json, key)?;
    let mut chars = rest.strip_prefix('"')?.chars();
    let mut out = String::new();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                'b' => out.push('\u{8}'),
                'f' => out.push('\u{c}'),
                'u' => {
                    let hex: String = chars.by_ref().take(4).collect();
                    out.push(
                        char::from_u32(u32::from_str_radix(&hex, 16).ok()?).unwrap_or('\u{FFFD}'),
                    );
                }
                other => out.push(other),
            },
            c => out.push(c),
        }
    }
    None
}

fn json_number(json: &str, key: &str) -> Option<i64> {
    let rest = value_after_key(json, key)?;
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '-'))
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn value_after_key<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\"");
    let at = json.find(&needle)? + needle.len();
    let rest = json[at..].trim_start().strip_prefix(':')?;
    Some(rest.trim_start())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_flat_json_fields() {
        let j = r#"{"title":"Café \"notes\"","notebook_template":"plain","total_page":27,"delete_state":0}"#;
        assert_eq!(json_string(j, "title").as_deref(), Some("Café \"notes\""));
        assert_eq!(
            json_string(j, "notebook_template").as_deref(),
            Some("plain")
        );
        assert_eq!(json_number(j, "total_page"), Some(27));
        assert_eq!(json_number(j, "delete_state"), Some(0));
        assert_eq!(json_string(j, "missing"), None);
    }
}
