//! Release checksums (internal/pluginstore/checksum.go, direct.go).

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

/// Go `ParseChecksums`: `sha256  name` lines (`*name` for binary mode); blank and
/// `#` lines skipped.
pub fn parse_checksums(data: &[u8]) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    for (number, raw) in String::from_utf8_lossy(data).split('\n').enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 2 {
            return Err(format!("line {}: invalid checksum entry", number + 1));
        }
        let hash = fields[0].trim().to_lowercase();
        if hash.len() != 64 {
            return Err(format!("line {}: invalid sha256 length", number + 1));
        }
        if let Some(bad) = hash.bytes().position(|b| !b.is_ascii_hexdigit()) {
            return Err(format!(
                "line {}: invalid sha256: encoding/hex: invalid byte: {}",
                number + 1,
                super::registry::go_rune_literal(char::from(hash.as_bytes()[bad]))
            ));
        }
        let name = fields[1].trim();
        out.insert(name.strip_prefix('*').unwrap_or(name).to_owned(), hash);
    }
    Ok(out)
}

/// Go `VerifyChecksum`.
pub fn verify_checksum(name: &str, data: &[u8], checksums: &BTreeMap<String, String>) -> Result<(), String> {
    let expected = checksums.get(name).map(|h| h.trim().to_lowercase()).unwrap_or_default();
    if expected.is_empty() {
        return Err(format!("checksum for {name} not found"));
    }
    if sha256_hex(data) != expected {
        return Err(format!("checksum mismatch for {name}"));
    }
    Ok(())
}

pub(super) fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}
