use crate::app_server::{Model, ModelSelection};
use crossterm::event::KeyCode;
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::Line,
    widgets::Paragraph,
    Frame,
};

pub enum PickerResult {
    Pending,
    Cancelled,
    Selected(ModelSelection),
}
pub struct ModelPicker {
    models: Vec<Model>,
    model_index: usize,
    effort_index: usize,
    choosing_effort: bool,
    current: Option<ModelSelection>,
}
impl ModelPicker {
    pub fn new(models: &[Model], current: Option<&ModelSelection>) -> Option<Self> {
        let models = models
            .iter()
            .filter(|m| !m.hidden)
            .cloned()
            .collect::<Vec<_>>();
        if models.is_empty() {
            return None;
        }
        let model_index = current
            .and_then(|s| models.iter().position(|m| m.id == s.id))
            .unwrap_or(0);
        Some(Self {
            models,
            model_index,
            effort_index: 0,
            choosing_effort: false,
            current: current.cloned(),
        })
    }
    pub fn handle(&mut self, code: KeyCode) -> PickerResult {
        if code == KeyCode::Esc {
            return PickerResult::Cancelled;
        }
        let count = if self.choosing_effort {
            self.models[self.model_index]
                .supported_reasoning_efforts
                .len()
        } else {
            self.models.len()
        };
        let index = if self.choosing_effort {
            &mut self.effort_index
        } else {
            &mut self.model_index
        };
        match code {
            KeyCode::Up => *index = (*index + count - 1) % count,
            KeyCode::Down => *index = (*index + 1) % count,
            KeyCode::Enter => {
                let model = &self.models[self.model_index];
                if self.choosing_effort {
                    let mut selected = ModelSelection::from_model(model);
                    selected.effort = Some(
                        model.supported_reasoning_efforts[self.effort_index]
                            .reasoning_effort
                            .clone(),
                    );
                    return PickerResult::Selected(selected);
                } else if model.supported_reasoning_efforts.is_empty() {
                    return PickerResult::Selected(ModelSelection::from_model(model));
                } else {
                    let effort = self
                        .current
                        .as_ref()
                        .filter(|s| s.id == model.id)
                        .and_then(|s| s.effort.as_deref())
                        .or_else(|| model.default_effort());
                    self.effort_index = model
                        .supported_reasoning_efforts
                        .iter()
                        .position(|e| Some(e.reasoning_effort.as_str()) == effort)
                        .unwrap_or(0);
                    self.choosing_effort = true;
                }
            }
            _ => {}
        }
        PickerResult::Pending
    }
    pub fn height(&self, screen_height: u16) -> u16 {
        screen_height.saturating_sub(1).min(12).max(1)
    }
    pub fn render(&self, frame: &mut Frame, rect: Rect) {
        let model = &self.models[self.model_index];
        let title = if self.choosing_effort {
            format!("{} · Select reasoning effort", model.display_name)
        } else {
            "Select model".into()
        };
        let (index, labels, description) = if self.choosing_effort {
            (
                self.effort_index,
                model
                    .supported_reasoning_efforts
                    .iter()
                    .map(|e| {
                        format!(
                            "{}{}",
                            e.reasoning_effort,
                            if model.default_reasoning_effort.as_deref()
                                == Some(e.reasoning_effort.as_str())
                            {
                                " (default)"
                            } else {
                                ""
                            }
                        )
                    })
                    .collect::<Vec<_>>(),
                model.supported_reasoning_efforts[self.effort_index]
                    .description
                    .as_str(),
            )
        } else {
            (
                self.model_index,
                self.models
                    .iter()
                    .map(|m| {
                        format!(
                            "{}{}",
                            m.display_name,
                            if m.is_default { " (default)" } else { "" }
                        )
                    })
                    .collect::<Vec<_>>(),
                model.description.as_str(),
            )
        };
        let available = rect.height.saturating_sub(4) as usize;
        let start = index.saturating_sub(available.saturating_sub(1));
        let mut lines = vec![Line::from(title)];
        for (i, label) in labels.iter().enumerate().skip(start).take(available) {
            lines.push(Line::styled(
                format!("{} {label}", if index == i { "›" } else { " " }),
                Style::default().fg(if index == i {
                    Color::Cyan
                } else {
                    Color::Reset
                }),
            ));
        }
        lines.push(Line::styled(
            description,
            Style::default().fg(Color::DarkGray),
        ));
        lines.push(Line::from(""));
        lines.push(Line::styled(
            "↑↓ navigate · Enter select · Esc cancel",
            Style::default().fg(Color::DarkGray),
        ));
        frame.render_widget(Paragraph::new(lines), rect);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn models() -> Vec<Model> {
        serde_json::from_value(json!([{"id":"a","model":"a-slug","displayName":"A","isDefault":true,"defaultReasoningEffort":"deep","supportedReasoningEfforts":[{"reasoningEffort":"light"},{"reasoningEffort":"deep"}]},{"id":"b","model":"b-slug","displayName":"B","defaultReasoningEffort":"custom","supportedReasoningEfforts":[{"reasoningEffort":"custom"}]}])).unwrap()
    }
    #[test]
    fn navigation_model_effort_selection_and_cancel() {
        let models = models();
        let mut picker = ModelPicker::new(&models, None).unwrap();
        picker.handle(KeyCode::Down);
        assert_eq!(picker.model_index, 1);
        picker.handle(KeyCode::Up);
        picker.handle(KeyCode::Enter);
        assert!(picker.choosing_effort);
        assert_eq!(picker.effort_index, 1);
        picker.handle(KeyCode::Up);
        let PickerResult::Selected(s) = picker.handle(KeyCode::Enter) else {
            panic!("selection missing")
        };
        assert_eq!(s.model, "a-slug");
        assert_eq!(s.effort.as_deref(), Some("light"));
        let mut picker = ModelPicker::new(&models, Some(&s)).unwrap();
        picker.handle(KeyCode::Enter);
        assert_eq!(picker.effort_index, 0); // Reopening preserves the chosen effort.
        assert!(matches!(
            picker.handle(KeyCode::Esc),
            PickerResult::Cancelled
        ));
        assert_eq!(s.effort.as_deref(), Some("light"));
    }
    #[test]
    fn models_without_efforts_select_directly_and_hidden_models_are_excluded() {
        let mut models = models();
        models[0].hidden = true;
        models[1].supported_reasoning_efforts.clear();
        let mut picker = ModelPicker::new(&models, None).unwrap();
        assert_eq!(picker.models.len(), 1);
        let PickerResult::Selected(selection) = picker.handle(KeyCode::Enter) else {
            panic!("selection missing")
        };
        assert_eq!(selection.id, "b");
        assert!(selection.effort.is_none());
    }
}
