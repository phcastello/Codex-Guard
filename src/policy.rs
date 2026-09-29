use crate::{
    app_server::RateSnapshot,
    config::{self, BurnAction, Profile},
};
use anyhow::{bail, Result};
use std::{
    collections::{HashSet, VecDeque},
    time::{Duration, Instant},
};

pub const QUOTA_WRAP_UP: &str = "You are approaching the included usage limit.\n\nPrioritize completing work already in progress.\n\nDo not start unrelated refactors, broad investigations, optional improvements, or new subtasks unless they are necessary to complete the current objective.\n\nIf everything cannot reasonably be completed before the limit:\n- leave the repository in a consistent state;\n- finish partially implemented changes when practical;\n- run only work that is essential to reach a safe stopping point;\n- prepare to provide a concise handoff describing what was completed, what remains, relevant files, and the exact recommended next step.";
pub const QUOTA_CRITICAL: &str = "The included usage limit is now very close.\n\nDo not open new workstreams.\n\nFocus on reaching a safe stopping point for the work already underway. Prefer completing or stabilizing current changes over further investigation.\n\nIf the full task cannot be completed, finish what is reasonably possible and prepare a concise handoff of the remaining work.";
pub const CREDITS_STARTED: &str = "This execution has started consuming paid credits.\n\nContinue only with work necessary to complete or safely stop the task already in progress.\n\nAvoid optional refactors, broad investigation, additional improvements, or expensive work that is not necessary to finish the current objective.";
pub const CREDIT_FINALIZE: &str = "The paid-credit budget for this execution is approaching its limit.\n\nBegin finalizing now.\n\nDo not start new investigations or optional work. Complete the work currently in progress where practical and leave the repository in a consistent state.\n\nBefore finishing, provide a concise handoff containing:\n- completed work;\n- remaining work;\n- current state;\n- relevant files;\n- important commands/tests still needed;\n- the exact recommended next step.";
pub const CREDIT_URGENT: &str = "The paid-credit budget is almost exhausted.\n\nFinalize immediately.\n\nDo not begin new tests, investigations, refactors, or features unless absolutely required to leave the repository in a consistent state.\n\nStop expanding the scope.\n\nComplete only what is necessary for a safe stopping point, then provide the handoff report.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Included,
    WrappingUp,
    Paid,
    PaidFinalizing,
    PaidCritical,
    Interrupting,
    Completed,
    Killed,
    Failed,
}
impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Included => "RUNNING",
            Self::WrappingUp => "WRAPPING UP",
            Self::Paid => "PAID",
            Self::PaidFinalizing => "FINALIZING",
            Self::PaidCritical => "PAID CRITICAL",
            Self::Interrupting => "INTERRUPTING",
            Self::Completed => "COMPLETED",
            Self::Killed => "KILLED",
            Self::Failed => "FAILED",
        }
    }
}

