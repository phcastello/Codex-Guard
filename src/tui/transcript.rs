use ratatui::style::Color;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub text: String,
    pub color: Option<Color>,
}

impl Entry {
    fn new(text: impl Into<String>, color: Option<Color>) -> Self {
        Self {
            text: text.into(),
            color,
        }
    }
}

#[derive(Default)]
pub struct Transcript {
    pub pending: VecDeque<Entry>,
    running: HashMap<String, String>,
    last_commentary: Option<String>,
}

impl Transcript {
    pub fn push(&mut self, text: impl Into<String>, color: Option<Color>) {
        self.pending.push_back(Entry::new(text, color));
    }
    pub fn user(&mut self, prompt: &str) {
        self.last_commentary = None;
        let mut lines = prompt.lines();
        let first = lines.next().unwrap_or_default();
        let rest = lines.map(|line| format!("  {line}")).collect::<Vec<_>>();
        let text = if rest.is_empty() {
            format!("\n› {first}\n")
        } else {
            format!("\n› {first}\n{}\n", rest.join("\n"))
        };
        self.push(text, None);
    }
    pub fn agent(&mut self, message: &str, final_answer: bool) {
        if final_answer && self.last_commentary.as_deref() == Some(message) {
            self.push("✓ Codex finished", Some(Color::Green));
            self.last_commentary = None;
            return;
        }
        self.last_commentary = (!final_answer).then(|| message.to_owned());
        let label = if final_answer {
            "Codex"
        } else {
            "Codex · commentary"
        };
        self.push(format!("\n{label}\n\n{message}\n"), None);
    }
    pub fn boundary(&mut self) {
        self.running.clear();
        self.last_commentary = None;
        self.push("\n──────── new thread ────────\n", Some(Color::DarkGray));
    }
    pub fn steer(&mut self, message: &str, automatic: bool) {
        if automatic {
            self.push(format!("⚠ Auto steer · {message}"), Some(Color::Yellow));
        } else {
            self.push(format!("↳ steer: {message}"), Some(Color::DarkGray));
        }
    }
    pub fn warning(&mut self, message: &str) {
        self.push(format!("⚠ {message}"), Some(Color::Yellow));
    }
    pub fn error(&mut self, message: &str) {
        self.push(format!("■ {message}"), Some(Color::Red));
    }
    pub fn success(&mut self, message: &str) {
        self.push(format!("✓ {message}"), Some(Color::Green));
    }
    pub fn tool(&mut self, method: &str, item: &Value) {
        let kind = item.get("type").and_then(Value::as_str).unwrap_or("tool");
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let started = method == "item/started";
        let label = tool_label(kind, item);
        if started {
            if !id.is_empty() {
                self.running.insert(id, label.clone());
            }
            // Commands have useful progress; edits and dynamic calls are shown on completion.
            if kind == "commandExecution" {
                self.push(format!("• Running {label}"), Some(Color::Cyan));
            }
        } else {
            let prior = self.running.remove(&id);
            let success = item
                .get("exitCode")
                .and_then(Value::as_i64)
                .is_none_or(|x| x == 0)
                && !matches!(
                    item.get("status").and_then(Value::as_str),
                    Some("failed" | "error")
                );
            if kind == "commandExecution" {
                let verb = if prior.is_some() { "" } else { "Ran " };
                let symbol = if success { "✓" } else { "✗" };
                let suffix = if success { "passed" } else { "failed" };
                let mut text = format!("{symbol} {verb}{label} {suffix}");
                if !success {
                    if let Some(output) = item.get("aggregatedOutput").and_then(Value::as_str) {
                        for line in output.lines().filter(|s| !s.trim().is_empty()).take(3) {
                            text.push_str("\n  ");
                            text.push_str(&one_line(line, 180));
                        }
                    }
                }
                self.push(text, Some(if success { Color::Green } else { Color::Red }));
            } else {
                self.push(format!("• {label}"), Some(Color::Cyan));
            }
        }
    }
}

