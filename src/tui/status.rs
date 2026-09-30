use crate::{
    app_server::{ModelSelection, ThreadUsage},
    policy::Policy,
};
use std::time::Instant;

pub struct StatusContext<'a> {
    pub policy: &'a Policy,
    pub lifecycle: &'a str,
    pub profile: &'a str,
    pub mode: &'a str,
    pub started: Instant,
    pub usage: Option<&'a ThreadUsage>,
    pub tools_done: u64,
    pub tools_running: u64,
}

pub fn compact(ctx: &StatusContext<'_>, selection: Option<&ModelSelection>, width: u16) -> String {
    let p = ctx.policy;
    let model = selection
        .map(|s| s.display_name.as_str())
        .unwrap_or("model: default");
    let short_model = selection
        .and_then(|s| s.display_name.split_whitespace().last())
        .unwrap_or("default");
    let effort = selection.and_then(|s| s.effort.as_deref());
    let quota = p
        .latest
        .primary
        .as_ref()
        .map(|w| format!("{:.0}%", (100.0 - w.used).max(0.0)))
        .unwrap_or_else(|| "?".into());
    let paid = if p.paid_now() { "PAID " } else { "" };
    let spend = format!("{paid}{:.2}/{:.2} cr", p.spent, p.profile.credits.max_spend);
    let number = |n: f64| {
        let s = format!("{n:.2}");
        s.trim_end_matches('0').trim_end_matches('.').to_owned()
    };
    let short_spend = format!(
        "{paid}{}/{}",
        number(p.spent),
        number(p.profile.credits.max_spend)
    );
    let mut full = vec![ctx.lifecycle.to_owned(), model.into()];
    if let Some(effort) = effort {
        full.push(effort.into());
    }
    full.extend([spend, format!("5h {quota} left")]);
    let mut extras = full.clone();
    if let Some(w) = &p.latest.secondary {
        extras.push(format!("weekly {:.0}% left", (100.0 - w.used).max(0.0)));
    }
    extras.push(ctx.profile.to_owned());
    let mut candidates = vec![extras.join(" · ")];
    extras.pop();
    candidates.push(extras.join(" · "));
    candidates.push(full.join(" · "));
    let short_agent = format!(
        "{short_model}{}",
        effort.map(|e| format!("/{e}")).unwrap_or_default()
    );
    candidates.push(format!(
        "{} · {short_agent} · {short_spend} cr · {quota} left",
        ctx.lifecycle
    ));
    candidates.push(format!(
        "{} · {short_agent} · {short_spend} · {quota}",
        ctx.lifecycle
    ));
    candidates.push(format!("{} · {short_agent} · {short_spend}", ctx.lifecycle));
    let fallback = candidates.last().unwrap().clone();
    candidates
        .into_iter()
        .find(|s| ratatui::text::Line::from(s.as_str()).width() <= width as usize)
        .unwrap_or_else(|| truncate_cells(&fallback, width))
}

