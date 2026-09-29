mod app_server;
mod cli;
mod config;
mod logging;
mod policy;
mod supervisor;
mod tui;

use anyhow::{Context, Result};
use app_server::{Client, ThreadUsage};
use clap::Parser;
use cli::{Cli, Command, ConfigCommand};
use config::Loaded;
use crossterm::event::EventStream;
use futures_util::StreamExt;
use logging::Logger;
use policy::{Action, Policy};
use serde_json::{json, Value};
use std::{
    future::Future,
    time::{Duration, Instant},
};
use supervisor::ProcessSupervisor;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    sync::mpsc,
    task::JoinHandle,
    time::{interval, MissedTickBehavior},
};
use tui::Tui;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let loaded = config::load(
        cli.run.profile.as_deref(),
        cli.run.credits,
        cli.run.time.as_deref(),
        cli.run.attended,
        cli.run.no_bell,
    )?;
    match cli.command {
        Some(Command::Config {
            command: ConfigCommand::Path,
        }) => {
            println!("{}", loaded.path.display());
            return Ok(());
        }
        Some(Command::Config {
            command: ConfigCommand::Show,
        }) => {
            println!(
                "# profile: {} ({})\n{}",
                loaded.profile_name,
                loaded.mode,
                toml::to_string_pretty(&loaded.profile)?
            );
            return Ok(());
        }
        Some(Command::Profiles) => {
            for name in &loaded.names {
                println!("{name}");
            }
            return Ok(());
        }
        None => {}
    }
    let prompt = cli.run.prompt.join(" ");
    let initial_prompt = (!prompt.trim().is_empty()).then_some(prompt);
    run(loaded, initial_prompt).await
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionState {
    Ready,
    Running,
    Stopping,
    Completed,
}
impl SessionState {
    fn active(self) -> bool {
        matches!(self, Self::Running | Self::Stopping)
    }
    fn label(self) -> &'static str {
        match self {
            Self::Ready => "READY",
            Self::Running => "RUNNING",
            Self::Stopping => "STOPPING",
            Self::Completed => "COMPLETED",
        }
    }
    fn turn_started(&mut self) {
        debug_assert!(!self.active());
        *self = Self::Running;
    }
    fn turn_completed(&mut self) {
        debug_assert!(self.active());
        *self = Self::Completed;
    }
}

#[derive(Debug, PartialEq, Eq)]
enum UserInput {
    Prompt(String),
    Command(String),
    SteerRequired,
    Empty,
}
fn classify_input(text: String, state: SessionState) -> UserInput {
    let text = text.trim().to_owned();
    if text.is_empty() {
        UserInput::Empty
    } else if text.starts_with('/') {
        UserInput::Command(text)
    } else if state.active() {
        UserInput::SteerRequired
    } else {
        UserInput::Prompt(text)
    }
}

struct Stop {
    reason: String,
    at: tokio::time::Instant,
    stage: u8,
    interrupt: Option<JoinHandle<Result<()>>>,
}

#[derive(Debug, PartialEq, Eq)]
enum StopStep {
    Graceful,
    Force,
}

impl Stop {
    fn new(
        reason: String,
        at: tokio::time::Instant,
        interrupt: impl Future<Output = Result<()>> + Send + 'static,
    ) -> Self {
        // The deadline starts at detection, before the interrupt RPC can wait for
        // TurnAborted. The RPC remains a request with its own id, not a notification.
        let interrupt = tokio::spawn(interrupt);
        Self {
            reason,
            at,
            stage: 0,
            interrupt: Some(interrupt),
        }
    }

    fn next_step(
        &mut self,
        interrupt_grace: Duration,
        terminate_grace: Duration,
    ) -> Option<StopStep> {
        if self.stage == 0 && self.at.elapsed() >= interrupt_grace {
            self.stage = 1;
            self.at = tokio::time::Instant::now();
            Some(StopStep::Graceful)
        } else if self.stage == 1 && self.at.elapsed() >= terminate_grace {
            self.stage = 2;
            Some(StopStep::Force)
        } else {
            None
        }
    }
}

