use crate::commands::{CommandSpec, COMMANDS};
use crossterm::event::KeyCode;
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::Line,
    widgets::Paragraph,
    Frame,
};

#[derive(Default)]
pub struct CommandPopup {
    matches: Vec<usize>,
    selected: usize,
    query: String,
    dismissed: Option<String>,
}
impl CommandPopup {
    pub fn sync(&mut self, draft: &str) {
        if draft != self.query {
            self.selected = 0;
            self.query = draft.into();
            self.dismissed = None;
        }
        self.matches.clear();
        if !draft.starts_with('/')
            || draft.chars().any(char::is_whitespace)
            || self.dismissed.as_deref() == Some(draft)
        {
            return;
        }
        self.matches = COMMANDS
            .iter()
            .enumerate()
            .filter(|(_, s)| s.name.starts_with(draft))
            .map(|(i, _)| i)
            .collect();
        self.matches.sort_by_key(|i| COMMANDS[*i].name != draft);
        self.selected = self.selected.min(self.matches.len().saturating_sub(1));
    }
    pub fn visible(&self) -> bool {
        !self.matches.is_empty()
    }
    pub fn rows(&self) -> u16 {
        self.matches.len().min(8) as u16
    }
    pub fn dismiss(&mut self, draft: &str) {
        self.query = draft.into();
        self.dismissed = Some(draft.into());
        self.matches.clear();
    }
    pub fn navigate(&mut self, code: KeyCode) {
        if self.matches.is_empty() {
            return;
        }
        match code {
            KeyCode::Up => {
                self.selected = (self.selected + self.matches.len() - 1) % self.matches.len()
            }
            KeyCode::Down => self.selected = (self.selected + 1) % self.matches.len(),
            _ => {}
        }
    }
    pub fn selected(&self) -> Option<&'static CommandSpec> {
        self.matches.get(self.selected).map(|i| &COMMANDS[*i])
    }
    pub fn completion(&self) -> Option<String> {
        self.selected()
            .map(|s| format!("{}{}", s.name, if s.argument { " " } else { "" }))
    }
    pub fn render(&self, frame: &mut Frame, rect: Rect, active: bool) {
        let height = rect.height as usize;
        let start = self.selected.saturating_sub(height.saturating_sub(1));
        let lines = self
            .matches
            .iter()
            .enumerate()
            .skip(start)
            .take(height)
            .map(|(index, command)| {
                let spec = &COMMANDS[*command];
                let unavailable = if active && spec.idle_only {
                    " · after current turn"
                } else {
                    ""
                };
                Line::styled(
                    format!(
                        "{} {:12} {}{}",
                        if index == self.selected { "›" } else { " " },
                        spec.name,
                        spec.description,
                        unavailable
                    ),
                    Style::default().fg(if index == self.selected {
                        Color::Cyan
                    } else {
                        Color::DarkGray
                    }),
                )
            })
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(lines), rect);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn filters_navigates_completes_and_preserves_esc_draft() {
        let mut popup = CommandPopup::default();
        popup.sync("/");
        assert_eq!(popup.matches.len(), COMMANDS.len());
        popup.sync("/m");
        assert_eq!(popup.completion().as_deref(), Some("/model"));
        popup.sync("/st");
        assert_eq!(popup.completion().as_deref(), Some("/status"));
        popup.navigate(KeyCode::Down);
        assert_eq!(popup.completion().as_deref(), Some("/steer "));
        popup.navigate(KeyCode::Up);
        assert_eq!(popup.completion().as_deref(), Some("/status"));
        popup.dismiss("/st");
        popup.sync("/st");
        assert!(!popup.visible());
        popup.sync("/ste");
        assert!(popup.visible());
        popup.sync("/budget");
        assert_eq!(popup.completion().as_deref(), Some("/budget "));
        popup.sync("/steer draft");
        assert!(!popup.visible());
    }
}
