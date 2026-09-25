//! Import-time compaction. A harness can only compact a session by feeding it
//! to its model, so a session larger than the model's context window can never
//! be compacted once it lands. When (and only when) an imported session does
//! not fit the target model, older history is folded into deterministic
//! checkpoint messages and the recent tail is kept verbatim, shrinking until
//! the whole transcript fits the landing budget (200k tokens by default).

use crate::model::{Block, Harness, Msg, Role};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Tokens set aside for the harness' own system prompt and tool schemas.
const RESERVE: usize = 32_000;
/// Default landing size for a compacted session.
const DEFAULT_BUDGET: usize = 200_000;
/// Rough source-token span covered by one checkpoint.
const CHUNK_TOKENS: usize = 150_000;
const MAX_CHECKPOINTS: usize = 12;

/// User-facing knobs (CLI flags). `None` means auto-detect / default.
#[derive(Clone, Copy, Debug, Default)]
pub struct Policy {
    pub window: Option<usize>,
    pub budget: Option<usize>,
    pub disabled: bool,
}

impl Policy {
    /// Resolved limits for a target harness, or `None` when compaction is off.
    pub fn limits(&self, target: Harness) -> Option<Limits> {
        if self.disabled {
            return None;
        }
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let window = self.window.unwrap_or_else(|| detect_window(&home, target));
        Some(Limits::new(window, self.budget))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Target model's context window, in tokens.
    pub window: usize,
    /// Size a compacted session must land at, in tokens.
    pub budget: usize,
}

impl Limits {
    /// Budget defaults to 200k, capped at 60% of the window so the harness
    /// keeps room to work and to run its own compaction later.
    pub fn new(window: usize, budget: Option<usize>) -> Self {
        let cap = window * 3 / 5;
        Self {
            window,
            budget: budget.unwrap_or(DEFAULT_BUDGET).min(cap).max(1),
        }
    }

    fn fits(&self, tokens: usize) -> bool {
        tokens + RESERVE <= self.window
    }

