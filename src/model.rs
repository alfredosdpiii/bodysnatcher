use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, clap::ValueEnum)]
pub enum Harness {
    #[value(name = "factory")]
    Factory,
    #[value(name = "pi")]
    Pi,
    #[value(name = "omp")]
    Omp,
}

impl Harness {
    pub fn label(self) -> &'static str {
        match self {
            Harness::Factory => "FAC",
            Harness::Pi => "PI",
            Harness::Omp => "OMP",
        }
    }

    pub fn full(self) -> &'static str {
        match self {
            Harness::Factory => "factory",
            Harness::Pi => "pi",
            Harness::Omp => "omp",
        }
    }

    pub fn infer_from_path(path: &Path) -> Option<Harness> {
        let s = path.to_string_lossy().to_lowercase();
        if s.contains(".factory") {
            Some(Harness::Factory)
        } else if s.contains("/.pi/") {
            Some(Harness::Pi)
        } else if s.contains("/.omp/") {
            Some(Harness::Omp)
        } else {
            None
        }
    }
}

/// Root directory of each harness' session store.
#[derive(Clone, Debug)]
pub struct Store {
    pub factory: PathBuf,
    pub pi: PathBuf,
    pub omp: PathBuf,
}

impl Store {
    pub fn new() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        Self {
            factory: home.join(".factory/sessions"),
            pi: home.join(".pi/agent/sessions"),
            omp: home.join(".omp/agent/sessions"),
        }
    }

    pub fn root(&self, h: Harness) -> &PathBuf {
        match h {
            Harness::Factory => &self.factory,
            Harness::Pi => &self.pi,
            Harness::Omp => &self.omp,
        }
    }
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

/// Canonical session summary (cheap to build for discovery).
#[derive(Clone, Debug)]
pub struct Session {
    pub harness: Harness,
    pub path: PathBuf,
    pub id: String,
    pub title: String,
    pub cwd: String,
    pub model: String,
    pub msgs: usize,
    pub preview: String,
    pub modified: Option<SystemTime>,
}

/// Canonical message. Content is structured into typed blocks so tool calls,
/// tool results, and thinking survive a handoff as native blocks instead of
/// flattened prose.
#[derive(Clone, Debug, PartialEq)]
pub struct Msg {
    pub role: Role,
    pub blocks: Vec<Block>,
    pub ts: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    Tool,
}

/// Canonical content block. Tool results are normalized to their own message
/// (`Role::Tool`), matching Pi/OMP's `toolResult` role; Factory instead stores
/// them as `tool_result` blocks inside a user message, so the adapters split
/// and merge them at the boundary.
#[derive(Clone, Debug, PartialEq)]
pub enum Block {
    Text(String),
    Thinking(String),
    ToolCall {
        id: String,
        name: String,
        args: Value,
    },
    ToolResult {
        call_id: String,
        name: Option<String>,
        content: String,
        is_error: bool,
    },
}

/// Flatten an Anthropic-style content (string or block array) into text.
/// Handles text, thinking, toolCall/tool_use, toolResult/tool_result, image.
pub fn render_content(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => {
            let mut out = String::new();
            for b in blocks {
                let Some(obj) = b.as_object() else { continue };
                let ty = obj.get("type").and_then(Value::as_str).unwrap_or("");
                match ty {
                    "text" => {
                        if !out.is_empty() && !out.ends_with('\n') {
                            out.push('\n');
                        }
                        push(&mut out, obj.get("text"));
                    }
                    "thinking" => {
                        if let Some(t) = obj.get("thinking").and_then(Value::as_str)
                            && !t.is_empty()
                        {
                            out.push_str("[thinking]\n");
                            out.push_str(t);
                            out.push_str("\n[/thinking]\n");
                        }
                    }
                    "toolCall" | "tool_use" => {
                        out.push_str("[tool ");
                        out.push_str(obj.get("name").and_then(Value::as_str).unwrap_or("call"));
                        out.push_str("] ");
                        if let Some(args) = obj.get("arguments").or_else(|| obj.get("input")) {
                            out.push_str(&args.to_string());
                        }
                        out.push('\n');
                    }
                    "toolResult" | "tool_result" => {
                        out.push_str("[tool result");
                        if let Some(id) = obj.get("toolCallId").or_else(|| obj.get("tool_use_id"))
                            && let Some(id) = id.as_str()
                        {
                            out.push(' ');
                            out.push_str(id);
                        }
                        out.push_str("]\n");
                        if let Some(c) = obj.get("content") {
                            if c.is_array() {
                                out.push_str(&render_content(c));
                            } else if let Some(txt) = c.as_str() {
                                out.push_str(txt);
                                out.push('\n');
                            }
                        }
                    }
                    "image" => out.push_str("[image]\n"),
                    _ => {}
                }
            }
            out
        }
        other => other.to_string(),
    }
}

