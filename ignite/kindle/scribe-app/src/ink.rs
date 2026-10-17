//! Strict pigment byte parsing for the live notebook runtime.
//!
//! Durable / compositor ink is canonical `00` black / `ff` white. Hardware polarity
//! for `grayscale=1` is applied only at the mmap copy boundary (`kindle_platform::fb_polarity`).
//! Only `00` / `ff` (optionally `0x`-prefixed) are accepted.
//! Omitted vs explicit is preserved so existing pages can ignore today's default.

/// Default ink when creating a **new** page with `--ink-byte` omitted.
pub const DEFAULT_INK_BYTE: u8 = 0x00;

/// CLI ink intent: omitted vs explicitly requested.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InkRequest {
    /// `--ink-byte` was not present.
    Default,
    /// Caller passed an explicit 00/ff value.
    Explicit(u8),
}

impl InkRequest {
    #[allow(dead_code)]
    pub fn is_explicit(self) -> bool {
        matches!(self, InkRequest::Explicit(_))
    }
}

/// Parse an explicit `--ink-byte` value. Rejects anything other than 00/ff.
pub fn parse_ink_byte(raw: &str) -> Result<u8, String> {
    let s = raw.trim();
    let hex = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    match hex.to_ascii_lowercase().as_str() {
        "00" => Ok(0x00),
        "ff" => Ok(0xff),
        _ => Err(format!(
            "REFUSE LIVE SESSION: --ink-byte must be 00 or ff (got {raw:?})"
        )),
    }
}

/// Parse CLI ink **intent** without collapsing omitted → 00.
pub fn parse_ink_request(args: &[String]) -> Result<InkRequest, String> {
    let Some(raw) = args
        .windows(2)
        .find(|w| w[0] == "--ink-byte")
        .map(|w| w[1].as_str())
    else {
        return Ok(InkRequest::Default);
    };
    Ok(InkRequest::Explicit(parse_ink_byte(raw)?))
}

/// Ink byte used when creating a new page.
pub fn ink_for_new_page(request: InkRequest) -> u8 {
    match request {
        InkRequest::Default => DEFAULT_INK_BYTE,
        InkRequest::Explicit(b) => b,
    }
}

/// Resolve ink for an existing page against the persisted profile.
///
/// - Default → use persisted
/// - Explicit(X) → require X == persisted
pub fn resolve_existing_ink(request: InkRequest, persisted: u8) -> Result<u8, String> {
    match request {
        InkRequest::Default => Ok(persisted),
        InkRequest::Explicit(requested) => {
            if requested == persisted {
                Ok(persisted)
            } else {
                Err(format!(
                    "REFUSE PAGE OPEN:\n\
                     requested ink profile does not match persisted page render profile\n\
                     persisted={persisted:02x} requested={requested:02x}"
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_00_and_ff() {
        assert_eq!(parse_ink_byte("00").unwrap(), 0x00);
        assert_eq!(parse_ink_byte("ff").unwrap(), 0xff);
        assert_eq!(parse_ink_byte("0x00").unwrap(), 0x00);
        assert_eq!(parse_ink_byte("0XFF").unwrap(), 0xff);
    }

    #[test]
    fn rejects_invalid_explicit() {
        for bad in ["12", "255", "banana", "f0", "0x12", ""] {
            assert!(parse_ink_byte(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn omitted_is_default_intent() {
        assert_eq!(parse_ink_request(&[]).unwrap(), InkRequest::Default);
        assert_eq!(ink_for_new_page(InkRequest::Default), 0x00);
    }

    #[test]
    fn explicit_invalid_refuses_request() {
        let args = vec!["--ink-byte".into(), "12".into()];
        assert!(parse_ink_request(&args).is_err());
    }

    #[test]
    fn existing_default_uses_persisted() {
        assert_eq!(
            resolve_existing_ink(InkRequest::Default, 0xff).unwrap(),
            0xff
        );
    }

    #[test]
    fn existing_matching_explicit_ok() {
        assert_eq!(
            resolve_existing_ink(InkRequest::Explicit(0x00), 0x00).unwrap(),
            0x00
        );
    }

    #[test]
    fn existing_mismatched_explicit_refuses() {
        let e = resolve_existing_ink(InkRequest::Explicit(0xff), 0x00).unwrap_err();
        assert!(e.contains("REFUSE PAGE OPEN"));
        assert!(e.contains("persisted=00"));
        assert!(e.contains("requested=ff"));
    }
}