async fn run(loaded: Loaded, initial_prompt: Option<String>) -> Result<()> {
    let (mut process, stdin, stdout, stderr) = ProcessSupervisor::spawn()?;
    let (client, mut events, mut activity) = Client::new(stdin, stdout);
    let (stderr_tx, mut stderr_rx) = mpsc::channel::<String>(64);
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if stderr_tx.send(line).await.is_err() {
                break;
            }
        }
    });
    let startup: Result<Policy> = async {
        client.initialize().await?;
        let first = client.rate_limits().await?;
        Policy::new(loaded.profile.clone(), first)
    }
    .await;
    let mut policy = match startup {
        Ok(x) => x,
        Err(error) => {
            let _ = process.force_kill_tree().await;
            return Err(error);
        }
    };
    let mut logger = match Logger::new() {
        Ok(x) => x,
        Err(error) => {
            let _ = process.force_kill_tree().await;
            return Err(error);
        }
    };
    logger.record(
        "session_start",
        json!({"profile":loaded.profile_name,"mode":loaded.mode,"pid":process.id()}),
    );
    logger.record(
        "credit_snapshot",
        json!({"balance":policy.balance(),"spent":policy.spent}),
    );
    logger.record("quota_snapshot", quota_json(&policy.latest));
    logger.record("session_settings", json!({"approval_policy":loaded.profile.session.approval_policy,"sandbox":loaded.profile.session.sandbox,"attended_partial":loaded.mode == "ATTENDED"}));
    let mut ui = match Tui::new() {
        Ok(x) => x,
        Err(error) => {
            let _ = process.force_kill_tree().await;
            return Err(error);
        }
    };
    let mut input = EventStream::new();
    let started = Instant::now();
    let mut redraw = interval(Duration::from_millis(200));
    redraw.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut next_poll = Box::pin(tokio::time::sleep(config::duration(
        &policy.profile.monitor.poll_interval,
    )));
    let (usage_tx, mut usage_rx) = mpsc::channel(2);
    let mut usage_task: Option<JoinHandle<()>> = None;
    let mut thread: Option<String> = None;
    let mut turn: Option<String> = None;
    let mut state = SessionState::Ready;
    let mut turn_started: Option<Instant> = None;
    let mut turns = 0_u32;
    let mut quit_after_stop = false;
    let mut stop: Option<Stop> = None;
    let mut consecutive_poll_failures = 0_u32;
    let mut result: Option<String> = None;
    let mut tools_done = 0_u64;
    let mut tools_running = 0_u64;
    let mut thread_usage: Option<ThreadUsage> = None;
    let mut process_exit_seen: Option<Instant> = None;
    if let Some(prompt) = initial_prompt {
        if let Err(error) = start_turn(
            &client,
            &loaded,
            &mut policy,
            &mut logger,
            &mut ui,
            &mut thread,
            &mut turn,
            &mut stop,
            &mut state,
            &mut turn_started,
            &mut turns,
            prompt,
        )
        .await
        {
            logger.record("error", json!({"start_turn":error.to_string()}));
            ui.notice(format!("Could not start task: {error}"));
        }
    }
    if let Some(thread_id) = thread.as_ref() {
        usage_task = Some(spawn_usage_poll(
            client.clone(),
            thread_id.clone(),
            usage_tx.clone(),
            config::duration(&policy.profile.monitor.poll_interval),
        ));
    }
    next_poll
        .as_mut()
        .reset(tokio::time::Instant::now() + session_poll_interval(&policy, state));

    loop {
        tokio::select! {
            _ = redraw.tick() => {
                if state == SessionState::Running && turn_started.is_some_and(|at| at.elapsed() >= config::duration(&policy.profile.runtime.max)) {
                    if let (Some(thread_id), Some(turn_id)) = (thread.as_deref(), turn.as_deref()) {
                        begin_stop(&client, &mut policy, &mut logger, &mut stop, thread_id, turn_id, "runtime limit reached".into());
                        state = SessionState::Stopping;
                    }
                }
                if let Some(s) = stop.as_mut() {
                    if s.interrupt.as_ref().is_some_and(JoinHandle::is_finished) {
                        if let Some(handle) = s.interrupt.take() {
                            match handle.await {
                                Ok(Ok(())) => {},
                                Ok(Err(error)) => logger.record("error", json!({"interrupt":error.to_string()})),
                                Err(error) => logger.record("error", json!({"interrupt_task":error.to_string()})),
                            }
                        }
                    }
                    match s.next_step(
                        config::duration(&policy.profile.runtime.interrupt_grace),
                        config::duration(&policy.profile.runtime.terminate_grace),
                    ) {
                        Some(StopStep::Graceful) => {
                            logger.record("process_termination", json!({"stage":"graceful","reason":s.reason}));
                            if let Err(error) = process.graceful_terminate().await { logger.record("error", json!({"graceful_termination":error.to_string()})); }
                        }
                        Some(StopStep::Force) => {
                            logger.record("process_termination", json!({"stage":"force","reason":s.reason}));
                            process.force_kill_tree().await?;
                            result = Some(format!("killed: {}", s.reason));
                            break;
                        }
                        None => {},
                    }
                }
                if process.try_wait()?.is_some() && result.is_none() {
                    let since = process_exit_seen.get_or_insert_with(Instant::now);
                    if since.elapsed() >= Duration::from_secs(5) {
                        result = Some("App Server exited unexpectedly".into());
                        break;
                    }
                }
                ui.draw(&policy, state.label(), &loaded.profile_name, &loaded.mode, started, &logger, thread_usage.as_ref(), tools_done, tools_running)?;
            }
            _ = &mut next_poll, if stop.is_none() => {
                let success = if let (Some(thread_id), Some(turn_id)) = (thread.as_deref(), turn.as_deref()) {
                    reconcile_rate_limits(&client, &mut policy, &mut logger, &mut ui, &mut stop, thread_id, turn_id, &mut consecutive_poll_failures).await
                } else {
                    reconcile_idle(&client, &mut policy, &mut logger, &mut ui).await
                };
                if stop.is_some() { state = SessionState::Stopping; }
                let delay = if success { session_poll_interval(&policy, state) } else { config::duration(&policy.profile.monitor.retry_interval) };
                next_poll.as_mut().reset(tokio::time::Instant::now() + delay);
            }
            maybe = events.recv() => {
                let Some(event) = maybe else { result = Some("App Server event stream closed".into()); break; };
                match event.method.as_str() {
                    "turn/completed" => {
                        let event_turn = event.params.pointer("/turn/id").and_then(Value::as_str);
                        if event_turn == turn.as_deref() && turn.is_some() {
                            let status = event.params.pointer("/turn/status").and_then(Value::as_str).unwrap_or("unknown");
                            let error = event.params.pointer("/turn/error/message").and_then(Value::as_str);
                            logger.record("turn_completion", json!({"thread":thread,"turn":turn,"status":status,"error":error}));
                            ui.notice(match error { Some(error) => format!("Turn {status}: {error}"), None => format!("Turn {status}. Enter another prompt or /quit.") });
                            // Reconcile the last debit while the completed turn still owns it.
                            match client.rate_limits().await {
                                Ok(snapshot) => {
                                    logger.record("quota_snapshot", quota_json(&snapshot));
                                    let _ = policy.observe(snapshot);
                                    logger.record("credit_snapshot", json!({"balance":policy.balance(),"session_spent":policy.spent}));
                                }
                                Err(error) => logger.record("warning", json!({"turn_final_reconciliation":error.to_string()})),
                            }
                            policy.finish_turn();
                            ui.turn_completed();
                            turn = None;
                            turn_started = None;
                            stop = None;
                            state.turn_completed();
                            next_poll.as_mut().reset(tokio::time::Instant::now() + session_poll_interval(&policy, state));
                            if quit_after_stop { result = Some("quit after interrupt".into()); break; }
                        }
                    }
                    "account/rateLimits/updated" => {
                        client.ack_rate_update();
                        if stop.is_none() {
                            let success = if let (Some(thread_id), Some(turn_id)) = (thread.as_deref(), turn.as_deref()) {
                                reconcile_rate_limits(&client, &mut policy, &mut logger, &mut ui, &mut stop, thread_id, turn_id, &mut consecutive_poll_failures).await
                            } else {
                                reconcile_idle(&client, &mut policy, &mut logger, &mut ui).await
                            };
                            if stop.is_some() { state = SessionState::Stopping; }
                            let delay = if success { session_poll_interval(&policy, state) } else { config::duration(&policy.profile.monitor.retry_interval) };
                            next_poll.as_mut().reset(tokio::time::Instant::now() + delay);
                        }
                    }
                    "item/completed" => {
                        if let Some((message, is_final)) = agent_message(&event.params) {
                            logger.record("agent_message", json!({"phase":if is_final {"final_answer"} else {"commentary"},"text":message}));
                            ui.agent_message(&message, is_final);
                        }
                    }
                    "guard/unsupportedRequest" => {
                        let method = event.params.get("method").and_then(Value::as_str).unwrap_or("unknown");
                        logger.record("error", json!({"unsupported_server_request":method}));
                        if let (Some(thread_id), Some(turn_id)) = (thread.as_deref(), turn.as_deref()) {
                            begin_stop(&client, &mut policy, &mut logger, &mut stop, thread_id, turn_id, format!("unsupported App Server request: {method}"));
                            state = SessionState::Stopping;
                        }
                    }
                    "guard/disconnected" | "guard/transportError" => { logger.record("error", json!({"transport":event.params})); result = Some("App Server disconnected".into()); break; }
                    "error" => logger.record("error", event.params),
                    _ => {},
                }
            }
            maybe = activity.recv(), if !activity.is_closed() => {
                if let Some(event) = maybe {
                    if let Some(item) = event.params.get("item") {
                        let kind = item.get("type").and_then(Value::as_str).unwrap_or("item");
                        if event.method == "item/started" { tools_running += 1; }
                        else { tools_done += 1; tools_running = tools_running.saturating_sub(1); }
                        let detail = item.get("command").or_else(|| item.get("changes")).unwrap_or(item).to_string();
                        logger.record(if event.method == "item/started" { "tool_start" } else { "tool_complete" }, json!({"type":kind,"detail":detail.chars().take(500).collect::<String>()}));
                    }
                }
            }
            maybe = usage_rx.recv(), if !usage_rx.is_closed() => {
                match maybe {
                    Some(Ok(Some(usage))) => { logger.record("thread_usage", json!(usage)); thread_usage = Some(usage); }
                    Some(Ok(None)) => { /* Optional telemetry is unavailable for this billing route. */ }
                    Some(Err(error)) => { logger.record("warning", json!({"thread_usage":error.to_string()})); }
                    None => {},
                }
            }
            Some(line) = stderr_rx.recv() => { logger.record("app_server_stderr", json!(line.chars().take(500).collect::<String>())); }
            maybe = input.next() => {
                if let Some(Ok(crossterm::event::Event::Key(key))) = maybe {
                    if let Some(text) = ui.handle_key(key, state.active()) {
                        match classify_input(text, state) {
                            UserInput::Prompt(prompt) => {
                                match start_turn(&client, &loaded, &mut policy, &mut logger, &mut ui, &mut thread, &mut turn, &mut stop, &mut state, &mut turn_started, &mut turns, prompt).await {
                                    Ok(()) => {
                                        if usage_task.is_none() {
                                            if let Some(thread_id) = thread.as_ref() {
                                                usage_task = Some(spawn_usage_poll(client.clone(), thread_id.clone(), usage_tx.clone(), config::duration(&policy.profile.monitor.poll_interval)));
                                            }
                                        }
                                        next_poll.as_mut().reset(tokio::time::Instant::now() + session_poll_interval(&policy, state));
                                        tools_done = 0;
                                        tools_running = 0;
                                        consecutive_poll_failures = 0;
                                    }
                                    Err(error) => { logger.record("error", json!({"start_turn":error.to_string()})); ui.notice(format!("Could not start task: {error}")); }
                                }
                            }
                            UserInput::SteerRequired => ui.notice("A task is running. Use /steer <message> to guide the active turn."),
                            UserInput::Command(command) => {
                                if handle_command(&command, &client, &mut process, &mut policy, &mut logger, &mut ui, &mut stop, thread.as_deref(), turn.as_deref(), &loaded, &mut quit_after_stop).await? {
                                    result = Some(if command == "/kill-confirmed" { "killed by user" } else { "closed" }.into());
                                    break;
                                }
                                if stop.is_some() { state = SessionState::Stopping; }
                            }
                            UserInput::Empty => {},
                        }
                    }
                }
            }
        }
    }
    if let Some(task) = usage_task {
        task.abort();
    }
    if process.try_wait()?.is_none() && thread.is_some() {
        match client.thread_usage(thread.as_deref().unwrap()).await {
            Ok(Some(usage)) => {
                logger.record("thread_usage", json!(usage));
                thread_usage = Some(usage);
            }
            Ok(None) => {}
            Err(error) => logger.record("warning", json!({"final_thread_usage":error.to_string()})),
        }
    }
    // Reconcile once more before closing the server so the final summary includes late charges.
    if process.try_wait()?.is_none() {
        match client.rate_limits().await {
            Ok(snapshot) => {
                logger.record("quota_snapshot", quota_json(&snapshot));
                logger.record("credit_snapshot", json!({"balance":snapshot.balance}));
                if turn.is_some() {
                    for action in policy.observe(snapshot) {
                        match action {
                            Action::Billing(label) => {
                                logger.record("billing_transition", json!(label))
                            }
                            Action::QuotaReset => logger.record("quota_reset", json!({})),
                            _ => {}
                        }
                    }
                    policy.finish_turn();
                } else if let Err(error) = policy.observe_idle(snapshot) {
                    logger.record("warning", json!({"final_idle_snapshot":error.to_string()}));
                }
            }
            Err(error) => logger.record("error", json!({"final_reconciliation":error.to_string()})),
        }
    }
    // The server is owned by this Guard session, not by an individual turn.
    if process.try_wait()?.is_none() {
        let _ = client.close_stdin().await;
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && process.try_wait()?.is_none() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if process.try_wait()?.is_none() {
            let _ = process.graceful_terminate().await;
        }
        let deadline = Instant::now() + config::duration(&policy.profile.runtime.terminate_grace);
        while Instant::now() < deadline && process.try_wait()?.is_none() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if process.try_wait()?.is_none() {
            let _ = process.force_kill_tree().await;
        }
    }
    let result = result.unwrap_or_else(|| "closed".into());
    logger.record("summary", json!({"result":result,"turns":turns,"runtime_seconds":started.elapsed().as_secs(),"paid_runtime_seconds":policy.paid_runtime.as_secs(),"credits_spent":policy.spent,"thread_usage":thread_usage,"automatic_steers":policy.automatic_steers,"quota_initial":policy.first.primary.as_ref().map(|x|x.used),"quota_final":policy.latest.primary.as_ref().map(|x|x.used)}));
    drop(ui);
    println!("Session {result}\nRuntime: {}s\nTurns: {turns}\nPaid runtime (sampled): {}s\nCredits spent: {:.2} / {:.2}\n5h usage: {} → {}\nAutomatic steers: {}\nLog: {}", started.elapsed().as_secs(), policy.paid_runtime.as_secs(), policy.spent, policy.profile.credits.max_spend, policy.first.primary.as_ref().map(|x|format!("{:.0}%",x.used)).unwrap_or_else(||"?".into()), policy.latest.primary.as_ref().map(|x|format!("{:.0}%",x.used)).unwrap_or_else(||"?".into()), policy.automatic_steers, logger.path.display());
    Ok(())
}

