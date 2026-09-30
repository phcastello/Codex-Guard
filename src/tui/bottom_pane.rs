use super::{
    command_popup::CommandPopup,
    composer::Editor,
    model_picker::{ModelPicker, PickerResult},
};
use crate::{
    app_server::{Model, ModelSelection},
    commands,
};
use crossterm::event::{Event, KeyCode, KeyEventKind};
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    widgets::Paragraph,
    Frame,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Confirm {
    Interrupt,
    Kill,
    Quit,
    Clear,
}
impl Confirm {
    fn accepts(self, answer: &str) -> bool {
        match self {
            Self::Interrupt => answer.eq_ignore_ascii_case("y"),
            Self::Kill => answer == "kill",
            Self::Quit => answer == "quit",
            Self::Clear => answer == "clear",
        }
    }
    fn command(self) -> &'static str {
        match self {
            Self::Interrupt => "/interrupt-confirmed",
            Self::Kill => "/kill-confirmed",
            Self::Quit => "/quit-confirmed",
            Self::Clear => "/clear-confirmed",
        }
    }
    fn prompt(self) -> &'static str {
        match self {
            Self::Interrupt => "Interrupt current turn? [y/N]",
            Self::Kill => "Kill process tree? Type \"kill\"",
            Self::Quit => "Quit and interrupt? Type \"quit\"",
            Self::Clear => "Clear active thread? Type \"clear\"",
        }
    }
}
fn confirmation_for(command: &str, active: bool) -> Option<Confirm> {
    match command {
        "/interrupt" if active => Some(Confirm::Interrupt),
        "/kill" => Some(Confirm::Kill),
        "/quit" if active => Some(Confirm::Quit),
        "/clear" if active => Some(Confirm::Clear),
        _ => None,
    }
}

