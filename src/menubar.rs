// ---------------------------------------------------------------------------
// Menu bar integration (SwiftBar / xbar plugin format)
//
// `claude-usage menubar` prints one refresh of the menu: a compact title for
// the bar itself, then the dropdown — running loops with stage meters and
// hoverable tooltips, click-through stage submenus, per-session quit, a
// keep-awake toggle, and dashboard links. SwiftBar runs it on the cadence in
// the plugin filename (claude-usage-loops.15s.sh).
//
// Icons are SF Symbols via SwiftBar's `sfimage` param — vector-crisp on any
// display and tinted to match the menu automatically. xbar ignores the param
// and just shows text.
//
// Visibility: running loops always show; stopped loops fade out after 72h
// (they stay in the CLI and dashboard) and carry a Dismiss action to hide
// them immediately. Dismissed loops reappear if they run again.
// ---------------------------------------------------------------------------

use crate::{awake, loops};
use anyhow::{anyhow, Result};
use std::{fs, path::PathBuf};

/// SwiftBar item text must not contain the param delimiter, newlines, or
/// double quotes (which would close a quoted param value).
fn clean(s: &str) -> String {
    s.replace('|', "¦").replace('\n', " ").replace('"', "'")
}

fn sf(name: &str) -> String {
    format!(" sfimage={}", name)
}

fn exe() -> String {
    std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "claude-usage".into())
}

fn dashboard_up() -> bool {
    use std::net::TcpStream;
    use std::time::Duration;
    TcpStream::connect_timeout(
        &"127.0.0.1:4711".parse().unwrap(),
        Duration::from_millis(150),
    )
    .is_ok()
}

fn hours_since(ts: Option<&str>, now: chrono::DateTime<chrono::Utc>) -> i64 {
    ts.and_then(loops::parse_ts)
        .map(|t| (now - t).num_hours())
        .unwrap_or(i64::MAX)
}

// Dot meters read cleanly at menu size in the system font; terminal-style
// █░ blocks smear into slabs there, and ▰▱ renders as tiny slivers.
fn meter(done: usize, total: usize) -> String {
    let total = total.max(1);
    let filled = done.min(total);
    format!("{}{}", "●".repeat(filled), "○".repeat(total - filled))
}

fn pct_meter(pct: f64) -> String {
    meter((pct / 10.0).round() as usize, 10)
}

/// Same thresholds as the CLI statusline (ctx_colored): green under 50%,
/// amber under 80%, red past that.
///
/// One hex must serve both menu appearances (SwiftBar's light,dark pairs
/// mis-detect on dark-wallpaper menu bars), and a single value tops out
/// around 3.6:1 against both a light and a dark menu — these are picked at
/// that balance point, deep enough not to wash out on white. The colored
/// SF Symbol dot doubles the cue so the state never rides on text tint alone.
fn usage_color(pct: f64) -> &'static str {
    if pct < 50.0 {
        "#0a8f0a"
    } else if pct < 80.0 {
        "#b37400"
    } else {
        "#d64545"
    }
}

