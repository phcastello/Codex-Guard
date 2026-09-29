mod app_server;
mod cli;
mod config;
mod logging;
mod policy;
mod supervisor;
mod tui;

use anyhow::{bail, Context, Result};
use app_server::Client;
use clap::Parser;
use cli::{Cli, Command, ConfigCommand};
use config::Loaded;
use crossterm::event::EventStream;
use futures_util::StreamExt;
use logging::Logger;
use policy::{Action, Phase, Policy};
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use supervisor::ProcessSupervisor;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    sync::mpsc,
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
    at: Instant,
    stage: u8,
}

async fn run(loaded: Loaded, prompt: String) -> Result<()> {
    let (mut process, stdin, stdout, stderr) = ProcessSupervisor::spawn()?;
    let (client, mut events) = Client::new(stdin, stdout);
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
        let thread = client.call("thread/start", Some(json!({"cwd":cwd,"approvalPolicy":"never","sandbox":"workspaceWrite","serviceName":"codex_guard"}))).await?
            .pointer("/thread/id").and_then(Value::as_str).context("thread/start did not return thread.id")?.to_owned();
        let turn = client.call("turn/start", Some(json!({"threadId":thread,"input":[{"type":"text","text":prompt}]}))).await?
            .pointer("/turn/id").and_then(Value::as_str).context("turn/start did not return turn.id")?.to_owned();
        Ok((policy, thread, turn))
    }.await;
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
    let mut poll = interval(config::duration(&policy.profile.monitor.poll_interval));
    poll.set_missed_tick_behavior(MissedTickBehavior::Skip);
    poll.tick().await; // first snapshot was already read before task start
    let mut stop: Option<Stop> = None;
    let mut result: Option<String> = None;
    let mut tools_done = 0_u64;
    let mut tools_running = 0_u64;
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

    loop {
        tokio::select! {
            _ = redraw.tick() => {
                if stop.is_none() && started.elapsed() >= config::duration(&policy.profile.runtime.max) {
                    begin_stop(&client, &mut policy, &mut logger, &mut stop, &thread, &turn, "runtime limit reached".into()).await;
                }
                if let Some(s) = stop.as_mut() {
                    if s.stage == 0 && s.at.elapsed() >= config::duration(&policy.profile.runtime.interrupt_grace) {
                        logger.record("process_termination", json!({"stage":"graceful","reason":s.reason}));
                        if let Err(error) = process.graceful_terminate().await { logger.record("error", json!({"graceful_termination":error.to_string()})); }
                        s.stage = 1; s.at = Instant::now();
                    } else if s.stage == 1 && s.at.elapsed() >= config::duration(&policy.profile.runtime.terminate_grace) {
                        logger.record("process_termination", json!({"stage":"force","reason":s.reason}));
                        process.force_kill_tree().await?;
                        policy.phase = Phase::Killed;
                        result = Some(format!("killed: {}", s.reason));
                        break;
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
                ui.draw(&policy, &loaded.profile_name, &loaded.mode, started, &logger, tools_done, tools_running)?;
            }
            _ = poll.tick() => {
                match client.rate_limits().await {
                    Ok(snapshot) => {
                        logger.record("quota_snapshot", quota_json(&snapshot));
                        logger.record("credit_snapshot", json!({"balance":snapshot.balance,"task_spent":policy.spent}));
                        let actions = policy.observe(snapshot);
                        apply_actions(actions, &client, &mut policy, &mut logger, &mut ui, &mut stop, &thread, &turn).await;
                    }
                    Err(error) => { logger.record("error", json!({"poll":error.to_string()})); begin_stop(&client, &mut policy, &mut logger, &mut stop, &thread, &turn, "rate-limit polling failed".into()).await; }
                }
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
                        match client.rate_limits().await {
                            Ok(snapshot) => {
                                logger.record("quota_snapshot", quota_json(&snapshot));
                                logger.record("credit_snapshot", json!({"balance":snapshot.balance,"task_spent":policy.spent}));
                                let actions = policy.observe(snapshot);
                                apply_actions(actions, &client, &mut policy, &mut logger, &mut ui, &mut stop, &thread, &turn).await;
                            }
                            Err(error) => { logger.record("error", json!({"event_reconciliation":error.to_string()})); begin_stop(&client, &mut policy, &mut logger, &mut stop, &thread, &turn, "rate-limit reconciliation failed".into()).await; }
                        }
                    }
                    "item/started" | "item/completed" => {
                        if let Some(item) = event.params.get("item") {
                            let kind = item.get("type").and_then(Value::as_str).unwrap_or("item");
                            if matches!(kind, "commandExecution" | "fileChange" | "dynamicToolCall") {
                                if event.method == "item/started" { tools_running += 1; }
                                else { tools_done += 1; tools_running = tools_running.saturating_sub(1); }
                                let detail = item.get("command").or_else(|| item.get("changes")).unwrap_or(item).to_string();
                                logger.record(if event.method == "item/started" { "tool_start" } else { "tool_complete" }, json!({"type":kind,"detail":detail.chars().take(500).collect::<String>()}));
                            } else if kind == "agentMessage" && event.method == "item/completed" { logger.record("agent_message", json!(item.to_string().chars().take(500).collect::<String>())); }
                        }
                    }
                    "guard/disconnected" | "guard/transportError" => { logger.record("error", json!({"transport":event.params})); policy.phase = Phase::Failed; result = Some("App Server disconnected".into()); break; }
                    "error" => logger.record("error", event.params),
                    _ => {},
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
    logger.record("summary", json!({"result":result,"runtime_seconds":started.elapsed().as_secs(),"paid_runtime_seconds":policy.paid_runtime.as_secs(),"credits_spent":policy.spent,"automatic_steers":policy.automatic_steers,"quota_initial":policy.first.primary.as_ref().map(|x|x.used),"quota_final":policy.latest.primary.as_ref().map(|x|x.used)}));
    drop(ui);
    println!("Task {result}\nRuntime: {}s\nPaid runtime (sampled): {}s\nCredits spent: {:.2} / {:.2}\n5h usage: {} → {}\nAutomatic steers: {}\nLog: {}", started.elapsed().as_secs(), policy.paid_runtime.as_secs(), policy.spent, policy.profile.credits.max_spend, policy.first.primary.as_ref().map(|x|format!("{:.0}%",x.used)).unwrap_or_else(||"?".into()), policy.latest.primary.as_ref().map(|x|format!("{:.0}%",x.used)).unwrap_or_else(||"?".into()), policy.automatic_steers, logger.path.display());
    Ok(())
}

fn quota_json(snapshot: &app_server::RateSnapshot) -> Value {
    json!({"primary_used":snapshot.primary.as_ref().map(|x|x.used),"primary_resets_at":snapshot.primary.as_ref().and_then(|x|x.resets_at),"secondary_used":snapshot.secondary.as_ref().map(|x|x.used),"ordinary_usage_allowed":snapshot.ordinary_usage_allowed})
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
            Action::Stop(reason) => {
                begin_stop(client, policy, logger, stop, thread, turn, reason).await
            }
            Action::Bell if policy.profile.monitor.bell => {
                let _ = crossterm::execute!(std::io::stdout(), crossterm::style::Print("\x07"));
            }
            _ => {}
        }
    }
}
async fn begin_stop(
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
    policy.phase = Phase::Interrupting;
    logger.record("turn_interrupt", json!({"reason":reason}));
    *stop = Some(Stop {
        reason,
        at: Instant::now(),
        stage: 0,
    });
    if let Err(error) = client.interrupt(thread, turn).await {
        logger.record("error", json!({"interrupt":error.to_string()}));
    }
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
                    )
                    .await;
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
        )
        .await;
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
