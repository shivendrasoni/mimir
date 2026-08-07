#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    OpenOverlay(super::app::OverlayKind),
    CloseOverlay,
    Confirm,
    SelectNext,
    SelectPrev,
    SubmitPrompt,
    HistoryPrev,
    HistoryNext,
    CursorLeft,
    CursorRight,
    Backspace,
    DeleteForward,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCode {
    Char(char),
    Enter,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Esc,
    Tab,
    F(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent {
    pub code: KeyCode,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl KeyEvent {
    #[must_use]
    pub fn plain(code: KeyCode) -> Self {
        Self {
            code,
            ctrl: false,
            alt: false,
            shift: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputBinding {
    pub key: KeyEvent,
    pub action: Action,
}

impl InputBinding {
    #[must_use]
    pub fn new(key: KeyEvent, action: Action) -> Self {
        Self { key, action }
    }
}
