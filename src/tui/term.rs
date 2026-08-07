#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "terminal feature negotiation is a compact independent capability bitmap"
)]
pub struct TerminalCapabilities {
    pub ansi: bool,
    pub alternate_screen: bool,
    pub cursor_addressing: bool,
    pub color: bool,
    pub unicode: bool,
}

impl TerminalCapabilities {
    #[must_use]
    pub const fn rich_ansi() -> Self {
        Self {
            ansi: true,
            alternate_screen: true,
            cursor_addressing: true,
            color: true,
            unicode: true,
        }
    }

    #[must_use]
    pub const fn plain() -> Self {
        Self {
            ansi: false,
            alternate_screen: false,
            cursor_addressing: false,
            color: false,
            unicode: false,
        }
    }

    #[must_use]
    pub fn screen_prefix(self) -> &'static str {
        if self.ansi && self.alternate_screen && self.cursor_addressing {
            "\u{1b}[?1049h\u{1b}[2J\u{1b}[H"
        } else {
            ""
        }
    }
}
