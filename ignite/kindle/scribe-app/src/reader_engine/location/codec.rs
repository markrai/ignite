use serde::{Deserialize, Serialize};

use super::{
    validate_anchor, ContentLocation, DocumentIdentity, SavedPosition, TextAnchor,
    SAVED_POSITION_FORMAT,
};

pub const MAX_SAVED_POSITION_BYTES: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PositionCodecError {
    TooLarge,
    Malformed,
    UnsupportedFormat(u16),
    InvalidDocument,
    InvalidAnchor,
}

impl std::fmt::Display for PositionCodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge => write!(f, "reading position exceeds size limit"),
            Self::Malformed => write!(f, "malformed reading position"),
            Self::UnsupportedFormat(format) => {
                write!(f, "unsupported reading-position format {format}")
            }
            Self::InvalidDocument => write!(f, "invalid document identity"),
            Self::InvalidAnchor => write!(f, "invalid reading-position anchor"),
        }
    }
}

impl std::error::Error for PositionCodecError {}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRecord {
    format: u16,
    document: WireDocument,
    location: WireLocation,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireDocument {
    sha256: String,
    len: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum WireLocation {
    FixedPage {
        page: u32,
    },
    Text {
        chapter: u32,
        offset: u32,
        text_model: String,
        context: String,
    },
}

#[derive(Deserialize)]
struct WireVersion {
    format: u16,
}

fn hex_sha256(hash: &[u8; 32]) -> String {
    const DIGITS: &[u8] = b"0123456789abcdef";
    let mut result = String::with_capacity(64);
    for &byte in hash {
        result.push(char::from(DIGITS[usize::from(byte >> 4)]));
        result.push(char::from(DIGITS[usize::from(byte & 15)]));
    }
    result
}

fn parse_sha256(value: &str) -> Result<[u8; 32], PositionCodecError> {
    if value.len() != 64 {
        return Err(PositionCodecError::InvalidDocument);
    }
    let mut hash = [0u8; 32];
    for (out, pair) in hash.iter_mut().zip(value.as_bytes().chunks_exact(2)) {
        let decode = |byte| match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            _ => None,
        };
        let high = decode(pair[0]).ok_or(PositionCodecError::InvalidDocument)?;
        let low = decode(pair[1]).ok_or(PositionCodecError::InvalidDocument)?;
        *out = (high << 4) | low;
    }
    Ok(hash)
}

fn validate(position: &SavedPosition) -> Result<(), PositionCodecError> {
    if position.format != SAVED_POSITION_FORMAT {
        return Err(PositionCodecError::UnsupportedFormat(position.format));
    }
    if position.document.len == 0 {
        return Err(PositionCodecError::InvalidDocument);
    }
    if let ContentLocation::Text(anchor) = &position.location {
        if validate_anchor(anchor).is_err() || anchor.context.chars().any(char::is_control) {
            return Err(PositionCodecError::InvalidAnchor);
        }
    }
    Ok(())
}

impl TryFrom<WireRecord> for SavedPosition {
    type Error = PositionCodecError;

    fn try_from(record: WireRecord) -> Result<Self, Self::Error> {
        let location = match record.location {
            WireLocation::FixedPage { page } => ContentLocation::FixedPage(page),
            WireLocation::Text {
                chapter,
                offset,
                text_model,
                context,
            } => ContentLocation::Text(TextAnchor {
                chapter,
                offset,
                text_model,
                context,
            }),
        };
        let result = SavedPosition {
            format: record.format,
            document: DocumentIdentity {
                sha256: parse_sha256(&record.document.sha256)?,
                len: record.document.len,
            },
            location,
        };
        validate(&result)?;
        Ok(result)
    }
}

impl From<&SavedPosition> for WireRecord {
    fn from(record: &SavedPosition) -> Self {
        let location = match &record.location {
            ContentLocation::FixedPage(page) => WireLocation::FixedPage { page: *page },
            ContentLocation::Text(anchor) => WireLocation::Text {
                chapter: anchor.chapter,
                offset: anchor.offset,
                text_model: anchor.text_model.clone(),
                context: anchor.context.clone(),
            },
        };
        Self {
            format: record.format,
            document: WireDocument {
                sha256: hex_sha256(&record.document.sha256),
                len: record.document.len,
            },
            location,
        }
    }
}

impl SavedPosition {
    pub fn to_json_bytes(&self) -> Result<Vec<u8>, PositionCodecError> {
        validate(self)?;
        let result = serde_json::to_vec(&WireRecord::from(self))
            .map_err(|_| PositionCodecError::Malformed)?;
        if result.len() > MAX_SAVED_POSITION_BYTES {
            return Err(PositionCodecError::TooLarge);
        }
        Ok(result)
    }