pub enum Action {
    Steer(&'static str, &'static str),
    Warn(String),
    Stop(String),
    Bell,
    Billing(&'static str),
    QuotaReset,
}

pub struct Policy {
    pub profile: Profile,
    pub phase: Phase,
    pub first: RateSnapshot,
    pub latest: RateSnapshot,
    pub spent: f64,
    pub paid_runtime: Duration,
    pub automatic_steers: u32,
    pub last_steer: Option<&'static str>,
    last_sample: Instant,
    last_balance: f64,
    account_id: Option<String>,
    paid_now: bool,
    triggered: HashSet<&'static str>,
    burn: VecDeque<(Instant, f64)>,
}

impl Policy {
    pub fn new(profile: Profile, first: RateSnapshot) -> Result<Self> {
        let balance = finite_balance(&first)?;
        if profile.quota.hard_stop {
            if first.primary.is_none() && first.ordinary_usage_allowed.is_none() {
                bail!("quota hard stop cannot be enforced without quota telemetry");
            }
            if first.primary.as_ref().is_some_and(|w| w.used >= 100.0)
                || first.ordinary_usage_allowed == Some(false)
            {
                bail!("included quota is already exhausted (quota hard stop enabled)");
            }
        }
        Ok(Self {
            profile,
            phase: Phase::Included,
            first: first.clone(),
            account_id: first.account_id.clone(),
            latest: first,
            spent: 0.0,
            paid_runtime: Duration::ZERO,
            automatic_steers: 0,
            last_steer: None,
            last_sample: Instant::now(),
            last_balance: balance,
            paid_now: false,
            triggered: HashSet::new(),
            burn: VecDeque::new(),
        })
    }
    pub fn paid_now(&self) -> bool {
        self.paid_now
    }
    pub fn balance(&self) -> f64 {
        self.last_balance
    }
    pub fn poll_interval(&self) -> Duration {
        let monitor = &self.profile.monitor;
        if self.spent >= self.profile.credits.max_spend * self.profile.credits.urgent_finalize_at
            || (self.profile.quota.hard_stop
                && self.latest.primary.as_ref().is_some_and(|w| {
                    100.0 - w.used <= self.profile.quota.critical_remaining_percent
                }))
        {
            config::duration(&monitor.critical_poll_interval)
        } else if self.paid_now {
            config::duration(&monitor.paid_poll_interval)
        } else if self
            .latest
            .primary
            .as_ref()
            .is_some_and(|w| 100.0 - w.used <= self.profile.quota.wrap_remaining_percent)
        {
            config::duration(&monitor.wrap_poll_interval)
        } else {
            config::duration(&monitor.poll_interval)
        }
    }
    pub fn can_tolerate_poll_failure(&self) -> bool {
        !self.paid_now
            && !self.profile.quota.hard_stop
            && self.spent < self.profile.credits.max_spend * self.profile.credits.urgent_finalize_at
            && self.latest.ordinary_usage_allowed != Some(false)
            && self
                .latest
                .primary
                .as_ref()
                .is_some_and(|w| 100.0 - w.used > self.profile.quota.wrap_remaining_percent)
    }
    pub fn observe(&mut self, snapshot: RateSnapshot) -> Vec<Action> {
        let mut actions = Vec::new();
        let now = Instant::now();
        if self.paid_now {
            self.paid_runtime += now.duration_since(self.last_sample);
        }
        self.last_sample = now;
        let reset = self.latest.primary.as_ref().and_then(|w| w.resets_at)
            != snapshot.primary.as_ref().and_then(|w| w.resets_at)
            && self
                .latest
                .primary
                .as_ref()
                .and_then(|w| w.resets_at)
                .is_some();
        if reset {
            if snapshot.ordinary_usage_allowed == Some(true) && self.paid_now {
                self.paid_now = false;
                actions.push(Action::Billing("INCLUDED"));
            }
            self.triggered.remove("quota_wrap_up");
            self.triggered.remove("quota_critical");
            actions.push(Action::QuotaReset);
        }
        if self.account_id.is_some()
            && snapshot.account_id.is_some()
            && self.account_id != snapshot.account_id
        {
            actions.push(Action::Stop(
                "account changed during task; credit baseline is invalid".into(),
            ));
            self.latest = snapshot;
            return actions;
        }
        if self.account_id.is_none() {
            self.account_id = snapshot.account_id.clone();
        }
        let balance = match finite_balance(&snapshot) {
            Ok(balance) => balance,
            Err(error) => {
                actions.push(Action::Stop(error.to_string()));
                self.latest = snapshot;
                return actions;
            }
        };
        let delta = (self.last_balance - balance).max(0.0);
        self.last_balance = balance;
        self.spent += delta;
        if self.profile.quota.hard_stop {
            let quota_exhausted = snapshot.primary.as_ref().is_some_and(|w| w.used >= 100.0)
                || snapshot.ordinary_usage_allowed == Some(false)
                || delta > 0.000001;
            let quota_unknown =
                snapshot.primary.is_none() && snapshot.ordinary_usage_allowed.is_none();
            if quota_exhausted || quota_unknown {
                actions.push(Action::Stop(if quota_unknown {
                    "quota telemetry unavailable with hard stop enabled".into()
                } else {
                    "quota hard stop reached".into()
                }));
                self.latest = snapshot;
                return actions;
            }
        }
        if delta > 0.000001 {
            if !self.paid_now {
                actions.push(Action::Billing("PAID CREDITS"));
            }
            self.paid_now = true;
            self.burn.push_back((now, delta));
            actions.push(Action::Bell);
        }
        if self.paid_now && self.triggered.insert("credits_started") {
            actions.push(Action::Steer("credits_started", CREDITS_STARTED));
        }
        let window = config::duration(&self.profile.burn_rate.window);
        while self
            .burn
            .front()
            .is_some_and(|(t, _)| now.duration_since(*t) > window)
        {
            self.burn.pop_front();
        }
        let recent_spend: f64 = self.burn.iter().map(|(_, amount)| amount).sum();
        if recent_spend >= self.profile.burn_rate.max_credit_spend
            && self.triggered.insert("burn_rate")
        {
            match self.profile.burn_rate.action {
                BurnAction::Warn => actions.push(Action::Warn(format!(
                    "high credit burn rate: {recent_spend:.2} credits"
                ))),
                BurnAction::Stop => actions.push(Action::Stop(format!(
                    "credit burn rate reached {recent_spend:.2}"
                ))),
            }
        }
        if self.paid_now && balance <= self.profile.credits.reserve {
            actions.push(Action::Stop("account credit reserve reached".into()));
        }
        if self.spent >= self.profile.credits.max_spend {
            actions.push(Action::Stop("task paid-credit budget reached".into()));
        } else if self.spent
            >= self.profile.credits.max_spend * self.profile.credits.urgent_finalize_at
        {
            self.phase = Phase::PaidCritical;
            self.triggered.insert("credit_finalize");
            if self.triggered.insert("credit_finalize_urgent") {
                actions.push(Action::Steer("credit_finalize_urgent", CREDIT_URGENT));
            }
        } else if self.spent >= self.profile.credits.max_spend * self.profile.credits.finalize_at {
            self.phase = Phase::PaidFinalizing;
            if self.triggered.insert("credit_finalize") {
                actions.push(Action::Steer("credit_finalize", CREDIT_FINALIZE));
            }
        } else if self.paid_now {
            self.phase = Phase::Paid;
        }
        if !self.paid_now {
            self.phase = Phase::Included;
            if let Some(window) = &snapshot.primary {
                let remaining = 100.0 - window.used;
                if remaining <= self.profile.quota.critical_remaining_percent {
                    self.phase = Phase::WrappingUp;
                    self.triggered.insert("quota_wrap_up");
                    if self.triggered.insert("quota_critical") {
                        actions.push(Action::Steer("quota_critical", QUOTA_CRITICAL));
                    }
                } else if remaining <= self.profile.quota.wrap_remaining_percent {
                    self.phase = Phase::WrappingUp;
                    if self.triggered.insert("quota_wrap_up") {
                        actions.push(Action::Steer("quota_wrap_up", QUOTA_WRAP_UP));
                    }
                }
            }
        }
        self.latest = snapshot;
        actions
    }
    pub fn set_budget(&mut self, budget: f64) -> Result<()> {
        if !budget.is_finite() || budget <= 0.0 {
            bail!("budget must be positive");
        }
        self.profile.credits.max_spend = budget;
        Ok(())
    }
    pub fn retry_trigger(&mut self, name: &'static str) {
        self.triggered.remove(name);
    }
}

fn finite_balance(snapshot: &RateSnapshot) -> Result<f64> {
    if snapshot.unlimited {
        bail!("credit balance is unlimited; a paid-credit hard budget cannot be enforced");
    }
    match (snapshot.balance, snapshot.has_credits) {
        (Some(balance), _) => Ok(balance),
        (None, Some(false)) => Ok(0.0),
        (None, _) => {
            bail!("App Server did not provide a numeric credit balance; cannot enforce hard budget")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_server::Window;

    fn sample(used: f64, reset: i64, balance: f64, allowed: Option<bool>) -> RateSnapshot {
        RateSnapshot {
            primary: Some(Window {
                used,
                resets_at: Some(reset),
                duration_mins: 300,
            }),
            secondary: None,
            balance: Some(balance),
            has_credits: Some(true),
            unlimited: false,
            ordinary_usage_allowed: allowed,
            account_id: Some("account-a".into()),
        }
    }
    #[test]
    fn paid_spend_survives_quota_reset_and_steers_once() {
        let mut profile = Profile::default();
        profile.credits.reserve = 0.0;
        let mut policy = Policy::new(profile, sample(80.0, 1, 100.0, Some(true))).unwrap();
        let actions = policy.observe(sample(90.0, 1, 100.0, Some(true)));
        assert_eq!(
            actions
                .iter()
                .filter(|x| matches!(x, Action::Steer("quota_wrap_up", _)))
                .count(),
            1
        );
        let actions = policy.observe(sample(100.0, 1, 97.0, Some(false)));
        assert_eq!(
            actions
                .iter()
                .filter(|x| matches!(x, Action::Steer("credits_started", _)))
                .count(),
            1
        );
        policy.observe(sample(2.0, 2, 97.0, Some(true)));
        assert_eq!(policy.spent, 3.0);
        assert!(!policy.paid_now());
        let actions = policy.observe(sample(100.0, 2, 95.0, Some(false)));
        assert_eq!(policy.spent, 5.0);
        assert!(!actions
            .iter()
            .any(|x| matches!(x, Action::Steer("credits_started", _))));
    }
    #[test]
    fn burn_rate_can_stop_before_total_budget() {
        let mut profile = Profile::default();
        profile.credits.reserve = 0.0;
        profile.burn_rate.max_credit_spend = 2.0;
        profile.burn_rate.action = BurnAction::Stop;
        let mut policy = Policy::new(profile, sample(0.0, 1, 100.0, Some(true))).unwrap();
        let actions = policy.observe(sample(100.0, 1, 97.0, Some(false)));
        assert!(actions.iter().any(|x| matches!(x, Action::Stop(_))));
        assert_eq!(policy.spent, 3.0);
    }
    #[test]
    fn quota_hard_stop_precedes_paid_transition() {
        let mut profile = Profile::default();
        profile.quota.hard_stop = true;
        let mut policy = Policy::new(profile, sample(95.0, 1, 100.0, Some(true))).unwrap();
        let actions = policy.observe(sample(100.0, 1, 99.0, Some(false)));
        assert!(actions.iter().any(|x| matches!(x, Action::Stop(_))));
        assert!(!policy.paid_now());
        assert_eq!(policy.spent, 1.0);
        assert!(!actions
            .iter()
            .any(|x| matches!(x, Action::Steer("credits_started", _))));
    }
    #[test]
    fn poll_interval_tracks_quota_and_credit_risk() {
        let mut profile = Profile::default();
        profile.credits.reserve = 0.0;
        let mut policy = Policy::new(profile, sample(20.0, 1, 100.0, Some(true))).unwrap();
        assert_eq!(policy.poll_interval(), Duration::from_secs(60));
        assert!(policy.can_tolerate_poll_failure());
        policy.observe(sample(86.0, 1, 100.0, Some(true)));
        assert_eq!(policy.poll_interval(), Duration::from_secs(30));
        assert!(!policy.can_tolerate_poll_failure());
        policy.observe(sample(100.0, 1, 99.0, Some(false)));
        assert_eq!(policy.poll_interval(), Duration::from_secs(10));
        policy.observe(sample(100.0, 1, 82.0, Some(false)));
        assert_eq!(policy.poll_interval(), Duration::from_secs(5));
    }
    #[test]
    fn zero_credits_with_null_balance_is_a_finite_zero() {
        let mut first = sample(20.0, 1, 0.0, Some(true));
        first.balance = None;
        first.has_credits = Some(false);
        let policy = Policy::new(Profile::default(), first).unwrap();
        assert_eq!(policy.balance(), 0.0);
        assert_eq!(policy.spent, 0.0);
    }
    #[test]
    fn available_credits_with_null_balance_fail_closed() {
        let mut first = sample(20.0, 1, 0.0, Some(true));
        first.balance = None;
        first.has_credits = Some(true);
        assert!(Policy::new(Profile::default(), first).is_err());
    }
    #[test]
    fn top_up_from_zero_is_not_spend_but_later_debit_is() {
        let mut profile = Profile::default();
        profile.credits.reserve = 0.0;
        let mut first = sample(20.0, 1, 0.0, Some(true));
        first.balance = None;
        first.has_credits = Some(false);
        let mut policy = Policy::new(profile, first).unwrap();
        let actions = policy.observe(sample(20.0, 1, 10.0, Some(true)));
        assert_eq!(policy.balance(), 10.0);
        assert_eq!(policy.spent, 0.0);
        assert!(!policy.paid_now());
        assert!(!actions.iter().any(|a| matches!(a, Action::Billing(_))));
        let actions = policy.observe(sample(100.0, 1, 8.0, Some(false)));
        assert_eq!(policy.spent, 2.0);
        assert!(policy.paid_now());
        assert!(actions
            .iter()
            .any(|a| matches!(a, Action::Billing("PAID CREDITS"))));
    }
}
