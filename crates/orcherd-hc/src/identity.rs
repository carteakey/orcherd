//! Which agent runs in a herdr-core terminal: first the name the program
//! reports for itself (OSC 7501's `app`), then the process in front, looking
//! past a runtime such as `node` to the script it runs.

use crate::wire::{Foreground, StatusState, Terminal, TerminalState};

/// Agents `orcherd` manages on `herdr-core` seats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentKind {
    Claude,
    Zcode,
    Codex,
    Opencode,
    Antigravity,
    Other(String),
}

/// Runtimes that run an agent's script rather than being one.
const RUNTIMES: &[&str] = &["bun", "deno", "node", "nodejs"];
/// Runtime options whose value is the next argument, not the script.
const VALUED: &[&str] = &[
    "-r",
    "--require",
    "--import",
    "--loader",
    "--experimental-loader",
    "--env-file",
    "--title",
    "--inspect-port",
    "--conditions",
    "-C",
    "--preload",
    "--config",
];
/// Options after which the runtime runs code from the command line, so no
/// script follows.
const INLINE: &[&str] = &["-e", "--eval", "-p", "--print"];

/// The longest agent name a program may report for itself, as shown.
const MAX_NAME: usize = 32;

impl AgentKind {
    /// The canonical agent name used in seat records and CLI flags.
    pub fn name(&self) -> &str {
        match self {
            Self::Claude => "claude",
            Self::Zcode => "zcode",
            Self::Codex => "codex",
            Self::Opencode => "opencode",
            Self::Antigravity => "antigravity",
            Self::Other(name) => name.as_str(),
        }
    }

    /// The executable binary for this agent.
    pub fn binary(&self) -> &str {
        match self {
            Self::Antigravity => "agy",
            other => other.name(),
        }
    }

    /// Whether this kind matches `expected` (e.g. both `"agy"` and
    /// `"antigravity"` match [`Self::Antigravity`]).
    pub fn matches(&self, expected: &str) -> bool {
        if self.name().eq_ignore_ascii_case(expected.trim()) {
            return true;
        }
        Self::lookup(expected).as_ref() == Some(self)
    }

    /// The agent behind `name`: a reported app such as `claude-code`, or a
    /// program such as `/usr/local/bin/zcode` or `codex.exe`.
    pub fn lookup(name: &str) -> Option<Self> {
        Some(match base(name).to_ascii_lowercase().as_str() {
            "claude" | "claude-code" => Self::Claude,
            "zcode" | "z-code" => Self::Zcode,
            "codex" => Self::Codex,
            "opencode" | "opencode2" | "open-code" => Self::Opencode,
            "agy" | "antigravity" | "antigravity-cli" | "antigravity_cli" => Self::Antigravity,
            "pi" => Self::Other("pi".to_owned()),
            "omp" => Self::Other("omp".to_owned()),
            "copilot" | "github-copilot" | "ghcs" => Self::Other("copilot".to_owned()),
            "devin" | "devin-cli" => Self::Other("devin".to_owned()),
            "droid" => Self::Other("droid".to_owned()),
            "kimi" | "kimi-code" => Self::Other("kimi".to_owned()),
            "mastracode" | "mastra-code" => Self::Other("mastracode".to_owned()),
            "hermes" | "hermes-agent" => Self::Other("hermes".to_owned()),
            "qodercli" | "qoderclicn" | "qoder" | "qodercn" => Self::Other("qodercli".to_owned()),
            "qwen" | "qwen-code" => Self::Other("qwen".to_owned()),
            "kilo" | "kilo-code" => Self::Other("kilo".to_owned()),
            "cursor" | "cursor-agent" => Self::Other("cursor".to_owned()),
            "grok" | "grok-build" => Self::Other("grok".to_owned()),
            "letta" | "letta-code" => Self::Other("letta".to_owned()),
            "aider" => Self::Other("aider".to_owned()),
            "gemini" | "gemini-cli" => Self::Other("gemini".to_owned()),
            _ => return None,
        })
    }

