use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyAction {
    Prefix,
    SwitchFocus,
    EnterScrollMode,
    ExitScrollMode,
    Detach,
    OpenCommandPalette,
    OpenDaemonList,
    ToggleHelp,
    Redraw,
    CloseRequested,
    ScrollUp,
    ScrollDown,
    PageUp,
    PageDown,
    JumpTop,
    JumpBottom,
    BeginSearch,
    NextSearch,
    PreviousSearch,
    Escape,
    Input(KeyEvent),
    Ignored,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrefixKeymap {
    prefix: KeyChord,
    prefix_bindings: Vec<(KeyChord, KeyAction)>,
    scroll_bindings: Vec<(KeyChord, KeyAction)>,
}

impl PrefixKeymap {
    pub fn default_ctrl_bracket() -> Self {
        Self {
            prefix: KeyChord::new(KeyCode::Char(']'), KeyModifiers::CONTROL),
            prefix_bindings: vec![
                (KeyChord::plain(KeyCode::Tab), KeyAction::SwitchFocus),
                (
                    KeyChord::plain(KeyCode::Char('[')),
                    KeyAction::EnterScrollMode,
                ),
                (
                    KeyChord::plain(KeyCode::Char(']')),
                    KeyAction::ExitScrollMode,
                ),
                (KeyChord::plain(KeyCode::Char('d')), KeyAction::Detach),
                (
                    KeyChord::plain(KeyCode::Char('p')),
                    KeyAction::OpenCommandPalette,
                ),
                (
                    KeyChord::plain(KeyCode::Char('l')),
                    KeyAction::OpenDaemonList,
                ),
                (KeyChord::plain(KeyCode::Char('?')), KeyAction::ToggleHelp),
                (KeyChord::plain(KeyCode::Char('r')), KeyAction::Redraw),
                (
                    KeyChord::plain(KeyCode::Char('q')),
                    KeyAction::CloseRequested,
                ),
            ],
            scroll_bindings: vec![
                (KeyChord::plain(KeyCode::Up), KeyAction::ScrollUp),
                (KeyChord::plain(KeyCode::Char('k')), KeyAction::ScrollUp),
                (KeyChord::plain(KeyCode::Down), KeyAction::ScrollDown),
                (KeyChord::plain(KeyCode::Char('j')), KeyAction::ScrollDown),
                (KeyChord::plain(KeyCode::PageUp), KeyAction::PageUp),
                (KeyChord::plain(KeyCode::Char('b')), KeyAction::PageUp),
                (KeyChord::plain(KeyCode::PageDown), KeyAction::PageDown),
                (KeyChord::plain(KeyCode::Char('f')), KeyAction::PageDown),
                (KeyChord::plain(KeyCode::Home), KeyAction::JumpTop),
                (KeyChord::plain(KeyCode::Char('g')), KeyAction::JumpTop),
                (KeyChord::plain(KeyCode::End), KeyAction::JumpBottom),
                (KeyChord::plain(KeyCode::Char('G')), KeyAction::JumpBottom),
                (KeyChord::plain(KeyCode::Char('/')), KeyAction::BeginSearch),
                (KeyChord::plain(KeyCode::Char('n')), KeyAction::NextSearch),
                (
                    KeyChord::plain(KeyCode::Char('N')),
                    KeyAction::PreviousSearch,
                ),
                (KeyChord::plain(KeyCode::Esc), KeyAction::Escape),
            ],
        }
    }

    pub fn is_prefix(&self, event: KeyEvent) -> bool {
        KeyChord::from(event) == self.prefix
    }

    pub fn prefix_action(&self, event: KeyEvent) -> Option<KeyAction> {
        let chord = KeyChord::from(event);
        self.prefix_bindings
            .iter()
            .find_map(|(candidate, action)| (candidate == &chord).then(|| action.clone()))
    }

    pub fn scroll_action(&self, event: KeyEvent) -> Option<KeyAction> {
        let chord = KeyChord::from(event);
        self.scroll_bindings
            .iter()
            .find_map(|(candidate, action)| (candidate == &chord).then(|| action.clone()))
    }

    pub fn prefix_event(&self) -> KeyEvent {
        KeyEvent::new(self.prefix.code, self.prefix.modifiers)
    }
}

impl Default for PrefixKeymap {
    fn default() -> Self {
        Self::default_ctrl_bracket()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct KeyChord {
    code: KeyCode,
    modifiers: KeyModifiers,
}

impl KeyChord {
    fn plain(code: KeyCode) -> Self {
        Self::new(code, KeyModifiers::NONE)
    }

    fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        Self { code, modifiers }
    }
}

impl From<KeyEvent> for KeyChord {
    fn from(event: KeyEvent) -> Self {
        Self {
            code: event.code,
            modifiers: event.modifiers,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_prefix_is_ctrl_right_bracket() {
        let keymap = PrefixKeymap::default();
        assert!(keymap.is_prefix(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::CONTROL)));
        assert!(!keymap.is_prefix(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE)));
    }

    #[test]
    fn keymap_decodes_required_prefix_bindings() {
        let keymap = PrefixKeymap::default();

        assert_eq!(
            keymap.prefix_action(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Some(KeyAction::SwitchFocus)
        );
        assert_eq!(
            keymap.prefix_action(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)),
            Some(KeyAction::Detach)
        );
        assert_eq!(
            keymap.prefix_action(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE)),
            Some(KeyAction::ToggleHelp)
        );
    }
}