/// CodexBar-style usage block: promo window, 5h/7d rate-limit bars with pace
/// markers and reset countdowns, day/week token totals, session cost, and any
/// API incident. Everything comes from local caches the statusline keeps
/// fresh — no network from the menu's 15s cadence.
fn print_usage(now: chrono::DateTime<chrono::Utc>) {
    let mut lines: Vec<String> = vec![];

    if let Ok(cfg) = crate::load_config() {
        let s = crate::evaluate(cfg, now);
        if !s.active_windows.is_empty() {
            // "1x off-peak" reads like a bug — name the multiplier only when
            // it actually multiplies
            let mult = (s.multiplier != 1.0)
                .then(|| format!("{:.0}x ", s.multiplier))
                .unwrap_or_default();
            if s.favorable {
                lines.push(format!(
                    "{}off-peak · ends in {} |{} sfcolor=#0a8f0a color=#0a8f0a",
                    mult,
                    crate::fmt_mins_opt(s.mins_until_change),
                    sf("bolt.fill")
                ));
            } else {
                let next = s
                    .active_windows
                    .iter()
                    .filter(|w| w.favorable)
                    .map(|w| w.multiplier)
                    .next()
                    .unwrap_or(2.0);
                lines.push(format!(
                    "{}peak · {:.0}x in {} |{} sfcolor=#b37400 color=#b37400",
                    mult,
                    next,
                    crate::fmt_mins_opt(s.mins_until_favorable),
                    sf("clock")
                ));
            }
        }
    }

    let cache = crate::load_statusline_cache();
    let age_mins = cache
        .as_ref()
        .and_then(|c| loops::parse_ts(&c.updated))
        .map(|t| (now - t).num_minutes())
        .unwrap_or(i64::MAX);
    if let Some(c) = &cache {
        let now_ts = now.timestamp();
        let windows: [(&str, Option<f64>, Option<i64>, i64); 2] = [
            ("5h", c.five_hour_pct, c.five_hour_resets_at, 18_000),
            ("7d", c.seven_day_pct, c.seven_day_resets_at, 604_800),
        ];
        for (label, pct, resets_at, _duration) in windows {
            let Some(pct) = pct else { continue };
            let reset = resets_at
                .filter(|&ts| ts > now_ts)
                .map(|ts| format!(" · resets {}", crate::fmt_mins(((ts - now_ts) / 60) as u32)))
                .unwrap_or_default();
            lines.push(format!(
                "{}  {}  {:.0}%{} |{} sfcolor={} color={}",
                label,
                pct_meter(pct),
                pct,
                reset,
                sf("circle.fill"),
                usage_color(pct),
                usage_color(pct)
            ));
        }
        // pace verdict off the 7d window, like CodexBar's "Pace: Behind"
        if let (Some(pct), Some(ts)) = (c.seven_day_pct, c.seven_day_resets_at) {
            if ts > now_ts {
                let elapsed = 604_800 - (ts - now_ts);
                let pace = (elapsed as f64 / 604_800.0 * 100.0).clamp(0.0, 100.0);
                let delta = pct - pace;
                let word = if delta > 1.0 { "ahead" } else { "behind" };
                if delta.abs() > 1.0 {
                    lines.push(format!("pace: {} ({:+.0}%) | size=12", word, delta));
                }
            }
        }
        if age_mins <= 120 {
            // sum cost over recently-active sessions — with several Claudes
            // running, any single session's figure is misleading
            let fresh: Vec<_> = c
                .sessions
                .values()
                .filter(|s| {
                    loops::parse_ts(&s.updated)
                        .map(|t| (now - t).num_minutes() <= 120)
                        .unwrap_or(false)
                })
                .filter(|s| s.cost_usd > 0.001)
                .collect();
            if !fresh.is_empty() {
                let total: f64 = fresh.iter().map(|s| s.cost_usd).sum();
                let label = if fresh.len() == 1 {
                    "session".to_string()
                } else {
                    format!("{} sessions", fresh.len())
                };
                let rate = (fresh.len() == 1)
                    .then(|| fresh[0])
                    .filter(|s| s.duration_ms > 60_000)
                    .map(|s| format!(" (${:.2}/h)", s.cost_usd / (s.duration_ms as f64 / 3_600_000.0)))
                    .unwrap_or_default();
                lines.push(format!("{} ~${:.2}{}", label, total, rate));
            }
        } else if age_mins < i64::MAX {
            lines.push(format!(
                "as of {} ago | size=12",
                crate::fmt_mins(age_mins.min(u32::MAX as i64) as u32)
            ));
        }
    }

    let state = crate::load_usage_state();
    let day = state
        .daily
        .get(&now.format("%Y-%m-%d").to_string())
        .copied()
        .unwrap_or(0);
    let week = state
        .weekly
        .get(&crate::iso_week_key(now))
        .copied()
        .unwrap_or(0);
    if day > 0 || week > 0 {
        lines.push(format!(
            "today {} · week {} tokens",
            crate::fmt_tokens(day),
            crate::fmt_tokens(week)
        ));
    }

    if let Some(api) = crate::load_cached_api_status() {
        if api.indicator != "none" && api.indicator != "unknown" {
            lines.push(format!(
                "API: {} |{} sfcolor=#d64545 color=#d64545 href=https://status.claude.com",
                clean(&api.description),
                sf("exclamationmark.triangle")
            ));
        }
    }

    if !lines.is_empty() {
        println!("Claude Usage | size=11");
        for l in lines {
            println!("{}", l);
        }
        println!("---");
    }
}

