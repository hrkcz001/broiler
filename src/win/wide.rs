//! UTF-16 helpers for the `*W` Win32 APIs.

/// Convert a Rust string into a NUL-terminated UTF-16 buffer.
pub fn to_wide(text: &str) -> Vec<u16> {
    let mut buffer: Vec<u16> = text.encode_utf16().collect();
    buffer.push(0);
    buffer
}

/// Read a fixed-size UTF-16 field up to its first NUL.
pub fn from_wide_field(field: &[u16]) -> String {
    let end = field.iter().position(|&c| c == 0).unwrap_or(field.len());
    String::from_utf16_lossy(&field[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_round_trip() {
        let buffer = to_wide("MSI MAG275R");
        assert_eq!(*buffer.last().unwrap(), 0);
        assert_eq!(from_wide_field(&buffer), "MSI MAG275R");
    }

    #[test]
    fn field_without_nul_is_read_to_the_end() {
        let field: Vec<u16> = "A".encode_utf16().collect();
        assert_eq!(from_wide_field(&field), "A");
    }

    #[test]
    fn fixed_buffer_padding_is_ignored() {
        let mut field = [0u16; 8];
        for (slot, unit) in field.iter_mut().zip("AB".encode_utf16()) {
            *slot = unit;
        }
        assert_eq!(from_wide_field(&field), "AB");
    }
}