fn tool_label(kind: &str, item: &Value) -> String {
    match kind {
        "commandExecution" => {
            let command = item
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("command");
            one_line(command_body(command), 110)
        }
        "fileChange" => {
            let paths = item
                .get("changes")
                .and_then(Value::as_array)
                .map(|changes| {
                    changes
                        .iter()
                        .filter_map(|x| x.get("path").and_then(Value::as_str))
                        .take(3)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if paths.is_empty() {
                "Edited files".into()
            } else {
                format!("Edited {}", paths.join(", "))
            }
        }
        "dynamicToolCall" => {
            let tool = item
                .get("tool")
                .or_else(|| item.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("tool");
            format!("Called {}", one_line(tool, 80))
        }
        other => format!("Used {}", one_line(other, 80)),
    }
}

// Hide the shell wrapper in the transcript without changing the executed command.
fn command_body(command: &str) -> &str {
    let trimmed = command.trim();
    let (executable, rest) = if let Some(quoted) = trimmed.strip_prefix('"') {
        let Some(end) = quoted.find('"') else {
            return command;
        };
        (&quoted[..end], &quoted[end + 1..])
    } else {
        let end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
        (&trimmed[..end], &trimmed[end..])
    };
    let name = executable.rsplit(['\\', '/']).next().unwrap_or(executable);
    if !["powershell.exe", "powershell", "pwsh.exe", "pwsh"]
        .iter()
        .any(|shell| name.eq_ignore_ascii_case(shell))
    {
        return command;
    }
    let rest = rest.trim_start();
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    if !rest[..end].eq_ignore_ascii_case("-Command") {
        return command;
    }
    let body = rest[end..].trim();
    if body.is_empty() {
        return command;
    }
    body.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(body)
}

fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        format!("{}…", flat.chars().take(max).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn formats_conversation_and_guard_entries() {
        let mut t = Transcript::default();
        t.user("first\nsecond");
        t.agent("Done\nwith detail", true);
        t.warning("quota low");
        t.boundary();
        t.steer("focus", false);
        t.steer("quota_wrap_up", true);
        let text = t
            .pending
            .iter()
            .map(|x| x.text.as_str())
            .collect::<String>();
        for expected in [
            "› first\n  second",
            "Codex\n\nDone\nwith detail",
            "⚠ quota low",
            "new thread",
            "↳ steer: focus",
            "Auto steer · quota_wrap_up",
        ] {
            assert!(text.contains(expected), "{expected}");
        }
    }
    #[test]
    fn tools_are_compact_and_unknowns_never_dump_json() {
        let mut t = Transcript::default();
        let command =
            json!({"id":"1","type":"commandExecution","command":"cargo check","exitCode":0});
        t.tool("item/started", &command);
        t.tool("item/completed", &command);
        t.tool(
            "item/completed",
            &json!({"type":"fileChange","changes":[{"path":"src/tui.rs"}]}),
        );
        t.tool(
            "item/completed",
            &json!({"type":"dynamicToolCall","tool":"search"}),
        );
        t.tool(
            "item/completed",
            &json!({"type":"futureThing","secret":{"x":1}}),
        );
        let text = t
            .pending
            .iter()
            .map(|x| x.text.as_str())
            .collect::<String>();
        assert!(text.contains("Running cargo check"));
        assert!(text.contains("cargo check passed"));
        assert!(text.contains("Edited src/tui.rs"));
        assert!(text.contains("Called search"));
        assert!(text.contains("Used futureThing"));
        assert!(!text.contains("secret"));
    }
    #[test]
    fn powershell_wrapper_is_hidden_in_started_and_completed_commands() {
        let mut t = Transcript::default();
        let command = json!({
            "id": "ps", "type": "commandExecution", "exitCode": 0,
            "command": r#""C:\WINDOWS\System32\WindowsPowerShell\v1.0\powershell.exe" -Command "Get-Content \"file.txt\"""#
        });
        t.tool("item/started", &command);
        t.tool("item/completed", &command);
        assert_eq!(t.pending[0].text, r#"• Running Get-Content \"file.txt\""#);
        assert_eq!(t.pending[1].text, r#"✓ Get-Content \"file.txt\" passed"#);
    }

    #[test]
    fn shell_display_preserves_other_commands_and_arguments() {
        assert_eq!(command_body("pwsh -command \"cargo check\""), "cargo check");
        assert_eq!(command_body("powershell.exe -Command Get-Date"), "Get-Date");
        for command in [
            "cargo check",
            "cmd.exe /C echo hello",
            "powershell.exe -File script.ps1",
            "powershell.exe -Command",
            "powershell.exe -CommandWithArgs hello",
        ] {
            assert_eq!(command_body(command), command);
        }
    }

    #[test]
    fn failed_command_shows_bounded_output() {
        let mut t = Transcript::default();
        t.tool("item/completed", &json!({"type":"commandExecution","command":"cargo check","exitCode":1,"aggregatedOutput":"error: failed\nline 2\nline 3\nline 4"}));
        let text = &t.pending.back().unwrap().text;
        assert!(text.contains("✗ Ran cargo check failed"));
        assert!(text.contains("error: failed"));
        assert!(!text.contains("line 4"));
    }
    #[test]
    fn completed_turn_clear_and_follow_up_preserve_earlier_entries() {
        let mut t = Transcript::default();
        t.user("first turn");
        t.agent("first answer", true);
        t.boundary();
        t.user("follow up");
        t.agent("second answer", true);
        let entries = t
            .pending
            .iter()
            .map(|x| x.text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(entries.len(), 5);
        assert!(entries[0].contains("first turn"));
        assert!(entries[1].contains("first answer"));
        assert!(entries[2].contains("new thread"));
        assert!(entries[3].contains("follow up"));
        assert!(entries[4].contains("second answer"));
        t.agent("draft", false);
        t.agent("draft", true);
        assert_eq!(t.pending.back().unwrap().text, "✓ Codex finished");
    }
}
