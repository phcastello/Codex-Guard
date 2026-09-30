use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    widgets::{Block, Borders},
    Frame,
};
use tui_textarea::TextArea;

pub struct Editor {
    pub area: TextArea<'static>,
}
impl Editor {
    pub fn new() -> Self {
        let mut area = TextArea::default();
        area.set_cursor_style(Style::default().fg(Color::Black).bg(Color::Yellow));
        area.set_cursor_line_style(Style::default());
        Self { area }
    }
    pub fn text(&self) -> String {
        self.area.lines().join("\n")
    }
    pub fn replace(&mut self, text: &str) {
        *self = Self::new();
        self.area.insert_str(text);
    }
    pub fn clear(&mut self) {
        *self = Self::new();
    }
    pub fn handle(&mut self, event: Event) -> Option<String> {
        match event {
            Event::Paste(text) => {
                self.area
                    .insert_str(text.replace("\r\n", "\n").replace('\r', "\n"));
            }
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::F(2) | KeyCode::Char('d' | 'D')
                    if key.code == KeyCode::F(2)
                        || key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    let text = self.text();
                    self.clear();
                    return Some(text);
                }
                // Enter is a newline even when Windows paste arrives as ordinary keys.
                KeyCode::Enter | KeyCode::Char('\n' | '\r') => self.area.insert_newline(),
                _ => {
                    self.area.input(key);
                }
            },
            _ => {}
        }
        None
    }
    pub fn height(&self, terminal_height: u16) -> u16 {
        if terminal_height < 4 {
            return terminal_height.saturating_sub(1).max(1);
        }
        let ceiling = ((terminal_height as u32 * 40) / 100).max(3) as u16;
        u16::try_from(self.area.lines().len())
            .unwrap_or(u16::MAX)
            .saturating_add(2)
            .clamp(3, ceiling.min(terminal_height.saturating_sub(1)))
    }
    pub fn render(&mut self, frame: &mut Frame, rect: Rect, prompt: &str) {
        self.area.set_block(
            Block::default()
                .title(prompt.to_owned())
                .borders(Borders::TOP | Borders::BOTTOM)
                .border_style(Style::default().fg(Color::DarkGray)),
        );
        frame.render_widget(&self.area, rect);
        for y in rect.y..rect.bottom() {
            for x in rect.x..rect.right() {
                if frame
                    .buffer_mut()
                    .cell((x, y))
                    .is_some_and(|cell| cell.style().bg == Some(Color::Yellow))
                {
                    frame.set_cursor_position((x, y));
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;
    use ratatui::{backend::TestBackend, Terminal};
    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }
    #[test]
    fn multiline_paste_and_enter_require_explicit_submit() {
        let mut editor = Editor::new();
        assert!(editor
            .handle(Event::Paste("alpha\r\nβeta".into()))
            .is_none());
        assert!(editor
            .handle(key(KeyCode::Enter, KeyModifiers::NONE))
            .is_none());
        assert_eq!(
            editor.handle(key(KeyCode::F(2), KeyModifiers::NONE)),
            Some("alpha\nβeta\n".into())
        );
        for code in [
            KeyCode::Char('a'),
            KeyCode::Enter,
            KeyCode::Char('b'),
            KeyCode::Enter,
        ] {
            assert!(editor.handle(key(code, KeyModifiers::NONE)).is_none());
        }
        assert_eq!(
            editor.handle(key(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            Some("a\nb\n".into())
        );
    }
    #[test]
    fn unicode_navigation_large_prompt_and_cursor() {
        let mut editor = Editor::new();
        editor.handle(Event::Paste(format!("🙂 café\n{}", "x".repeat(600))));
        for code in [
            KeyCode::Left,
            KeyCode::Up,
            KeyCode::Home,
            KeyCode::Right,
            KeyCode::Down,
            KeyCode::End,
            KeyCode::Backspace,
            KeyCode::Delete,
        ] {
            editor.handle(key(code, KeyModifiers::NONE));
        }
        assert!(editor.text().starts_with("🙂 café\n"));
        editor.handle(Event::Paste("\n".repeat(100)));
        assert!(editor.height(40) <= 16);
        let mut terminal = Terminal::new(TestBackend::new(20, 5)).unwrap();
        terminal
            .draw(|f| {
                let r = f.area();
                editor.render(f, r, "›");
            })
            .unwrap();
        assert!(terminal
            .backend()
            .buffer()
            .content
            .iter()
            .any(|cell| cell.style().bg == Some(Color::Yellow)));
    }
}