fn push(out: &mut String, v: Option<&Value>) {
    if let Some(v) = v {
        if let Some(s) = v.as_str() {
            out.push_str(s);
        } else if v.is_array() {
            out.push_str(&render_content(v));
        }
    }
}

/// Session dir slug for a cwd, per harness convention.
/// factory: `/a/b` -> `-a-b` ; omp: strips `$HOME`, `~/Projects/x` -> `-Projects-x` ;
/// pi: `/a/b` -> `--a-b--`.
pub fn slug_for(h: Harness, cwd: &str) -> String {
    match h {
        Harness::Factory => cwd.replace('/', "-"),
        Harness::Omp => {
            // OMP names workspace dirs relative to $HOME when cwd lives under it.
            let home = std::env::var_os("HOME")
                .map(|h| h.to_string_lossy().into_owned())
                .unwrap_or_default();
            let rel = if home.is_empty() {
                cwd
            } else {
                cwd.strip_prefix(&home).unwrap_or(cwd)
            };
            format!("-{}", rel.trim_start_matches('/').replace('/', "-"))
        }
        Harness::Pi => format!("--{}--", cwd.trim_start_matches('/').replace('/', "-")),
    }
}

/// 32-hex random id (uuid-ish, good enough for session ids).
pub fn uuid() -> String {
    let mut buf = [0u8; 16];
    use std::io::Read;
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_err()
    {
        // ponytail: urandom is always there on unix; fallback only for exotic setups
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        buf[..8].copy_from_slice(&t.to_le_bytes());
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// Short relative age for display.
pub fn rel_age(m: SystemTime) -> String {
    let Ok(d) = SystemTime::now().duration_since(m) else {
        return "now".into();
    };
    let s = d.as_secs();
    if s < 60 {
        "just now".into()
    } else if s < 3600 {
        format!("{}m ago", s / 60)
    } else if s < 86400 {
        format!("{}h ago", s / 3600)
    } else if s < 86400 * 30 {
        format!("{}d ago", s / 86400)
    } else if s < 86400 * 365 {
        format!("{}w ago", s / 86400 / 7)
    } else {
        format!("{}y ago", s / 86400 / 365)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_tool_blocks() {
        let c = json!([
            {"type": "text", "text": "hello\n"},
            {"type": "thinking", "thinking": "hmm"},
            {"type": "tool_use", "name": "Read", "input": {"file_path": "x.rs"}},
            {"type": "tool_result", "tool_use_id": "t1", "content": "file contents"}
        ]);
        let out = render_content(&c);
        assert!(out.contains("hello"));
        assert!(out.contains("[thinking]\nhmm\n[/thinking]"));
        assert!(out.contains("[tool Read]"));
        assert!(out.contains("[tool result t1]"));
        assert!(out.contains("file contents"));
    }

    #[test]
    fn joins_blocks_without_double_newlines() {
        let c = json!([{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]);
        assert_eq!(render_content(&c), "a\nb");
        let d = json!([
            {"type": "tool_use", "name": "Read", "input": {"file_path": "x.rs"}},
            {"type": "text", "text": "done"}
        ]);
        assert_eq!(
            render_content(&d),
            "[tool Read] {\"file_path\":\"x.rs\"}\ndone"
        );
    }

    #[test]
    fn renders_string_content() {
        assert_eq!(render_content(&json!("plain")), "plain");
    }

    #[test]
    fn slugs_match_real_conventions() {
        assert_eq!(
            slug_for(Harness::Factory, "/home/bryan/.pi"),
            "-home-bryan-.pi"
        );
        assert_eq!(
            slug_for(Harness::Omp, "/attic/attic-inference-gateway"),
            "-attic-attic-inference-gateway"
        );
        assert_eq!(
            slug_for(Harness::Pi, "/home/bryan/attic/testpoc"),
            "--home-bryan-attic-testpoc--"
        );
    }
    #[test]
    fn omp_slug_strips_home() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/u".into());
        assert_eq!(
            slug_for(Harness::Omp, &format!("{home}/Projects/x")),
            "-Projects-x"
        );
        assert_eq!(
            slug_for(Harness::Omp, &format!("{home}/.factory")),
            "-.factory"
        );
    }

    #[test]
    fn uuid_is_32_hex() {
        let id = uuid();
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn rel_age_formats() {
        let now = SystemTime::now();
        assert_eq!(
            rel_age(now - std::time::Duration::from_secs(30)),
            "just now"
        );
        assert_eq!(rel_age(now - std::time::Duration::from_secs(60)), "1m ago");
        assert_eq!(rel_age(now - std::time::Duration::from_secs(90)), "1m ago");
        assert_eq!(
            rel_age(now - std::time::Duration::from_secs(3600)),
            "1h ago"
        );
        assert_eq!(
            rel_age(now - std::time::Duration::from_secs(7200)),
            "2h ago"
        );
        assert_eq!(
            rel_age(now - std::time::Duration::from_secs(86400)),
            "1d ago"
        );
        assert_eq!(
            rel_age(now - std::time::Duration::from_secs(172800)),
            "2d ago"
        );
        assert_eq!(
            rel_age(now - std::time::Duration::from_secs(86400 * 30)),
            "4w ago"
        );
        assert_eq!(
            rel_age(now - std::time::Duration::from_secs(86400 * 365)),
            "1y ago"
        );
        assert_eq!(rel_age(now + std::time::Duration::from_secs(10)), "now");
    }

    #[test]
    fn infers_harness_from_path() {
        assert_eq!(
            Harness::infer_from_path(Path::new("/home/b/.factory/sessions/x/y.jsonl")),
            Some(Harness::Factory)
        );
        assert_eq!(
            Harness::infer_from_path(Path::new("/home/b/.omp/agent/sessions/x/y.jsonl")),
            Some(Harness::Omp)
        );
        assert_eq!(
            Harness::infer_from_path(Path::new("/home/b/.pi/agent/sessions/x/y.jsonl")),
            Some(Harness::Pi)
        );
        assert_eq!(Harness::infer_from_path(Path::new("/tmp/x.jsonl")), None);
    }
    #[test]
    fn renders_image_block() {
        let out = render_content(&json!([{"type": "image", "source": "x.png"}]));
        assert!(out.contains("[image]"));
    }

    #[test]
    fn harness_labels() {
        assert_eq!(Harness::Factory.label(), "FAC");
        assert_eq!(Harness::Pi.label(), "PI");
        assert_eq!(Harness::Omp.label(), "OMP");
        assert_eq!(Harness::Factory.full(), "factory");
        assert_eq!(Harness::Pi.full(), "pi");
        assert_eq!(Harness::Omp.full(), "omp");
    }

    #[test]
    fn store_root_picks_harness_dir() {
        let store = Store {
            factory: PathBuf::from("/f"),
            pi: PathBuf::from("/p"),
            omp: PathBuf::from("/o"),
        };
        assert_eq!(store.root(Harness::Factory), &PathBuf::from("/f"));
        assert_eq!(store.root(Harness::Pi), &PathBuf::from("/p"));
        assert_eq!(store.root(Harness::Omp), &PathBuf::from("/o"));
    }
}
