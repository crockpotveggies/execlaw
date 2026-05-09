//! Input-guard utilities (§7.4, §7.8).
//!
//! Deterministic, cheap pre-filter. Full pattern-match list lands in Phase 2
//! (ported from selfhosted-claw's `inbound-guard.ts`). This module ships
//! the zero-width/bidi strip and a bounded homoglyph folder so downstream
//! code can compose.

/// Strip zero-width and bidi-control characters that are a common vector
/// for hiding "ignore previous instructions"-style payloads.
pub fn strip_invisible(input: &str) -> String {
    input.chars().filter(|c| !is_invisible_bidi(*c)).collect()
}

fn is_invisible_bidi(c: char) -> bool {
    matches!(
        c as u32,
        // Zero-width
        0x200B | 0x200C | 0x200D | 0xFEFF
        // Bidi controls
        | 0x202A..=0x202E
        | 0x2066..=0x2069
        // Invisible maths
        | 0x2061..=0x2064
    )
}

/// Minimal homoglyph folder — Cyrillic `а/е/о/р/с/у/х` → Latin equivalents.
/// Not a full normalization; a sharper pass lands with the Phase 2 port.
pub fn fold_common_homoglyphs(input: &str) -> String {
    input
        .chars()
        .map(|c| match c {
            'а' => 'a',
            'е' => 'e',
            'о' => 'o',
            'р' => 'p',
            'с' => 'c',
            'у' => 'y',
            'х' => 'x',
            'А' => 'A',
            'Е' => 'E',
            'О' => 'O',
            'Р' => 'P',
            'С' => 'C',
            'У' => 'Y',
            'Х' => 'X',
            _ => c,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_zero_width_joiner() {
        let dirty = "ignore\u{200B}previous\u{200C}instructions";
        assert_eq!(strip_invisible(dirty), "ignorepreviousinstructions");
    }

    #[test]
    fn strips_bidi_control() {
        let dirty = "safe\u{202E}evil";
        assert_eq!(strip_invisible(dirty), "safeevil");
    }

    #[test]
    fn folds_cyrillic_lookalikes() {
        // "раypal" uses Cyrillic 'а' and 'р'.
        let s = "раypal.com";
        assert_eq!(fold_common_homoglyphs(s), "paypal.com");
    }

    #[test]
    fn preserves_unrelated_characters() {
        let s = "hello, world";
        assert_eq!(fold_common_homoglyphs(s), s);
        assert_eq!(strip_invisible(s), s);
    }

    /// RLO (Right-to-Left Override, U+202E) is the canonical bidi
    /// attack vector — `doc\u{202E}cod.exe` rendered as `doc exe.cod`.
    /// Explicit test so a narrower strip can't silently regress.
    #[test]
    fn strips_rlo_override_specifically() {
        let dirty = "doc\u{202E}cod.exe";
        assert_eq!(strip_invisible(dirty), "doccod.exe");
    }

    /// Byte-order-mark (U+FEFF) is another common smuggle point.
    #[test]
    fn strips_byte_order_mark() {
        assert_eq!(strip_invisible("\u{FEFF}hello"), "hello");
    }

    /// Invisible-math characters (U+2061..U+2064) must also be stripped.
    #[test]
    fn strips_invisible_math_operators() {
        let dirty = "a\u{2061}b\u{2064}c";
        assert_eq!(strip_invisible(dirty), "abc");
    }

    /// Capital-form Cyrillic homoglyphs are folded too.
    #[test]
    fn folds_capital_cyrillic_lookalikes() {
        // "РАУPAL" uses Cyrillic Р, А, У before a Latin PAL.
        assert_eq!(fold_common_homoglyphs("РАУPAL"), "PAYPAL");
    }

    /// A string mixing both zero-width joiners AND homoglyphs needs
    /// BOTH passes applied to produce a clean result — ensure callers
    /// can chain them cleanly without interference.
    #[test]
    fn strip_then_fold_composes() {
        let dirty = "р\u{200B}аypal";
        let cleaned = fold_common_homoglyphs(&strip_invisible(dirty));
        assert_eq!(cleaned, "paypal");
    }
}
