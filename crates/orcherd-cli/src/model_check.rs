//! Post-launch model verification for seats spawned with `--model`.
//!
//! Some harnesses silently fall back to their default model when `--model` fails to resolve
//! (for example due to a stale binary on `herdr-core`'s `PATH` or uncached auth). Reading a
//! bounded slice of the first screen right after `agent::ready` catches mismatches loudly
//! before the seat starts work.

use orcherd_core::{
    Actor, ModelCheck, ModelCheckStatus, Op, Receipt, ReceiptData, RequestEnvelope, Store,
};
use orcherd_hc::{AgentKind, Core, Error};
use std::{
    thread,
    time::{Duration, Instant},
};

/// Upper bound for waiting on the harness's first screen to render its model line.
pub(crate) const MODEL_CHECK_TIMEOUT: Duration = Duration::from_secs(20);
const MODEL_CHECK_POLL: Duration = Duration::from_millis(250);
const SCREEN_HISTORY_LINES: u32 = 40;

/// Model name extracted from a terminal's startup screen, alongside the observed line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ObservedModel {
    pub line: String,
    pub model: String,
}

/// Whether we know how to read a model line from this harness's startup screen.
pub(crate) fn harness_names_model(agent: &str) -> bool {
    matches!(AgentKind::lookup(agent), Some(AgentKind::Antigravity))
}

/// Reads a bounded slice of `terminal`'s screen via `herdr-core` (up to ~20 s) and compares
/// any recognised model line against `requested_model`.
pub(crate) fn verify_launched_model(
    core: &Core,
    terminal: &str,
    agent: &str,
    requested_model: &str,
) -> ModelCheck {
    verify_model_with_reader(
        agent,
        requested_model,
        MODEL_CHECK_TIMEOUT,
        MODEL_CHECK_POLL,
        || read_terminal_slice(core, terminal),
    )
}

/// Testable core of `verify_launched_model`: polls `read_screen` until a model line appears,
/// the reader signals stop (`None`), or `timeout` elapses.
pub(crate) fn verify_model_with_reader<F>(
    agent: &str,
    requested_model: &str,
    timeout: Duration,
    poll: Duration,
    mut read_screen: F,
) -> ModelCheck
where
    F: FnMut() -> Option<String>,
{
    let requested = requested_model.trim().to_owned();
    if !harness_names_model(agent) {
        return ModelCheck {
            requested,
            observed: None,
            status: ModelCheckStatus::Unverified,
        };
    }

    let deadline = Instant::now() + timeout;
    loop {
        let Some(screen) = read_screen() else {
            return ModelCheck {
                requested,
                observed: None,
                status: ModelCheckStatus::Unverified,
            };
        };
        if let Some(found) = extract_observed_model(agent, &screen) {
            let status = if model_matches(&requested, &found.model) {
                ModelCheckStatus::Verified
            } else {
                ModelCheckStatus::Mismatch
            };
            return ModelCheck {
                requested,
                observed: Some(found.line),
                status,
            };
        }
        let now = Instant::now();
        if now >= deadline {
            return ModelCheck {
                requested,
                observed: None,
                status: ModelCheckStatus::Unverified,
            };
        }
        thread::sleep(poll.min(deadline.saturating_duration_since(now)));
    }
}

/// Records `check` on a spawn `Receipt`. Returns `true` when the model mismatched and
/// `receipt.failed_stage` / `receipt.next` were set to fail the spawn loudly.
pub(crate) fn apply_model_check(
    receipt: &mut Receipt,
    terminal_address: &str,
    agent: &str,
    check: ModelCheck,
) -> bool {
    let mismatch = check.status == ModelCheckStatus::Mismatch;
    if mismatch {
        let observed = check.observed.as_deref().unwrap_or("(unknown)");
        receipt.failed_stage = Some("model_mismatch".to_owned());
        receipt.next = Some(format!(
            "requested model `{}` but terminal {terminal_address} showed `{observed}` (terminal stopped); run `orcherd preflight --agent {agent}` to check PATH drift before retrying",
            check.requested
        ));
    }
    if let ReceiptData::SpawnPrepared {
        ref mut model_check,
        ..
    } = receipt.data
    {
        *model_check = Some(check);
    }
    mismatch
}

