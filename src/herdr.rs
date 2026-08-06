//! herdr directory resolution (spec: 起動 → ディレクトリ解決の優先順位).
//!
//! When `HERDR_ENV=1`, try in order:
//! 1. `herdr worktree list` → `result.source.repo_root`
//! 2. `herdr agent list` → the first agent whose `workspace_id` equals
//!    `HERDR_WORKSPACE_ID`, using its `cwd`
//!
//! revpick itself stays herdr-independent: `herdr` missing from PATH (or
//! any command failing, or hanging past [`HERDR_TIMEOUT`]) yields `None`
//! and the caller falls back to the current directory — never a crash or
//! a stuck startup (spec test 15).

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

/// How long herdr may take before we give up and fall back to the cwd —
/// the queries run before the TUI opens, so a hung herdr must not block
/// startup forever.
const HERDR_TIMEOUT: Duration = Duration::from_millis(1500);

/// Resolve the project root from herdr, if we're inside a herdr workspace
/// and the `herdr` CLI answers. `None` on any failure (the caller falls
/// back to the current directory).
pub fn resolve_root() -> Option<PathBuf> {
    if std::env::var("HERDR_ENV").as_deref() != Ok("1") {
        return None;
    }
    worktree_root().or_else(agent_cwd)
}

/// `herdr worktree list` → the worktree of the CURRENT workspace
/// (`open_workspace_id == HERDR_WORKSPACE_ID`).
fn worktree_root() -> Option<PathBuf> {
    let v = herdr_json(&["worktree", "list"])?;
    let ws = std::env::var("HERDR_WORKSPACE_ID").ok()?;
    worktree_root_in(&v, &ws)
}

/// Pure part of [`worktree_root`] (unit-tested directly).
///
/// The worktree list only contains the herdr UI's *focused* workspace,
/// so when the focus lives elsewhere (the pane's workspace differs) no
/// entry matches and the resolution falls through to [`agent_cwd`],
/// which filters the (all-workspace) agent list by workspace id.
/// `source.repo_root` is deliberately NOT used: it follows the UI focus
/// and would silently resolve to the wrong repo (verified 2026-08-05).
fn worktree_root_in(v: &Value, ws: &str) -> Option<PathBuf> {
    let worktrees = v.pointer("/result/worktrees")?.as_array()?;
    worktrees
        .iter()
        .find(|wt| wt.get("open_workspace_id").and_then(Value::as_str) == Some(ws))
        .and_then(|wt| wt.get("path").and_then(Value::as_str))
        .map(PathBuf::from)
}

/// `herdr agent list` → the cwd of the sole agent in the current tab,
/// else the sole agent in the current workspace.
fn agent_cwd() -> Option<PathBuf> {
    let v = herdr_json(&["agent", "list"])?;
    let tab = std::env::var("HERDR_TAB_ID").ok();
    let ws = std::env::var("HERDR_WORKSPACE_ID").ok();
    let me = std::env::var("HERDR_PANE_ID").ok();
    agent_cwd_in(&v, tab.as_deref(), ws.as_deref(), me.as_deref())
}

/// Pure part of [`agent_cwd`] (unit-tested directly) — the same
/// resolution herdr-reviewr ships. Excludes our own pane (`me`), and
/// refuses (returns `None`, so the caller falls back to the cwd) when
/// several agents could match: guessing among them picks wrong repos
/// (e.g. a workspace-scope agent whose cwd is `~/src`).
fn agent_cwd_in(
    v: &Value,
    tab: Option<&str>,
    ws: Option<&str>,
    me: Option<&str>,
) -> Option<PathBuf> {
    let agents = v.pointer("/result/agents")?.as_array()?;
    // Only real agents count; our own pane never does.
    let candidates: Vec<&Value> = agents
        .iter()
        .filter(|a| a.get("agent").and_then(Value::as_str).is_some())
        .filter(|a| a.get("pane_id").and_then(Value::as_str) != me)
        .collect();
    let in_tab: Vec<&&Value> = candidates
        .iter()
        .filter(|a| a.get("tab_id").and_then(Value::as_str) == tab)
        .collect();
    if let [agent] = in_tab.as_slice() {
        return agent.get("cwd").and_then(Value::as_str).map(PathBuf::from);
    }
    let in_ws: Vec<&&Value> = candidates
        .iter()
        .filter(|a| a.get("workspace_id").and_then(Value::as_str) == ws)
        .collect();
    if let [agent] = in_ws.as_slice() {
        return agent.get("cwd").and_then(Value::as_str).map(PathBuf::from);
    }
    None
}

