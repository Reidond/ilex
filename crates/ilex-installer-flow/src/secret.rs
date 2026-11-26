//! A password the interface holds until the helper takes it.

use std::fmt;
use zeroize::Zeroize;

/// A secret typed by the person: wiped when dropped, never printed. It is
/// only ever handed to the helper through `Backend::set_secret` (which sends a
/// file descriptor, not text), never into a plan request, a marker or a log.
///
/// Interfaces carry it as a `Secret` from the moment they read the field (the
/// toolkit's text widget, copied once, straight into this type) to the flow's
/// setters, and put it in their message enums as one: its `Debug` is redacted,
/// so a derived `Debug` on such an enum prints nothing of it.
#[derive(Default)]
pub struct Secret(Vec<u8>);

impl Secret {
    pub fn new(text: &str) -> Self {
        Self(text.as_bytes().to_vec())
    }

    /// From the bytes of a text a toolkit holds (a C string's contents), with
    /// no intermediate `String`. `None` when they are not UTF-8.
    pub fn from_utf8(bytes: &[u8]) -> Option<Self> {
        std::str::from_utf8(bytes).ok()?;
        Some(Self(bytes.to_vec()))
    }

    pub fn expose(&self) -> &str {
        // Built from UTF-8 and never modified, so always UTF-8.
        std::str::from_utf8(&self.0).unwrap_or_default()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Bytes, as `chpasswd` and LUKS will count them.
    pub fn len(&self) -> usize {
        self.0.len()
    }
}

impl PartialEq for Secret {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for Secret {}

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