/// Settles the dispatch created by a spawn that failed `model_mismatch` so the task
/// returns to `ready`, no orphaned active dispatch remains, and the stale unread
/// `dispatch` inbox item is cleared.
pub(crate) fn cleanup_model_mismatch(
    store: &mut Store,
    actor: &Actor,
    team_id: &str,
    dispatch_id: Option<&str>,
    receipt: &mut Receipt,
) {
    let Some(did) = dispatch_id else {
        return;
    };
    let Ok(abandoned) = store.apply(
        actor,
        RequestEnvelope::new(Op::SeatAbandon {
            dispatch_id: did.to_owned(),
        })
        .with_team(team_id),
    ) else {
        return;
    };
    if let ReceiptData::Dispatch(disp) = abandoned.data
        && let ReceiptData::SpawnPrepared {
            dispatch: ref mut r_disp,
            task: ref mut r_task,
            ..
        } = receipt.data
    {
        let task_id = disp.task_id.clone();
        *r_disp = Some(disp);
        if let Ok(t_receipt) = store.apply(
            actor,
            RequestEnvelope::new(Op::TaskShow { task_id }).with_team(team_id),
        ) && let ReceiptData::Task(updated_task) = t_receipt.data
        {
            *r_task = Some(updated_task);
        }
    }
}

/// Reads a bounded screen slice, returning `None` when the terminal is gone so polling stops.
fn read_terminal_slice(core: &Core, terminal: &str) -> Option<String> {
    match core.read_screen(terminal, SCREEN_HISTORY_LINES) {
        Ok(screen) => Some(screen.text),
        Err(Error::Refused {
            kind: Some(ref k), ..
        }) if k == "not_found" => None,
        Err(Error::Lost) => None,
        Err(_) => core
            .read_preview(terminal, SCREEN_HISTORY_LINES, Duration::from_secs(1))
            .ok(),
    }
}

/// Extracts a recognised model line from a harness's startup screen, or `None` when no
/// recognisable model line is present yet.
pub(crate) fn extract_observed_model(agent: &str, raw_screen: &str) -> Option<ObservedModel> {
    if !harness_names_model(agent) {
        return None;
    }
    let clean = strip_ansi(raw_screen);
    let lines: Vec<&str> = clean.lines().collect();
    extract_antigravity_model(&lines)
}

fn extract_antigravity_model(lines: &[&str]) -> Option<ObservedModel> {
    for (idx, line) in lines.iter().enumerate() {
        let Some(pos) = find_ascii_case_insensitive(line, "antigravity cli") else {
            continue;
        };
        let header = line[pos..].trim();
        // Compact header: `Antigravity CLI 1.5.17 · Gemini 4 Neon (High)`
        if let Some((left, right)) = header.split_once('·') {
            let model = right.trim();
            if looks_like_model_name(model) {
                return Some(ObservedModel {
                    line: format!("{} · {model}", left.trim()),
                    model: model.to_owned(),
                });
            }
        }
        // Multi-line ASCII logo banner:
        //   ▄▀▀▄      Antigravity CLI 1.5.18
        //  ▀▀▀▀▀▀     user@example.com (Plan)
        // ▀▀▀▀▀▀▀▀    Gemini 4 Neon (High)
        // ▄▀▀  ▀▀▄    ~/repo
        for candidate in lines.iter().skip(idx + 1).take(4) {
            let text = strip_banner_art(candidate);
            if text.is_empty()
                || text.contains('@')
                || text.starts_with("~/")
                || text.starts_with('/')
                || text.starts_with('>')
                || !looks_like_model_name(text)
            {
                continue;
            }
            return Some(ObservedModel {
                line: format!("{header} · {text}"),
                model: text.to_owned(),
            });
        }
    }

    // Fallback: agy footer status bar (`auto · Gemini 4 Neon · high`).
    for line in lines.iter().rev() {
        if let Some(bar) = extract_agy_status_bar(line.trim()) {
            return Some(bar);
        }
    }
    None
}

fn extract_agy_status_bar(line: &str) -> Option<ObservedModel> {
    // Take the rightmost segment separated by runs of 2+ spaces (the right-aligned status widget).
    let widget = line.rsplit("  ").find(|part| !part.trim().is_empty())?.trim();
    let parts: Vec<&str> = widget.split('·').map(str::trim).collect();
    if parts.len() != 3 {
        return None;
    }
    let mode = parts[0].to_ascii_lowercase();
    let effort = parts[2].to_ascii_lowercase();
    let known_mode = matches!(mode.as_str(), "auto" | "plan" | "review" | "ask" | "edit" | "default");
    let known_effort = matches!(effort.as_str(), "low" | "medium" | "high" | "xhigh" | "max" | "auto");
    if known_mode && known_effort && !parts[1].is_empty() {
        return Some(ObservedModel {
            line: widget.to_owned(),
            model: format!("{} {}", parts[1], parts[2]),
        });
    }
    None
}