fn session_poll_interval(policy: &Policy, state: SessionState) -> Duration {
    if state.active() {
        policy.poll_interval()
    } else {
        config::duration(&policy.profile.monitor.poll_interval)
    }
}

fn spawn_usage_poll(
    client: Client,
    thread: String,
    sender: mpsc::Sender<Result<Option<ThreadUsage>>>,
    interval: Duration,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if sender
                .send(client.thread_usage(&thread).await)
                .await
                .is_err()
            {
                break;
            }
            tokio::time::sleep(interval).await;
        }
    })
}

async fn reconcile_idle(
    client: &Client,
    policy: &mut Policy,
    logger: &mut Logger,
    ui: &mut Tui,
) -> bool {
    match client.rate_limits().await {
        Ok(snapshot) => {
            logger.record("quota_snapshot", quota_json(&snapshot));
            match policy.observe_idle(snapshot) {
                Ok(reset) => {
                    logger.record(
                        "credit_snapshot",
                        json!({"balance":policy.balance(),"session_spent":policy.spent}),
                    );
                    if reset {
                        logger.record("quota_reset", json!({}));
                    }
                    true
                }
                Err(error) => {
                    logger.record("warning", json!({"idle_rate_limits":error.to_string()}));
                    ui.notice(format!("Account telemetry unavailable: {error}"));
                    false
                }
            }
        }
        Err(error) => {
            logger.record("warning", json!({"idle_rate_limits":error.to_string()}));
            ui.notice(format!("Rate-limit read failed; retrying: {error}"));
            false
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn start_turn(
    client: &Client,
    loaded: &Loaded,
    policy: &mut Policy,
    logger: &mut Logger,
    ui: &mut Tui,
    thread: &mut Option<String>,
    turn: &mut Option<String>,
    stop: &mut Option<Stop>,
    state: &mut SessionState,
    turn_started: &mut Option<Instant>,
    turns: &mut u32,
    prompt: String,
) -> Result<()> {
    // A READY session may have been open through a quota reset or top-up.
    let snapshot = client.rate_limits().await?;
    logger.record("quota_snapshot", quota_json(&snapshot));
    policy.observe_idle(snapshot)?;
    policy.prepare_turn()?;
    if thread.is_none() {
        let cwd = std::env::current_dir()?.to_string_lossy().into_owned();
        let mut params = json!({"cwd":cwd,"serviceName":"codex_guard"});
        if let Some(value) = &loaded.profile.session.approval_policy {
            params["approvalPolicy"] = json!(value);
        }
        if let Some(value) = &loaded.profile.session.sandbox {
            params["sandbox"] = json!(value);
        }
        let id = client
            .call("thread/start", Some(params))
            .await?
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .context("thread/start did not return thread.id")?
            .to_owned();
        logger.record("thread_start", json!({"thread":id}));
        *thread = Some(id);
    }
    let thread_id = thread.as_deref().context("thread unavailable")?;
    let turn_id = client
        .call(
            "turn/start",
            Some(json!({"threadId":thread_id,"input":[{"type":"text","text":prompt}]})),
        )
        .await?
        .pointer("/turn/id")
        .and_then(Value::as_str)
        .context("turn/start did not return turn.id")?
        .to_owned();
    logger.record(
        "turn_start",
        json!({"thread":thread_id,"turn":turn_id,"prompt":prompt}),
    );
    *turn = Some(turn_id);
    state.turn_started();
    *turn_started = Some(Instant::now());
    *turns += 1;
    ui.notice("Turn started. Use /steer <message> to guide it.");
    let actions = policy.observe(policy.latest.clone());
    apply_actions(
        actions,
        client,
        policy,
        logger,
        ui,
        stop,
        thread_id,
        turn.as_deref().unwrap(),
    )
    .await;
    if stop.is_some() {
        *state = SessionState::Stopping;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cli_accepts_no_prompt_with_or_without_overrides() {
        let plain = Cli::try_parse_from(["codex-guard"]).unwrap();
        assert!(plain.run.prompt.is_empty());
        let override_only =
            Cli::try_parse_from(["codex-guard", "-p", "conservative", "-c", "5", "--attended"])
                .unwrap();
        assert!(override_only.run.prompt.is_empty());
        assert_eq!(override_only.run.profile.as_deref(), Some("conservative"));
    }
    #[test]
    fn ready_prompt_running_text_and_follow_up_routing() {
        let mut state = SessionState::Ready;
        assert_eq!(
            classify_input("fix the bug".into(), state),
            UserInput::Prompt("fix the bug".into())
        );
        state.turn_started();
        assert_eq!(
            classify_input("extra instructions".into(), state),
            UserInput::SteerRequired
        );
        assert_eq!(
            classify_input("/steer finish".into(), SessionState::Running),
            UserInput::Command("/steer finish".into())
        );
        state.turn_completed();
        assert_eq!(
            classify_input("review it".into(), state),
            UserInput::Prompt("review it".into())
        );
        assert_eq!(
            classify_input(" ".into(), SessionState::Ready),
            UserInput::Empty
        );
    }
    #[test]
    fn no_active_turn_commands_remain_commands_without_starting_a_prompt() {
        assert!(!SessionState::Ready.active());
        assert_eq!(
            classify_input("/interrupt".into(), SessionState::Ready),
            UserInput::Command("/interrupt".into())
        );
        assert_eq!(
            classify_input("/quit".into(), SessionState::Ready),
            UserInput::Command("/quit".into())
        );
    }
    #[test]
    fn extracts_full_agent_text_and_final_phase() {
        let text = "Completed work\n\nRemaining work:\n- run manual validation";
        let params = json!({"item":{"type":"agentMessage","text":text,"phase":"final_answer"}});
        assert_eq!(agent_message(&params), Some((text.into(), true)));
        assert!(agent_message(&json!({"item":{"type":"commandExecution","text":text}})).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn delayed_interrupt_response_does_not_block_escalation() {
        let mut stop = Stop::new(
            "budget reached".into(),
            tokio::time::Instant::now(),
            async { std::future::pending::<Result<()>>().await },
        );
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(8)).await;
        assert!(!stop.interrupt.as_ref().unwrap().is_finished());
        assert_eq!(
            stop.next_step(Duration::from_secs(8), Duration::from_secs(4)),
            Some(StopStep::Graceful)
        );
        tokio::time::advance(Duration::from_secs(4)).await;
        assert_eq!(
            stop.next_step(Duration::from_secs(8), Duration::from_secs(4)),
            Some(StopStep::Force)
        );
    }
}

fn quota_json(snapshot: &app_server::RateSnapshot) -> Value {
    json!({"primary_used":snapshot.primary.as_ref().map(|x|x.used),"primary_resets_at":snapshot.primary.as_ref().and_then(|x|x.resets_at),"secondary_used":snapshot.secondary.as_ref().map(|x|x.used),"ordinary_usage_allowed":snapshot.ordinary_usage_allowed})
}
fn agent_message(params: &Value) -> Option<(String, bool)> {
    let item = params.get("item")?;
    if item.get("type")?.as_str()? != "agentMessage" {
        return None;
    }
    let text = item.get("text")?.as_str()?;
    if text.is_empty() {
        return None;
    }
    let is_final = item.get("phase").and_then(Value::as_str) == Some("final_answer");
    Some((text.to_owned(), is_final))
}

#[allow(clippy::too_many_arguments)]
async fn reconcile_rate_limits(
    client: &Client,
    policy: &mut Policy,
    logger: &mut Logger,
    ui: &mut Tui,
    stop: &mut Option<Stop>,
    thread: &str,
    turn: &str,
    failures: &mut u32,
) -> bool {
    match client.rate_limits().await {
        Ok(snapshot) => {
            *failures = 0;
            logger.record("quota_snapshot", quota_json(&snapshot));
            let actions = policy.observe(snapshot);
            logger.record(
                "credit_snapshot",
                json!({"balance":policy.latest.balance,"task_spent":policy.spent}),
            );
            apply_actions(actions, client, policy, logger, ui, stop, thread, turn).await;
            true
        }
        Err(error) => {
            *failures = failures.saturating_add(1);
            let tolerated = policy.can_tolerate_poll_failure()
                && *failures <= policy.profile.monitor.max_consecutive_failures;
            logger.record(
                if tolerated { "warning" } else { "error" },
                json!({"rate_limit_poll":error.to_string(),"consecutive_failures":*failures}),
            );
            if tolerated {
                ui.notice(format!(
                    "Rate-limit read failed ({}/{}); retrying soon",
                    failures, policy.profile.monitor.max_consecutive_failures
                ));
            } else {
                begin_stop(
                    client,
                    policy,
                    logger,
                    stop,
                    thread,
                    turn,
                    "rate-limit telemetry unavailable".into(),
                );
            }
            false
        }
    }
}
async fn apply_actions(
    actions: Vec<Action>,
    client: &Client,
    policy: &mut Policy,
    logger: &mut Logger,
    ui: &mut Tui,
    stop: &mut Option<Stop>,
    thread: &str,
    turn: &str,
) {
    let has_stop = actions
        .iter()
        .any(|action| matches!(action, Action::Stop(_)));
    for action in actions {
        match action {
            Action::Steer(name, message) if stop.is_none() && !has_stop => {
                match client.steer(thread, turn, message).await {
                    Ok(()) => {
                        policy.automatic_steers += 1;
                        policy.last_steer = Some(name);
                        logger.record("steer_sent", json!({"trigger":name}));
                        ui.notice(format!("Automatic steer: {name}"));
                    }
                    Err(error) => {
                        policy.retry_trigger(name);
                        logger.record("error", json!({"steer":name,"message":error.to_string()}));
                    }
                }
            }
            Action::Warn(message) => {
                logger.record("warning", json!(message));
                ui.notice(message);
            }
            Action::Billing(label) => {
                logger.record("billing_transition", json!(label));
                ui.notice(format!("Billing: {label}"));
            }
            Action::QuotaReset => {
                logger.record("quota_reset", json!({}));
                ui.notice("5h quota window reset");
            }
            Action::Stop(reason) => begin_stop(client, policy, logger, stop, thread, turn, reason),
            Action::Bell if policy.profile.monitor.bell => {
                let _ = crossterm::execute!(std::io::stdout(), crossterm::style::Print("\x07"));
            }
            _ => {}
        }
    }
}
fn begin_stop(
    client: &Client,
    _policy: &mut Policy,
    logger: &mut Logger,
    stop: &mut Option<Stop>,
    thread: &str,
    turn: &str,
    reason: String,
) {
    if stop.is_some() {
        return;
    }
    let detected_at = tokio::time::Instant::now();
    logger.record("turn_interrupt", json!({"reason":reason}));
    let client = client.clone();
    let thread = thread.to_owned();
    let turn = turn.to_owned();
    *stop = Some(Stop::new(reason, detected_at, async move {
        client.interrupt(&thread, &turn).await
    }));
}
#[allow(clippy::too_many_arguments)]
async fn handle_command(
    command: &str,
    client: &Client,
    process: &mut ProcessSupervisor,
    policy: &mut Policy,
    logger: &mut Logger,
    ui: &mut Tui,
    stop: &mut Option<Stop>,
    thread: Option<&str>,
    turn: Option<&str>,
    loaded: &Loaded,
    quit_after_stop: &mut bool,
) -> Result<bool> {
    if command == "/help" {
        ui.notice(
            "/help /status /logs /steer <text> /budget <credits> /profile /interrupt /kill /quit",
        );
    } else if command == "/status" {
        ui.notice(format!(
            "{} · spent {:.2}/{:.2} · balance {:.2}",
            if turn.is_some() { "RUNNING" } else { "READY" },
            policy.spent,
            policy.profile.credits.max_spend,
            policy.balance()
        ));
    } else if command == "/profile" {
        ui.notice(format!(
            "{} · {} · config {}",
            loaded.profile_name,
            loaded.mode,
            loaded.path.display()
        ));
    } else if command == "/steer" || command.starts_with("/steer ") {
        let message = command.strip_prefix("/steer ").unwrap_or("");
        if turn.is_none() {
            ui.notice("No active turn.");
        } else if message.trim().is_empty() {
            ui.notice("Steer text cannot be empty");
        } else if stop.is_some() {
            ui.notice("Task is already interrupting");
        } else {
            match client.steer(thread.unwrap(), turn.unwrap(), message).await {
                Ok(()) => {
                    policy.last_steer = Some("manual");
                    logger.record("steer_sent", json!({"trigger":"manual","text":message}));
                    ui.notice("Manual steer accepted");
                }
                Err(error) => {
                    logger.record("error", json!({"manual_steer":error.to_string()}));
                    ui.notice(format!("Steer failed: {error}"));
                }
            }
        }
    } else if command == "/budget" {
        ui.notice(format!(
            "Session budget: {:.2} credits; spent {:.2}",
            policy.profile.credits.max_spend, policy.spent
        ));
    } else if let Some(value) = command.strip_prefix("/budget ") {
        match value
            .trim()
            .parse::<f64>()
            .map_err(anyhow::Error::from)
            .and_then(|n| {
                policy.set_budget(n)?;
                Ok(n)
            }) {
            Ok(n) => {
                logger.record("budget_changed", json!({"max_spend":n}));
                ui.notice(format!("Budget: {n:.2}"));
                if policy.spent >= n && turn.is_some() {
                    begin_stop(
                        client,
                        policy,
                        logger,
                        stop,
                        thread.unwrap(),
                        turn.unwrap(),
                        "new task budget already reached".into(),
                    );
                }
            }
            Err(error) => ui.notice(format!("Invalid budget: {error}")),
        }
    } else if command == "/interrupt" {
        ui.notice("No active turn.");
    } else if command == "/quit" {
        return Ok(true);
    } else if command == "/interrupt-confirmed" || command == "/quit-confirmed" {
        if let (Some(thread), Some(turn)) = (thread, turn) {
            if command == "/quit-confirmed" {
                *quit_after_stop = true;
            }
            begin_stop(
                client,
                policy,
                logger,
                stop,
                thread,
                turn,
                "user requested interruption".into(),
            );
            ui.notice("Interrupt requested");
        } else {
            ui.notice("No active turn.");
        }
    } else if command == "/kill-confirmed" {
        logger.record(
            "process_termination",
            json!({"stage":"force","reason":"user command"}),
        );
        process.force_kill_tree().await?;
        return Ok(true);
    } else {
        ui.notice("Unknown command; enter /help");
    }
    Ok(false)
}