    pub fn from_json_bytes(bytes: &[u8]) -> Result<Self, PositionCodecError> {
        if bytes.len() > MAX_SAVED_POSITION_BYTES {
            return Err(PositionCodecError::TooLarge);
        }
        let version: WireVersion =
            serde_json::from_slice(bytes).map_err(|_| PositionCodecError::Malformed)?;
        if version.format != SAVED_POSITION_FORMAT {
            return Err(PositionCodecError::UnsupportedFormat(version.format));
        }
        let record: WireRecord =
            serde_json::from_slice(bytes).map_err(|_| PositionCodecError::Malformed)?;
        record.try_into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader_engine::location::{CONTEXT_UNITS, TEXT_MODEL};

    fn document() -> DocumentIdentity {
        DocumentIdentity {
            sha256: [0x5a; 32],
            len: 8765,
        }
    }

    fn fixed() -> SavedPosition {
        SavedPosition::new(document(), ContentLocation::FixedPage(42))
    }

    fn text() -> SavedPosition {
        SavedPosition::new(
            document(),
            ContentLocation::Text(TextAnchor {
                text_model: TEXT_MODEL.to_owned(),
                chapter: 3,
                offset: 170,
                context: "Café世界🎨".to_owned(),
            }),
        )
    }

    fn json(value: &SavedPosition) -> String {
        String::from_utf8(value.to_json_bytes().unwrap()).unwrap()
    }

    fn decode(value: &str) -> Result<SavedPosition, PositionCodecError> {
        SavedPosition::from_json_bytes(value.as_bytes())
    }

    #[test]
    fn fixed_layout_record_is_stable_and_round_trips() {
        let value = fixed();
        let expected = format!(
            r#"{{"format":1,"document":{{"sha256":"{}","len":8765}},"location":{{"kind":"fixed_page","page":42}}}}"#,
            "5a".repeat(32)
        );
        assert_eq!(json(&value), expected);
        assert_eq!(decode(&expected), Ok(value));
    }

    #[test]
    fn epub_record_round_trips_with_owned_unicode_context() {
        let value = text();
        let raw = json(&value);
        assert!(raw.contains(r#""kind":"text""#));
        assert!(raw.contains(r#""context":"Café世界🎨""#));
        assert_eq!(decode(&raw), Ok(value));
    }

    #[test]
    fn restored_records_are_independent_of_field_order_and_whitespace() {
        let value = fixed();
        let raw = format!(
            r#"{{ "location": {{ "page": 42, "kind": "fixed_page" }}, "document": {{ "len": 8765, "sha256": "{}" }}, "format": 1 }}"#,
            "5a".repeat(32)
        );
        assert_eq!(decode(&raw), Ok(value));
        assert_eq!(json(&decode(&raw).unwrap()), json(&value));
    }

    #[test]
    fn rejects_empty_truncated_invalid_utf8_and_trailing_bytes() {
        let raw = json(&fixed());
        for bytes in [
            Vec::new(),
            raw.as_bytes()[..raw.len() - 1].to_vec(),
            vec![0xff, 0xfe],
            format!("{raw}{{}}").into_bytes(),
            format!("{raw} trailing").into_bytes(),
        ] {
            assert_eq!(
                SavedPosition::from_json_bytes(&bytes),
                Err(PositionCodecError::Malformed)
            );
        }
    }

    #[test]
    fn rejects_oversized_input_before_parsing() {
        let raw = vec![b' '; MAX_SAVED_POSITION_BYTES + 1];
        assert_eq!(
            SavedPosition::from_json_bytes(&raw),
            Err(PositionCodecError::TooLarge)
        );
    }

    #[test]
    fn rejects_unknown_format_even_with_future_location_shape() {
        let raw = r#"{"format":2,"document":{"sha256":"irrelevant"},"location":{"kind":"future"}}"#;
        assert_eq!(decode(raw), Err(PositionCodecError::UnsupportedFormat(2)));
        let mut value = fixed();
        value.format = 2;
        assert_eq!(
            value.to_json_bytes(),
            Err(PositionCodecError::UnsupportedFormat(2))
        );
    }

    #[test]
    fn rejects_unknown_fields_and_duplicate_keys() {
        let raw = json(&fixed());
        let unknown = raw.replace(r#""format":1"#, r#""format":1,"unexpected":true"#);
        let duplicate = raw.replace(r#""format":1"#, r#""format":1,"format":1"#);
        let unknown_nested = raw.replace(r#""page":42"#, r#""page":42,"unexpected":"field""#);
        let duplicate_nested = raw.replace(r#""page":42"#, r#""page":42,"page":43"#);
        let missing = raw.replace(r#","len":8765"#, "");
        for candidate in [
            unknown,
            duplicate,
            unknown_nested,
            duplicate_nested,
            missing,
        ] {
            assert_eq!(decode(&candidate), Err(PositionCodecError::Malformed));
        }
    }

    #[test]
    fn rejects_wrong_field_types_and_location_variants() {
        let raw = json(&fixed());
        for candidate in [
            raw.replace(r#""page":42"#, r#""page":-1"#),
            raw.replace(r#""page":42"#, r#""page":3.5"#),
            raw.replace(r#""page":42"#, r#""page":"42""#),
            raw.replace(r#""page":42"#, r#""page":4294967296"#),
            raw.replace(r#""kind":"fixed_page""#, r#""kind":"future""#),
            raw.replace(r#""format":1"#, r#""format":"1""#),
            raw.replace(r#""len":8765"#, r#""len":-1"#),
        ] {
            assert_eq!(decode(&candidate), Err(PositionCodecError::Malformed));
        }
    }

    #[test]
    fn requires_lowercase_sha256_and_valid_length() {
        let raw = json(&fixed());
        for hash in [
            "5A".repeat(32),
            "5a".repeat(31),
            "5a".repeat(33),
            format!("{}zz", "5a".repeat(31)),
        ] {
            let candidate = raw.replace(&"5a".repeat(32), &hash);
            assert_eq!(decode(&candidate), Err(PositionCodecError::InvalidDocument));
        }
        let zero_len = raw.replace(r#""len":8765"#, r#""len":0"#);
        assert_eq!(decode(&zero_len), Err(PositionCodecError::InvalidDocument));
    }

    #[test]
    fn refuses_invalid_text_anchors_before_use() {
        let raw = json(&text());
        let invalids = [
            raw.replace(TEXT_MODEL, ""),
            raw.replace(TEXT_MODEL, &"m".repeat(65)),
            raw.replace(r#""context":"Café世界🎨""#, r#""context":"has space""#),
            raw.replace(r#""context":"Café世界🎨""#, r#""context":"\u0007""#),
            raw.replace(
                r#""context":"Café世界🎨""#,
                &format!(r#""context":"{}""#, "x".repeat(CONTEXT_UNITS + 1)),
            ),
        ];
        for candidate in invalids {
            assert_eq!(decode(&candidate), Err(PositionCodecError::InvalidAnchor));
        }
    }

    #[test]
    fn refuses_invalid_positions_when_encoding_too() {
        let mut value = text();
        if let ContentLocation::Text(ref mut anchor) = value.location {
            anchor.context = "a b".to_owned();
        }
        assert_eq!(
            value.to_json_bytes(),
            Err(PositionCodecError::InvalidAnchor)
        );
    }

    #[test]
    fn accepts_fixed_page_boundaries_without_interpreting_document_pages() {
        let mut value = fixed();
        value.location = ContentLocation::FixedPage(u32::MAX);
        assert_eq!(
            SavedPosition::from_json_bytes(&value.to_json_bytes().unwrap()),
            Ok(value)
        );
    }
}