    /// Cheap pre-check from file size: serialized JSONL is always at least as
    /// large as the content it carries, so a small file never needs a parse.
    pub fn file_may_overflow(&self, path: &Path) -> bool {
        std::fs::metadata(path)
            .map(|m| !self.fits(m.len() as usize / 4))
            .unwrap_or(false)
    }
}

/// Approximate token count (~4 bytes per token, which overestimates for
/// non-ASCII text — the safe direction).
pub fn estimate(msgs: &[Msg]) -> usize {
    msgs.iter().map(msg_tokens).sum()
}

fn msg_tokens(m: &Msg) -> usize {
    4 + m
        .blocks
        .iter()
        .map(|b| match b {
            Block::Text(t) | Block::Thinking(t) => t.len(),
            Block::ToolCall { name, args, .. } => name.len() + args.to_string().len(),
            Block::ToolResult { content, .. } => content.len(),
        })
        .sum::<usize>()
        / 4
}

/// Compacted history, or `None` when the session already fits the target.
pub fn compact(msgs: &[Msg], lim: Limits) -> Option<Vec<Msg>> {
    let total = estimate(msgs);
    if lim.fits(total) {
        return None;
    }
    let ctx = Ctx { total, lim };
    for keep_pct in [50, 30, 15, 5] {
        let cut = tail_start(msgs, lim.budget * keep_pct / 100);
        for detail in [Detail::Full, Detail::Brief, Detail::Minimal] {
            let out = build(msgs, cut, detail, &ctx);
            if estimate(&out) <= lim.budget {
                return Some(out);
            }
        }
    }
    // Last resort: the tail itself is huge (e.g. one giant tool result).
    // Clip long blocks in the kept tail; if that is not enough (e.g. a huge
    // tool call), fold more of the tail into checkpoints until it fits.
    let mut cut = tail_start(msgs, lim.budget / 20);
    loop {
        let mut out = build(msgs, cut, Detail::Minimal, &ctx);
        let head = out.len() - (msgs.len() - cut);
        let mut max = 64_000;
        while estimate(&out) > lim.budget && max > 256 {
            max /= 2;
            for m in &mut out[head..] {
                for b in &mut m.blocks {
                    clip_block(b, max);
                }
            }
        }
        if estimate(&out) <= lim.budget || cut >= msgs.len() {
            return Some(out);
        }
        cut = (cut + 1..msgs.len())
            .find(|&j| msgs[j].role != Role::Tool)
            .unwrap_or(msgs.len());
    }
}

/// One-line stderr note that a session was compacted, and by how much.
pub fn report(before: &[Msg], after: &[Msg], lim: Limits, target: Harness) {
    eprintln!(
        "bodysnatcher: ~{}k tokens exceeds {}'s {}k context window; compacted {} msgs -> {} (~{}k tokens) into a new session",
        estimate(before) / 1000,
        target.full(),
        lim.window / 1000,
        before.len(),
        after.len(),
        estimate(after) / 1000,
    );
}

struct Ctx {
    total: usize,
    lim: Limits,
}

#[derive(Clone, Copy)]
enum Detail {
    Full,
    Brief,
    Minimal,
}

impl Detail {
    /// (requests, request chars, notes, note chars, files, errors)
    fn caps(self) -> (usize, usize, usize, usize, usize, usize) {
        match self {
            Detail::Full => (30, 400, 20, 300, 40, 5),
            Detail::Brief => (12, 200, 8, 150, 20, 3),
            Detail::Minimal => (5, 120, 0, 0, 10, 0),
        }
    }
}

/// Index where the verbatim tail begins: newest messages totalling at most
/// `keep` tokens, moved forward past any tool results so none is orphaned
/// from its call. Always keeps at least the final turn.
fn tail_start(msgs: &[Msg], keep: usize) -> usize {
    let mut acc = 0;
    let mut i = msgs.len();
    while i > 0 && acc + msg_tokens(&msgs[i - 1]) <= keep {
        acc += msg_tokens(&msgs[i - 1]);
        i -= 1;
    }
    let boundary = |j: &usize| msgs[*j].role != Role::Tool;
    (i..msgs.len())
        .find(boundary)
        .or_else(|| (0..i).rev().find(boundary))
        .unwrap_or(msgs.len())
}

/// Checkpoint pairs for `msgs[..cut]`, followed by `msgs[cut..]` verbatim.
fn build(msgs: &[Msg], cut: usize, detail: Detail, ctx: &Ctx) -> Vec<Msg> {
    let chunks = chunk(&msgs[..cut]);
    let n = chunks.len();
    let mut out = Vec::with_capacity(n * 2 + msgs.len() - cut);
    for (k, &(a, b)) in chunks.iter().enumerate() {
        let mut text = String::new();
        if k == 0 {
            text.push_str(&format!(
                "[bodysnatcher] This session was compacted on import: the original \
                 history (~{}k tokens, {} messages) exceeded the target model's \
                 {}k-token context window. The oldest {} messages are summarized in \
                 {} checkpoint(s); the most recent {} messages follow verbatim.\n\n",
                ctx.total / 1000,
                msgs.len(),
                ctx.lim.window / 1000,
                cut,
                n,
                msgs.len() - cut,
            ));
        }
        text.push_str(&format!(
            "[bodysnatcher checkpoint {}/{} · messages {}–{} of {}]\n",
            k + 1,
            n,
            a + 1,
            b,
            msgs.len()
        ));
        text.push_str(&digest(&msgs[a..b], detail));
        let ts = msgs[b - 1].ts.clone();
        out.push(Msg {
            role: Role::User,
            blocks: vec![Block::Text(text)],
            ts: ts.clone(),
        });
        out.push(Msg {
            role: Role::Assistant,
            blocks: vec![Block::Text(format!("Checkpoint {}/{} noted.", k + 1, n))],
            ts,
        });
    }
    // keep roles alternating when the tail opens with an assistant turn
    if msgs.get(cut).is_some_and(|m| m.role != Role::User) {
        out.pop();
    }
    out.extend_from_slice(&msgs[cut..]);
    out
}

/// Split the compacted head into up to MAX_CHECKPOINTS spans, never breaking
/// between a tool call and its result.
fn chunk(head: &[Msg]) -> Vec<(usize, usize)> {
    if head.is_empty() {
        return Vec::new();
    }
    let tokens = estimate(head);
    let n = tokens.div_ceil(CHUNK_TOKENS).clamp(1, MAX_CHECKPOINTS);
    let target = tokens.div_ceil(n);
    let mut out = Vec::new();
    let (mut start, mut acc) = (0, 0);
    for (i, m) in head.iter().enumerate() {
        if acc >= target && m.role != Role::Tool && out.len() + 1 < n {
            out.push((start, i));
            start = i;
            acc = 0;
        }
        acc += msg_tokens(m);
    }
    out.push((start, head.len()));
    out
}

/// Deterministic summary of one span: what the user asked, where the
/// assistant landed, which tools ran, which files were touched, what failed.
fn digest(span: &[Msg], detail: Detail) -> String {
    let (n_req, req_len, n_notes, note_len, n_files, n_err) = detail.caps();
    let mut requests = Vec::new();
    let mut notes = Vec::new();
    let mut last_note: Option<&str> = None;
    let mut tools: BTreeMap<&str, usize> = BTreeMap::new();
    let mut files: Vec<&str> = Vec::new();
    let mut errors = Vec::new();
    for m in span {
        for b in &m.blocks {
            match (m.role, b) {
                (Role::User, Block::Text(t)) if !t.trim().is_empty() => {
                    if let Some(n) = last_note.take() {
                        notes.push(n);
                    }
                    requests.push(t.as_str());
                }
                (Role::Assistant, Block::Text(t)) if !t.trim().is_empty() => {
                    last_note = Some(t);
                }
                (_, Block::ToolCall { name, args, .. }) => {
                    *tools.entry(name.as_str()).or_default() += 1;
                    for key in ["file_path", "path", "filePath", "notebook_path"] {
                        if let Some(p) = args.get(key).and_then(Value::as_str) {
                            files.retain(|f| *f != p);
                            files.push(p);
                        }
                    }
                }
                (
                    _,
                    Block::ToolResult {
                        content,
                        is_error: true,
                        ..
                    },
                ) => errors.push(content),
                _ => {}
            }
        }
    }
    notes.extend(last_note);

    let mut out = String::new();
    section(&mut out, "User requests", &requests, n_req, req_len);
    section(&mut out, "Assistant outcomes", &notes, n_notes, note_len);
    if !tools.is_empty() {
        let list: Vec<String> = tools.iter().map(|(n, c)| format!("{n}×{c}")).collect();
        out.push_str(&format!("Tools used: {}\n", list.join(", ")));
    }
    if n_files > 0 && !files.is_empty() {
        let skip = files.len().saturating_sub(n_files);
        out.push_str(&format!("Files touched: {}\n", files[skip..].join(", ")));
    }
    let errors: Vec<&str> = errors.iter().map(|e| e.as_str()).collect();
    section(&mut out, "Errors", &errors, n_err, 200);
    out
}

/// Bullet list of the most recent `max` items, each clipped to `len` chars.
fn section(out: &mut String, title: &str, items: &[&str], max: usize, len: usize) {
    if max == 0 || items.is_empty() {
        return;
    }
    let skip = items.len().saturating_sub(max);
    out.push_str(title);
    if skip > 0 {
        out.push_str(&format!(" (last {max} of {})", items.len()));
    }
    out.push_str(":\n");
    for it in &items[skip..] {
        out.push_str("- ");
        out.push_str(&clip(
            &it.split_whitespace().collect::<Vec<_>>().join(" "),
            len,
        ));
        out.push('\n');
    }
}

fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Keep head and tail of an oversized block, noting how much was dropped.
fn clip_block(b: &mut Block, max: usize) {
    let s = match b {
        Block::Text(t) | Block::Thinking(t) => t,
        Block::ToolResult { content, .. } => content,
        Block::ToolCall { .. } => return,
    };
    if s.len() <= max {
        return;
    }
    let half = max / 2;
    let mut a = half;
    while !s.is_char_boundary(a) {
        a -= 1;
    }
    let mut z = s.len() - half;
    while !s.is_char_boundary(z) {
        z += 1;
    }
    *s = format!(
        "{}\n[… {} bytes elided by bodysnatcher …]\n{}",
        &s[..a],
        z - a,
        &s[z..]
    );
}

/// Best-effort context window of the model each harness is configured to
/// use. Falls back to conservative per-harness defaults.
fn detect_window(home: &Path, h: Harness) -> usize {
    let read_json = |p: &str| {
        std::fs::read_to_string(home.join(p))
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
    };
    let found = match h {
        Harness::Claude => {
            let model = std::env::var("ANTHROPIC_MODEL").ok().or_else(|| {
                read_json(".claude/settings.json")?
                    .get("model")?
                    .as_str()
                    .map(String::from)
            });
            model
                .map(|m| {
                    if m.to_lowercase().contains("[1m]") {
                        1_000_000
                    } else {
                        200_000
                    }
                })
                .or_else(|| claude_used_1m(&home.join(".claude/projects")).then_some(1_000_000))
        }
        Harness::Codex => {
            let cfg = std::fs::read_to_string(home.join(".codex/config.toml")).unwrap_or_default();
            toml_value(&cfg, "model_context_window")
                .and_then(|v| v.parse().ok())
                .or_else(|| {
                    let model = toml_value(&cfg, "model")?;
                    find_window(&read_json(".codex/models_cache.json")?, &model)
                })
        }
        Harness::Pi => read_json(".pi/agent/settings.json").and_then(|s| {
            let model = s.get("defaultModel")?.as_str()?;
            find_window(&read_json(".pi/agent/models.json")?, model)
        }),
        Harness::Factory => read_json(".factory/settings.json").and_then(|s| {
            let model = s.pointer("/sessionDefaultSettings/model")?.as_str()?;
            find_window(&s, model)
        }),
        // ponytail: OMP's model roles live in YAML; add detection if a real need shows up
        Harness::Omp => None,
    };
    found.unwrap_or(match h {
        Harness::Codex => 272_000,
        _ => 200_000,
    })
}

/// Whether any recent Claude Code transcript has a turn whose prompt went
/// past 200k tokens, which only a 1M-context model accepts. Claude Code does
/// not record the context size itself, so usage is the only evidence.
fn claude_used_1m(projects: &Path) -> bool {
    let mut files = Vec::new();
    for ws in std::fs::read_dir(projects).into_iter().flatten().flatten() {
        for f in std::fs::read_dir(ws.path()).into_iter().flatten().flatten() {
            let p = f.path();
            if p.extension().is_some_and(|e| e == "jsonl")
                && let Ok(m) = f.metadata().and_then(|m| m.modified())
            {
                files.push((m, p));
            }
        }
    }
    files.sort_by(|a, b| b.0.cmp(&a.0));
    files.iter().take(5).any(|(_, p)| {
        let Ok(text) = std::fs::read_to_string(p) else {
            return false;
        };
        text.lines().filter(|l| l.contains("\"usage\"")).any(|l| {
            let Ok(v) = serde_json::from_str::<Value>(l) else {
                return false;
            };
            let u = &v["message"]["usage"];
            let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
            n("input_tokens") + n("cache_read_input_tokens") + n("cache_creation_input_tokens")
                > 200_000
        })
    })
}

/// Top-level `key = value` from a TOML file (before any `[table]`).
fn toml_value(cfg: &str, key: &str) -> Option<String> {
    cfg.lines()
        .take_while(|l| !l.trim_start().starts_with('['))
        .find_map(|l| {
            let (k, v) = l.split_once('=')?;
            (k.trim() == key).then(|| v.trim().trim_matches('"').to_string())
        })
}

/// Search a model catalog for an entry whose id/slug/model equals `model`
/// and return its context window field.
fn find_window(v: &Value, model: &str) -> Option<usize> {
    match v {
        Value::Object(o) => {
            let named = ["id", "slug", "model"]
                .iter()
                .any(|k| o.get(*k).and_then(Value::as_str) == Some(model));
            if named
                && let Some(w) = ["contextWindow", "context_window", "maxContextLimit"]
                    .iter()
                    .find_map(|k| o.get(*k).and_then(Value::as_u64))
            {
                return Some(w as usize);
            }
            o.values().find_map(|c| find_window(c, model))
        }
        Value::Array(a) => a.iter().find_map(|c| find_window(c, model)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text(role: Role, s: &str) -> Msg {
        Msg {
            role,
            blocks: vec![Block::Text(s.to_string())],
            ts: None,
        }
    }

    /// `turns` user/assistant/tool cycles, each carrying ~`size` bytes of tool output.
    fn session(turns: usize, size: usize) -> Vec<Msg> {
        let mut msgs = Vec::new();
        for i in 0..turns {
            msgs.push(text(Role::User, &format!("request {i}")));
            msgs.push(Msg {
                role: Role::Assistant,
                blocks: vec![Block::ToolCall {
                    id: format!("c{i}"),
                    name: "Read".into(),
                    args: json!({"file_path": format!("src/f{i}.rs")}),
                }],
                ts: None,
            });
            msgs.push(Msg {
                role: Role::Tool,
                blocks: vec![Block::ToolResult {
                    call_id: format!("c{i}"),
                    name: None,
                    content: "x".repeat(size),
                    is_error: i % 7 == 0,
                }],
                ts: None,
            });
            msgs.push(text(Role::Assistant, &format!("done {i}")));
        }
        msgs
    }

    #[test]
    fn fitting_session_is_untouched() {
        let msgs = session(10, 1000);
        assert_eq!(compact(&msgs, Limits::new(1_000_000, None)), None);
    }

    #[test]
    fn oversized_session_lands_within_budget() {
        // ~3M tokens into a 1M window: land at 200k
        let msgs = session(3000, 4000);
        assert!(estimate(&msgs) > 2_900_000);
        let lim = Limits::new(1_000_000, None);
        assert_eq!(lim.budget, 200_000);
        let out = compact(&msgs, lim).unwrap();
        assert!(estimate(&out) <= 200_000, "{}", estimate(&out));
        // newest turn kept verbatim
        assert_eq!(out.last(), msgs.last());
        // checkpoints lead, alternating user/assistant
        let Block::Text(first) = &out[0].blocks[0] else {
            panic!()
        };
        assert!(first.contains("compacted on import"));
        assert!(first.contains("checkpoint 1/"));
        assert_eq!(out[1].role, Role::Assistant);
    }

    #[test]
    fn tail_never_starts_with_orphan_tool_result() {
        let msgs = session(400, 4000);
        let out = compact(&msgs, Limits::new(200_000, None)).unwrap();
        let tail_start = out
            .iter()
            .position(|m| {
                !matches!(&m.blocks[0], Block::Text(t) if t.contains("checkpoint") || t.starts_with("Checkpoint"))
            })
            .unwrap();
        assert_ne!(out[tail_start].role, Role::Tool);
        assert!(estimate(&out) <= Limits::new(200_000, None).budget);
    }

    #[test]
    fn single_prompt_agent_run_still_checkpoints() {
        let mut msgs = session(300, 4000);
        msgs.retain(|m| m.role != Role::User);
        msgs.insert(0, text(Role::User, "do the whole thing"));
        let lim = Limits::new(200_000, None);
        let out = compact(&msgs, lim).unwrap();
        assert!(estimate(&out) <= lim.budget);
        let Block::Text(first) = &out[0].blocks[0] else {
            panic!()
        };
        assert!(first.contains("checkpoint 1/"));
        assert!(first.contains("do the whole thing"));
        assert_eq!(out.last(), msgs.last());
    }

    #[test]
    fn giant_final_tool_result_is_clipped_to_fit() {
        let mut msgs = session(2, 10);
        msgs.push(text(Role::User, "read the huge file"));
        msgs.push(Msg {
            role: Role::Tool,
            blocks: vec![Block::ToolResult {
                call_id: "c".into(),
                name: None,
                content: "y".repeat(5_000_000),
                is_error: false,
            }],
            ts: None,
        });
        let lim = Limits::new(272_000, None);
        let out = compact(&msgs, lim).unwrap();
        assert!(estimate(&out) <= lim.budget);
        let Block::ToolResult { content, .. } = &out.last().unwrap().blocks[0] else {
            panic!()
        };
        assert!(content.contains("elided by bodysnatcher"));
    }

    #[test]
    fn huge_tool_call_in_tail_still_fits() {
        let mut msgs = session(3, 10);
        msgs.push(text(Role::User, "write it"));
        msgs.push(Msg {
            role: Role::Assistant,
            blocks: vec![Block::ToolCall {
                id: "w".into(),
                name: "Write".into(),
                args: json!({"file_path": "big.txt", "content": "z".repeat(2_000_000)}),
            }],
            ts: None,
        });
        let lim = Limits::new(200_000, None);
        let out = compact(&msgs, lim).unwrap();
        assert!(estimate(&out) <= lim.budget, "{}", estimate(&out));
    }

    #[test]
    fn detects_1m_claude_from_transcript_usage() {
        let home = std::env::temp_dir().join(format!("bs-cl-{}", crate::model::uuid()));
        let ws = home.join(".claude/projects/-w");
        std::fs::create_dir_all(&ws).unwrap();
        let turn = |n: u64| {
            format!(
                "{{\"message\":{{\"role\":\"assistant\",\"usage\":{{\"input_tokens\":10,\"cache_read_input_tokens\":{n}}}}}}}\n"
            )
        };
        std::fs::write(ws.join("a.jsonl"), turn(150_000)).unwrap();
        assert!(!claude_used_1m(&home.join(".claude/projects")));
        std::fs::write(ws.join("b.jsonl"), turn(640_000)).unwrap();
        assert!(claude_used_1m(&home.join(".claude/projects")));
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn digest_lists_requests_tools_files_errors() {
        let d = digest(&session(3, 10), Detail::Full);
        assert!(d.contains("- request 2"));
        assert!(d.contains("- done 1"));
        assert!(d.contains("Read×3"));
        assert!(d.contains("src/f2.rs"));
        assert!(d.contains("Errors:"));
    }

    #[test]
    fn budget_caps_at_share_of_window() {
        assert_eq!(Limits::new(1_000_000, None).budget, 200_000);
        assert_eq!(Limits::new(200_000, None).budget, 120_000);
        assert_eq!(Limits::new(1_000_000, Some(300_000)).budget, 300_000);
    }

    #[test]
    fn disabled_policy_yields_no_limits() {
        let p = Policy {
            disabled: true,
            ..Default::default()
        };
        assert_eq!(p.limits(Harness::Claude), None);
        let p = Policy {
            window: Some(500_000),
            ..Default::default()
        };
        assert_eq!(p.limits(Harness::Pi).unwrap().window, 500_000);
    }

    #[test]
    fn detects_windows_from_harness_configs() {
        let home = std::env::temp_dir().join(format!("bs-cw-{}", crate::model::uuid()));
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::create_dir_all(home.join(".pi/agent")).unwrap();
        std::fs::create_dir_all(home.join(".factory")).unwrap();
        std::fs::write(
            home.join(".codex/config.toml"),
            "model = \"gpt-x\"\n[projects.\"/a\"]\nmodel = \"no\"\n",
        )
        .unwrap();
        std::fs::write(
            home.join(".codex/models_cache.json"),
            r#"{"models":[{"slug":"gpt-x","context_window":400000}]}"#,
        )
        .unwrap();
        std::fs::write(
            home.join(".pi/agent/settings.json"),
            r#"{"defaultModel":"k"}"#,
        )
        .unwrap();
        std::fs::write(
            home.join(".pi/agent/models.json"),
            r#"{"providers":{"p":{"models":[{"id":"k","contextWindow":1048576}]}}}"#,
        )
        .unwrap();
        std::fs::write(
            home.join(".factory/settings.json"),
            r#"{"sessionDefaultSettings":{"model":"custom:m-0"},"customModels":[{"id":"custom:m-0","maxContextLimit":272000}]}"#,
        )
        .unwrap();
        assert_eq!(detect_window(&home, Harness::Codex), 400_000);
        assert_eq!(detect_window(&home, Harness::Pi), 1_048_576);
        assert_eq!(detect_window(&home, Harness::Factory), 272_000);
        assert_eq!(detect_window(&home, Harness::Omp), 200_000);
        std::fs::write(
            home.join(".codex/config.toml"),
            "model_context_window = 123456\n",
        )
        .unwrap();
        assert_eq!(detect_window(&home, Harness::Codex), 123_456);
        std::fs::remove_dir_all(home).ok();
    }
}
