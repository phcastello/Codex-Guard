use crate::{
    app_server::{RateSnapshot, ThreadUsage},
    logging::Logger,
    policy::Policy,
};
use anyhow::Result;
use crossterm::{
    event::{
        DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders, Paragraph, Wrap},
    Terminal,
};
use std::{
    io::{self, Stdout},
    time::Instant,
};
use tui_textarea::TextArea;

pub struct Tui {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    editor: Editor,
    confirm: Option<Confirm>,
    logs_expanded: bool,
    notice: String,
    agent_text: String,
    agent_is_final: bool,
    message_scroll: u16,
}
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
}

fn confirmation_for(command: &str, active_turn: bool) -> Option<Confirm> {
    match command {
        "/interrupt" if active_turn => Some(Confirm::Interrupt),
        "/kill" => Some(Confirm::Kill),
        "/quit" if active_turn => Some(Confirm::Quit),
        "/clear" if active_turn => Some(Confirm::Clear),
        _ => None,
    }
}

struct Editor {
    area: TextArea<'static>,
}
impl Editor {
    fn new() -> Self {
        let mut area = TextArea::default();
        area.set_cursor_style(Style::default().fg(Color::Black).bg(Color::Yellow));
        area.set_cursor_line_style(Style::default());
        Self { area }
    }
    fn text(&self) -> String {
        self.area.lines().join("\n")
    }
    fn replace(&mut self, text: &str) {
        *self = Self::new();
        self.area.insert_str(text);
    }
    fn clear(&mut self) {
        *self = Self::new();
    }
    fn handle(&mut self, event: Event) -> Option<String> {
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
                // Crossterm's legacy Windows reader can expose a paste as ordinary
                // key events. An unmodified Enter can therefore never safely
                // submit: every Enter, including pasted CR/LF, is a newline.
                KeyCode::Enter | KeyCode::Char('\n' | '\r') => self.area.insert_newline(),
                _ => {
                    self.area.input(key);
                }
            },
            _ => {}
        }
        None
    }
}

fn input_height(lines: usize, height: u16) -> u16 {
    if height < 3 {
        return height;
    }
    let ceiling = ((height as u32 * 40) / 100).max(3) as u16;
    u16::try_from(lines)
        .unwrap_or(u16::MAX)
        .saturating_add(3)
        .clamp(3, ceiling.min(height))
}

