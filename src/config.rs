use anyhow::{bail, Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, path::PathBuf, time::Duration};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub codex: Codex,
    pub runtime: Runtime,
    pub quota: Quota,
    pub credits: Credits,
    pub monitor: Monitor,
    pub burn_rate: BurnRate,
    pub session: Session,
}
impl Default for Profile {
    fn default() -> Self {
        Self {
            codex: Codex::default(),
            runtime: Runtime::default(),
            quota: Quota::default(),
            credits: Credits::default(),
            monitor: Monitor::default(),
            burn_rate: BurnRate::default(),
            session: Session::default(),
        }
    }
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CodexMode {
    #[default]
    Inherit,
    Yolo,
}
impl CodexMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Inherit => "INHERIT",
            Self::Yolo => "YOLO",
        }
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Codex {
    pub mode: CodexMode,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Runtime {
    pub max: String,
    pub interrupt_grace: String,
    pub terminate_grace: String,
}
impl Default for Runtime {
    fn default() -> Self {
        Self {
            max: "4h".into(),
            interrupt_grace: "8s".into(),
            terminate_grace: "4s".into(),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Quota {
    pub wrap_remaining_percent: f64,
    pub critical_remaining_percent: f64,
    pub hard_stop: bool,
}
impl Default for Quota {
    fn default() -> Self {
        Self {
            wrap_remaining_percent: 15.0,
            critical_remaining_percent: 5.0,
            hard_stop: false,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Credits {
    pub max_spend: f64,
    pub finalize_at: f64,
    pub urgent_finalize_at: f64,
    pub reserve: f64,
}
impl Default for Credits {
    fn default() -> Self {
        Self {
            max_spend: 20.0,
            finalize_at: 0.75,
            urgent_finalize_at: 0.90,
            reserve: 20.0,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Monitor {
    pub poll_interval: String,
    pub wrap_poll_interval: String,
    pub paid_poll_interval: String,
    pub critical_poll_interval: String,
    pub retry_interval: String,
    pub max_consecutive_failures: u32,
    pub bell: bool,
}
impl Default for Monitor {
    fn default() -> Self {
        Self {
            poll_interval: "60s".into(),
            wrap_poll_interval: "30s".into(),
            paid_poll_interval: "10s".into(),
            critical_poll_interval: "5s".into(),
            retry_interval: "5s".into(),
            max_consecutive_failures: 2,
            bell: true,
        }
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    // None means the App Server inherits the user's ordinary Codex config.
    pub approval_policy: Option<String>,
    pub sandbox: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct BurnRate {
    pub window: String,
    pub max_credit_spend: f64,
    pub action: BurnAction,
}
impl Default for BurnRate {
    fn default() -> Self {
        Self {
            window: "10m".into(),
            max_credit_spend: 5.0,
            action: BurnAction::Warn,
        }
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BurnAction {
    Warn,
    Stop,
}

#[derive(Default, Deserialize)]
struct FileConfig {
    default_profile: Option<String>,
    default_mode: Option<String>,
    runtime: Option<toml::Value>,
    quota: Option<toml::Value>,
    credits: Option<toml::Value>,
    monitor: Option<toml::Value>,
    burn_rate: Option<toml::Value>,
    session: Option<toml::Value>,
    codex: Option<toml::Value>,
    #[serde(default)]
    profiles: BTreeMap<String, toml::Value>,
}

pub struct Loaded {
    pub profile_name: String,
    pub mode: String,
    pub profile: Profile,
    pub path: PathBuf,
    pub names: Vec<String>,
}

pub fn path() -> Result<PathBuf> {
    let dirs =
        ProjectDirs::from("", "", "codex-guard").context("cannot determine config directory")?;
    Ok(dirs.config_dir().join("config.toml"))
}

pub fn load(
    name: Option<&str>,
    credits: Option<f64>,
    time: Option<&str>,
    attended: bool,
    no_bell: bool,
    yolo: bool,
) -> Result<Loaded> {
    let path = path()?;
    let file: FileConfig = if path.exists() {
        toml::from_str(
            &fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?,
        )?
    } else {
        FileConfig::default()
    };
    resolve(file, path, name, credits, time, attended, no_bell, yolo)
}

#[allow(clippy::too_many_arguments)]
fn resolve(
    file: FileConfig,
    path: PathBuf,
    name: Option<&str>,
    credits: Option<f64>,
    time: Option<&str>,
    attended: bool,
    no_bell: bool,
    yolo: bool,
) -> Result<Loaded> {
    let profile_name = name
        .or(file.default_profile.as_deref())
        .unwrap_or("default")
        .to_owned();
    if profile_name != "default" && !file.profiles.contains_key(&profile_name) {
        bail!("unknown profile: {profile_name}");
    }
    let mut value = toml::Value::try_from(Profile::default())?;
    for (key, part) in [
        ("runtime", file.runtime),
        ("quota", file.quota),
        ("credits", file.credits),
        ("monitor", file.monitor),
        ("burn_rate", file.burn_rate),
        ("session", file.session),
        ("codex", file.codex),
    ] {
        if let Some(part) = part {
            merge(value.get_mut(key).context("missing default section")?, part);
        }
    }
    if let Some(part) = file.profiles.get(&profile_name) {
        merge(&mut value, part.clone());
    }
    let mut profile: Profile = value.try_into()?;
    if let Some(x) = credits {
        profile.credits.max_spend = x;
    }
    if let Some(x) = time {
        profile.runtime.max = x.into();
    }
    if no_bell {
        profile.monitor.bell = false;
    }
    if yolo {
        profile.codex.mode = CodexMode::Yolo;
    }
    validate(&profile)?;
    let mode = if attended {
        "ATTENDED"
    } else {
        file.default_mode.as_deref().unwrap_or("unattended")
    }
    .to_ascii_uppercase();
    if mode != "ATTENDED" && mode != "UNATTENDED" {
        bail!("default_mode must be attended or unattended");
    }
    let mut names: Vec<_> = file.profiles.keys().cloned().collect();
    if !names.iter().any(|x| x == "default") {
        names.insert(0, "default".into());
    }
    Ok(Loaded {
        profile_name,
        mode,
        profile,
        path,
        names,
    })
}

fn merge(base: &mut toml::Value, overlay: toml::Value) {
    if let (Some(a), Some(b)) = (base.as_table_mut(), overlay.as_table()) {
        for (key, value) in b {
            if let Some(slot) = a.get_mut(key) {
                merge(slot, value.clone());
            } else {
                a.insert(key.clone(), value.clone());
            }
        }
    } else {
        *base = overlay;
    }
}
fn validate(p: &Profile) -> Result<()> {
    for x in [
        &p.runtime.max,
        &p.runtime.interrupt_grace,
        &p.runtime.terminate_grace,
        &p.monitor.poll_interval,
        &p.monitor.wrap_poll_interval,
        &p.monitor.paid_poll_interval,
        &p.monitor.critical_poll_interval,
        &p.monitor.retry_interval,
        &p.burn_rate.window,
    ] {
        let d = humantime::parse_duration(x)?;
        if d.is_zero() {
            bail!("duration must be positive: {x}");
        }
    }
    if !(0.0..=100.0).contains(&p.quota.critical_remaining_percent)
        || !(p.quota.critical_remaining_percent..=100.0).contains(&p.quota.wrap_remaining_percent)
    {
        bail!("invalid quota thresholds");
    }
    if !p.credits.max_spend.is_finite()
        || p.credits.max_spend <= 0.0
        || !p.credits.reserve.is_finite()
        || p.credits.reserve < 0.0
    {
        bail!("invalid credit budget/reserve");
    }
    if !(0.0..1.0).contains(&p.credits.finalize_at)
        || !(p.credits.finalize_at..1.0).contains(&p.credits.urgent_finalize_at)
    {
        bail!("invalid credit thresholds");
    }
    if !p.burn_rate.max_credit_spend.is_finite() || p.burn_rate.max_credit_spend <= 0.0 {
        bail!("invalid burn rate limit");
    }
    if p.monitor.max_consecutive_failures > 5 {
        bail!("max_consecutive_failures must be 0..=5");
    }
    if p.session
        .approval_policy
        .as_deref()
        .is_some_and(str::is_empty)
        || p.session.sandbox.as_deref().is_some_and(str::is_empty)
    {
        bail!("session approval_policy/sandbox must not be empty");
    }
    Ok(())
}
pub fn duration(s: &str) -> Duration {
    humantime::parse_duration(s).expect("validated duration")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn codex_mode_merges_defaults_global_profile_and_cli() {
        let default = resolve(
            FileConfig::default(),
            PathBuf::new(),
            None,
            None,
            None,
            false,
            false,
            false,
        )
        .unwrap();
        assert_eq!(default.profile.codex.mode, CodexMode::Inherit);
        let file: FileConfig = toml::from_str(
            "[codex]\nmode = 'yolo'\n[profiles.conservative.codex]\nmode = 'inherit'",
        )
        .unwrap();
        let global = resolve(file, PathBuf::new(), None, None, None, false, false, false).unwrap();
        assert_eq!(global.profile.codex.mode, CodexMode::Yolo);
        let file: FileConfig = toml::from_str(
            "[codex]\nmode = 'yolo'\n[profiles.conservative.codex]\nmode = 'inherit'",
        )
        .unwrap();
        let profile = resolve(
            file,
            PathBuf::new(),
            Some("conservative"),
            None,
            None,
            false,
            false,
            false,
        )
        .unwrap();
        assert_eq!(profile.profile.codex.mode, CodexMode::Inherit);
        let file: FileConfig =
            toml::from_str("[profiles.conservative.codex]\nmode = 'inherit'").unwrap();
        let cli = resolve(
            file,
            PathBuf::new(),
            Some("conservative"),
            Some(5.0),
            None,
            false,
            false,
            true,
        )
        .unwrap();
        assert_eq!(cli.profile.codex.mode, CodexMode::Yolo);
        assert_eq!(cli.profile.credits.max_spend, 5.0);
    }
}
