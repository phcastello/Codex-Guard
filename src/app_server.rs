use anyhow::{anyhow, bail, Context, Result};
mod models;
pub use models::{Model, ModelSelection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdin, ChildStdout},
    sync::{mpsc, oneshot, Mutex},
};

#[derive(Debug)]
pub struct Event {
    pub method: String,
    pub params: Value,
}

#[derive(Clone)]
pub struct Client {
    writer: Arc<Mutex<ChildStdin>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>,
    next_id: Arc<AtomicU64>,
    rate_update_queued: Arc<AtomicBool>,
}

impl Client {
    pub fn new(
        stdin: ChildStdin,
        stdout: ChildStdout,
    ) -> (Self, mpsc::UnboundedReceiver<Event>, mpsc::Receiver<Event>) {
        let client = Self {
            writer: Arc::new(Mutex::new(stdin)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
            rate_update_queued: Arc::new(AtomicBool::new(false)),
        };
        // Only a small set of lifecycle events is critical. Activity events have a
        // bounded lossy queue; neither queue can make the stdout reader await capacity.
        let (critical_tx, critical_rx) = mpsc::unbounded_channel();
        let (activity_tx, activity_rx) = mpsc::channel(256);
        let pending = client.pending.clone();
        let reply = client.clone();
        let rate_update_queued = client.rate_update_queued.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                let line = match lines.next_line().await {
                    Ok(Some(line)) => line,
                    Ok(None) => break,
                    Err(error) => {
                        let _ = critical_tx.send(Event {
                            method: "guard/transportError".into(),
                            params: json!({"message": error.to_string()}),
                        });
                        break;
                    }
                };
                let message: Value = match serde_json::from_str(&line) {
                    Ok(x) => x,
                    Err(_) => continue,
                };
                if let Some(id) = message.get("id") {
                    if let Some(method) = message.get("method").and_then(Value::as_str) {
                        // No approval UI yet. Fail the request and tell the supervisor so
                        // the turn stops instead of spending quota retrying around denial.
                        let _ = critical_tx.send(Event {
                            method: "guard/unsupportedRequest".into(),
                            params: json!({"method":method}),
                        });
                        let reply = reply.clone();
                        let id = id.clone();
                        tokio::spawn(async move {
                            let _ = reply.send_value(json!({"id": id, "error": {"code": -32601, "message": "Unsupported by Codex Guard"}})).await;
                        });
                    } else if let Some(number) = id.as_u64() {
                        if let Some(sender) = pending.lock().await.remove(&number) {
                            let result = if let Some(error) = message.get("error") {
                                Err(anyhow!("App Server error: {error}"))
                            } else {
                                Ok(message.get("result").cloned().unwrap_or(Value::Null))
                            };
                            let _ = sender.send(result);
                        }
                    }
                } else if let Some(method) = message.get("method").and_then(Value::as_str) {
                    forward_notification(
                        method,
                        message.get("params").cloned().unwrap_or(Value::Null),
                        &critical_tx,
                        &activity_tx,
                        &rate_update_queued,
                    );
                }
            }
            let mut map = pending.lock().await;
            for (_, sender) in map.drain() {
                let _ = sender.send(Err(anyhow!("App Server stdout closed")));
            }
            let _ = critical_tx.send(Event {
                method: "guard/disconnected".into(),
                params: Value::Null,
            });
        });
        (client, critical_rx, activity_rx)
    }

    pub fn ack_rate_update(&self) {
        self.rate_update_queued.store(false, Ordering::Release);
    }

    async fn send_value(&self, value: Value) -> Result<()> {
        let mut writer = self.writer.lock().await;
        writer
            .write_all(serde_json::to_string(&value)?.as_bytes())
            .await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
        Ok(())
    }
    pub async fn call(&self, method: &str, params: Option<Value>) -> Result<Value> {
        self.call_with_timeout(method, params, Duration::from_secs(15))
            .await
    }
    async fn call_with_timeout(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let mut request = json!({"id": id, "method": method});
        if let Some(p) = params {
            request["params"] = p;
        }
        if let Err(error) = self.send_value(request).await {
            self.pending.lock().await.remove(&id);
            return Err(error);
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(result) => result.context("App Server response dropped")?,
            Err(_) => {
                self.pending.lock().await.remove(&id);
                bail!("App Server request timed out: {method}");
            }
        }
    }
    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.send_value(json!({"method": method, "params": params}))
            .await
    }
    pub async fn close_stdin(&self) -> Result<()> {
        self.writer
            .lock()
            .await
            .shutdown()
            .await
            .context("close App Server stdin")
    }
    pub async fn initialize(&self) -> Result<()> {
        self.call("initialize", Some(json!({"clientInfo":{"name":"codex_guard","title":"Codex Guard","version":env!("CARGO_PKG_VERSION")}}))).await?;
        self.notify("initialized", json!({})).await
    }
    pub async fn rate_limits(&self) -> Result<RateSnapshot> {
        let result = self
            .call_with_timeout("account/rateLimits/read", None, Duration::from_secs(5))
            .await?;
        RateSnapshot::parse(&result)
    }
    pub async fn list_models(&self) -> Result<Vec<Model>> {
        tokio::time::timeout(
            Duration::from_secs(15),
            models::collect_models(|params| {
                self.call_with_timeout("model/list", Some(params), Duration::from_secs(5))
            }),
        )
        .await
        .context("model/list catalog timed out")?
    }
    pub async fn thread_usage(&self, thread_id: &str) -> Result<Option<ThreadUsage>> {
        let result = self
            .call_with_timeout(
                "account/usage/read",
                Some(json!({"threadId":thread_id})),
                Duration::from_secs(5),
            )
            .await?;
        let Some(value) = result.get("threadUsage").filter(|x| !x.is_null()) else {
            return Ok(None);
        };
        let usage: ThreadUsage = serde_json::from_value(value.clone())?;
        if usage.thread_id != thread_id {
            bail!("thread usage response belongs to a different thread");
        }
        Ok(Some(usage))
    }
    pub async fn steer(&self, thread: &str, turn: &str, text: &str) -> Result<()> {
        self.call("turn/steer", Some(json!({"threadId":thread,"expectedTurnId":turn,"input":[{"type":"text","text":text}]}))).await?;
        Ok(())
    }
    pub async fn interrupt(&self, thread: &str, turn: &str) -> Result<()> {
        self.call(
            "turn/interrupt",
            Some(json!({"threadId":thread,"turnId":turn})),
        )
        .await?;
        Ok(())
    }
}

