//! Tolerant parser for the DDC/CI capabilities string.
//!
//! The capabilities string is advisory. ddcutil documents monitors that omit supported codes,
//! misreport replies, and respond slowly to the multi-exchange request. Therefore this parser
//! never authorizes a write: it produces evidence that a code was *advertised*, next to the
//! separate evidence that a getter succeeded. A code missing here is not proof of anything.
//!
//! The string is a nested key/value list, for example:
//!
//! ```text
//! (prot(monitor)type(lcd)model(MAG275R)cmds(01 02 03 0C 10 12)mccs_ver(2.2)
//!  vcp(10 12 14(01 02 05) 16 18 1A 62 8D)mswhql(1))
//! ```

use serde::Serialize;

use crate::win::ddc::CapabilitiesRaw;

/// Parsed capabilities evidence.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Capabilities {
    /// Raw string as returned by the monitor, without the terminating NUL.
    pub raw: String,
    /// Length in characters the monitor declared for the string.
    pub declared_length: u32,
    /// `prot(...)` value, normally `monitor`.
    pub protocol: Option<String>,
    /// `type(...)` value, for example `lcd`.
    pub kind: Option<String>,
    /// `model(...)` value.
    pub model: Option<String>,
    /// `mccs_ver(...)` value, for example `2.2`.
    pub mccs_version: Option<String>,
    /// Codes listed in `cmds(...)`, in string order.
    pub commands: Vec<u8>,
    /// Codes listed in `vcp(...)`, including ones with a value list.
    pub vcp_codes: Vec<u8>,
    /// Whether the monitor reported any MCCS version at all.
    pub version_reported: bool,
}

impl Capabilities {
    /// True when the code appears in `cmds(...)`.
    pub fn advertises_command(&self, code: u8) -> bool {
        self.commands.contains(&code)
    }

    /// True when the code appears in `vcp(...)`.
    pub fn lists_vcp(&self, code: u8) -> bool {
        self.vcp_codes.contains(&code)
    }

    /// True when the code appears anywhere in the string.
    ///
    /// The two sections are not interchangeable: `cmds(...)` lists supported commands, while
    /// `vcp(...)` lists features with optional value lists. A monitor can name a feature in one
    /// section and omit it from the other, so both count as "advertised" evidence.
    pub fn mentions(&self, code: u8) -> bool {
        self.advertises_command(code) || self.lists_vcp(code)
    }
}

/// Collect `key(value)` pairs at the top nesting level of `text`.
fn top_level_pairs(text: &[u8]) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let mut index = 0usize;

    while index < text.len() {
        while index < text.len() && !text[index].is_ascii_alphanumeric() && text[index] != b'_' {
            index += 1;
        }
        let key_start = index;
        while index < text.len() && (text[index].is_ascii_alphanumeric() || text[index] == b'_') {
            index += 1;
        }
        if key_start == index {
            break;
        }
        let key = String::from_utf8_lossy(&text[key_start..index]).to_string();

        while index < text.len() && text[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= text.len() || text[index] != b'(' {
            continue;
        }

        index += 1;
        let value_start = index;
        let mut depth = 1usize;
        while index < text.len() && depth > 0 {
            match text[index] {
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ => {}
            }
            index += 1;
        }
        let value_end = if depth == 0 { index - 1 } else { index };
        pairs.push((
            key,
            String::from_utf8_lossy(&text[value_start..value_end])
                .trim()
                .to_string(),
        ));
    }
    pairs
}

fn push_code(run: &str, collected: &mut Vec<u8>) {
    // A run is either one code of one or two digits, or a concatenation of two-digit codes.
    if run.len() == 1 {
        if let Ok(code) = u8::from_str_radix(run, 16) {
            if !collected.contains(&code) {
                collected.push(code);
            }
        }
        return;
    }
    if run.len() % 2 != 0 {
        return;
    }
    for chunk in run.as_bytes().chunks(2) {
        if let Ok(text) = std::str::from_utf8(chunk) {
            if let Ok(code) = u8::from_str_radix(text, 16) {
                if !collected.contains(&code) {
                    collected.push(code);
                }
            }
        }
    }
}

/// Extract VCP codes from a `cmds(...)` or `vcp(...)` body.
///
/// Monitors disagree on separators: some write `01 02 03`, others concatenate the pairs into
/// `010203` with no separator at all (observed on the MSI MAG275R). A nested value list such as
/// `14(05 06 08 0B)` therefore contributes the code `0x14` and not the nested values.
fn codes(value: &str) -> Vec<u8> {
    let mut collected = Vec::new();
    let mut run = String::new();
    let mut depth = 0usize;

    for byte in value.bytes() {
        match byte {
            b'(' => {
                if !run.is_empty() {
                    push_code(&run, &mut collected);
                    run.clear();
                }
                depth += 1;
            }
            b')' => {
                if !run.is_empty() {
                    push_code(&run, &mut collected);
                    run.clear();
                }
                depth = depth.saturating_sub(1);
            }
            _ if byte.is_ascii_hexdigit() => {
                if depth == 0 {
                    run.push(byte as char);
                }
            }
            _ => {
                if !run.is_empty() {
                    push_code(&run, &mut collected);
                    run.clear();
                }
            }
        }
    }
    if !run.is_empty() {
        push_code(&run, &mut collected);
    }
    collected
}

