pub const MAX_IDENTIFIER_BYTES: usize = 128;
pub const MAX_PATH_BYTES: usize = 4_096;
pub(crate) const MAX_RECEIPT_SPANS: usize = 257;

/// Bounds of the `distill.context/v3` artifact selector. They are part of the
/// public contract: adapters publish them, and the engine enforces them before
/// any store read.
pub const MAX_SELECTOR_PATTERN_BYTES: usize = 512;
pub const MAX_SELECTOR_CONTEXT_LINES: u64 = 16;
pub const MAX_SELECTOR_MATCHES: u64 = 32;
pub const DEFAULT_SELECTOR_CONTEXT_LINES: u64 = 2;
pub const DEFAULT_SELECTOR_MATCHES: u64 = 8;

pub(crate) fn valid_correlation_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_IDENTIFIER_BYTES
}

pub(crate) fn valid_identifier(value: &str) -> bool {
    valid_correlation_id(value)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

pub(crate) fn is_lower_hex(byte: u8) -> bool {
    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
}