/// Run `herdr <args>` and parse stdout as JSON; `None` if herdr is
/// missing, exits non-zero, emits non-JSON, or is still running at
/// [`HERDR_TIMEOUT`] (the child is killed then). The list outputs are
/// small (well under the pipe buffer), so reading after exit is safe —
/// and if herdr somehow fills the pipe and blocks, the timeout kills it.
fn herdr_json(args: &[&str]) -> Option<Value> {
    let mut child = Command::new("herdr")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + HERDR_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                use std::io::Read;
                let mut out = Vec::new();
                child.stdout.take()?.read_to_end(&mut out).ok()?;
                return serde_json::from_slice(&out).ok();
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn absent_herdr_env_var_short_circuits() {
        // Without HERDR_ENV=1 we must not even try to spawn herdr.
        // (Env mutation is process-global; the previous value is restored
        // so parallel tests aren't left with a forged environment.)
        let prev = std::env::var("HERDR_ENV").ok();
        unsafe {
            std::env::remove_var("HERDR_ENV");
        }
        assert_eq!(resolve_root(), None);
        if let Some(v) = prev {
            unsafe {
                std::env::set_var("HERDR_ENV", v);
            }
        }
    }

    #[test]
    fn worktree_resolves_own_workspace_only() {
        // The worktree list only carries the focused workspace: when it
        // is ours the path resolves, when it isn't the lookup must miss
        // (the caller then falls through to the agent list).
        let v = json!({
            "result": { "worktrees": [{ "open_workspace_id": "wA", "path": "/tao" }] }
        });
        assert_eq!(worktree_root_in(&v, "wA"), Some(PathBuf::from("/tao")));
        assert_eq!(worktree_root_in(&v, "wY"), None, "focused-only list has no wY");
        // Malformed output never panics.
        assert_eq!(worktree_root_in(&json!({}), "wA"), None);
        assert_eq!(worktree_root_in(&json!({"result": {"worktrees": "x"}}), "wA"), None);
    }

    #[test]
    fn agent_cwd_resolves_sole_agent_in_tab_then_workspace() {
        // Tab wY:tV has exactly one real agent besides ourselves → it wins.
        let v = json!({
            "result": { "agents": [
                { "agent": "claude", "pane_id": "wY:p1K", "tab_id": "wY:tV",
                  "workspace_id": "wY", "cwd": "/hermes" },
                { "agent": "pi", "pane_id": "wY:p21", "tab_id": "wY:tY",
                  "workspace_id": "wY", "cwd": "/revpick" }
            ] }
        });
        assert_eq!(
            agent_cwd_in(&v, Some("wY:tV"), Some("wY"), Some("wY:p21")),
            Some(PathBuf::from("/hermes"))
        );
        // No tab match, but excluding our own pane leaves exactly one
        // workspace agent → it wins.
        assert_eq!(
            agent_cwd_in(&v, Some("wY:tZ"), Some("wY"), Some("wY:p21")),
            Some(PathBuf::from("/hermes"))
        );
    }

    #[test]
    fn agent_cwd_refuses_when_ambiguous() {
        // Several workspace agents with different cwds → None (caller
        // falls back to the cwd instead of guessing a wrong repo).
        let v = json!({
            "result": { "agents": [
                { "agent": "hermes", "pane_id": "wY:p1W", "tab_id": "wY:tX",
                  "workspace_id": "wY", "cwd": "/Users/nagata/src/tries" },
                { "agent": "claude", "pane_id": "wY:p1K", "tab_id": "wY:tV",
                  "workspace_id": "wY", "cwd": "/hermes" }
            ] }
        });
        assert_eq!(agent_cwd_in(&v, Some("wY:tZ"), Some("wY"), None), None);
    }

    #[test]
    fn agent_cwd_ignores_agentless_panes() {
        // Entries without an "agent" field (plain shells) never resolve.
        let v = json!({
            "result": { "agents": [
                { "pane_id": "wY:p1W", "tab_id": "wY:tV",
                  "workspace_id": "wY", "cwd": "/shell" }
            ] }
        });
        assert_eq!(agent_cwd_in(&v, Some("wY:tV"), Some("wY"), None), None);
    }

    #[test]
    fn missing_herdr_binary_yields_none() {
        // Spawning a nonexistent binary returns Err → None, never a panic.
        let out = Command::new("definitely-not-herdr-xyz").args(["list"]).output();
        assert!(out.is_err());
    }
}