/// Guards against misparsing transient startup or auth lines (such as `You are not logged
/// into Antigravity`, `Sign in required`, `Loading...`, or `Update available`) as a model name.
fn looks_like_model_name(text: &str) -> bool {
    let tokens = tokenize_model(text);
    if tokens.is_empty() {
        return false;
    }
    let has_family = tokens.iter().any(|tok| {
        matches!(
            tok.as_str(),
            "gemini"
                | "claude"
                | "opus"
                | "sonnet"
                | "haiku"
                | "gpt"
                | "qwen"
                | "deepseek"
                | "grok"
                | "llama"
                | "mistral"
                | "o1"
                | "o3"
                | "o4"
        )
    });
    let has_effort_tag = matches!(
        text.rsplit_once('('),
        Some((_, tail))
            if matches!(
                tail.trim_end_matches(')').to_ascii_lowercase().as_str(),
                "low" | "medium" | "high" | "xhigh" | "max"
            )
    );
    has_family || has_effort_tag
}

/// Conservatively checks whether a requested `--model` ID matches the screen's display name.
///
/// For example, `gemini-4-neon-high` matches `Gemini 4 Neon (High)` and `Gemini 4 Neon · high`,
/// while rejecting `Claude Opus 5.5`, `Gemini 4 Neon (Low)`, or `Gemini 4 Flash (High)`.
pub(crate) fn model_matches(requested: &str, observed: &str) -> bool {
    let mut req = tokenize_model(requested);
    let obs = tokenize_model(observed);
    if req.is_empty() || obs.is_empty() {
        return false;
    }

    // Drop a trailing YYYYMMDD snapshot suffix on the requested ID when absent from the UI name.
    if req.len() > 1
        && let Some(last) = req.last()
        && is_date_stamp(last)
        && !obs.iter().any(|tok| tok == last)
    {
        req.pop();
    }

    // If both name an explicit model family/vendor up front, they must agree.
    if let (Some(rf), Some(of)) = (family_token(req.first()), family_token(obs.first()))
        && rf != of
    {
        return false;
    }

    if is_ordered_subsequence(&req, &obs) {
        return true;
    }

    // Allow a leading vendor prefix (`claude-opus-4-6` -> `Opus 4.6`, `google-gemini-...`)
    // when the observed display name omits the vendor word.
    if req.len() > 1
        && matches!(
            req[0].as_str(),
            "claude" | "anthropic" | "google" | "openai"
        )
        && is_ordered_subsequence(&req[1..], &obs)
    {
        return true;
    }

    false
}

fn tokenize_model(raw: &str) -> Vec<String> {
    raw.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .collect()
}

fn is_date_stamp(token: &str) -> bool {
    token.len() == 8
        && token.starts_with("20")
        && token.bytes().all(|b| b.is_ascii_digit())
}

fn family_token(tok: Option<&String>) -> Option<&'static str> {
    match tok?.as_str() {
        "claude" | "opus" | "sonnet" | "haiku" => Some("claude"),
        "gemini" => Some("gemini"),
        "gpt" => Some("gpt"),
        "qwen" => Some("qwen"),
        "deepseek" => Some("deepseek"),
        "grok" => Some("grok"),
        "llama" => Some("llama"),
        _ => None,
    }
}

fn is_ordered_subsequence(needle: &[String], haystack: &[String]) -> bool {
    let mut hi = 0;
    for tok in needle {
        let Some(pos) = haystack[hi..].iter().position(|h| h == tok) else {
            return false;
        };
        hi += pos + 1;
    }
    true
}

fn strip_banner_art(line: &str) -> &str {
    line.trim_start_matches(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '▄' | '▀' | '█' | '▌' | '▐' | '▓' | '▒' | '░' | '│' | '┃' | '║' | '╭' | '╰' | '┌'
                    | '└'
            )
    })
    .trim_end_matches(|c: char| c.is_whitespace() || matches!(c, '│' | '┃' | '║'))
}

fn find_ascii_case_insensitive(haystack: &str, needle_lower: &str) -> Option<usize> {
    let h = haystack.as_bytes();
    let n = needle_lower.as_bytes();
    if n.is_empty() || h.len() < n.len() {
        return None;
    }
    for i in 0..=(h.len() - n.len()) {
        if h[i..i + n.len()].eq_ignore_ascii_case(n) {
            return Some(i);
        }
    }
    None
}

/// Strips ANSI CSI (`ESC [ ...`) and OSC (`ESC ] ... BEL/ST`) escape sequences.
fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.peek().copied() {
            Some('[') => {
                chars.next();
                for ch in chars.by_ref() {
                    if ('@'..='~').contains(&ch) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                while let Some(ch) = chars.next() {
                    if ch == '\x07' {
                        break;
                    }
                    if ch == '\x1b' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}
