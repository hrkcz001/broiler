//! EDID decoding for monitor identity.
//!
//! Only the identity-relevant fields are decoded: manufacturer, product code, the numeric
//! and the textual serial number, and the monitor name descriptor. Nothing here decides
//! whether a control is supported; an EDID says what the display claims to be, not what its
//! DDC implementation does.

use serde::Serialize;

const EDID_MIN_LEN: usize = 128;
const HEADER: [u8; 8] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];

/// Identity fields decoded from an EDID block.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct EdidIdentity {
    /// Three-letter PNP manufacturer id, for example `MSI`.
    pub manufacturer: Option<String>,
    pub product_code: Option<u16>,
    /// Binary serial number from bytes 12..16, when nonzero.
    pub numeric_serial: Option<u32>,
    /// Serial string descriptor (`0xFF`), trimmed.
    pub serial_string: Option<String>,
    /// Monitor name descriptor (`0xFC`), trimmed.
    pub monitor_name: Option<String>,
    /// Manufacturing week and year, when the fields are not "unknown".
    pub manufacture_year: Option<u16>,
    pub manufacture_week: Option<u8>,
}

impl EdidIdentity {
    /// Serial used for identity: the textual serial first, then a nonzero numeric serial.
    ///
    /// A serial that many units share is still not unique, which is why the identifier also
    /// carries the manufacturer and product code and why callers must handle a duplicate.
    pub fn serial_for_identity(&self) -> Option<String> {
        if let Some(serial) = self.serial_string.as_ref() {
            let trimmed = serial.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
        self.numeric_serial
            .filter(|value| *value != 0)
            .map(|value| value.to_string())
    }
}

/// Decode the packed 5-bit manufacturer id, as stored in EDID bytes 8..10.
///
/// Also used for `DISPLAYCONFIG_TARGET_DEVICE_NAME::edidManufactureId`, which carries the same
/// packed value, so identity can still name a vendor when no EDID blob was cached.
pub fn decode_manufacturer(value: u16) -> Option<String> {
    if value == 0 {
        return None;
    }
    let letters: Vec<char> = (0..3)
        .map(|index| {
            let code = ((value >> (10 - index * 5)) & 0x1F) as u8;
            (b'A' + code.saturating_sub(1)) as char
        })
        .collect();
    let text: String = letters.into_iter().collect();
    if text.chars().all(|c| c.is_ascii_uppercase()) {
        Some(text)
    } else {
        None
    }
}

fn descriptor_text(block: &[u8]) -> Option<String> {
    // Descriptor text payload is bytes 5..18, ASCII, NUL padded, newline terminated by
    // convention. Trailing whitespace is dropped.
    let raw = &block[5..18.min(block.len())];
    let end = raw
        .iter()
        .position(|&b| b == 0x0A || b == 0x00)
        .unwrap_or(raw.len());
    let text: String = raw[..end]
        .iter()
        .filter(|b| b.is_ascii_graphic() || **b == b' ')
        .map(|b| *b as char)
        .collect();
    let trimmed = text.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Decode one 128-byte EDID block. Returns `None` when the header or length is wrong.
pub fn parse(block: &[u8]) -> Option<EdidIdentity> {
    if block.len() < EDID_MIN_LEN || block[..8] != HEADER {
        return None;
    }

    let manufacturer = decode_manufacturer(((block[8] as u16) << 8) | block[9] as u16);
    let product_code = u16::from_le_bytes([block[10], block[11]]);
    let numeric = u32::from_le_bytes([block[12], block[13], block[14], block[15]]);
    let week = block[16];
    let year = 1990u16.saturating_add(block[17] as u16);

    let mut identity = EdidIdentity {
        manufacturer,
        product_code: Some(product_code),
        numeric_serial: if numeric == 0 { None } else { Some(numeric) },
        serial_string: None,
        monitor_name: None,
        manufacture_year: (block[17] != 0).then_some(year),
        manufacture_week: (week != 0 && week <= 53).then_some(week),
    };

    // Preferred timing descriptor is skipped; the remaining three blocks may hold text.
    for offset in [54usize, 72, 90, 108] {
        let descriptor = &block[offset..offset + 18];
        if descriptor[..3] != [0x00, 0x00, 0x00] {
            continue;
        }
        match descriptor[3] {
            0xFF => identity.serial_string = descriptor_text(descriptor),
            0xFC => identity.monitor_name = descriptor_text(descriptor),
            _ => {}
        }
    }

    Some(identity)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// EDID with header, MSI manufacturer id, product 0x3CA7, numeric serial 1234,
    /// serial string descriptor "ABC123" and name descriptor "MAG275R".
    fn sample() -> Vec<u8> {
        let mut block = vec![0u8; 128];
        block[..8].copy_from_slice(&HEADER);
        // 0x4D53 => 'M' 'S' 'I'? encoded as 5-bit letters: M=13, S=19, I=9
        let manufacturer: u16 = (13 << 10) | (19 << 5) | 9;
        block[8] = (manufacturer >> 8) as u8;
        block[9] = manufacturer as u8;
        block[10..12].copy_from_slice(&0x3CA7u16.to_le_bytes());
        block[12..16].copy_from_slice(&1234u32.to_le_bytes());
        block[16] = 20;
        block[17] = 34;

        block[54..57].copy_from_slice(&[0x00, 0x00, 0x00]);
        block[54 + 3] = 0xFF;
        block[54 + 5..54 + 5 + 6].copy_from_slice(b"ABC123");
        block[54 + 5 + 6] = 0x0A;

        block[72..75].copy_from_slice(&[0x00, 0x00, 0x00]);
        block[72 + 3] = 0xFC;
        block[72 + 5..72 + 5 + 7].copy_from_slice(b"MAG275R");
        block[72 + 5 + 7] = 0x0A;
        block
    }

    #[test]
    fn decodes_identity_fields() {
        let identity = parse(&sample()).expect("valid edid");
        assert_eq!(identity.manufacturer.as_deref(), Some("MSI"));
        assert_eq!(identity.product_code, Some(0x3CA7));
        assert_eq!(identity.numeric_serial, Some(1234));
        assert_eq!(identity.serial_string.as_deref(), Some("ABC123"));
        assert_eq!(identity.monitor_name.as_deref(), Some("MAG275R"));
        assert_eq!(identity.manufacture_year, Some(2024));
        assert_eq!(identity.manufacture_week, Some(20));
        assert_eq!(identity.serial_for_identity().as_deref(), Some("ABC123"));
    }

    #[test]
    fn text_serial_wins_over_numeric_serial() {
        let mut block = sample();
        block[54 + 3] = 0xFD; // not a serial descriptor any more
        let identity = parse(&block).expect("valid edid");
        assert!(identity.serial_string.is_none());
        assert_eq!(identity.serial_for_identity().as_deref(), Some("1234"));
    }

    #[test]
    fn missing_serial_has_no_identity_serial() {
        let mut block = sample();
        block[12..16].copy_from_slice(&0u32.to_le_bytes());
        block[54 + 3] = 0xFE;
        let identity = parse(&block).expect("valid edid");
        assert!(identity.serial_for_identity().is_none());
    }

    #[test]
    fn rejects_short_and_misheaded_blocks() {
        assert!(parse(&[0u8; 64]).is_none());
        let mut block = sample();
        block[0] = 0x12;
        assert!(parse(&block).is_none());
    }

    #[test]
    fn unknown_manufacture_fields_are_absent() {
        let mut block = sample();
        block[16] = 0;
        block[17] = 0;
        let identity = parse(&block).expect("valid edid");
        assert_eq!(identity.manufacture_year, None);
        assert_eq!(identity.manufacture_week, None);
    }
}