/// Parse a raw capabilities reply.
pub fn parse(raw: &CapabilitiesRaw) -> Capabilities {
    let pairs = top_level_pairs(raw.text.as_bytes());
    let mut parsed = Capabilities {
        raw: raw.text.clone(),
        declared_length: raw.declared_length,
        ..Capabilities::default()
    };

    for (key, value) in pairs {
        match key.as_str() {
            "prot" => parsed.protocol = Some(value),
            "type" => parsed.kind = Some(value),
            "model" => parsed.model = Some(value),
            "mccs_ver" => {
                parsed.version_reported = !value.is_empty();
                parsed.mccs_version = Some(value);
            }
            "cmds" => parsed.commands = codes(&value),
            "vcp" => parsed.vcp_codes = codes(&value),
            _ => {}
        }
    }
    parsed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(text: &str) -> CapabilitiesRaw {
        CapabilitiesRaw {
            text: text.to_string(),
            declared_length: text.chars().count() as u32,
        }
    }

    const SAMPLE: &str = "(prot(monitor)type(lcd)model(MAG275R)\
cmds(01 02 03 0C 0E 0F 10 12 14 16 18 1A 52 60 62 8D)\
mccs_ver(2.2)\
vcp(02 04 05 10 12 14(01 02 05 06 07 08 0B 0C) 16 18 1A 52 60 62 6C 6E 70 AC AE B2 B6 C0 C6 C8 C9 D6(01 04 05) DC DF E2 F0 F1 F2)\
mswhql(1))";

    #[test]
    fn parses_identity_fields() {
        let parsed = parse(&raw(SAMPLE));
        assert_eq!(parsed.protocol.as_deref(), Some("monitor"));
        assert_eq!(parsed.kind.as_deref(), Some("lcd"));
        assert_eq!(parsed.model.as_deref(), Some("MAG275R"));
        assert_eq!(parsed.mccs_version.as_deref(), Some("2.2"));
        assert!(parsed.version_reported);
    }

    #[test]
    fn parses_command_and_vcp_lists() {
        let parsed = parse(&raw(SAMPLE));
        for code in [0x10u8, 0x12, 0x14, 0x16, 0x18, 0x1A, 0x62, 0x8D] {
            assert!(parsed.advertises_command(code), "cmds missing {code:#04x}");
        }
        assert!(!parsed.advertises_command(0x87));
        assert!(parsed.lists_vcp(0x14));
        assert!(parsed.lists_vcp(0xD6));
        assert!(!parsed.lists_vcp(0x87));
    }

    #[test]
    fn nested_value_lists_do_not_leak_into_codes() {
        let parsed = parse(&raw("(vcp(14(01 02 05) 10))"));
        assert_eq!(parsed.vcp_codes, vec![0x14, 0x10]);
        assert!(parsed.commands.is_empty());
        assert!(!parsed.version_reported);
    }

    #[test]
    fn truncated_and_empty_strings_are_tolerated() {
        let parsed = parse(&raw("(prot(monitor)cmds(10 12"));
        assert_eq!(parsed.protocol.as_deref(), Some("monitor"));
        assert_eq!(parsed.commands, vec![0x10, 0x12]);

        let empty = parse(&raw(""));
        assert!(empty.commands.is_empty());
        assert!(empty.mccs_version.is_none());
    }

    #[test]
    fn junk_tokens_are_ignored() {
        let parsed = parse(&raw("(cmds(zz 1 10 1234))"));
        assert_eq!(parsed.commands, vec![0x01, 0x10, 0x12, 0x34]);
    }

    /// Exact reply observed from the MSI MAG275R on 2026-10-25, separator-free hex pairs.
    const OBSERVED_MSI: &str = "(prot(monitor)type(lcd)model(MAG275R)cmds(010203070C4EF3E3)\
vcp(020405080B0C101214(0506080B)16181A6C6E70ACAEB6C0C6C8C9CACC(0102030405060708090A0C0D0F18192325)\
D6(0104)DF60(11120F)628D(0102)FF)mswhql(1)mccs_ver(2.1)asset_eep(32)mpu_ver(01))";

    #[test]
    fn parses_a_separator_free_reply_from_a_real_monitor() {
        let parsed = parse(&raw(OBSERVED_MSI));
        assert_eq!(parsed.model.as_deref(), Some("MAG275R"));
        assert_eq!(parsed.mccs_version.as_deref(), Some("2.1"));
        assert_eq!(parsed.commands, vec![0x01, 0x02, 0x03, 0x07, 0x0C, 0x4E, 0xF3, 0xE3]);
        for code in [0x10u8, 0x12, 0x14, 0x16, 0x18, 0x1A, 0x62, 0x8D, 0xD6] {
            assert!(parsed.lists_vcp(code), "vcp list missing {code:#04x}");
        }
        // 0x05 really is a top-level code here; 0x19 only appears inside the 0xCC value list.
        assert!(parsed.lists_vcp(0x05));
        assert!(!parsed.lists_vcp(0x19));
        assert!(!parsed.lists_vcp(0x23));
    }
}
