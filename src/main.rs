mod app_server;
mod cli;
mod config;
mod logging;
mod policy;
mod supervisor;
mod tui;

use anyhow::{bail, Context, Result};
use app_server::{Client, ThreadUsage};
use clap::Parser;
use cli::{Cli, Command, ConfigCommand};
use config::Loaded;
use crossterm::event::EventStream;
use futures_util::StreamExt;
use logging::Logger;
use policy::{Action, Phase, Policy};
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
    if cli.run.prompt.is_empty() {
        bail!("provide a task prompt");
    }
    run(loaded, cli.run.prompt.join(" ")).await
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

async fn run(loaded: Loaded, prompt: String) -> Result<()> {
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
    let startup: Result<(Policy, String, String)> = async {
        client.initialize().await?;
        let first = client.rate_limits().await?;
        let policy = Policy::new(loaded.profile.clone(), first)?;
        let cwd = std::env::current_dir()?.to_string_lossy().into_owned();
        let mut thread_params = json!({"cwd":cwd,"serviceName":"codex_guard"});
        // Omitted overrides inherit the user's normal Codex configuration.
        // Attended is still partial: unsupported approval requests stop the turn.
        if let Some(value) = &loaded.profile.session.approval_policy {
            thread_params["approvalPolicy"] = json!(value);
        }
        if let Some(value) = &loaded.profile.session.sandbox {
            thread_params["sandbox"] = json!(value);
        }
        let thread = client
            .call("thread/start", Some(thread_params))
            .await?
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .context("thread/start did not return thread.id")?
            .to_owned();
        let turn = client
            .call(
                "turn/start",
                Some(json!({"threadId":thread,"input":[{"type":"text","text":prompt}]})),
            )
            .await?
            .pointer("/turn/id")
            .and_then(Value::as_str)
            .context("turn/start did not return turn.id")?
            .to_owned();
        Ok((policy, thread, turn))
    }
    .await;
    let (mut policy, thread, turn) = match startup {
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
    logger.record("task_start", json!({"thread":thread,"turn":turn,"profile":loaded.profile_name,"mode":loaded.mode,"pid":process.id(),"prompt":prompt}));
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
    let mut next_poll = Box::pin(tokio::time::sleep(policy.poll_interval()));
    let (usage_tx, mut usage_rx) = mpsc::channel(2);
    let usage_client = client.clone();
    let usage_thread = thread.clone();
    let usage_interval = config::duration(&policy.profile.monitor.poll_interval);
    let usage_task = tokio::spawn(async move {
        loop {
            let result = usage_client.thread_usage(&usage_thread).await;
            if usage_tx.send(result).await.is_err() {
                break;
            }
            tokio::time::sleep(usage_interval).await;
        }
    });
    let mut stop: Option<Stop> = None;
    let mut consecutive_poll_failures = 0_u32;
    let mut result: Option<String> = None;
    let mut tools_done = 0_u64;
    let mut tools_running = 0_u64;
    let mut thread_usage: Option<ThreadUsage> = None;
    let mut last_message: Option<String> = None;
    let mut final_message: Option<String> = None;
    let mut process_exit_seen: Option<Instant> = None;
    let initial_actions = policy.observe(policy.latest.clone());
    apply_actions(
        initial_actions,
        &client,
        &mut policy,
        &mut logger,
        &mut ui,
        &mut stop,
        &thread,
        &turn,
    )
    .await;
    next_poll
        .as_mut()
        .reset(tokio::time::Instant::now() + policy.poll_interval());

    loop {
        tokio::select! {
            _ = redraw.tick() => {
                if stop.is_none() && started.elapsed() >= config::duration(&policy.profile.runtime.max) {
                    begin_stop(&client, &mut policy, &mut logger, &mut stop, &thread, &turn, "runtime limit reached".into());
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
                            policy.phase = Phase::Killed;
                            result = Some(format!("killed: {}", s.reason));
                            break;
                        }
                        None => {},
                    }
                }
                if process.try_wait()?.is_some() && result.is_none() {
                    let since = process_exit_seen.get_or_insert_with(Instant::now);
                    if since.elapsed() >= Duration::from_secs(5) {
                        policy.phase = Phase::Failed;
                        result = Some("App Server exited before turn/completed".into());
                        break;
                    }
                }
                ui.draw(&policy, &loaded.profile_name, &loaded.mode, started, &logger, thread_usage.as_ref(), tools_done, tools_running)?;
            }
            _ = &mut next_poll, if stop.is_none() => {
                let success = reconcile_rate_limits(&client, &mut policy, &mut logger, &mut ui, &mut stop, &thread, &turn, &mut consecutive_poll_failures).await;
                let delay = if success { policy.poll_interval() } else { config::duration(&policy.profile.monitor.retry_interval) };
                next_poll.as_mut().reset(tokio::time::Instant::now() + delay);
            }
            maybe = events.recv() => {
                let Some(event) = maybe else { policy.phase = Phase::Failed; result = Some("App Server event stream closed".into()); break; };
                match event.method.as_str() {
                    "turn/completed" => {
                        let event_turn = event.params.pointer("/turn/id").and_then(Value::as_str);
                        if event_turn == Some(turn.as_str()) {
                            let status = event.params.pointer("/turn/status").and_then(Value::as_str).unwrap_or("unknown");
                            policy.phase = match status { "completed" => Phase::Completed, "interrupted" => Phase::Completed, _ => Phase::Failed };
                            let error = event.params.pointer("/turn/error/message").and_then(Value::as_str);
                            result = Some(match error { Some(error) => format!("{status}: {error}"), None => status.into() });
                            logger.record("task_completion", json!({"status":status,"error":error}));
                            break;
                        }
                    }
                    "account/rateLimits/updated" => {
                        client.ack_rate_update();
                        if stop.is_none() {
                            let success = reconcile_rate_limits(&client, &mut policy, &mut logger, &mut ui, &mut stop, &thread, &turn, &mut consecutive_poll_failures).await;
                            let delay = if success { policy.poll_interval() } else { config::duration(&policy.profile.monitor.retry_interval) };
                            next_poll.as_mut().reset(tokio::time::Instant::now() + delay);
                        }
                    }
                    "item/completed" => {
                        if let Some((message, is_final)) = agent_message(&event.params) {
                            logger.record("agent_message", json!({"phase":if is_final {"final_answer"} else {"commentary"},"text":message}));
                            ui.agent_message(&message, is_final);
                            last_message = Some(message.clone());
                            if is_final { final_message = Some(message); }
                        }
                    }
                    "guard/unsupportedRequest" => {
                        let method = event.params.get("method").and_then(Value::as_str).unwrap_or("unknown");
                        logger.record("error", json!({"unsupported_server_request":method}));
                        begin_stop(&client, &mut policy, &mut logger, &mut stop, &thread, &turn, format!("unsupported App Server request: {method}"));
                    }
                    "guard/disconnected" | "guard/transportError" => { logger.record("error", json!({"transport":event.params})); policy.phase = Phase::Failed; result = Some("App Server disconnected".into()); break; }
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
                    if let Some(command) = ui.handle_key(key) {
                        handle_command(&command, &client, &mut process, &mut policy, &mut logger, &mut ui, &mut stop, &thread, &turn, &loaded).await?;
                        if command == "/kill-confirmed" { result = Some("killed by user".into()); break; }
                    }
                }
            }
        }
    }
    usage_task.abort();
    if process.try_wait()?.is_none() {
        match client.thread_usage(&thread).await {
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
                for action in policy.observe(snapshot) {
                    match action {
                        Action::Billing(label) => logger.record("billing_transition", json!(label)),
                        Action::QuotaReset => logger.record("quota_reset", json!({})),
                        _ => {}
                    }
                }
            }
            Err(error) => logger.record("error", json!({"final_reconciliation":error.to_string()})),
        }
    }
    // A completed turn no longer needs the server. Close its input and allow a short normal exit.
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
    let result = result.unwrap_or_else(|| "unknown".into());
    logger.record("summary", json!({"result":result,"runtime_seconds":started.elapsed().as_secs(),"paid_runtime_seconds":policy.paid_runtime.as_secs(),"credits_spent":policy.spent,"thread_usage":thread_usage,"automatic_steers":policy.automatic_steers,"quota_initial":policy.first.primary.as_ref().map(|x|x.used),"quota_final":policy.latest.primary.as_ref().map(|x|x.used)}));
    drop(ui);
    println!("Task {result}\nRuntime: {}s\nPaid runtime (sampled): {}s\nCredits spent: {:.2} / {:.2}\n5h usage: {} → {}\nAutomatic steers: {}\nLog: {}", started.elapsed().as_secs(), policy.paid_runtime.as_secs(), policy.spent, policy.profile.credits.max_spend, policy.first.primary.as_ref().map(|x|format!("{:.0}%",x.used)).unwrap_or_else(||"?".into()), policy.latest.primary.as_ref().map(|x|format!("{:.0}%",x.used)).unwrap_or_else(||"?".into()), policy.automatic_steers, logger.path.display());
    let has_final = final_message.is_some();
    if let Some(message) = final_message.or(last_message) {
        println!(
            "\n{}:\n\n{message}",
            if has_final {
                "Final agent response"
            } else {
                "Last agent message"
            }
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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
    policy: &mut Policy,
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
    policy.phase = Phase::Interrupting;
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
    thread: &str,
    turn: &str,
    loaded: &Loaded,
) -> Result<()> {
    if command == "/help" {
        ui.notice(
            "/help /status /logs /steer <text> /budget <credits> /profile /interrupt /kill /quit",
        );
    } else if command == "/status" {
        ui.notice(format!(
            "{} · spent {:.2}/{:.2} · balance {:.2}",
            policy.phase.label(),
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
    } else if let Some(message) = command.strip_prefix("/steer ") {
        if message.trim().is_empty() {
            ui.notice("Steer text cannot be empty");
        } else if stop.is_some() {
            ui.notice("Task is already interrupting");
        } else {
            match client.steer(thread, turn, message).await {
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
                if policy.spent >= n {
                    begin_stop(
                        client,
                        policy,
                        logger,
                        stop,
                        thread,
                        turn,
                        "new task budget already reached".into(),
                    );
                }
            }
            Err(error) => ui.notice(format!("Invalid budget: {error}")),
        }
    } else if command == "/interrupt-confirmed" || command == "/quit-confirmed" {
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
    } else if command == "/kill-confirmed" {
        logger.record(
            "process_termination",
            json!({"stage":"force","reason":"user command"}),
        );
        process.force_kill_tree().await?;
        policy.phase = Phase::Killed;
    } else {
        ui.notice("Unknown command; enter /help");
    }
    Ok(())
}
