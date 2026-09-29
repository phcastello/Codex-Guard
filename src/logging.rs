use anyhow::{Context, Result};
use chrono::Local;
use directories::ProjectDirs;
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    fs::{self, File, OpenOptions},
    io::Write,
    path::PathBuf,
};

pub struct Logger {
    file: File,
    pub path: PathBuf,
    pub recent: VecDeque<String>,
}
impl Logger {
    pub fn new() -> Result<Self> {
        let dirs =
            ProjectDirs::from("", "", "codex-guard").context("cannot determine log directory")?;
        fs::create_dir_all(dirs.data_local_dir())?;
        let path = dirs.data_local_dir().join(format!(
            "run-{}-{}.jsonl",
            Local::now().format("%Y%m%d-%H%M%S-%3f"),
            std::process::id()
        ));
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)?;
        Ok(Self {
            file,
            path,
            recent: VecDeque::with_capacity(100),
        })
    }
    pub fn record(&mut self, kind: &str, detail: Value) {
        let at = Local::now();
        let entry = json!({"at":at.to_rfc3339(),"event":kind,"detail":detail});
        let _ = writeln!(self.file, "{entry}");
        let _ = self.file.flush();
        let brief = if let Some(s) = detail.as_str() {
            s.to_owned()
        } else {
            detail.to_string()
        };
        let brief: String = brief.chars().take(140).collect();
        self.recent
            .push_back(format!("{} {} {}", at.format("%H:%M:%S"), kind, brief));
        if self.recent.len() > 100 {
            self.recent.pop_front();
        }
    }
}
