mod bottom_pane;
mod command_popup;
mod composer;
mod inline;
mod model_picker;
mod status;
mod transcript;

use crate::{
    app_server::{Model, ModelSelection},
    logging::Logger,
};
use anyhow::Result;
use bottom_pane::{BottomPane, PaneEvent};
use crossterm::{
    cursor::Show,
    event::{DisableBracketedPaste, EnableBracketedPaste, Event},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};
use ratatui::{backend::CrosstermBackend, style::Color, Terminal};
use serde_json::Value;
pub use status::StatusContext;
use std::{
    io::{self, Stdout},
    panic,
};
use transcript::Transcript;

pub struct Tui {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    bottom: BottomPane,
    transcript: Transcript,
    catalog: Vec<Model>,
    selection: Option<ModelSelection>,
    previous_hook: Option<Box<dyn Fn(&panic::PanicHookInfo<'_>) + Sync + Send + 'static>>,
}
impl Tui {
    pub fn new(catalog: Vec<Model>) -> Result<Self> {
        enable_raw_mode()?;
        if let Err(error) = execute!(io::stdout(), EnableBracketedPaste) {
            cleanup_terminal();
            return Err(error.into());
        }
        let mut terminal =
            match Terminal::with_options(CrosstermBackend::new(io::stdout()), inline::options(4)) {
                Ok(t) => t,
                Err(error) => {
                    cleanup_terminal();
                    return Err(error.into());
                }
            };
        if let Err(error) =
            inline::sync_bottom(&mut terminal, 4, |_| CrosstermBackend::new(io::stdout()))
        {
            cleanup_terminal();
            return Err(error);
        }
        let previous_hook = panic::take_hook();
        panic::set_hook(Box::new(|info| {
            cleanup_terminal();
            eprintln!("{info}");
        }));
        let selection = ModelSelection::initial(&catalog);
        Ok(Self {
            terminal,
            bottom: BottomPane::new(),
            transcript: Transcript::default(),
            catalog,
            selection,
            previous_hook: Some(previous_hook),
        })
    }
    pub fn header(&mut self, profile: &str, mode: &str) {
        self.transcript.push(
            format!("Codex Guard · {profile} · {mode} · YOLO\n"),
            Some(Color::DarkGray),
        );
    }
    pub fn selection(&self) -> Option<&ModelSelection> {
        self.selection.as_ref()
    }
    pub fn open_model(&mut self, active: bool) {
        if let Err(message) = self
            .bottom
            .open_model(&self.catalog, self.selection.as_ref(), active)
        {
            self.notice(message);
        }
    }
    pub fn notice(&mut self, message: impl Into<String>) {
        self.transcript.push(message, Some(Color::DarkGray));
    }
    pub fn warning(&mut self, message: &str) {
        self.transcript.warning(message);
    }
    pub fn error(&mut self, message: &str) {
        self.transcript.error(message);
    }
    pub fn success(&mut self, message: &str) {
        self.transcript.success(message);
    }
    pub fn user_prompt(&mut self, prompt: &str) {
        self.transcript.user(prompt);
    }
    pub fn agent_message(&mut self, text: &str, final_answer: bool) {
        self.transcript.agent(text, final_answer);
    }
    pub fn tool_event(&mut self, method: &str, item: &Value) {
        self.transcript.tool(method, item);
    }
    pub fn steer(&mut self, message: &str, automatic: bool) {
        self.transcript.steer(message, automatic);
    }
    pub fn turn_completed(&mut self) {
        self.bottom.reset_confirmation();
    }
    pub fn clear_thread(&mut self) {
        self.bottom.reset_confirmation();
        self.transcript.boundary();
    }
    pub fn restore_input(&mut self, text: &str) {
        self.bottom.editor.replace(text);
    }
    pub fn logs(&mut self, logger: &Logger) {
        let lines = logger
            .recent
            .iter()
            .rev()
            .take(25)
            .rev()
            .cloned()
            .collect::<Vec<_>>();
        self.transcript.push(
            format!(
                "\nSession log (last {})\n{}\n",
                lines.len(),
                lines.join("\n")
            ),
            Some(Color::DarkGray),
        );
    }
    pub fn status(&mut self, ctx: &StatusContext<'_>) {
        self.transcript
            .push(status::detail(ctx, self.selection.as_ref()), None);
    }
    pub fn handle_event(&mut self, event: Event, active: bool) -> Option<String> {
        match self.bottom.handle(event, active) {
            PaneEvent::Submit(text) => return Some(text),
            PaneEvent::Notice(text) => self.notice(text),
            PaneEvent::ModelChanged(selection) => {
                self.notice(format!(
                    "Model changed to {}{}",
                    selection.display_name,
                    selection
                        .effort
                        .as_deref()
                        .map(|e| format!(" · {e}"))
                        .unwrap_or_default()
                ));
                self.selection = Some(selection);
            }
            PaneEvent::None => {}
        }
        None
    }
    pub fn draw(&mut self, ctx: &StatusContext<'_>) -> Result<()> {
        let size = self.terminal.size()?;
        inline::sync_bottom(&mut self.terminal, self.bottom.height(size.height), |_| {
            CrosstermBackend::new(io::stdout())
        })?;
        inline::append_pending(&mut self.terminal, &mut self.transcript)?;
        let footer = status::compact(ctx, self.selection.as_ref(), size.width);
        self.terminal.draw(|frame| {
            self.bottom.render(
                frame,
                &footer,
                size.height,
                matches!(ctx.lifecycle, "RUNNING" | "STOPPING"),
            )
        })?;
        Ok(())
    }
}
fn cleanup_terminal() {
    let _ = execute!(io::stdout(), DisableBracketedPaste, Show);
    let _ = disable_raw_mode();
}
impl Drop for Tui {
    fn drop(&mut self) {
        let _ = self.terminal.clear();
        cleanup_terminal();
        if !std::thread::panicking() {
            if let Some(hook) = self.previous_hook.take() {
                panic::set_hook(hook);
            }
        }
    }
}