pub fn run_menubar() {
    let mut cache = loops::load_scan_cache();
    let all = loops::collect_loops(&[], &mut cache);
    loops::save_scan_cache(&cache);
    let dismissed = loops::load_dismissed();
    let awake_state = awake::status();
    let now = chrono::Utc::now();
    let exe = exe();
    let up = dashboard_up();

    let ralphs: Vec<_> = all
        .iter()
        .filter(|l| l.kind == "ralph" && !loops::is_dismissed(l, &dismissed))
        .collect();
    let running: Vec<_> = ralphs.iter().filter(|l| l.running).copied().collect();
    let recent: Vec<_> = ralphs
        .iter()
        .filter(|l| !l.running && hours_since(l.updated.as_deref(), now) <= 72)
        .copied()
        .collect();
    let older = ralphs.len() - running.len() - recent.len();
    let sessions: Vec<_> = all.iter().filter(|l| l.kind != "ralph").collect();

    // ── Title ──────────────────────────────────────────────────────────────
    let mut title = match running.as_slice() {
        [] => String::new(),
        [one] => match &one.stage {
            Some(s) => format!("{}/{}", s.current, s.total),
            None => "1".into(),
        },
        many => format!("{}", many.len()),
    };
    if awake_state.is_some() {
        title.push_str(" ☕");
    }
    println!("{} |{}", title, sf("repeat"));
    println!("---");

    print_usage(now);

    // ── Ralph loops ────────────────────────────────────────────────────────
    println!("Ralph Loops | size=11");
    if running.is_empty() && recent.is_empty() {
        println!("No active loops | size=12");
    }
    for l in running.iter().chain(recent.iter()) {
        let icon = match l.state.as_str() {
            "running" => sf("repeat"),
            "completed" => sf("checkmark.circle"),
            "failed" => sf("exclamationmark.triangle"),
            _ => sf("pause.circle"),
        };
        let mut text = l.name.clone();
        if let Some(s) = &l.stage {
            text.push_str(&format!("  ·  stage {}/{}", s.current, s.total));
            if let Some(t) = &s.title {
                text.push_str(&format!(" — {}", t));
            }
        } else if !l.running {
            text.push_str(&format!("  ·  {}", l.state));
        }
        let tooltip = l
            .last_event
            .as_ref()
            .map(|e| clean(&format!("{}: {}", e.topic, e.text)))
            .unwrap_or_default();
        let action = if up { " href=http://127.0.0.1:4711" } else { "" };
        println!(
            "{} | tooltip=\"{}\"{}{}",
            clean(&text),
            tooltip.chars().take(200).collect::<String>(),
            icon,
            action
        );

        // click-through: stages, freshness, dismiss — these `--` items must
        // directly follow the name row so the submenu anchors to it
        for s in &l.stages {
            let mark = match s.status.as_str() {
                "done" => "✓",
                "current" => "▶",
                _ => "·",
            };
            let closed = s.tasks.iter().filter(|t| t.status == "closed").count();
            let tally = if s.tasks.is_empty() {
                String::new()
            } else {
                format!("  ({}/{} tasks)", closed, s.tasks.len())
            };
            println!(
                "-- {} {} {}{}",
                mark,
                s.n,
                clean(s.title.as_deref().unwrap_or("")),
                tally
            );
        }
        if let Some(u) = &l.updated {
            println!("-- updated {}", loops::ago(u, now));
        }
        if !l.running {
            println!(
                "-- Dismiss from menu | bash=\"{}\" param1=loops param2=dismiss param3=\"{}\" terminal=false refresh=true{}",
                exe,
                l.id,
                sf("eye.slash")
            );
        }

        // quiet stage meter under the row (its own item, no submenu)
        if l.running && !l.stages.is_empty() {
            let done = l.stages.iter().filter(|s| s.status == "done").count();
            let mut bar = format!("{}  {}/{}", meter(done, l.stages.len()), done, l.stages.len());
            if let Some(s) = &l.stage {
                if let (Some(a), Some(b)) = (s.step, s.step_total) {
                    bar.push_str(&format!(" · step {}/{}", a, b));
                }
                if let Some(i) = l.iteration {
                    bar.push_str(&format!(" · it {}", i));
                }
            }
            println!("{} | size=12", bar);
        }
    }
    if older > 0 {
        let action = if up { " href=http://127.0.0.1:4711" } else { "" };
        println!("{} older in the dashboard | size=12{}", older, action);
    }

    // ── Sessions ───────────────────────────────────────────────────────────
    if !sessions.is_empty() {
        println!("---");
        println!("Claude Sessions | size=11");
        for l in &sessions {
            let icon = match l.state.as_str() {
                "busy" => sf("ellipsis.bubble"),
                "shell" => sf("terminal"),
                _ => sf("bubble.left"),
            };
            let goal = l
                .goal
                .as_ref()
                .map(|g| format!("  ⌖ {}", g.text.chars().take(60).collect::<String>()))
                .unwrap_or_default();
            // label the preview clearly — it's the session's own chatter, and
            // an unlabeled quote about e.g. ralph work reads like a claim
            // about this session's state
            let tip = l
                .recent
                .last()
                .map(|m| format!("last message ({}): {}", m.role, m.text))
                .unwrap_or_else(|| l.dir.clone());
            println!(
                "{}  ·  {}{} | tooltip=\"{}\"{}",
                clean(&l.name),
                l.state,
                clean(&goal),
                clean(&tip).chars().take(200).collect::<String>(),
                icon
            );
            if let Some(u) = &l.updated {
                println!("-- {} · {}", loops::ago(u, now), clean(&l.dir));
            }
            if let Some(pid) = l.pid {
                println!(
                    "-- Quit session | bash=\"{}\" param1=loops param2=quit param3={} terminal=false refresh=true{}",
                    exe,
                    pid,
                    sf("power")
                );
                println!("---- transcript is saved; `claude --resume` restores it | size=12");
            }
        }
    }

    // ── Keep awake ─────────────────────────────────────────────────────────
    println!("---");
    match &awake_state {
        Some(s) => {
            let lid = if s.lid { " · lid-closed mode" } else { "" };
            println!(
                "Awake{} — click to let it sleep | bash=\"{}\" param1=awake param2=off terminal=false refresh=true{}",
                lid,
                exe,
                sf("cup.and.saucer.fill")
            );
        }
        None => {
            println!(
                "Keep awake | bash=\"{}\" param1=awake param2=on terminal=false refresh=true{}",
                exe,
                sf("moon.zzz")
            );
            println!(
                "-- with closed-lid support (sudo) | bash=\"{}\" param1=awake param2=on param3=--lid terminal=true refresh=true",
                exe
            );
        }
    }

    // ── Dashboard ──────────────────────────────────────────────────────────
    if up {
        println!("Open dashboard | href=http://127.0.0.1:4711{}", sf("gauge"));
    } else {
        println!(
            "Start dashboard | bash=\"{}\" param1=loops param2=--serve param3=--open terminal=false refresh=true{}",
            exe,
            sf("gauge")
        );
    }
    println!("Refresh |{} refresh=true", sf("arrow.clockwise"));
}