fn forward_notification(
    method: &str,
    params: Value,
    critical: &mpsc::UnboundedSender<Event>,
    activity: &mpsc::Sender<Event>,
    rate_update_queued: &AtomicBool,
) {
    let event = Event {
        method: method.into(),
        params,
    };
    match method {
        "turn/completed" | "error" => {
            let _ = critical.send(event);
        }
        "account/rateLimits/updated" => {
            if !rate_update_queued.swap(true, Ordering::AcqRel) {
                let _ = critical.send(event);
            }
        }
        "item/completed"
            if event.params.pointer("/item/type").and_then(Value::as_str)
                == Some("agentMessage") =>
        {
            let _ = critical.send(event);
        }
        "item/started" | "item/completed" => {
            let kind = event.params.pointer("/item/type").and_then(Value::as_str);
            if matches!(
                kind,
                Some("commandExecution" | "fileChange" | "dynamicToolCall")
            ) {
                let _ = activity.try_send(event);
            }
        }
        _ => {} // Deliberately omit deltas and unrelated notifications.
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadUsage {
    pub thread_id: String,
    pub estimated_usage_credits_micros: i64,
    pub estimated_usage_usd_micros: Option<i64>,
    #[serde(default)]
    pub groups: Vec<UsageGroup>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageGroup {
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub total_tokens: Option<i64>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub estimated_usage_credits_micros: i64,
}

#[derive(Clone, Debug)]
pub struct Window {
    pub used: f64,
    pub resets_at: Option<i64>,
    pub duration_mins: u64,
}
#[derive(Clone, Debug)]
pub struct RateSnapshot {
    pub primary: Option<Window>,
    pub secondary: Option<Window>,
    pub balance: Option<f64>,
    pub has_credits: Option<bool>,
    pub unlimited: bool,
    pub ordinary_usage_allowed: Option<bool>,
    pub account_id: Option<String>,
}
impl RateSnapshot {
    pub fn parse(root: &Value) -> Result<Self> {
        let rate = root.get("rateLimits").context("rateLimits missing")?;
        let credits = rate
            .get("credits")
            .filter(|x| !x.is_null())
            .or_else(|| root.pointer("/rateLimitsByLimitId/codex/credits"));
        let balance = credits
            .and_then(|x| x.get("balance"))
            .and_then(Value::as_str)
            .and_then(|x| x.parse::<f64>().ok());
        let unlimited = credits
            .and_then(|x| x.get("unlimited"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let has_credits = credits
            .and_then(|x| x.get("hasCredits"))
            .and_then(Value::as_bool);
        if balance.is_some_and(|x| !x.is_finite() || x < 0.0) {
            bail!("invalid credit balance");
        }
        let windows = [window(rate.get("primary")), window(rate.get("secondary"))];
        let primary = windows
            .iter()
            .flatten()
            .find(|w| w.duration_mins == 300)
            .cloned();
        let secondary = windows
            .iter()
            .flatten()
            .find(|w| w.duration_mins == 10080)
            .cloned();
        Ok(Self {
            primary,
            secondary,
            balance,
            has_credits,
            unlimited,
            ordinary_usage_allowed: root.get("ordinaryUsageAllowed").and_then(Value::as_bool),
            account_id: root
                .get("accountId")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }
}
fn window(value: Option<&Value>) -> Option<Window> {
    let value = value?;
    Some(Window {
        used: value.get("usedPercent")?.as_f64()?,
        resets_at: value.get("resetsAt").and_then(Value::as_i64),
        duration_mins: value.get("windowDurationMins")?.as_u64()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn critical_events_survive_full_activity_queue() {
        let (critical_tx, mut critical_rx) = mpsc::unbounded_channel();
        let (activity_tx, mut activity_rx) = mpsc::channel(1);
        let queued = AtomicBool::new(false);
        let tool = json!({"item":{"type":"commandExecution","id":"tool-1"}});
        forward_notification(
            "item/started",
            tool.clone(),
            &critical_tx,
            &activity_tx,
            &queued,
        );
        forward_notification("item/completed", tool, &critical_tx, &activity_tx, &queued);
        forward_notification(
            "item/completed",
            json!({"item":{"type":"agentMessage","text":"full reply"}}),
            &critical_tx,
            &activity_tx,
            &queued,
        );
        forward_notification(
            "turn/completed",
            json!({"turn":{"status":"completed"}}),
            &critical_tx,
            &activity_tx,
            &queued,
        );
        forward_notification(
            "account/rateLimits/updated",
            json!({}),
            &critical_tx,
            &activity_tx,
            &queued,
        );
        forward_notification(
            "account/rateLimits/updated",
            json!({}),
            &critical_tx,
            &activity_tx,
            &queued,
        );
        assert_eq!(activity_rx.try_recv().unwrap().method, "item/started");
        assert_eq!(critical_rx.try_recv().unwrap().method, "item/completed");
        assert_eq!(critical_rx.try_recv().unwrap().method, "turn/completed");
        assert_eq!(
            critical_rx.try_recv().unwrap().method,
            "account/rateLimits/updated"
        );
        assert!(critical_rx.try_recv().is_err());
    }
    #[test]
    fn parses_thread_usage_estimate() {
        let usage: ThreadUsage = serde_json::from_value(json!({"threadId":"thr-1","estimatedUsageCreditsMicros":1250000,"estimatedUsageUsdMicros":50000,"groups":[{"model":"gpt-test","reasoningEffort":"high","totalTokens":1200,"estimatedUsageCreditsMicros":1250000}]})).unwrap();
        assert_eq!(usage.estimated_usage_credits_micros, 1_250_000);
        assert_eq!(usage.groups[0].reasoning_effort.as_deref(), Some("high"));
    }
    #[test]
    fn selects_five_hour_window_and_parses_credit_balance() {
        let snapshot = RateSnapshot::parse(&json!({
            "rateLimits": {"primary":{"usedPercent":10.0,"windowDurationMins":300,"resetsAt":100},"secondary":{"usedPercent":30.0,"windowDurationMins":10080,"resetsAt":200},"credits":{"balance":"186.00","hasCredits":true,"unlimited":false}},
            "ordinaryUsageAllowed":true,"accountId":"acct"
        })).unwrap();
        assert_eq!(snapshot.primary.unwrap().used, 10.0);
        assert_eq!(snapshot.secondary.unwrap().used, 30.0);
        assert_eq!(snapshot.balance, Some(186.0));
        assert_eq!(snapshot.has_credits, Some(true));
    }
    #[test]
    fn preserves_credit_availability_with_null_balance() {
        let snapshot = RateSnapshot::parse(&json!({
            "rateLimits":{"credits":{"balance":null,"hasCredits":false,"unlimited":false}}
        }))
        .unwrap();
        assert_eq!(snapshot.balance, None);
        assert_eq!(snapshot.has_credits, Some(false));
    }
}
