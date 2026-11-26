//! A password the interface holds until the helper takes it.

use std::fmt;
use zeroize::Zeroize;

/// A secret typed by the person: wiped when dropped, never printed. It is
/// only ever handed to the helper through `Backend::set_secret` (which sends a
/// file descriptor, not text), never into a plan request, a marker or a log.
#[derive(Default)]
pub struct Secret(Vec<u8>);

impl Secret {
    pub fn new(text: &str) -> Self {
        Self(text.as_bytes().to_vec())
    }

    pub fn expose(&self) -> &str {
        // Built from a `&str` and never modified, so always UTF-8.
        std::str::from_utf8(&self.0).unwrap_or_default()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Bytes, as `chpasswd` and LUKS will count them.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn set(&mut self, text: &str) {
        self.0.zeroize();
        self.0 = text.as_bytes().to_vec();
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}