fn truncate_cells(text: &str, width: u16) -> String {
    let mut result = String::new();
    for ch in text.chars() {
        result.push(ch);
        if ratatui::text::Line::from(result.as_str()).width() > width as usize {
            result.pop();
            break;
        }
    }
    result
}
pub fn detail(ctx: &StatusContext<'_>, selection: Option<&ModelSelection>) -> String {
    let p = ctx.policy;
    let quota = p
        .latest
        .primary
        .as_ref()
        .map(|w| {
            format!(
                "{:.0}% used · {:.0}% remaining",
                w.used,
                (100.0 - w.used).max(0.0)
            )
        })
        .unwrap_or_else(|| "unavailable".into());
    let reset = p
        .latest
        .primary
        .as_ref()
        .and_then(|w| w.resets_at)
        .map(|at| {
            let seconds = (at - chrono::Utc::now().timestamp()).max(0) as u64;
            format!(
                "{:02}:{:02}:{:02}",
                seconds / 3600,
                seconds / 60 % 60,
                seconds % 60
            )
        })
        .unwrap_or_else(|| "unknown".into());
    let weekly = p
        .latest
        .secondary
        .as_ref()
        .map(|w| format!("{:.0}% used", w.used))
        .unwrap_or_else(|| "unavailable".into());
    let billing = if p.paid_now() { "PAID" } else { "INCLUDED" };
    let usage = ctx
        .usage
        .map(|u| {
            format!(
                "{:.3} cr",
                u.estimated_usage_credits_micros as f64 / 1_000_000.0
            )
        })
        .unwrap_or_else(|| "unavailable".into());
    let runtime = ctx.started.elapsed().as_secs();
    let mut text = format!("\nSession\n  State         {}\n  Profile       {}\n  Mode          {}\n  Permissions   YOLO · no sandbox / no approvals\n  Runtime       {:02}:{:02}:{:02}\n\nQuota\n  5h            {}\n  Reset         {}\n  Weekly        {}\n\nBilling\n  State         {}\n  Policy        {}\n  Session       {:.2} / {:.2} cr\n  Account       {:.2} cr\n  Thread est.   {}\n\nAgent\n  Last steer    {}\n  Tools         {} completed · {} running\n", ctx.lifecycle, ctx.profile, ctx.mode,  runtime/3600, runtime/60%60, runtime%60, quota, reset, weekly, billing, p.phase.label(), p.spent, p.profile.credits.max_spend, p.balance(), usage, p.last_steer.unwrap_or("—"), ctx.tools_done, ctx.tools_running);
    if let Some(s) = selection {
        text.push_str(&format!(
            "  Model         {}\n  Model ID      {}\n  Model slug    {}\n  Reasoning     {}\n",
            s.display_name,
            s.id,
            s.model,
            s.effort.as_deref().unwrap_or("default")
        ));
    } else {
        text.push_str("  Model         App Server default\n  Reasoning     App Server default\n");
    }
    if let Some(usage) = ctx.usage {
        for group in &usage.groups {
            text.push_str(&format!(
                "  Model         {} · {} · {} tokens\n",
                group.model.as_deref().unwrap_or("unknown"),
                group
                    .reasoning_effort
                    .as_deref()
                    .unwrap_or("unknown effort"),
                group
                    .total_tokens
                    .map(|x| x.to_string())
                    .unwrap_or_else(|| "?".into())
            ));
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        app_server::{RateSnapshot, Window},
        config::Profile,
    };
    fn policy() -> Policy {
        Policy::new(
            Profile::default(),
            RateSnapshot {
                primary: Some(Window {
                    used: 12.0,
                    resets_at: None,
                    duration_mins: 300,
                }),
                secondary: Some(Window {
                    used: 32.0,
                    resets_at: None,
                    duration_mins: 10080,
                }),
                balance: Some(20.0),
                has_credits: Some(true),
                unlimited: false,
                ordinary_usage_allowed: Some(true),
                account_id: Some("a".into()),
            },
        )
        .unwrap()
    }
    fn ctx<'a>(policy: &'a Policy, lifecycle: &'a str, started: Instant) -> StatusContext<'a> {
        StatusContext {
            policy,
            lifecycle,
            profile: "default",
            mode: "UNATTENDED",
            started,
            usage: None,
            tools_done: 0,
            tools_running: 0,
        }
    }
    #[test]
    fn ready_running_paid_and_narrow_status() {
        let mut p = policy();
        let started = Instant::now();
        let ready = compact(&ctx(&p, "READY", started), None, 120);
        assert!(ready.contains("READY · model: default"));
        assert!(ready.contains("weekly 68% left"));
        assert!(!ready.contains("YOLO"));
        let running = compact(&ctx(&p, "RUNNING", started), None, 120);
        assert!(running.contains("RUNNING · model: default"));
        assert!(running.contains("0.00/20.00 cr"));
        let narrow = compact(&ctx(&p, "RUNNING", started), None, 25);
        assert!(narrow.chars().count() <= 25);
        assert!(!narrow.contains("weekly"));
        p.observe(RateSnapshot {
            primary: p.latest.primary.clone(),
            secondary: None,
            balance: Some(19.0),
            has_credits: Some(true),
            unlimited: false,
            ordinary_usage_allowed: Some(false),
            account_id: Some("a".into()),
        });
        let paid = compact(&ctx(&p, "RUNNING", started), None, 120);
        assert!(paid.contains("PAID"));
        assert!(!paid.contains("weekly"));
        assert!(detail(&ctx(&p, "RUNNING", started), None).contains("Thread est.   unavailable"));
    }
    #[test]
    fn selected_model_effort_and_paid_spend_survive_responsive_layouts() {
        let mut p = policy();
        let started = Instant::now();
        let selection = ModelSelection {
            id: "catalog-id".into(),
            model: "actual-model-slug".into(),
            display_name: "Coding Gamma".into(),
            effort: Some("deep".into()),
        };
        let ready = compact(&ctx(&p, "READY", started), Some(&selection), 100);
        assert!(ready.contains("Coding Gamma · deep"));
        let medium = compact(&ctx(&p, "READY", started), Some(&selection), 45);
        assert!(medium.contains("Gamma/deep"));
        assert!(medium.contains("0/20"));
        for width in [0, 1, 12, 25, 45, 80] {
            let line = compact(&ctx(&p, "READY", started), Some(&selection), width);
            assert!(ratatui::text::Line::from(line.as_str()).width() <= width as usize);
            assert!(!line.contains('\n'));
        }
        let status = detail(&ctx(&p, "COMPLETED", started), Some(&selection));
        assert!(status.contains("Model ID      catalog-id"));
        assert!(status.contains("Reasoning     deep"));
        p.observe(RateSnapshot {
            primary: p.latest.primary.clone(),
            secondary: None,
            balance: Some(19.0),
            has_credits: Some(true),
            unlimited: false,
            ordinary_usage_allowed: Some(false),
            account_id: Some("a".into()),
        });
        let paid = compact(&ctx(&p, "RUNNING", started), Some(&selection), 40);
        assert!(paid.contains("Gamma/deep"));
        assert!(paid.contains("PAID 1/20"));
    }
}
