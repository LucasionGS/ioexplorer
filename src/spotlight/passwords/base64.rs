//! Just enough base64 to unwrap what a vault returns.
//!
//! Hand-written rather than pulled in as a dependency: this is the standard
//! alphabet, decode only, on inputs of a few dozen bytes. A crate for it would
//! be a supply-chain edge for thirty lines that are fully covered by the tests
//! below — including the malformed cases, which matter more here than the happy
//! path. A decoder that quietly accepts garbage would hand the clipboard
//! something that is not the user's password.

/// The standard alphabet, `A–Z a–z 0–9 + /`.
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Decodes standard base64, tolerating surrounding and embedded whitespace.
///
/// Padding is accepted but not required: `=` only ever appears at the end, and
/// what it pads is already implied by how many characters precede it.
pub fn decode(input: &str) -> Result<Vec<u8>, String> {
    let symbols = input
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .take_while(|byte| *byte != b'=')
        .collect::<Vec<_>>();

    // Anything after the padding that is not itself padding means this was never
    // one base64 document, and decoding the prefix would invent a value.
    let tail = input
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .skip(symbols.len());
    if tail.clone().any(|byte| byte != b'=') {
        return Err("unexpected character after the padding".to_string());
    }
    if tail.count() > 2 {
        return Err("too much padding".to_string());
    }

    // One leftover symbol carries six bits, which is not enough for a byte and
    // so cannot be the tail of any encoding.
    if symbols.len() % 4 == 1 {
        return Err("truncated".to_string());
    }

    let mut output = Vec::with_capacity(symbols.len() * 3 / 4);
    let mut accumulator: u32 = 0;
    let mut bits = 0u32;

    for symbol in symbols {
        let Some(value) = ALPHABET.iter().position(|candidate| *candidate == symbol) else {
            return Err(format!(
                "{:?} is not a base64 character",
                char::from(symbol)
            ));
        };

        accumulator = (accumulator << 6) | value as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((accumulator >> bits) as u8);
        }
    }

    // The bits a partial group leaves over are padding and must be zero. A
    // non-zero remainder means the input was not produced by an encoder.
    if bits > 0 && accumulator & ((1 << bits) - 1) != 0 {
        return Err("trailing bits are not zero".to_string());
    }

    Ok(output)
}

/// Decodes base64 that is expected to hold text.
///
/// The UTF-8 check is not a formality. A vault with client-side encryption on
/// returns ciphertext in the same field, and decoding that as a password would
/// put binary rubbish on the clipboard while claiming it was a secret. Failing
/// is the only honest outcome, and the caller turns it into a row that says so.
pub fn decode_text(input: &str) -> Result<String, String> {
    let bytes = decode(input)?;
    String::from_utf8(bytes).map_err(|_| "not text".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(input: &str) -> String {
        decode_text(input).expect("valid base64")
    }

    #[test]
    fn the_standard_vectors_decode() {
        // RFC 4648 section 10.
        assert_eq!(text(""), "");
        assert_eq!(text("Zg=="), "f");
        assert_eq!(text("Zm8="), "fo");
        assert_eq!(text("Zm9v"), "foo");
        assert_eq!(text("Zm9vYg=="), "foob");
        assert_eq!(text("Zm9vYmE="), "fooba");
        assert_eq!(text("Zm9vYmFy"), "foobar");
    }

    /// Passwork returns unpadded base64 for some field lengths, and padding is
    /// redundant anyway — the symbol count already says how many bytes there are.
    #[test]
    fn padding_is_optional() {
        assert_eq!(text("Zg"), "f");
        assert_eq!(text("Zm8"), "fo");
        assert_eq!(text("Zm9vYg"), "foob");
    }

    /// A JSON reply may wrap a long value, and a shell command may add a newline.
    #[test]
    fn whitespace_is_ignored() {
        assert_eq!(text("Zm9v YmFy\n"), "foobar");
        assert_eq!(text("  Zm9vYmFy  "), "foobar");
    }

    #[test]
    fn the_whole_byte_range_survives_a_round_trip() {
        // `AAECAw==` is 0x00 0x01 0x02 0x03; `//79/A==` is the high end.
        assert_eq!(decode("AAECAw==").expect("valid"), vec![0, 1, 2, 3]);
        assert_eq!(decode("//79").expect("valid"), vec![0xff, 0xfe, 0xfd]);
    }

    #[test]
    fn a_non_alphabet_character_is_refused() {
        let error = decode("Zm9v!mFy").expect_err("not base64");
        assert!(error.contains("not a base64 character"), "{error}");
        // URL-safe base64 is a different alphabet, and silently accepting it
        // would decode to the wrong bytes rather than to nothing.
        assert!(decode("Zm9-YmFy").is_err());
    }

    /// Six bits cannot be a byte, so this is not a truncated encoding of
    /// anything — it is not an encoding.
    #[test]
    fn a_lone_trailing_symbol_is_refused() {
        assert!(decode("Zm9vY").is_err());
        assert!(decode("Z").is_err());
    }

    #[test]
    fn misplaced_padding_is_refused() {
        assert!(decode("Zm==9v").is_err());
        assert!(decode("Zg===").is_err());
    }

    /// The bits a partial group leaves over belong to no byte. An encoder always
    /// zeroes them, so anything else was not produced by one.
    #[test]
    fn non_canonical_trailing_bits_are_refused() {
        // `Zh` carries the same first byte as `Zg` but sets a padding bit.
        assert_eq!(decode("Zg").expect("valid"), b"f");
        assert!(decode("Zh").is_err());
    }

    /// The property the password path depends on: ciphertext must not come back
    /// looking like a secret.
    #[test]
    fn binary_that_is_not_text_is_refused_as_text() {
        // Valid base64, but the bytes are not UTF-8.
        let error = decode_text("//79").expect_err("not text");
        assert_eq!(error, "not text");
    }

    #[test]
    fn text_beyond_ascii_survives() {
        assert_eq!(text("aMOlbmRlbHNl"), "håndelse");
    }
}