impl Tui {
    pub fn new() -> Result<Self> {
        enable_raw_mode()?;
        if let Err(error) = execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste) {
            let _ = execute!(io::stdout(), DisableBracketedPaste, LeaveAlternateScreen);
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        let terminal = match Terminal::new(CrosstermBackend::new(io::stdout())) {
            Ok(terminal) => terminal,
            Err(error) => {
                let _ = execute!(io::stdout(), DisableBracketedPaste, LeaveAlternateScreen);
                let _ = disable_raw_mode();
                return Err(error.into());
            }
        };
        Ok(Self {
            terminal,
            editor: Editor::new(),
            confirm: None,
            logs_expanded: false,
            notice: String::new(),
            agent_text: String::new(),
            agent_is_final: false,
            message_scroll: 0,
        })
    }
    pub fn notice(&mut self, message: impl Into<String>) {
        self.notice = message.into();
    }
    pub fn agent_message(&mut self, text: &str, is_final: bool) {
        self.agent_text = text.to_owned();
        self.agent_is_final = is_final;
        self.message_scroll = 0;
    }
    pub fn turn_completed(&mut self) {
        self.confirm = None;
    }
    pub fn clear_thread(&mut self) {
        self.agent_text.clear();
        self.agent_is_final = false;
        self.message_scroll = 0;
        self.confirm = None;
        self.notice("Thread cleared. Financial session budget is unchanged.");
    }
    pub fn restore_input(&mut self, text: &str) {
        self.editor.replace(text);
    }
    pub fn handle_event(&mut self, event: Event, active_turn: bool) -> Option<String> {
        match &event {
            Event::Key(key) if key.kind == KeyEventKind::Press && key.code == KeyCode::PageDown => {
                self.message_scroll = self.message_scroll.saturating_add(5);
                return None;
            }
            Event::Key(key) if key.kind == KeyEventKind::Press && key.code == KeyCode::PageUp => {
                self.message_scroll = self.message_scroll.saturating_sub(5);
                return None;
            }
            Event::Key(key)
                if key.kind == KeyEventKind::Press
                    && key.code == KeyCode::Esc
                    && self.confirm.is_some() =>
            {
                self.confirm = None;
                self.editor.clear();
                self.notice("Cancelled");
                return None;
            }
            _ => {}
        }
        if let Some(entered) = self.editor.handle(event) {
            if let Some(confirm) = self.confirm.take() {
                if confirm.accepts(&entered) {
                    return Some(confirm.command().into());
                }
                self.notice = "Cancelled".into();
                return None;
            }
            if let Some(confirm) = confirmation_for(entered.trim(), active_turn) {
                self.confirm = Some(confirm);
            } else if entered.trim() == "/logs" {
                self.logs_expanded = !self.logs_expanded;
            } else if matches!(
                entered.trim(),
                "/interrupt-confirmed" | "/kill-confirmed" | "/quit-confirmed" | "/clear-confirmed"
            ) {
                self.notice("Use the confirmation prompt for that action.");
            } else if !entered.trim().is_empty() {
                return Some(entered);
            }
        }
        None
    }
    pub fn draw(
        &mut self,
        policy: &Policy,
        lifecycle: &str,
        profile: &str,
        mode: &str,
        codex_mode: crate::config::CodexMode,
        started: Instant,
        logger: &Logger,
        thread_usage: Option<&ThreadUsage>,
        tools_done: u64,
        tools_running: u64,
    ) -> Result<()> {
        let snapshot = &policy.latest;
        let prompt = match self.confirm {
            Some(Confirm::Interrupt) => "Interrupt current turn? [y/N]",
            Some(Confirm::Kill) => {
                "Kill the Codex process and all child processes? Type \"kill\" to confirm:"
            }
            Some(Confirm::Quit) => "Quit and interrupt the active task? Type \"quit\" to confirm:",
            Some(Confirm::Clear) => {
                "Clear thread and interrupt the active task? Type \"clear\" to confirm:"
            }
            None => " Prompt / command · Enter newline · Ctrl+D or F2 send ",
        };
        let notice = self.notice.clone();
        let agent_text = if self.agent_text.is_empty() {
            "No active task. Type a prompt below; Ctrl+D or F2 sends it.".to_owned()
        } else {
            self.agent_text.clone()
        };
        let agent_title = if self.agent_is_final {
            " Final agent message (PgUp/PgDn) "
        } else {
            " Agent message (PgUp/PgDn) "
        };
        let message_scroll = self.message_scroll;
        let rows: Vec<String> = logger
            .recent
            .iter()
            .rev()
            .take(if self.logs_expanded { 25 } else { 5 })
            .cloned()
            .collect();
        let status = status_lines(
            snapshot,
            policy,
            lifecycle,
            profile,
            mode,
            codex_mode,
            started,
            thread_usage,
            tools_done,
            tools_running,
        );
        let editor = &mut self.editor.area;
        self.terminal.draw(|frame| {
            let height = frame.area().height;
            let input_rows = input_height(editor.lines().len(), height);
            let logs_rows = if height >= 30 {
                if self.logs_expanded {
                    7
                } else {
                    3
                }
            } else {
                0
            };
            let status_target = if height >= 28 {
                14
            } else if height >= 18 {
                8
            } else {
                4
            };
            let status_rows = status_target
                .min(height.saturating_sub(input_rows).saturating_sub(3))
                .max(1);
            let vertical = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(status_rows),
                    Constraint::Min(1),
                    Constraint::Length(logs_rows),
                    Constraint::Length(input_rows),
                ])
                .split(frame.area());
            let title = format!(" Codex Guard · {} · {} ", profile, mode);
            frame.render_widget(
                Paragraph::new(status)
                    .block(Block::default().title(title).borders(Borders::ALL))
                    .wrap(Wrap { trim: true }),
                vertical[0],
            );
            let log_title = if self.logs_expanded {
                " Events /logs (expanded) "
            } else {
                " Recent events /logs "
            };
            frame.render_widget(
                Paragraph::new(agent_text)
                    .block(Block::default().title(agent_title).borders(Borders::ALL))
                    .wrap(Wrap { trim: false })
                    .scroll((message_scroll, 0)),
                vertical[1],
            );
            if logs_rows > 0 {
                frame.render_widget(
                    Paragraph::new(rows.join("\n"))
                        .block(Block::default().title(log_title).borders(Borders::ALL))
                        .wrap(Wrap { trim: true }),
                    vertical[2],
                );
            }
            let input_rect = ratatui::layout::Rect::new(
                vertical[3].x,
                vertical[3].y,
                vertical[3].width,
                vertical[3].height.saturating_sub(1),
            );
            editor.set_block(Block::default().title(prompt).borders(Borders::ALL));
            frame.render_widget(&*editor, input_rect);
            let notice_rect = ratatui::layout::Rect::new(
                vertical[3].x,
                vertical[3].bottom().saturating_sub(1),
                vertical[3].width,
                1,
            );
            frame.render_widget(Paragraph::new(notice), notice_rect);
            let inner = Block::default().borders(Borders::ALL).inner(input_rect);
            // The editor owns viewport scrolling and Unicode cell widths.
            for y in inner.y..inner.bottom() {
                for x in inner.x..inner.right() {
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
        })?;
        Ok(())
    }
}
fn status_lines(
    snapshot: &RateSnapshot,
    policy: &Policy,
    lifecycle: &str,
    profile: &str,
    mode: &str,
    codex_mode: crate::config::CodexMode,
    started: Instant,
    thread_usage: Option<&ThreadUsage>,
    tools_done: u64,
    tools_running: u64,
) -> Vec<Line<'static>> {
    let primary = snapshot
        .primary
        .as_ref()
        .map(|w| format!("{:.0}% used · {:.0}% remaining", w.used, 100.0 - w.used))
        .unwrap_or_else(|| "unavailable".into());
    let weekly = snapshot
        .secondary
        .as_ref()
        .map(|w| format!("{:.0}% used", w.used))
        .unwrap_or_else(|| "unavailable".into());
    let reset = snapshot
        .primary
        .as_ref()
        .and_then(|w| w.resets_at)
        .map(|at| {
            let seconds = (at - chrono::Utc::now().timestamp()).max(0) as u64;
            format!(
                "reset in {:02}:{:02}:{:02}",
                seconds / 3600,
                (seconds / 60) % 60,
                seconds % 60
            )
        })
        .unwrap_or_else(|| "reset unknown".into());
    let billing = if policy.paid_now() {
        "PAID CREDITS (observed)"
    } else if snapshot.ordinary_usage_allowed == Some(false) {
        "INCLUDED EXHAUSTED / awaiting credit sample"
    } else {
        "INCLUDED"
    };
    let usage = thread_usage
        .map(|u| {
            let credits = u.estimated_usage_credits_micros as f64 / 1_000_000.0;
            let usd = u
                .estimated_usage_usd_micros
                .map(|x| format!(" · ${:.4}", x as f64 / 1_000_000.0))
                .unwrap_or_default();
            format!("{credits:.3} credits{usd}")
        })
        .unwrap_or_else(|| "unavailable".into());
    let breakdown = thread_usage
        .and_then(|u| u.groups.first())
        .map(|g| {
            format!(
                "{} · {} · {} tokens{}",
                g.model.as_deref().unwrap_or("unknown model"),
                g.reasoning_effort.as_deref().unwrap_or("unknown effort"),
                g.total_tokens
                    .map(|x| x.to_string())
                    .unwrap_or_else(|| "?".into()),
                if u_groups_more(thread_usage) {
                    " (+more groups)"
                } else {
                    ""
                }
            )
        })
        .unwrap_or_else(|| "unavailable".into());
    let text = format!("Status       {lifecycle} · {profile} · {mode} · {}s\nPolicy       {}\n5h quota     {primary} · {reset}\nWeekly       {weekly}\nBilling      {billing}\nCredits      {:.2} / {:.2} session budget\nAccount      {:.2}\nThread est.  {usage}\nModel/usage  {breakdown}\nLast steer   {}\nTools seen   {tools_done} completed · {tools_running} running", started.elapsed().as_secs(), policy.phase.label(), policy.spent, policy.profile.credits.max_spend, policy.balance(), policy.last_steer.unwrap_or("—"));
    let mut lines: Vec<_> = text
        .lines()
        .map(|line| Line::from(line.to_owned()))
        .collect();
    let label = match codex_mode {
        crate::config::CodexMode::Inherit => "Codex mode   INHERIT",
        crate::config::CodexMode::Yolo => "Codex mode   YOLO · no sandbox / no approvals",
    };
    lines.insert(
        1,
        Line::styled(
            label,
            if codex_mode == crate::config::CodexMode::Yolo {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default()
            },
        ),
    );
    let credits = lines.remove(6);
    let billing = lines.remove(5);
    let quota = lines.remove(3);
    lines.insert(2, credits);
    lines.insert(2, billing);
    lines.insert(2, quota);
    lines
}
fn u_groups_more(usage: Option<&ThreadUsage>) -> bool {
    usage.is_some_and(|u| u.groups.len() > 1)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;
    use ratatui::backend::TestBackend;
    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }
    #[test]
    fn idle_interrupt_and_quit_do_not_request_confirmation() {
        assert_eq!(confirmation_for("/interrupt", false), None);
        assert_eq!(confirmation_for("/quit", false), None);
        assert_eq!(
            confirmation_for("/interrupt", true),
            Some(Confirm::Interrupt)
        );
        assert_eq!(confirmation_for("/quit", true), Some(Confirm::Quit));
        assert_eq!(confirmation_for("/kill", false), Some(Confirm::Kill));
        assert_eq!(confirmation_for("/clear", false), None);
        assert_eq!(confirmation_for("/clear", true), Some(Confirm::Clear));
        assert!(!Confirm::Clear.accepts("y"));
        assert!(Confirm::Clear.accepts("clear"));
        assert_eq!(Confirm::Clear.command(), "/clear-confirmed");
    }
    #[test]
    fn paste_and_enter_edit_text_until_explicit_submit() {
        let mut editor = Editor::new();
        assert_eq!(
            editor.handle(Event::Paste("alpha\r\n\r\nbeta".into())),
            None
        );
        assert_eq!(editor.text(), "alpha\n\nbeta");
        assert_eq!(editor.handle(key(KeyCode::Enter, KeyModifiers::ALT)), None);
        editor.handle(Event::Paste("gamma".into()));
        assert_eq!(editor.handle(key(KeyCode::Enter, KeyModifiers::NONE)), None);
        editor.handle(Event::Paste("delta".into()));
        assert_eq!(
            editor.handle(key(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            Some("alpha\n\nbeta\ngamma\ndelta".into())
        );
        assert_eq!(editor.text(), "");
    }
    #[test]
    fn unbracketed_multiline_paste_keys_cannot_submit() {
        let mut editor = Editor::new();
        for event in [
            key(KeyCode::Char('a'), KeyModifiers::NONE),
            key(KeyCode::Enter, KeyModifiers::NONE),
            key(KeyCode::Char('b'), KeyModifiers::NONE),
            key(KeyCode::Enter, KeyModifiers::CONTROL), // Seen during Windows paste.
            key(KeyCode::Char('c'), KeyModifiers::NONE),
        ] {
            assert_eq!(editor.handle(event), None);
        }
        assert_eq!(editor.text(), "a\nb\nc");
        assert_eq!(
            editor.handle(key(KeyCode::F(2), KeyModifiers::NONE)),
            Some("a\nb\nc".into())
        );
    }
    #[test]
    fn unicode_navigation_and_large_prompt_remain_editable() {
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
        assert!(editor.text().len() > 500);
        assert!(input_height(100, 40) <= 16);
        assert_eq!(input_height(1, 40), 4);
    }
    #[test]
    fn scrolled_editor_renders_a_visible_cursor_cell() {
        let mut editor = Editor::new();
        editor.handle(Event::Paste(
            (0..40)
                .map(|i| format!("line {i} — 🙂"))
                .collect::<Vec<_>>()
                .join("\n"),
        ));
        let mut terminal = Terminal::new(TestBackend::new(20, 5)).unwrap();
        terminal
            .draw(|frame| {
                let rect = frame.area();
                frame.render_widget(&editor.area, rect);
                assert!(
                    (rect.y..rect.bottom()).any(|y| (rect.x..rect.right()).any(|x| frame
                        .buffer_mut()
                        .cell((x, y))
                        .is_some_and(|cell| cell.style().bg == Some(Color::Yellow))))
                );
            })
            .unwrap();
    }
}
impl Drop for Tui {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), DisableBracketedPaste, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}
