//! Parsing Linux sysfs `capabilities/*` bitmaps, honestly.
//!
//! The kernel prints these with `"%lx"` per `unsigned long`, most-significant word first, with
//! leading zeros stripped from every word. The word width is the **kernel's** `BITS_PER_LONG`,
//! which the text does not record: `"0 1"` is bit 0 under a 32-bit kernel and also bit 0 under a
//! 64-bit one, while `"1 0 0"` is bit 64 under one and bit 128 under the other. The original
//! parser here assumed 32-bit words unconditionally, which silently misreads every bit above 31
//! on a 64-bit kernel: exactly the range `BTN_TOOL_PEN` (0x140) and `BTN_TOUCH` (0x14a) live in.
//!
//! So this parser evaluates both plausible widths and reports [`BitEvidence::Ambiguous`] when
//! they disagree, rather than picking one. `EVIOCGBIT` (see [`crate::evdev_ioctl`]) is the
//! reliable mechanism and is preferred wherever the device node can be opened; this exists as a
//! read-only fallback for tooling and for fixture trees.

/// What a sysfs bitmap can tell us about one capability bit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BitEvidence {
    /// Both 32-bit and 64-bit readings agree the bit is set.
    Present,
    /// Both readings agree the bit is clear.
    Absent,
    /// The readings disagree, so the text alone cannot say. Treated as "no evidence".
    Ambiguous,
}

impl BitEvidence {
    pub fn is_present(self) -> bool {
        self == BitEvidence::Present
    }

    pub fn is_ambiguous(self) -> bool {
        self == BitEvidence::Ambiguous
    }
}

/// Test one bit assuming `word_bits`-wide kernel longs, MSW printed first.
pub fn bit_with_word_size(mask: &str, bit: usize, word_bits: usize) -> bool {
    let words: Vec<u64> = mask
        .split_whitespace()
        .filter_map(|w| u64::from_str_radix(w, 16).ok())
        .collect();
    if words.is_empty() || word_bits == 0 {
        return false;
    }
    let word_from_end = bit / word_bits;
    if word_from_end >= words.len() {
        return false;
    }
    let word = words[words.len() - 1 - word_from_end];
    let shift = bit % word_bits;
    (word >> shift) & 1 == 1
}

/// Evidence for one bit, agreeing across both plausible kernel word widths or not at all.
pub fn bit_evidence(mask: &str, bit: usize) -> BitEvidence {
    let as32 = bit_with_word_size(mask, bit, 32);
    let as64 = bit_with_word_size(mask, bit, 64);
    match (as32, as64) {
        (true, true) => BitEvidence::Present,
        (false, false) => BitEvidence::Absent,
        _ => BitEvidence::Ambiguous,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn low_bits_agree_across_word_widths() {
        // ABS_X | ABS_Y in the single low word.
        assert_eq!(bit_evidence("3", 0), BitEvidence::Present);
        assert_eq!(bit_evidence("3", 1), BitEvidence::Present);
        assert_eq!(bit_evidence("3", 2), BitEvidence::Absent);
    }

    #[test]
    fn abs_pressure_in_the_low_word_agrees() {
        // bits 0,1,0x18 -> 0x1000003, one word either way.
        let m = "1000003";
        assert_eq!(bit_evidence(m, 0x00), BitEvidence::Present);
        assert_eq!(bit_evidence(m, 0x01), BitEvidence::Present);
        assert_eq!(bit_evidence(m, 0x18), BitEvidence::Present);
        assert_eq!(bit_evidence(m, 0x19), BitEvidence::Absent);
    }

    #[test]
    fn btn_tool_pen_is_ambiguous_from_text_alone() {
        // BTN_TOOL_PEN = 0x140 = 320. Under 32-bit words that is word 10, under 64-bit word 5.
        // A bitmap printed by a 64-bit kernel with only that bit set:
        let mut words64 = ["0"; 6];
        words64[0] = "1"; // MSW first: word 5 == 1
        let mask = words64.join(" ");
        assert!(bit_with_word_size(&mask, 0x140, 64));
        assert!(!bit_with_word_size(&mask, 0x140, 32));
        assert_eq!(bit_evidence(&mask, 0x140), BitEvidence::Ambiguous);
    }

    #[test]
    fn a_bit_absent_under_both_widths_is_reported_absent() {
        let mask = "0 0 0 0 0 0";
        assert_eq!(bit_evidence(mask, 0x140), BitEvidence::Absent);
        assert_eq!(bit_evidence(mask, 0x14a), BitEvidence::Absent);
    }

    #[test]
    fn beyond_the_printed_words_is_absent() {
        assert_eq!(bit_evidence("1", 0x2ff), BitEvidence::Absent);
    }

    #[test]
    fn empty_or_garbage_masks_are_absent_not_a_panic() {
        assert_eq!(bit_evidence("", 0), BitEvidence::Absent);
        assert_eq!(bit_evidence("zzz", 0), BitEvidence::Absent);
        assert!(!bit_with_word_size("3", 0, 0));
    }

    #[test]
    fn the_old_32_bit_only_assumption_is_shown_to_be_wrong() {
        // Same physical capability, printed by a 64-bit kernel: BTN_TOUCH (0x14a) set.
        // 0x14a = 330 -> 64-bit word 5, shift 10 -> 0x400.
        let mask = "400 0 0 0 0 0";
        assert!(bit_with_word_size(mask, 0x14a, 64));
        // The pre-existing parser assumed 32-bit words and would have missed it.
        assert!(!bit_with_word_size(mask, 0x14a, 32));
        assert_eq!(bit_evidence(mask, 0x14a), BitEvidence::Ambiguous);
    }
}