pub enum PaneEvent {
    Submit(String),
    Notice(String),
    ModelChanged(ModelSelection),
    None,
}
pub struct BottomPane {
    pub editor: Editor,
    popup: CommandPopup,
    confirm: Option<Confirm>,
    picker: Option<ModelPicker>,
}
pub struct BottomLayout {
    pub popup: Rect,
    pub composer: Rect,
    pub footer: Rect,
}
impl BottomLayout {
    pub fn new(area: Rect, composer_height: u16, popup_height: u16) -> Self {
        let footer = Rect::new(
            area.x,
            area.bottom().saturating_sub(1),
            area.width,
            area.height.min(1),
        );
        let composer_height = composer_height.min(area.height.saturating_sub(1));
        let composer = Rect::new(
            area.x,
            footer.y.saturating_sub(composer_height),
            area.width,
            composer_height,
        );
        let popup_height = popup_height.min(
            area.height
                .saturating_sub(composer_height)
                .saturating_sub(1),
        );
        let popup = Rect::new(
            area.x,
            composer.y.saturating_sub(popup_height),
            area.width,
            popup_height,
        );
        Self {
            popup,
            composer,
            footer,
        }
    }
}
impl BottomPane {
    pub fn new() -> Self {
        Self {
            editor: Editor::new(),
            popup: CommandPopup::default(),
            confirm: None,
            picker: None,
        }
    }
    pub fn reset_confirmation(&mut self) {
        self.confirm = None;
    }
    pub fn open_model(
        &mut self,
        models: &[Model],
        selection: Option<&ModelSelection>,
        active: bool,
    ) -> Result<(), &'static str> {
        if active {
            return Err("Model cannot be changed while a turn is running.");
        }
        let picker = ModelPicker::new(models, selection)
            .ok_or("Model catalog is unavailable; using App Server defaults.")?;
        self.picker = Some(picker);
        Ok(())
    }
    pub fn handle(&mut self, event: Event, active: bool) -> PaneEvent {
        if let Some(picker) = self.picker.as_mut() {
            if let Event::Key(key) = event {
                if key.kind == KeyEventKind::Press {
                    match picker.handle(key.code) {
                        PickerResult::Selected(s) => {
                            self.picker = None;
                            return PaneEvent::ModelChanged(s);
                        }
                        PickerResult::Cancelled => self.picker = None,
                        PickerResult::Pending => {}
                    }
                }
            }
            return PaneEvent::None;
        }
        let draft = self.editor.text();
        if self.confirm.is_none() {
            self.popup.sync(&draft);
        }
        if let Event::Key(key) = &event {
            if key.kind == KeyEventKind::Press {
                if key.code == KeyCode::Esc && self.confirm.is_some() {
                    self.confirm = None;
                    self.editor.clear();
                    return PaneEvent::None;
                }
                if self.confirm.is_none() && self.popup.visible() {
                    match key.code {
                        KeyCode::Up | KeyCode::Down => {
                            self.popup.navigate(key.code);
                            return PaneEvent::None;
                        }
                        KeyCode::Tab => {
                            if let Some(completion) = self.popup.completion() {
                                self.editor.replace(&completion);
                                self.popup.dismiss(&completion);
                            }
                            return PaneEvent::None;
                        }
                        KeyCode::Esc => {
                            self.popup.dismiss(&draft);
                            return PaneEvent::None;
                        }
                        _ => {}
                    }
                }
            }
        }
        if let Some(entered) = self.editor.handle(event) {
            self.popup.sync("");
            if let Some(confirm) = self.confirm.take() {
                return if confirm.accepts(&entered) {
                    PaneEvent::Submit(confirm.command().into())
                } else {
                    PaneEvent::Notice("Cancelled".into())
                };
            }
            if let Some(confirm) = confirmation_for(entered.trim(), active) {
                self.confirm = Some(confirm);
                return PaneEvent::None;
            }
            if matches!(
                entered.trim(),
                "/interrupt-confirmed" | "/kill-confirmed" | "/quit-confirmed" | "/clear-confirmed"
            ) {
                return PaneEvent::Notice("Use the confirmation prompt for that action.".into());
            }
            if let Some(spec) = commands::find(&entered) {
                if !spec.argument && entered.trim() != spec.name {
                    return PaneEvent::Notice(format!("Usage: {}", spec.usage));
                }
            }
            if !entered.trim().is_empty() {
                return PaneEvent::Submit(entered);
            }
        }
        if self.confirm.is_none() {
            self.popup.sync(&self.editor.text());
        }
        PaneEvent::None
    }
    pub fn height(&self, screen_height: u16) -> u16 {
        if let Some(picker) = &self.picker {
            return picker
                .height(screen_height)
                .saturating_add(1)
                .min(screen_height);
        }
        let popup = if self.confirm.is_none() {
            self.popup.rows()
        } else {
            0
        };
        self.editor
            .height(screen_height)
            .saturating_add(popup)
            .saturating_add(1)
            .min(screen_height.saturating_sub(1).max(1))
    }
    pub fn render(&mut self, frame: &mut Frame, status: &str, screen_height: u16, active: bool) {
        let area = frame.area();
        let layout = BottomLayout::new(area, self.editor.height(screen_height), self.popup.rows());
        frame.render_widget(
            Paragraph::new(status).style(Style::default().fg(Color::DarkGray)),
            layout.footer,
        );
        if let Some(picker) = &self.picker {
            picker.render(
                frame,
                Rect::new(area.x, area.y, area.width, area.height.saturating_sub(1)),
            );
        } else {
            if self.confirm.is_none() {
                self.popup.render(frame, layout.popup, active);
            }
            let prompt = self
                .confirm
                .map(Confirm::prompt)
                .unwrap_or("› Enter newline · Ctrl+D/F2 send · /help");
            self.editor.render(frame, layout.composer, prompt);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};
    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }
    #[test]
    fn popup_events_edit_without_submitting_or_consuming_enter() {
        let mut pane = BottomPane::new();
        pane.handle(Event::Paste("/st".into()), false);
        pane.handle(key(KeyCode::Down), false);
        pane.handle(key(KeyCode::Tab), false);
        assert_eq!(pane.editor.text(), "/steer ");
        assert!(matches!(
            pane.handle(key(KeyCode::Enter), false),
            PaneEvent::None
        ));
        assert_eq!(pane.editor.text(), "/steer \n");
        pane.editor.replace("/m");
        pane.handle(key(KeyCode::Esc), false);
        assert_eq!(pane.editor.text(), "/m");
        assert!(!pane.popup.visible());
        assert!(matches!(
            pane.handle(key(KeyCode::F(2)), false),
            PaneEvent::Submit(_)
        ));
    }
    #[test]
    fn layout_is_bottom_anchored_and_popup_is_above_composer() {
        for (width, height) in [(80, 24), (40, 10), (100, 40)] {
            let area = Rect::new(0, height - 10.min(height), width, 10.min(height));
            let layout = BottomLayout::new(area, 4, 5);
            assert_eq!(layout.footer.bottom(), height);
            assert_eq!(layout.composer.bottom(), layout.footer.y);
            assert_eq!(layout.popup.bottom(), layout.composer.y);
        }
    }
    #[test]
    fn rendered_suggestions_stay_above_composer_and_footer_is_last_row() {
        use super::super::inline;
        use ratatui::{backend::TestBackend, Terminal};
        let mut pane = BottomPane::new();
        pane.handle(Event::Paste("/".into()), false);
        let mut terminal =
            Terminal::with_options(TestBackend::new(80, 24), inline::options(4)).unwrap();
        inline::sync_bottom(&mut terminal, pane.height(24), Clone::clone).unwrap();
        terminal
            .draw(|f| pane.render(f, "READY · Model/effort", 24, false))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let row = |y: u16| {
            (0..80)
                .map(|x| buffer.cell((x, y)).unwrap().symbol())
                .collect::<String>()
        };
        assert!(row(23).starts_with("READY · Model/effort"));
        assert!(row(12).contains("/status"));
        assert!(row(21).starts_with('/'));
        assert!(terminal
            .backend()
            .scrollback()
            .content
            .iter()
            .all(|c| !c.symbol().contains('/')));
    }
    #[test]
    fn picker_cancellation_preserves_draft_and_emits_no_selection() {
        let models: Vec<Model> = serde_json::from_value(serde_json::json!([{"id":"a","model":"a","displayName":"A","defaultReasoningEffort":"deep","supportedReasoningEfforts":[{"reasoningEffort":"deep"}]}])).unwrap();
        let mut pane = BottomPane::new();
        pane.editor.replace("partial draft");
        pane.open_model(&models, None, false).unwrap();
        assert!(matches!(
            pane.handle(key(KeyCode::Enter), false),
            PaneEvent::None
        ));
        assert!(matches!(
            pane.handle(key(KeyCode::Esc), false),
            PaneEvent::None
        ));
        assert!(pane.picker.is_none());
        assert_eq!(pane.editor.text(), "partial draft");
    }
    #[test]
    fn model_is_refused_while_running_and_failure_does_not_destroy_draft() {
        let mut pane = BottomPane::new();
        pane.editor.replace("draft");
        assert!(pane
            .open_model(&[], None, true)
            .unwrap_err()
            .contains("running"));
        assert!(pane
            .open_model(&[], None, false)
            .unwrap_err()
            .contains("unavailable"));
        assert!(pane.picker.is_none());
        assert_eq!(pane.editor.text(), "draft");
        assert!(
            matches!(pane.handle(key(KeyCode::F(2)),false),PaneEvent::Submit(text) if text == "draft")
        );
    }
    #[test]
    fn confirmations_remain_strong() {
        assert_eq!(confirmation_for("/clear", false), None);
        assert_eq!(confirmation_for("/clear", true), Some(Confirm::Clear));
        assert!(!Confirm::Clear.accepts("y"));
        assert!(Confirm::Clear.accepts("clear"));
        assert_eq!(Confirm::Clear.command(), "/clear-confirmed");
    }
}
