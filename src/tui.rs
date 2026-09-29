use crate::{
    app_server::{RateSnapshot, ThreadUsage},
    logging::Logger,
    policy::Policy,
};
use anyhow::Result;
use crossterm::{
    event::{KeyCode, KeyEvent, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Terminal,
};
use std::{
    io::{self, Stdout},
    time::Instant,
};

pub struct Tui {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    input: String,
    confirm: Option<Confirm>,
    logs_expanded: bool,
    notice: String,
    agent_text: String,
    agent_is_final: bool,
    message_scroll: u16,
}
#[derive(Clone, Copy)]
enum Confirm {
    Interrupt,
    Kill,
    Quit,
}

impl Tui {
    pub fn new() -> Result<Self> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen)?;
        let terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        Ok(Self {
            terminal,
            input: String::new(),
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
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<String> {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        match key.code {
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL) =>
            {
                self.input.push(c)
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Esc => {
                self.input.clear();
                self.confirm = None;
            }
            KeyCode::PageDown => {
                self.message_scroll = self.message_scroll.saturating_add(5);
            }
            KeyCode::PageUp => {
                self.message_scroll = self.message_scroll.saturating_sub(5);
            }
            KeyCode::Enter => {
                let entered = std::mem::take(&mut self.input);
                if let Some(confirm) = self.confirm.take() {
                    let accepted = match confirm {
                        Confirm::Interrupt => entered.eq_ignore_ascii_case("y"),
                        Confirm::Kill => entered == "kill",
                        Confirm::Quit => entered == "quit",
                    };
                    if accepted {
                        return Some(
                            match confirm {
                                Confirm::Interrupt => "/interrupt-confirmed",
                                Confirm::Kill => "/kill-confirmed",
                                Confirm::Quit => "/quit-confirmed",
                            }
                            .into(),
                        );
                    }
                    self.notice = "Cancelled".into();
                    return None;
                }
                match entered.as_str() {
                    "/interrupt" => {
                        self.confirm = Some(Confirm::Interrupt);
                    }
                    "/kill" => {
                        self.confirm = Some(Confirm::Kill);
                    }
                    "/quit" => {
                        self.confirm = Some(Confirm::Quit);
                    }
                    "/logs" => {
                        self.logs_expanded = !self.logs_expanded;
                    }
                    "" => {}
                    _ => return Some(entered),
                }
            }
            _ => {}
        }
        None
    }
    pub fn draw(
        &mut self,
        policy: &Policy,
        profile: &str,
        mode: &str,
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
            None => ">",
        };
        let input = self.input.clone();
        let notice = self.notice.clone();
        let agent_text = if self.agent_text.is_empty() {
            "Waiting for agent message...".to_owned()
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
            profile,
            mode,
            started,
            thread_usage,
            tools_done,
            tools_running,
        );
        self.terminal.draw(|frame| {
            let vertical = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(12),
                    Constraint::Min(5),
                    Constraint::Length(if self.logs_expanded { 7 } else { 3 }),
                    Constraint::Length(4),
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
            frame.render_widget(
                Paragraph::new(rows.join("\n"))
                    .block(Block::default().title(log_title).borders(Borders::ALL))
                    .wrap(Wrap { trim: true }),
                vertical[2],
            );
            let command = Paragraph::new(vec![
                Line::from(vec![
                    Span::styled(
                        prompt,
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!(" {input}")),
                ]),
                Line::from(notice),
            ])
            .block(Block::default().borders(Borders::ALL));
            frame.render_widget(command, vertical[3]);
        })?;
        Ok(())
    }
}
fn status_lines(
    snapshot: &RateSnapshot,
    policy: &Policy,
    profile: &str,
    mode: &str,
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
    let text = format!("Status       {} · {profile} · {mode} · {}s\n5h quota     {primary} · {reset}\nWeekly       {weekly}\nBilling      {billing}\nCredits      {:.2} / {:.2} task budget\nAccount      {:.2}\nThread est.  {usage}\nModel/usage  {breakdown}\nLast steer   {}\nTools seen   {tools_done} completed · {tools_running} running", policy.phase.label(), started.elapsed().as_secs(), policy.spent, policy.profile.credits.max_spend, policy.balance(), policy.last_steer.unwrap_or("—"));
    text.lines()
        .map(|line| Line::from(line.to_owned()))
        .collect()
}
fn u_groups_more(usage: Option<&ThreadUsage>) -> bool {
    usage.is_some_and(|u| u.groups.len() > 1)
}
impl Drop for Tui {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}