// ---------------------------------------------------------------------------
// Plugin installation
// ---------------------------------------------------------------------------

fn swiftbar_plugin_dir() -> Result<PathBuf> {
    // honor an existing SwiftBar plugin directory; register one if unset
    let read = std::process::Command::new("defaults")
        .args(["read", "com.ameba.SwiftBar", "PluginDirectory"])
        .output();
    if let Ok(o) = read {
        let dir = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if o.status.success() && !dir.is_empty() {
            return Ok(PathBuf::from(dir));
        }
    }
    let home = std::env::var("HOME").map_err(|_| anyhow!("HOME not set"))?;
    let dir = PathBuf::from(home).join(".config/swiftbar");
    fs::create_dir_all(&dir)?;
    let ok = std::process::Command::new("defaults")
        .args([
            "write",
            "com.ameba.SwiftBar",
            "PluginDirectory",
            "-string",
            &dir.display().to_string(),
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        return Err(anyhow!("could not register SwiftBar plugin directory"));
    }
    Ok(dir)
}

pub fn install_plugin() -> Result<()> {
    if !cfg!(target_os = "macos") {
        return Err(anyhow!("menu bar install is macOS-only (SwiftBar)"));
    }
    let dir = swiftbar_plugin_dir()?;
    let plugin = dir.join("claude-usage-loops.15s.sh");
    // prefer whatever claude-usage is on PATH (survives reinstalls), falling
    // back to the binary that ran --install
    let shim = format!(
        "#!/bin/sh\nPATH=\"$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH\"\nif command -v claude-usage >/dev/null 2>&1; then exec claude-usage menubar; fi\nexec \"{}\" menubar\n",
        exe()
    );
    fs::write(&plugin, shim)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&plugin, fs::Permissions::from_mode(0o755))?;
    }
    println!("  Plugin installed: {}", plugin.display());
    println!("  Refresh cadence:  15s (rename the file to change)");
    println!("  If SwiftBar isn't installed yet:  brew install --cask swiftbar");
    println!("  Then launch it:                   open -a SwiftBar");
    Ok(())
}