    /// Identifies an agent from a reported app name, process name, and argv.
    pub fn identify(reporter: Option<&str>, name: &str, argv: &[String]) -> Option<Self> {
        if let Some(kind) = reporter.and_then(Self::lookup) {
            return Some(kind);
        }
        identify_argv(name, argv)
    }

    /// Identifies an agent from a process name and argv, looking past node/bun/deno.
    pub fn identify_argv(program: &str, argv: &[String]) -> Option<Self> {
        identify_argv(program, argv)
    }
}

/// Identifies an agent from a process name and its argument vector, looking
/// past a runtime such as `node`, `bun`, or `deno` to the script it runs.
/// Shells and unrecognized programs return `None`.
pub fn identify_argv(program: &str, argv: &[String]) -> Option<AgentKind> {
    let first = argv.first().map_or(program, String::as_str);
    if let Some(kind) = AgentKind::lookup(first).or_else(|| AgentKind::lookup(program)) {
        return Some(kind);
    }
    let runtime = base(first).to_ascii_lowercase();
    if !RUNTIMES.contains(&runtime.as_str()) {
        return None;
    }
    // `bun run x.js` and `deno run x.js`; to Node, `run` is a script.
    let subcommand = !matches!(runtime.as_str(), "node" | "nodejs");
    let script = script(argv, subcommand)?;
    package(script).or_else(|| AgentKind::lookup(script))
}

/// The agent `record`'s terminal runs, by its report, else its foreground
/// process; `None` for a shell or any other program, and `None` once the
/// program is gone.
pub fn agent_kind(record: &Terminal) -> Option<AgentKind> {
    if !is_running(record) {
        return None;
    }
    let (name, argv) = foreground_process(record).map_or_else(
        || {
            let program = record.argv.first().map_or("", String::as_str);
            (program, record.argv.as_slice())
        },
        |process| (process.name.as_str(), process.argv.as_slice()),
    );
    AgentKind::identify(effective_reporter(record), name, argv)
}

/// What to call the agent in `record`'s terminal: the canonical name of the
/// identified [`AgentKind`], else, for an unknown program that reports a
/// status of its own, the sanitized name it reports. `None` for a shell or
/// any program that reports nothing, or once the program is gone.
pub fn agent_name(record: &Terminal) -> Option<String> {
    if !is_running(record) {
        return None;
    }
    if let Some(kind) = agent_kind(record) {
        return Some(kind.name().to_owned());
    }
    let reported = effective_reporter(record)?;
    let name = safe_text(reported, MAX_NAME);
    let name = name.trim();
    let known_state = record
        .status
        .as_ref()
        .is_some_and(|status| status.state != StatusState::Other);
    (!name.is_empty() && known_state).then(|| name.to_owned())
}

/// Whether `record`'s terminal still runs `expected_agent`. An agent's OSC 7501
/// report outlives it, so the terminal's own shell back in front means it left,
/// unless the agent is the program the terminal started (or `exec`ed into).
pub fn agent_in_front(record: &Terminal, expected_agent: &str) -> bool {
    if !is_running(record) {
        return false;
    }
    let matches_kind = agent_kind(record).is_some_and(|kind| kind.matches(expected_agent));
    let matches_name = agent_name(record)
        .as_deref()
        .is_some_and(|name| name.eq_ignore_ascii_case(expected_agent.trim()));
    if !matches_kind && !matches_name {
        return false;
    }
    let Some(front) = record
        .foreground
        .as_ref()
        .filter(|front| front.pid > 0 && record.pid == Some(front.pid))
    else {
        return true;
    };
    // The terminal's own program is in front: the agent is still there if
    // that program is this very agent, started as it or `exec`ed by its
    // shell. Another agent `exec`ed there may not have reported yet.
    let program = record.argv.first().map_or("", String::as_str);
    let is = |kind: Option<AgentKind>| kind.is_some_and(|kind| kind.matches(expected_agent));
    is(identify_argv(&front.name, &front.argv)) || is(identify_argv(program, &record.argv))
}

fn is_running(terminal: &Terminal) -> bool {
    match terminal.state {
        TerminalState::Finished => false,
        // A kept terminal stays live after its program is lost.
        TerminalState::Live if terminal.pid.is_none() && terminal.reason.is_some() => false,
        TerminalState::Live | TerminalState::Other => true,
    }
}

fn foreground_process(terminal: &Terminal) -> Option<&Foreground> {
    terminal.foreground.as_ref().filter(|front| front.pid > 0)
}

fn effective_reporter(terminal: &Terminal) -> Option<&str> {
    terminal
        .status
        .as_ref()
        .filter(|_| !agent_left(terminal))
        .map(|status| status.app.trim())
        .filter(|app| !app.is_empty())
}

/// Whether the agent whose report `terminal` holds has quit to its shell.
fn agent_left(terminal: &Terminal) -> bool {
    let Some(status) = &terminal.status else {
        return false;
    };
    let Some(reporter) = AgentKind::identify(Some(status.app.trim()), "", &[]) else {
        return false;
    };
    let Some(front) = terminal
        .foreground
        .as_ref()
        .filter(|front| front.pid > 0 && terminal.pid == Some(front.pid))
    else {
        return false;
    };
    let program = terminal.argv.first().map_or("", String::as_str);
    let is = |kind: Option<AgentKind>| kind.as_ref() == Some(&reporter);
    !is(identify_argv(&front.name, &front.argv)) && !is(identify_argv(program, &terminal.argv))
}

/// The script a runtime's command line runs: the first argument that is
/// neither an option nor an option's value, past a leading `run` when the
/// runtime takes `subcommand`s.
fn script(argv: &[String], subcommand: bool) -> Option<&str> {
    let mut args = argv.iter().skip(1);
    let mut first = true;
    while let Some(arg) = args.next() {
        if !arg.starts_with('-') {
            if subcommand && first && arg == "run" {
                first = false;
                continue;
            }
            return Some(arg);
        }
        if INLINE.contains(&arg.as_str()) {
            return None;
        }
        if VALUED.contains(&arg.as_str()) {
            args.next();
        }
    }
    None
}

/// The agent whose package folder holds `script`.
fn package(script: &str) -> Option<AgentKind> {
    let script = script.replace('\\', "/");
    let packages: &[(&str, AgentKind)] = &[
        ("@anthropic-ai/claude-code", AgentKind::Claude),
        ("@github/copilot", AgentKind::Other("copilot".to_owned())),
        ("@letta-ai/letta-code", AgentKind::Other("letta".to_owned())),
        ("@openai/codex", AgentKind::Codex),
        ("@qwen-code/qwen-code", AgentKind::Other("qwen".to_owned())),
        ("mastracode", AgentKind::Other("mastracode".to_owned())),
        ("opencode-ai", AgentKind::Opencode),
        ("zcode", AgentKind::Zcode),
    ];
    packages
        .iter()
        .find(|(folder, _)| script.contains(&format!("/node_modules/{folder}/")))
        .map(|(_, kind)| kind.clone())
}

/// A program's name without its folder, a login shell's leading dash, or a
/// Windows or script extension.
fn base(name: &str) -> &str {
    let name = name.trim();
    let name = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let name = name.trim_start_matches('-');
    [".exe", ".cmd", ".js", ".mjs", ".cjs"]
        .iter()
        .find_map(|extension| {
            let cut = name.len().checked_sub(extension.len())?;
            name.get(cut..)
                .filter(|tail| tail.eq_ignore_ascii_case(extension))
                .and_then(|_| name.get(..cut))
        })
        .unwrap_or(name)
}

/// A control or direction-override character, which untrusted text loses.
fn unsafe_char(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
                | '\u{200e}'
                | '\u{200f}'
                | '\u{061c}'
                | '\u{2028}'
                | '\u{2029}'
        )
}

/// `text` cut to `limit` characters, without the characters `unsafe_char`
/// refuses.
fn safe_text(text: &str, limit: usize) -> String {
    text.chars()
        .take(limit)
        .filter(|c| !unsafe_char(*c))
        .collect()
}
