use crate::model::{Block, Harness, Msg, Role, Session, Store, render_content, slug_for, uuid};
use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// Discover every session under a harness store.
pub fn discover(root: &Path, harness: Harness) -> Vec<Session> {
    if harness == Harness::Codex {
        return discover_codex(root, None);
    }
    let mut out = Vec::new();
    let Ok(workspaces) = fs::read_dir(root) else {
        return out;
    };
    for ws in workspaces.flatten() {
        if ws.path().is_dir() {
            out.extend(discover_workspace(&ws.path(), harness));
        }
    }
    out
}

/// Discover sessions for one cwd only: scan the single workspace dir whose
/// slug matches `cwd`, skip every other workspace. Keeps startup fast on
/// machines with thousands of sessions spread across many projects.
pub fn discover_for_cwd(root: &Path, harness: Harness, cwd: &Path) -> Vec<Session> {
    if harness == Harness::Codex {
        return discover_codex(root, Some(cwd));
    }
    let ws = root.join(slug_for(harness, &cwd.to_string_lossy()));
    if !ws.is_dir() {
        return Vec::new();
    }
    discover_workspace(&ws, harness)
}

fn discover_workspace(ws: &Path, harness: Harness) -> Vec<Session> {
    let mut out = Vec::new();
    let Ok(files) = fs::read_dir(ws) else {
        return out;
    };
    for f in files.flatten() {
        let p = f.path();
        if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        if let Some(s) = summarize(&p, harness) {
            out.push(s);
        }
    }
    out
}

fn discover_codex(root: &Path, cwd: Option<&Path>) -> Vec<Session> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl")
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("rollout-"))
            {
                out.push(path);
            }
        }
    }

    // ponytail: one summary scan per rollout; add an index only if this becomes slow.
    let mut paths = Vec::new();
    walk(root, &mut paths);
    paths.sort();
    paths
        .into_iter()
        .filter_map(|path| summarize(&path, Harness::Codex))
        .filter(|session| cwd.is_none_or(|cwd| session.cwd == cwd.to_string_lossy()))
        .collect()
}

/// Cheap summary: header line + first user text + message count + mtime.
pub fn summarize(path: &Path, harness: Harness) -> Option<Session> {
    let f = fs::File::open(path).ok()?;
    let modified = f.metadata().ok().and_then(|m| m.modified().ok());
    let mut reader = BufReader::new(f);

    let mut id = String::new();
    let mut title = String::new();
    let mut cwd = String::new();
    let mut model = String::new();
    let mut msgs = 0usize;
    let mut preview = String::new();

    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).ok()? == 0 {
            break;
        }
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let ty = v.get("type").and_then(Value::as_str).unwrap_or("");

        match harness {
            Harness::Factory => match ty {
                "session_start" => {
                    id = s(&v, "id");
                    title = s(&v, "title");
                    cwd = s(&v, "cwd");
                }
                "message" => {
                    msgs += 1;
                    let m = &v["message"];
                    let role = m.get("role").and_then(Value::as_str).unwrap_or("");
                    if role == "assistant" && model.is_empty() {
                        model = s(m, "model");
                    }
                    if role == "user" && preview.is_empty() {
                        if let Some(t) = user_text(m) {
                            preview = clip(&t);
                        }
                        if title.is_empty() || title == "New Session" {
                            title = preview.clone();
                        }
                    }
                }
                _ => {}
            },
            Harness::Claude => {
                if v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                let m = &v["message"];
                let role = m.get("role").and_then(Value::as_str).unwrap_or("");
                if cwd.is_empty() {
                    cwd = s(&v, "cwd");
                }
                if ty == "assistant" {
                    msgs += 1;
                    if model.is_empty() {
                        model = s(m, "model");
                    }
                } else if ty == "user" && role == "user" {
                    msgs += 1;
                    if preview.is_empty()
                        && let Some(t) = user_text(m)
                    {
                        preview = clip(&t);
                        title = preview.clone();
                    }
                }
            }
            Harness::Codex => {
                let payload = &v["payload"];
                match ty {
                    "session_meta" => {
                        if id.is_empty() {
                            id = s(payload, "id");
                            if id.is_empty() {
                                id = s(payload, "session_id");
                            }
                            cwd = s(payload, "cwd");
                        }
                    }
                    "response_item" if payload["type"] == "message" => {
                        msgs += 1;
                        if payload["role"] == "user" && preview.is_empty() {
                            let text = codex_text(&payload["content"], "input_text");
                            if !text.is_empty() {
                                preview = clip(&text);
                                title = preview.clone();
                            }
                        }
                    }
                    "turn_context" if model.is_empty() => model = s(payload, "model"),
                    _ => {}
                }
            }
            Harness::Pi | Harness::Omp => match ty {
                "title" => {
                    if title.is_empty() {
                        title = s(&v, "title");
                    }
                }
                "session" => {
                    id = s(&v, "id");
                    cwd = s(&v, "cwd");
                    if title.is_empty() {
                        title = s(&v, "title");
                    }
                }
                "model_change" => {
                    if model.is_empty() {
                        model = s(&v, "model");
                        if model.is_empty() {
                            model = s(&v, "modelId");
                        }
                    }
                }
                "message" => {
                    msgs += 1;
                    let m = &v["message"];
                    let role = m.get("role").and_then(Value::as_str).unwrap_or("");
                    if role == "assistant" && model.is_empty() {
                        model = s(m, "model");
                    }
                    if role == "user"
                        && preview.is_empty()
                        && let Some(t) = user_text(m)
                    {
                        preview = clip(&t);
                        if title.is_empty() {
                            title = preview.clone();
                        }
                    }
                }
                _ => {}
            },
        }
    }

    if id.is_empty() {
        id = path.file_stem()?.to_string_lossy().into_owned();
    }
    if title.is_empty() || title == "New Session" {
        title = path
            .file_stem()?
            .to_string_lossy()
            .split('_')
            .next_back()?
            .to_string();
    }
    if cwd.is_empty() {
        cwd = "?".into();
    }

    Some(Session {
        harness,
        path: path.to_path_buf(),
        id,
        title: clip(&title),
        cwd,
        model,
        msgs,
        preview,
        modified,
    })
}

fn s(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn user_text(m: &Value) -> Option<String> {
    let t = render_content(m.get("content")?);
    // skip system-reminder boilerplate and empty hook records
    let trimmed = t.trim();
    if trimmed.is_empty()
        || trimmed.starts_with("<system-reminder>")
        || trimmed.starts_with("<system-notice>")
    {
        None
    } else {
        Some(t)
    }
}

fn codex_text(content: &Value, text_type: &str) -> String {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|part| match part.get("type").and_then(Value::as_str) {
            Some(ty) if ty == text_type => part.get("text").and_then(Value::as_str),
            Some("input_image") => Some("[image]"),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn clip(s: &str) -> String {
    let s = s.replace('\n', " ");
    let s = s.trim();
    if s.chars().count() > 140 {
        let cut: String = s.chars().take(137).collect();
        format!("{cut}…")
    } else {
        s.to_string()
    }
}

/// Full parse of a session into canonical messages.
pub fn parse(path: &Path, harness: Harness) -> std::io::Result<(Session, Vec<Msg>)> {
    let f = fs::File::open(path)?;
    let mut sum = summarize(path, harness).unwrap_or(Session {
        harness,
        path: path.to_path_buf(),
        id: String::new(),
        title: String::new(),
        cwd: String::new(),
        model: String::new(),
        msgs: 0,
        preview: String::new(),
        modified: None,
    });
    let mut msgs = Vec::new();
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
        let is_conv = match harness {
            Harness::Claude => {
                v.get("message").and_then(|m| m.get("role")).is_some()
                    && v.get("isSidechain").and_then(Value::as_bool) != Some(true)
            }
            Harness::Codex => ty == "response_item",
            _ => ty == "message",
        };
        if !is_conv {
            continue;
        }
        let ts = v.get("timestamp").and_then(Value::as_str).map(String::from);
        if harness == Harness::Codex {
            parse_codex_item(&v["payload"], ts, &mut msgs);
            continue;
        }
        let m = &v["message"];
        let role = m.get("role").and_then(Value::as_str).unwrap_or("");
        match harness {
            Harness::Factory | Harness::Claude => match role {
                "assistant" => {
                    let blocks = assistant_blocks(&m["content"]);
                    if !blocks.is_empty() {
                        msgs.push(Msg {
                            role: Role::Assistant,
                            blocks,
                            ts,
                        });
                    }
                }
                "user" => {
                    let (user, tools) = split_factory_user(&m["content"], ts);
                    if let Some(u) = user {
                        msgs.push(u);
                    }
                    msgs.extend(tools);
                }
                _ => {}
            },
            Harness::Pi | Harness::Omp => match role {
                "assistant" => {
                    let blocks = assistant_blocks(&m["content"]);
                    if !blocks.is_empty() {
                        msgs.push(Msg {
                            role: Role::Assistant,
                            blocks,
                            ts,
                        });
                    }
                }
                "user" => {
                    if let Some(text) = user_text(m) {
                        msgs.push(Msg {
                            role: Role::User,
                            blocks: vec![Block::Text(text)],
                            ts,
                        });
                    }
                }
                "toolResult" => {
                    msgs.push(Msg {
                        role: Role::Tool,
                        blocks: vec![Block::ToolResult {
                            call_id: m
                                .get("toolCallId")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                            name: m.get("toolName").and_then(Value::as_str).map(String::from),
                            content: m.get("content").map(render_content).unwrap_or_default(),
                            is_error: m.get("isError").and_then(Value::as_bool).unwrap_or(false),
                        }],
                        ts,
                    });
                }
                _ => {}
            },
            Harness::Codex => unreachable!(),
        }
    }
    sum.msgs = msgs.len();
    Ok((sum, msgs))
}

fn parse_codex_item(payload: &Value, ts: Option<String>, msgs: &mut Vec<Msg>) {
    let (role, blocks) = match payload.get("type").and_then(Value::as_str) {
        Some("message") => match payload.get("role").and_then(Value::as_str) {
            Some("user") => (
                Role::User,
                vec![Block::Text(codex_text(&payload["content"], "input_text"))],
            ),
            Some("assistant") => (
                Role::Assistant,
                vec![Block::Text(codex_text(&payload["content"], "output_text"))],
            ),
            _ => return,
        },
        Some("reasoning") => {
            let text = payload["summary"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            (Role::Assistant, vec![Block::Thinking(text)])
        }
        Some("function_call") => {
            let arguments = s(payload, "arguments");
            (
                Role::Assistant,
                vec![Block::ToolCall {
                    id: s(payload, "call_id"),
                    name: s(payload, "name"),
                    args: serde_json::from_str(&arguments).unwrap_or(Value::String(arguments)),
                }],
            )
        }
        Some("function_call_output") => (
            Role::Tool,
            vec![Block::ToolResult {
                call_id: s(payload, "call_id"),
                name: None,
                content: s(payload, "output"),
                is_error: false,
            }],
        ),
        _ => return,
    };
    if !blocks
        .iter()
        .all(|block| matches!(block, Block::Text(t) | Block::Thinking(t) if t.is_empty()))
    {
        msgs.push(Msg { role, blocks, ts });
    }
}

/// Canonical blocks from an assistant `content` value (array of blocks).
/// Factory/Claude use `tool_use`/`input`; Pi/OMP use `toolCall`/`arguments`.
fn assistant_blocks(content: &Value) -> Vec<Block> {
    let Value::Array(items) = content else {
        return content
            .as_str()
            .map(|s| vec![Block::Text(s.to_string())])
            .unwrap_or_default();
    };
    let mut out = Vec::new();
    for b in items {
        let Some(o) = b.as_object() else { continue };
        match o.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = o.get("text").and_then(Value::as_str) {
                    out.push(Block::Text(t.to_string()));
                }
            }
            Some("thinking") => {
                if let Some(t) = o.get("thinking").and_then(Value::as_str)
                    && !t.is_empty()
                {
                    out.push(Block::Thinking(t.to_string()));
                }
            }
            Some("tool_use") | Some("toolCall") => out.push(Block::ToolCall {
                id: o
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                name: o
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                args: o
                    .get("input")
                    .or_else(|| o.get("arguments"))
                    .cloned()
                    .unwrap_or(Value::Null),
            }),
            Some("image") => out.push(Block::Text("[image]".to_string())),
            _ => {}
        }
    }
    out
}

/// Factory and Claude store tool results inside user messages. Split them out.
fn split_factory_user(content: &Value, ts: Option<String>) -> (Option<Msg>, Vec<Msg>) {
    let mut tools = Vec::new();
    let mut text_blocks = Vec::new();
    if let Value::Array(items) = content {
        for b in items {
            if b.get("type").and_then(Value::as_str) == Some("tool_result") {
                if let Some(o) = b.as_object() {
                    tools.push(Msg {
                        role: Role::Tool,
                        blocks: vec![Block::ToolResult {
                            call_id: o
                                .get("tool_use_id")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                            name: None,
                            content: o.get("content").map(render_content).unwrap_or_default(),
                            is_error: o.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                        }],
                        ts: ts.clone(),
                    });
                }
            } else {
                text_blocks.push(b.clone());
            }
        }
    }
    let text = match content {
        Value::String(s) => s.clone(),
        _ => render_content(&Value::Array(text_blocks)),
    };
    let user = if is_boilerplate(&text) {
        None
    } else {
        Some(Msg {
            role: Role::User,
            blocks: vec![Block::Text(text)],
            ts,
        })
    };
    (user, tools)
}

fn is_boilerplate(text: &str) -> bool {
    let t = text.trim();
    t.is_empty() || t.starts_with("<system-reminder>") || t.starts_with("<system-notice>")
}

/// Write a converted session into the target store. Returns the new file path.
pub fn write_session(
    store: &Store,
    target: Harness,
    src: &Session,
    msgs: &[Msg],
) -> std::io::Result<PathBuf> {
    let new_id = match target {
        Harness::Factory | Harness::Claude | Harness::Codex => uuid_v4(),
        Harness::Pi | Harness::Omp => uuid(),
    };
    let ts = ts_iso(src);
    let path = match target {
        Harness::Codex => {
            let dir = store
                .codex
                .join(format!("{}/{}/{}", &ts[0..4], &ts[5..7], &ts[8..10]));
            fs::create_dir_all(&dir)?;
            dir.join(format!(
                "rollout-{}-{new_id}.jsonl",
                ts[..19].replace(':', "-")
            ))
        }
        _ => {
            let dir = store.root(target).join(slug_for(target, &src.cwd));
            fs::create_dir_all(&dir)?;
            let filename = match target {
                Harness::Factory | Harness::Claude => format!("{new_id}.jsonl"),
                Harness::Pi | Harness::Omp => {
                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis();
                    format!("{now_ms}_{new_id}.jsonl")
                }
                Harness::Codex => unreachable!(),
            };
            dir.join(filename)
        }
    };
    let mut w = fs::File::create(&path)?;

    match target {
        Harness::Factory => {
            let start = json!({
                "type": "session_start",
                "id": new_id,
                "title": src.title,
                "isSessionTitleManuallySet": true,
                "owner": factory_owner(),
                "version": 2,
                "cwd": src.cwd,
            });
            writeln!(w, "{start}")?;
            // droid rebuilds the conversation from a parentId chain of
            // dashed-uuid record ids; without it the loader drops every message
            let mut parent: Option<String> = None;
            let mut i = 0;
            while i < msgs.len() {
                let m = &msgs[i];
                let ts = m.ts.clone().unwrap_or_else(|| ts_iso(src));
                let rid = uuid_v4();
                let parent_id = parent.clone();
                let mut rec = match m.role {
                    Role::Assistant => {
                        i += 1;
                        json!({
                            "type": "message",
                            "id": rid,
                            "timestamp": ts,
                            "message": {
                                "role": "assistant",
                                "content": blocks_to_anthropic(&m.blocks),
                            }
                        })
                    }
                    Role::User => {
                        i += 1;
                        json!({
                            "type": "message",
                            "id": rid,
                            "timestamp": ts,
                            "message": {
                                "role": "user",
                                "content": [{"type": "text", "text": user_text_of(&m.blocks)}],
                            }
                        })
                    }
                    Role::Tool => {
                        // group consecutive tool results into one user message
                        let mut content = Vec::new();
                        while i < msgs.len() && msgs[i].role == Role::Tool {
                            if let Some(Block::ToolResult {
                                call_id,
                                content: c,
                                is_error,
                                ..
                            }) = msgs[i].blocks.first()
                            {
                                content.push(json!({
                                    "type": "tool_result",
                                    "tool_use_id": call_id,
                                    "is_error": is_error,
                                    "content": c,
                                }));
                            }
                            i += 1;
                        }
                        json!({
                            "type": "message",
                            "id": rid,
                            "timestamp": ts,
                            "message": { "role": "user", "content": content },
                        })
                    }
                };
                // Factory's loader rejects a root record with `parentId: null`;
                // native root records omit the key entirely.
                if let Some(parent_id) = parent_id {
                    rec["parentId"] = Value::String(parent_id);
                }
                writeln!(w, "{rec}")?;
                parent = Some(rid);
            }
        }
        Harness::Claude => {
            let mut parent: Option<String> = None;
            let mut i = 0;
            while i < msgs.len() {
                let m = &msgs[i];
                let rid = uuid_v4();
                let message = match m.role {
                    Role::Assistant => {
                        i += 1;
                        let mut message = json!({
                            "role": "assistant",
                            "content": blocks_to_anthropic(&m.blocks),
                        });
                        if !src.model.is_empty() {
                            message["model"] = Value::String(src.model.clone());
                        }
                        message
                    }
                    Role::User => {
                        i += 1;
                        json!({
                            "role": "user",
                            "content": [{"type": "text", "text": user_text_of(&m.blocks)}],
                        })
                    }
                    Role::Tool => {
                        let mut content = Vec::new();
                        while i < msgs.len() && msgs[i].role == Role::Tool {
                            if let Some(Block::ToolResult {
                                call_id,
                                content: result,
                                is_error,
                                ..
                            }) = msgs[i].blocks.first()
                            {
                                content.push(json!({
                                    "type": "tool_result",
                                    "tool_use_id": call_id,
                                    "is_error": is_error,
                                    "content": result,
                                }));
                            }
                            i += 1;
                        }
                        json!({"role": "user", "content": content})
                    }
                };
                let rec = json!({
                    "type": message["role"],
                    "message": message,
                    "uuid": rid,
                    "parentUuid": parent,
                    "timestamp": m.ts.clone().unwrap_or_else(|| ts.clone()),
                    "sessionId": new_id,
                    "cwd": src.cwd,
                    "isSidechain": false,
                    "userType": "external",
                    "version": "2.1.220",
                });
                writeln!(w, "{rec}")?;
                parent = Some(rid);
            }
        }
        Harness::Codex => {
            let meta = json!({
                "timestamp": ts,
                "ordinal": 0,
                "type": "session_meta",
                "payload": {
                    "session_id": new_id,
                    "id": new_id,
                    "timestamp": ts,
                    "cwd": src.cwd,
                    "originator": "bodysnatcher",
                    "cli_version": "0.147.0",
                    "source": "cli",
                    "model_provider": "bodysnatcher",
                }
            });
            writeln!(w, "{meta}")?;
            let mut ordinal = 1;
            for m in msgs {
                let timestamp = m.ts.clone().unwrap_or_else(|| ts.clone());
                let payloads: Vec<Value> = match m.role {
                    Role::User => vec![json!({
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": user_text_of(&m.blocks)}],
                    })],
                    Role::Assistant => m
                        .blocks
                        .iter()
                        .filter_map(|block| match block {
                            Block::Text(text) => Some(json!({
                                "type": "message",
                                "role": "assistant",
                                "content": [{"type": "output_text", "text": text}],
                            })),
                            Block::Thinking(text) => Some(json!({
                                "type": "reasoning",
                                "summary": [{"type": "summary_text", "text": text}],
                                "content": null,
                            })),
                            Block::ToolCall { id, name, args } => Some(json!({
                                "type": "function_call",
                                "name": name,
                                "arguments": args.to_string(),
                                "call_id": id,
                            })),
                            Block::ToolResult { .. } => None,
                        })
                        .collect(),
                    Role::Tool => m
                        .blocks
                        .first()
                        .and_then(|block| match block {
                            Block::ToolResult {
                                call_id, content, ..
                            } => Some(vec![json!({
                                "type": "function_call_output",
                                "call_id": call_id,
                                "output": content,
                            })]),
                            _ => None,
                        })
                        .unwrap_or_default(),
                };
                for payload in payloads {
                    let rec = json!({
                        "timestamp": timestamp,
                        "ordinal": ordinal,
                        "type": "response_item",
                        "payload": payload,
                    });
                    writeln!(w, "{rec}")?;
                    ordinal += 1;
                }
            }
        }
        Harness::Pi | Harness::Omp => {
            if target == Harness::Omp {
                write!(w, "{}", title_slot_line(&src.title, &ts_iso(src)))?;
            }
            let header = json!({
                "type": "session",
                "version": 3,
                "id": new_id,
                "timestamp": ts_iso(src),
                "cwd": src.cwd,
                "title": src.title,
                "titleSource": "user",
            });
            writeln!(w, "{header}")?;
            // map tool-call ids -> names so Factory tool results (which carry
            // only an id) can be attributed when written out as Pi/OMP results
            let call_names: std::collections::HashMap<&str, &str> = msgs
                .iter()
                .flat_map(|m| m.blocks.iter())
                .filter_map(|b| match b {
                    Block::ToolCall { id, name, .. } => Some((id.as_str(), name.as_str())),
                    _ => None,
                })
                .collect();
            let mut parent: Option<String> = None;
            for m in msgs {
                let id = short_hex();
                let msg = match m.role {
                    Role::Assistant => {
                        // usage is dereferenced bare by the Pi/OMP renderer
                        // (usage.cacheRead); zeroed matches "unbilled" turns
                        let mut o = json!({
                            "role": "assistant",
                            "content": blocks_to_pi_omp(&m.blocks),
                            "provider": "bodysnatcher",
                            "usage": {
                                "input": 0,
                                "output": 0,
                                "cacheRead": 0,
                                "cacheWrite": 0,
                                "totalTokens": 0,
                                "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0},
                            },
                        });
                        if !src.model.is_empty() {
                            o["model"] = Value::String(src.model.clone());
                        }
                        o
                    }
                    Role::User => json!({
                        "role": "user",
                        "content": [{"type": "text", "text": user_text_of(&m.blocks)}],
                    }),
                    Role::Tool => {
                        let Some(Block::ToolResult {
                            call_id,
                            name,
                            content,
                            is_error,
                        }) = m.blocks.first()
                        else {
                            continue;
                        };
                        let name = name
                            .clone()
                            .or_else(|| call_names.get(call_id.as_str()).map(|s| s.to_string()))
                            .unwrap_or_default();
                        json!({
                            "role": "toolResult",
                            "toolCallId": call_id,
                            "toolName": name,
                            "content": [{"type": "text", "text": content}],
                            "isError": is_error,
                        })
                    }
                };
                let rec = json!({
                    "type": "message",
                    "id": id,
                    "parentId": parent,
                    "timestamp": m.ts.clone().unwrap_or_else(|| ts_iso(src)),
                    "message": msg,
                });
                writeln!(w, "{rec}")?;
                parent = Some(id);
            }
        }
    }
    Ok(path)
}

/// OMP's physical first line: a fixed-width 256-byte title slot (newline
/// included). Loader rejects sources outside "auto"|"user" and then rejects
/// the whole file (entries[0] must be the session header), so imported
/// sessions must emit exactly this shape.
fn title_slot_line(title: &str, updated_at: &str) -> String {
    const SLOT_BYTES: usize = 256;
    let build = |t: &str, pad: usize| {
        let line = json!({
            "type": "title",
            "v": 1,
            "title": t,
            "source": "user",
            "updatedAt": updated_at,
            "pad": " ".repeat(pad),
        });
        format!("{line}\n")
    };
    let mut t: String = title.to_string();
    while build(&t, 0).len() > SLOT_BYTES {
        t.pop();
    }
    let pad = SLOT_BYTES - build(&t, 0).len();
    build(&t, pad)
}

fn user_text_of(blocks: &[Block]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            Block::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn factory_owner() -> String {
    std::env::var("USER").unwrap_or_else(|_| "unknown".to_string())
}

fn blocks_to_anthropic(blocks: &[Block]) -> Vec<Value> {
    blocks
        .iter()
        .filter_map(|b| match b {
            Block::Text(t) => Some(json!({"type": "text", "text": t})),
            Block::Thinking(t) => Some(json!({
                "type": "thinking",
                "thinking": t,
                "signature": "reasoning_content",
                "signatureProvider": "factory",
            })),
            Block::ToolCall { id, name, args } => Some(json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": args,
            })),
            Block::ToolResult { .. } => None,
        })
        .collect()
}

fn blocks_to_pi_omp(blocks: &[Block]) -> Vec<Value> {
    blocks
        .iter()
        .filter_map(|b| match b {
            Block::Text(t) => Some(json!({"type": "text", "text": t})),
            Block::Thinking(t) => Some(json!({"type": "thinking", "thinking": t})),
            Block::ToolCall { id, name, args } => Some(json!({
                "type": "toolCall",
                "id": id,
                "name": name,
                "arguments": args,
            })),
            Block::ToolResult { .. } => None,
        })
        .collect()
}

fn ts_iso(src: &Session) -> String {
    // reuse the session's own creation time; fall back to now
    match src.modified {
        Some(m) => {
            let secs = m
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            format_iso(secs)
        }
        None => {
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            format_iso(secs)
        }
    }
}

/// Civil-date ISO like 2026-08-13T08:04:12.000Z, without chrono.
fn format_iso(secs: u64) -> String {
    let days = secs / 86400;
    let sod = secs % 86400;
    let (h, mi, se) = (sod / 3600, (sod % 3600) / 60, sod % 60);
    // Howard Hinnant's civil_from_days
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{se:02}.000Z")
}

fn short_hex() -> String {
    // 8-char hex like pi's entry ids
    uuid().chars().take(8).collect()
}

/// Dashed UUID v4 — Factory's session id/filename format.
fn uuid_v4() -> String {
    uuid_v4_from_hex(&uuid())
}

fn uuid_v4_from_hex(h: &str) -> String {
    let mut s = String::with_capacity(36);
    for (i, c) in h.chars().enumerate() {
        if matches!(i, 8 | 12 | 16 | 20) {
            s.push('-');
        }
        s.push(match i {
            12 => '4',
            16 => "89ab"
                .chars()
                .nth((h.as_bytes()[16] & 0x3) as usize)
                .unwrap(),
            _ => c,
        });
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_fixture(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        let mut f = fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p
    }

    const FACTORY: &str = r#"{"type":"session_start","id":"abc-123","title":"Fix auth bug","version":2,"cwd":"/home/u/app"}
{"type":"message","id":"m1","timestamp":"2026-08-01T10:00:00.000Z","message":{"role":"user","content":[{"type":"text","text":"Fix the login bug"}]}}
{"type":"message","id":"m2","timestamp":"2026-08-01T10:00:05.000Z","message":{"role":"assistant","model":"claude-sonnet-4-5","content":[{"type":"text","text":"On it"},{"type":"tool_use","name":"Read","input":{"file_path":"auth.ts"}}]}}
{"type":"message","id":"m3","timestamp":"2026-08-01T10:00:06.000Z","message":{"role":"user","content":[{"type":"text","text":"<system-reminder>noise</system-reminder>"}]}}
"#;

    const PI: &str = r#"{"type":"session","version":3,"id":"pi-id","timestamp":"2026-07-01T09:00:00.000Z","cwd":"/home/u/pi-app"}
{"type":"model_change","id":"c1","parentId":null,"timestamp":"2026-07-01T09:00:01.000Z","model":"gpt-5.6"}
{"type":"message","id":"zz1","parentId":null,"timestamp":"2026-07-01T09:00:01.500Z","message":{"role":"user","content":"<system-reminder>noise</system-reminder>"}}
{"type":"message","id":"zz2","parentId":null,"timestamp":"2026-07-01T09:00:01.600Z","message":{"role":"user","content":"<system-notice>also noise</system-notice>"}}
{"type":"message","id":"a1b2c3d4","parentId":null,"timestamp":"2026-07-01T09:00:02.000Z","message":{"role":"user","content":"Write a parser"}}
{"type":"message","id":"e5f6a7b8","parentId":"a1b2c3d4","timestamp":"2026-07-01T09:00:03.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Sure"},{"type":"thinking","thinking":"use nom"}],"model":"gpt-5.6","stopReason":"stop"}}
{"type":"message","id":"zz3","parentId":null,"timestamp":"2026-07-01T09:00:04.000Z","message":{"role":"user","content":""}}
"#;

    const CLAUDE: &str = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"Inspect the parser"}]},"uuid":"10000000-0000-4000-8000-000000000000","parentUuid":null,"timestamp":"2026-08-16T01:00:00.000Z","sessionId":"32158ce3-e9bd-4203-a961-f0c85814f443","cwd":"/home/u/claude_app","isSidechain":false,"userType":"external","version":"2.1.220"}
{"type":"assistant","message":{"role":"assistant","model":"claude-fable-5","content":[{"type":"thinking","thinking":"read first","signature":"sig"},{"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"parser.rs"}}]},"uuid":"20000000-0000-4000-8000-000000000000","parentUuid":"10000000-0000-4000-8000-000000000000","timestamp":"2026-08-16T01:00:01.000Z","sessionId":"32158ce3-e9bd-4203-a961-f0c85814f443","cwd":"/home/u/claude_app","isSidechain":false,"userType":"external","version":"2.1.220"}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","is_error":false,"content":"parser body"}]},"uuid":"30000000-0000-4000-8000-000000000000","parentUuid":"20000000-0000-4000-8000-000000000000","timestamp":"2026-08-16T01:00:02.000Z","sessionId":"32158ce3-e9bd-4203-a961-f0c85814f443","cwd":"/home/u/claude_app","isSidechain":false,"userType":"external","version":"2.1.220"}
{"type":"user","message":{"role":"user","content":"sidechain noise"},"uuid":"40000000-0000-4000-8000-000000000000","parentUuid":null,"timestamp":"2026-08-16T01:00:03.000Z","sessionId":"32158ce3-e9bd-4203-a961-f0c85814f443","cwd":"/home/u/claude_app","isSidechain":true}
"#;

    const CODEX: &str = r#"{"timestamp":"2026-08-16T02:00:00.000Z","ordinal":0,"type":"session_meta","payload":{"id":"01a0076f-fffd-7250-9f48-f9358e71e6b2","session_id":"01a0076f-fffd-7250-9f48-f9358e71e6b2","timestamp":"2026-08-16T02:00:00.000Z","cwd":"/home/u/codex-app","originator":"codex_cli_rs","cli_version":"0.90.0","source":"cli"}}
{"timestamp":"2026-08-16T02:00:01.000Z","ordinal":1,"type":"turn_context","payload":{"model":"gpt-5.6","cwd":"/home/u/codex-app"}}
{"timestamp":"2026-08-16T02:00:02.000Z","ordinal":2,"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Run the checks"},{"type":"input_image","image_url":"x"}]}}
{"timestamp":"2026-08-16T02:00:03.000Z","ordinal":3,"type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"inspect first"}],"content":null,"encrypted_content":"x"}}
{"timestamp":"2026-08-16T02:00:04.000Z","ordinal":4,"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Checking"}]}}
{"timestamp":"2026-08-16T02:00:05.000Z","ordinal":5,"type":"response_item","payload":{"type":"function_call","name":"shell_command","arguments":"{\"command\":\"cargo test\"}","call_id":"call_1"}}
{"timestamp":"2026-08-16T02:00:06.000Z","ordinal":6,"type":"response_item","payload":{"type":"function_call_output","call_id":"call_1","output":"ok"}}
"#;
    #[test]
    fn parses_factory_fixture() {
        let dir = std::env::temp_dir().join(format!("bs-test-{}", uuid()));
        fs::create_dir_all(&dir).unwrap();
        let p = write_fixture(&dir, "s.jsonl", FACTORY);
        let (sess, msgs) = parse(&p, Harness::Factory).unwrap();
        assert_eq!(sess.title, "Fix auth bug");
        assert_eq!(sess.cwd, "/home/u/app");
        assert_eq!(sess.id, "abc-123");
        assert_eq!(sess.model, "claude-sonnet-4-5");
        // system-reminder user message is dropped, hook boilerplate skipped
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, Role::User);
        assert_eq!(
            msgs[0].blocks,
            vec![Block::Text("Fix the login bug".into())]
        );
        assert_eq!(msgs[1].role, Role::Assistant);
        assert_eq!(msgs[1].blocks[0], Block::Text("On it".into()));
        let Block::ToolCall { name, args, .. } = &msgs[1].blocks[1] else {
            panic!("expected tool call");
        };
        assert_eq!(name, "Read");
        assert_eq!(args["file_path"], "auth.ts");
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn parses_pi_fixture() {
        let dir = std::env::temp_dir().join(format!("bs-test-{}", uuid()));
        fs::create_dir_all(&dir).unwrap();
        let p = write_fixture(&dir, "s.jsonl", PI);
        let (sess, msgs) = parse(&p, Harness::Omp).unwrap();
        assert_eq!(sess.id, "pi-id");
        assert_eq!(sess.cwd, "/home/u/pi-app");
        assert_eq!(sess.model, "gpt-5.6");
        // pi has no title record: title/preview derive from first user message
        assert_eq!(sess.title, "Write a parser");
        assert_eq!(sess.preview, "Write a parser");
        // boilerplate + empty user messages dropped
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].ts.as_deref(), Some("2026-07-01T09:00:02.000Z"));
        assert_eq!(msgs[1].blocks[0], Block::Text("Sure".into()));
        assert_eq!(msgs[1].blocks[1], Block::Thinking("use nom".into()));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn parses_claude_fixture() {
        let dir = std::env::temp_dir().join(format!("bs-test-{}", uuid()));
        fs::create_dir_all(&dir).unwrap();
        let p = write_fixture(&dir, "32158ce3-e9bd-4203-a961-f0c85814f443.jsonl", CLAUDE);
        let (sess, msgs) = parse(&p, Harness::Claude).unwrap();
        assert_eq!(sess.id, "32158ce3-e9bd-4203-a961-f0c85814f443");
        assert_eq!(sess.cwd, "/home/u/claude_app");
        assert_eq!(sess.model, "claude-fable-5");
        assert_eq!(sess.title, "Inspect the parser");
        assert_eq!(msgs.len(), 3);
        assert_eq!(
            msgs[0].blocks,
            vec![Block::Text("Inspect the parser".into())]
        );
        assert_eq!(msgs[1].blocks[0], Block::Thinking("read first".into()));
        assert_eq!(
            msgs[1].blocks[1],
            Block::ToolCall {
                id: "toolu_1".into(),
                name: "Read".into(),
                args: json!({"file_path": "parser.rs"}),
            }
        );
        assert_eq!(msgs[2].role, Role::Tool);
        assert_eq!(sess.msgs, 3);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn parses_codex_fixture() {
        let dir = std::env::temp_dir().join(format!("bs-test-{}", uuid()));
        fs::create_dir_all(&dir).unwrap();
        let p = write_fixture(&dir, "rollout-test.jsonl", CODEX);
        let (sess, msgs) = parse(&p, Harness::Codex).unwrap();
        assert_eq!(sess.id, "01a0076f-fffd-7250-9f48-f9358e71e6b2");
        assert_eq!(sess.cwd, "/home/u/codex-app");
        assert_eq!(sess.model, "gpt-5.6");
        assert_eq!(sess.title, "Run the checks [image]");
        assert_eq!(msgs.len(), 5);
        assert_eq!(
            msgs[0].blocks,
            vec![Block::Text("Run the checks\n[image]".into())]
        );
        assert_eq!(
            msgs[1].blocks,
            vec![Block::Thinking("inspect first".into())]
        );
        assert_eq!(msgs[2].blocks, vec![Block::Text("Checking".into())]);
        assert_eq!(
            msgs[3].blocks,
            vec![Block::ToolCall {
                id: "call_1".into(),
                name: "shell_command".into(),
                args: json!({"command": "cargo test"}),
            }]
        );
        assert_eq!(msgs[4].role, Role::Tool);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn summaries_count_only_conversation_messages() {
        let dir = std::env::temp_dir().join(format!("bs-test-{}", uuid()));
        fs::create_dir_all(&dir).unwrap();

        let claude = format!(
            "{}\n{}\n{}\n{}\n{}\n",
            r#"{"type":"user","message":{"role":"system","content":"ignore"},"uuid":"0","parentUuid":null,"cwd":"/x","isSidechain":false}"#,
            r#"{"type":"progress","message":{"role":"user","content":"ignore"},"uuid":"1","parentUuid":"0","cwd":"/x","isSidechain":false}"#,
            r#"{"type":"user","message":{"role":"user","content":"keep"},"uuid":"2","parentUuid":"1","cwd":"/x","isSidechain":false}"#,
            r#"{"type":"assistant","message":{"role":"assistant","model":"first","content":[{"type":"text","text":"answer"}]},"uuid":"3","parentUuid":"2","cwd":"/x","isSidechain":false}"#,
            r#"{"type":"assistant","message":{"role":"assistant","model":"later","content":[{"type":"text","text":"answer 2"}]},"uuid":"4","parentUuid":"3","cwd":"/x","isSidechain":false}"#,
        );
        let claude_path = write_fixture(&dir, "claude.jsonl", &claude);
        let claude_sum = summarize(&claude_path, Harness::Claude).unwrap();
        assert_eq!(claude_sum.msgs, 3);
        assert_eq!(claude_sum.preview, "keep");
        assert_eq!(claude_sum.model, "first");

        let codex = format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
            r#"{"type":"session_meta","payload":{"id":"c1","cwd":"/x"}}"#,
            r#"{"type":"response_item","payload":{"type":"function_call","name":"shell_command","arguments":"{}","call_id":"call"}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"ignore"}]}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"keep"}]}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"later"}]}}"#,
            r#"{"type":"turn_context","payload":{"model":"first"}}"#,
            r#"{"type":"turn_context","payload":{"model":"later"}}"#,
        );
        let codex_path = write_fixture(&dir, "codex.jsonl", &codex);
        let codex_sum = summarize(&codex_path, Harness::Codex).unwrap();
        assert_eq!(codex_sum.msgs, 3);
        assert_eq!(codex_sum.preview, "keep");
        assert_eq!(codex_sum.model, "first");

        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn claude_write_roundtrip() {
        let dir = std::env::temp_dir().join(format!("bs-test-{}", uuid()));
        fs::create_dir_all(&dir).unwrap();
        let src = write_fixture(&dir, "source.jsonl", CLAUDE);
        let (sess, msgs) = parse(&src, Harness::Claude).unwrap();
        let store = test_store(&dir);
        let pi = write_session(&store, Harness::Pi, &sess, &msgs).unwrap();
        let pi_msgs = parse(&pi, Harness::Pi).unwrap().1;
        assert_eq!(pi_msgs[..2], msgs[..2]);
        assert!(matches!(
            &pi_msgs[2].blocks[0],
            Block::ToolResult {
                call_id,
                name: Some(name),
                content,
                is_error: false,
            } if call_id == "toolu_1" && name == "Read" && content == "parser body"
        ));

        let out = write_session(&store, Harness::Claude, &sess, &tool_msgs()).unwrap();
        assert!(out.starts_with(store.claude.join(slug_for(Harness::Claude, &sess.cwd))));
        assert_eq!(out.file_stem().unwrap().to_string_lossy().len(), 36);
        let lines = read_lines(&out);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0]["parentUuid"], Value::Null);
        assert_eq!(lines[1]["parentUuid"], lines[0]["uuid"]);
        assert_eq!(lines[2]["parentUuid"], lines[1]["uuid"]);
        assert_eq!(lines[1]["message"]["content"].as_array().unwrap().len(), 2);
        assert_eq!(lines[1]["message"]["content"][0]["type"], "tool_result");
        let reparsed = parse(&out, Harness::Claude).unwrap().1;
        assert_eq!(
            reparsed
                .iter()
                .map(|m| (m.role, &m.blocks))
                .collect::<Vec<_>>(),
            tool_msgs()
                .iter()
                .map(|m| (m.role, &m.blocks))
                .collect::<Vec<_>>()
        );
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn codex_write_roundtrip() {
        let dir = std::env::temp_dir().join(format!("bs-test-{}", uuid()));
        fs::create_dir_all(&dir).unwrap();
        let src = write_fixture(&dir, "rollout-source.jsonl", CODEX);
        let (sess, msgs) = parse(&src, Harness::Codex).unwrap();
        let store = test_store(&dir);
        let factory = write_session(&store, Harness::Factory, &sess, &msgs).unwrap();
        assert_eq!(parse(&factory, Harness::Factory).unwrap().1, msgs);

        let out = write_session(&store, Harness::Codex, &sess, &msgs).unwrap();
        let lines = read_lines(&out);
        assert_eq!(lines[0]["type"], "session_meta");
        assert_eq!(lines[0]["payload"]["id"], lines[0]["payload"]["session_id"]);
        for (ordinal, line) in lines.iter().enumerate() {
            assert_eq!(line["ordinal"], ordinal);
        }
        let call = lines
            .iter()
            .find(|line| line["payload"]["type"] == "function_call")
            .unwrap();
        assert!(call["payload"]["arguments"].is_string());
        let output = lines
            .iter()
            .find(|line| line["payload"]["type"] == "function_call_output")
            .unwrap();
        assert_eq!(output["payload"]["output"], "ok");
        assert_eq!(parse(&out, Harness::Codex).unwrap().1, msgs);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn codex_discovery_filters_cwd_recursively() {
        let dir = std::env::temp_dir().join(format!("bs-test-{}", uuid()));
        let day = dir.join("2026/08/16");
        fs::create_dir_all(&day).unwrap();
        write_fixture(&day, "rollout-2026-08-16T02-00-00-a.jsonl", CODEX);
        write_fixture(
            &day,
            "rollout-2026-08-16T03-00-00-b.jsonl",
            &CODEX.replace("/home/u/codex-app", "/home/u/other"),
        );
        write_fixture(&day, "ignored.jsonl", CODEX);
        let all = discover(&dir, Harness::Codex);
        assert_eq!(all.len(), 2);
        let found = discover_for_cwd(&dir, Harness::Codex, Path::new("/home/u/codex-app"));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].cwd, "/home/u/codex-app");
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn factory_summarize_fills_new_session_title() {
        let dir = std::env::temp_dir().join(format!("bs-test-{}", uuid()));
        fs::create_dir_all(&dir).unwrap();
        let body = r#"{"type":"session_start","id":"n1","title":"New Session","version":2,"cwd":"/home/u/x"}
{"type":"message","id":"m1","timestamp":"2026-08-01T10:00:00.000Z","message":{"role":"user","content":[{"type":"text","text":"Actually do the thing"}]}}
"#;
        let p = write_fixture(&dir, "s.jsonl", body);
        let sess = summarize(&p, Harness::Factory).unwrap();
        assert_eq!(sess.title, "Actually do the thing");
        assert_eq!(sess.msgs, 1);
        fs::remove_dir_all(dir).ok();
    }
    #[test]
    fn factory_to_pi_roundtrip() {
        let dir = std::env::temp_dir().join(format!("bs-test-{}", uuid()));
        fs::create_dir_all(&dir).unwrap();
        let src = write_fixture(&dir, "src.jsonl", FACTORY);
        let (sess, msgs) = parse(&src, Harness::Factory).unwrap();
        let store = test_store(&dir);
        let out = write_session(&store, Harness::Pi, &sess, &msgs).unwrap();
        let (sess2, msgs2) = parse(&out, Harness::Pi).unwrap();
        assert_eq!(sess2.cwd, "/home/u/app");
        assert_eq!(sess2.title, "Fix auth bug");
        assert_eq!(msgs2.len(), msgs.len());
        assert_eq!(msgs2[0].role, Role::User);
        assert_eq!(msgs2[0].blocks, msgs[0].blocks);
        // tree structure: parentId chain intact
        let raw = fs::read_to_string(&out).unwrap();
        let lines: Vec<Value> = raw
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines[0]["type"], "session");
        assert_eq!(lines[0]["version"], 3);
        assert_eq!(lines[1]["parentId"], Value::Null);
        assert_eq!(lines[2]["parentId"], lines[1]["id"]);
        // omp target includes a title line first
        let out2 = write_session(&store, Harness::Omp, &sess, &msgs).unwrap();
        let raw2 = fs::read_to_string(&out2).unwrap();
        let l0: Value = serde_json::from_str(raw2.lines().next().unwrap()).unwrap();
        assert_eq!(l0["type"], "title");
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn pi_to_factory_roundtrip() {
        let dir = std::env::temp_dir().join(format!("bs-test-{}", uuid()));
        fs::create_dir_all(&dir).unwrap();
        let src = write_fixture(&dir, "src.jsonl", PI);
        let (sess, msgs) = parse(&src, Harness::Pi).unwrap();
        let store = test_store(&dir);
        let out = write_session(&store, Harness::Factory, &sess, &msgs).unwrap();
        let (sess2, msgs2) = parse(&out, Harness::Factory).unwrap();
        assert_eq!(sess2.title, "Write a parser");
        assert_eq!(msgs2.len(), msgs.len());
        assert_eq!(msgs2[1].role, Role::Assistant);
        let raw = fs::read_to_string(&out).unwrap();
        let lines: Vec<Value> = raw
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert!(lines[1].get("parentId").is_none());
        assert_eq!(lines[2]["parentId"], lines[1]["id"]);
        assert_eq!(lines[0]["type"], "session_start");
        assert_eq!(lines[1]["message"]["role"], "user");
        assert!(
            lines[2]["message"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("Sure")
        );

        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn iso_format_matches_shape() {
        let s = format_iso(1_750_000_000);
        assert_eq!(s, "2025-06-15T15:06:40.000Z");
    }

    #[test]
    fn discover_scans_workspaces() {
        let dir = std::env::temp_dir().join(format!("bs-test-{}", uuid()));
        let ws = dir.join("--home-u-app--");
        fs::create_dir_all(&ws).unwrap();
        write_fixture(&ws, "2026-01-01T00-00-00-000Z_x.jsonl", PI);
        let found = discover(&dir, Harness::Pi);
        assert_eq!(found[0].id, "pi-id");
        assert_eq!(found[0].msgs, 5);
        assert_eq!(found[0].title, "Write a parser");
        fs::remove_dir_all(dir).ok();
    }
    #[test]
    fn discover_for_cwd_skips_other_workspaces() {
        let dir = std::env::temp_dir().join(format!("bs-cwd-{}", uuid()));
        let match_ws = dir.join(slug_for(Harness::Pi, "/home/u/app"));
        let other_ws = dir.join(slug_for(Harness::Pi, "/home/u/other"));
        fs::create_dir_all(&match_ws).unwrap();
        fs::create_dir_all(&other_ws).unwrap();
        write_fixture(&match_ws, "2026-01-01T00-00-00-000Z_x.jsonl", PI);
        write_fixture(&other_ws, "2026-01-02T00-00-00-000Z_y.jsonl", PI);
        let found = discover_for_cwd(&dir, Harness::Pi, Path::new("/home/u/app"));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "pi-id");
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn iso_boundaries() {
        assert_eq!(format_iso(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(format_iso(1709078400), "2024-02-28T00:00:00.000Z");
        assert_eq!(format_iso(1709164800), "2024-02-29T00:00:00.000Z");
        assert_eq!(format_iso(951782400), "2000-02-29T00:00:00.000Z");
        assert_eq!(format_iso(951868800), "2000-03-01T00:00:00.000Z");
        assert_eq!(format_iso(1767139200), "2025-12-31T00:00:00.000Z");
        assert_eq!(format_iso(4102444800), "2100-01-01T00:00:00.000Z");
        assert_eq!(format_iso(4107542400), "2100-03-01T00:00:00.000Z");
    }

    #[test]
    fn clip_caps_at_140_chars() {
        let long = "x".repeat(200);
        let c = clip(&long);
        assert_eq!(c.chars().count(), 138);
        assert!(c.ends_with('…'));
        let ok = "y".repeat(140);
        assert_eq!(clip(&ok), ok);
        assert_eq!(clip("a\nb"), "a b");
    }

    #[test]
    fn short_hex_is_8_hex_digits() {
        let h = short_hex();
        assert_eq!(h.len(), 8);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn title_falls_back_when_no_user_msg() {
        let dir = std::env::temp_dir().join(format!("bs-test-{}", uuid()));
        fs::create_dir_all(&dir).unwrap();
        let body = r#"{"type":"session_start","id":"n2","title":"New Session","version":2,"cwd":"/home/u/y"}
{"type":"message","id":"m1","timestamp":"2026-08-01T10:00:00.000Z","message":{"role":"assistant","content":[{"type":"text","text":"hello there"}]}}
"#;
        let p = write_fixture(&dir, "abc.jsonl", body);
        let sess = summarize(&p, Harness::Factory).unwrap();
        // no user message: title comes from filename stem, preview stays empty
        assert_eq!(sess.title, "abc");
        assert!(sess.preview.is_empty());
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn pi_title_record_wins() {
        let dir = std::env::temp_dir().join(format!("bs-test-{}", uuid()));
        fs::create_dir_all(&dir).unwrap();
        let body = r#"{"type":"title","v":1,"title":"Custom Title","source":"auto","updatedAt":"2026-07-01T09:00:00.000Z","pad":""}
{"type":"session","version":3,"id":"t9","timestamp":"2026-07-01T09:00:00.000Z","cwd":"/x"}
{"type":"message","id":"m1","parentId":null,"timestamp":"2026-07-01T09:00:01.000Z","message":{"role":"user","content":"first words"}}
"#;
        let p = write_fixture(&dir, "s.jsonl", body);
        let sess = summarize(&p, Harness::Omp).unwrap();
        assert_eq!(sess.title, "Custom Title");
        assert_eq!(sess.preview, "first words");
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn user_msg_model_field_is_ignored() {
        let dir = std::env::temp_dir().join(format!("bs-test-{}", uuid()));
        fs::create_dir_all(&dir).unwrap();
        let body = r#"{"type":"session_start","id":"t7","title":"New Session","version":2,"cwd":"/x"}
{"type":"message","id":"m1","timestamp":"2026-07-01T09:00:01.000Z","message":{"role":"assistant","content":[{"type":"text","text":"hi"}]}}
{"type":"message","id":"m2","timestamp":"2026-07-01T09:00:02.000Z","message":{"role":"user","content":[{"type":"text","text":"hello"}],"model":"sneaky"}}
"#;
        let p = write_fixture(&dir, "s.jsonl", body);
        let sess = summarize(&p, Harness::Factory).unwrap();
        // model only ever comes from assistant messages
        assert!(sess.model.is_empty());
        let pi_body = r#"{"type":"session","version":3,"id":"t7","timestamp":"2026-07-01T09:00:00.000Z","cwd":"/x"}
{"type":"message","id":"m1","parentId":null,"timestamp":"2026-07-01T09:00:01.000Z","message":{"role":"assistant","content":[{"type":"text","text":"hi"}]}}
{"type":"message","id":"m2","parentId":null,"timestamp":"2026-07-01T09:00:02.000Z","message":{"role":"user","content":"hello","model":"sneaky"}}
"#;
        let p2 = write_fixture(&dir, "p.jsonl", pi_body);
        let sess2 = summarize(&p2, Harness::Pi).unwrap();
        assert!(sess2.model.is_empty());
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn model_change_beats_message_model() {
        let dir = std::env::temp_dir().join(format!("bs-test-{}", uuid()));
        fs::create_dir_all(&dir).unwrap();
        let body = r#"{"type":"session","version":3,"id":"t8","timestamp":"2026-07-01T09:00:00.000Z","cwd":"/x"}
{"type":"model_change","id":"c1","parentId":null,"timestamp":"2026-07-01T09:00:01.000Z","model":"gpt-5.6"}
{"type":"message","id":"m1","parentId":null,"timestamp":"2026-07-01T09:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"hi"}],"model":"older-model"}}
"#;
        let p = write_fixture(&dir, "s.jsonl", body);
        let sess = summarize(&p, Harness::Pi).unwrap();
        assert_eq!(sess.model, "gpt-5.6");
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn written_records_get_timestamps() {
        let dir = std::env::temp_dir().join(format!("bs-test-{}", uuid()));
        fs::create_dir_all(&dir).unwrap();
        let sess = Session {
            harness: Harness::Factory,
            path: dir.join("s.jsonl"),
            id: "x".into(),
            title: "t".into(),
            cwd: "/home/u/app".into(),
            model: String::new(),
            msgs: 1,
            preview: String::new(),
            modified: Some(std::time::UNIX_EPOCH),
        };
        let msgs = vec![Msg {
            role: Role::User,
            blocks: vec![Block::Text("hi".into())],
            ts: None,
        }];
        let store = test_store(&dir);
        let out = write_session(&store, Harness::Factory, &sess, &msgs).unwrap();
        let raw = fs::read_to_string(&out).unwrap();
        let lines: Vec<Value> = raw
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let ts = lines[1]["timestamp"].as_str().unwrap();
        assert_eq!(ts.len(), "1970-01-01T00:00:00.000Z".len());
        assert!(ts.ends_with(".000Z"));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn uuid_v4_shape() {
        assert_eq!(
            uuid_v4_from_hex("00000000000000000000000000000000"),
            "00000000-0000-4000-8000-000000000000"
        );
    }

    #[test]
    fn factory_filename_is_session_id() {
        let dir = std::env::temp_dir().join(format!("bs-test-{}", uuid()));
        fs::create_dir_all(&dir).unwrap();
        let sess = bare_session(&dir, "");
        let store = test_store(&dir);
        let msgs = vec![Msg {
            role: Role::User,
            blocks: vec![Block::Text("hi".into())],
            ts: None,
        }];
        let out = write_session(&store, Harness::Factory, &sess, &msgs).unwrap();
        // droid resolves sessions by filename: stem must equal session_start.id
        let stem = out.file_stem().unwrap().to_str().unwrap();
        let first: Value =
            serde_json::from_str(fs::read_to_string(&out).unwrap().lines().next().unwrap())
                .unwrap();
        assert_eq!(first["type"], "session_start");
        assert_eq!(first["id"].as_str().unwrap(), stem);
        let expected_owner = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());
        assert_eq!(first["owner"], expected_owner);
        assert_eq!(stem.len(), 36);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn omp_title_slot_matches_loader_contract() {
        let line = title_slot_line("My session", "2026-08-16T00:00:00.000Z");
        // exactly 256 bytes including the newline (fixed-width physical slot)
        assert_eq!(line.len(), 256);
        let v: Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(v["type"], "title");
        assert_eq!(v["v"], 1);
        // loader only accepts auto|user; anything else voids the whole file
        assert_eq!(v["source"], "user");
        assert_eq!(v["title"], "My session");
        assert!(v["pad"].as_str().unwrap().chars().all(|c| c == ' '));
        // long titles truncate to fit the slot
        let long = title_slot_line(&"x".repeat(500), "2026-08-16T00:00:00.000Z");
        assert_eq!(long.len(), 256);
        let lv: Value = serde_json::from_str(long.trim_end()).unwrap();
        assert!(lv["title"].as_str().unwrap().len() < 500);
        // boundary: title sized to exactly fill the slot is kept whole with
        // pad 0; one char longer loses exactly one char (pins the > check)
        let probe = |n: usize| {
            let l = title_slot_line(&"y".repeat(n), "2026-08-16T00:00:00.000Z");
            let v: Value = serde_json::from_str(l.trim_end()).unwrap();
            (
                v["title"].as_str().unwrap().len(),
                v["pad"].as_str().unwrap().len(),
            )
        };
        let fixed = 256 - probe(0).0 - probe(0).1; // non-title bytes incl newline
        let exact = 256 - fixed;
        assert_eq!(probe(exact), (exact, 0));
        assert_eq!(probe(exact + 1), (exact, 0));
    }

    #[test]
    fn parses_pi_tool_result_and_image() {
        let dir = std::env::temp_dir().join(format!("bs-test-{}", uuid()));
        fs::create_dir_all(&dir).unwrap();
        let body = r#"{"type":"session","version":3,"id":"pt","timestamp":"2026-07-01T09:00:00.000Z","cwd":"/x"}
{"type":"message","id":"a1","parentId":null,"timestamp":"2026-07-01T09:00:01.000Z","message":{"role":"user","content":"do it"}}
{"type":"message","id":"a2","parentId":"a1","timestamp":"2026-07-01T09:00:02.000Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"call1","name":"Read","arguments":{"file_path":"a.rs"}},{"type":"image","source":"x.png"}]}}
{"type":"message","id":"a3","parentId":"a2","timestamp":"2026-07-01T09:00:03.000Z","message":{"role":"toolResult","toolCallId":"call1","toolName":"Read","content":[{"type":"text","text":"file body"}],"isError":true}}
"#;
        let p = write_fixture(&dir, "s.jsonl", body);
        let (_sess, msgs) = parse(&p, Harness::Pi).unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[1].role, Role::Assistant);
        assert_eq!(
            msgs[1].blocks[0],
            Block::ToolCall {
                id: "call1".into(),
                name: "Read".into(),
                args: json!({"file_path": "a.rs"}),
            }
        );
        // image flattens to a placeholder text block
        assert_eq!(msgs[1].blocks[1], Block::Text("[image]".into()));
        assert_eq!(msgs[2].role, Role::Tool);
        assert_eq!(
            msgs[2].blocks[0],
            Block::ToolResult {
                call_id: "call1".into(),
                name: Some("Read".into()),
                content: "file body".into(),
                is_error: true,
            }
        );
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn parses_factory_bare_string_user() {
        let dir = std::env::temp_dir().join(format!("bs-test-{}", uuid()));
        fs::create_dir_all(&dir).unwrap();
        let body = r#"{"type":"session_start","id":"bs","title":"S","version":2,"cwd":"/x"}
{"type":"message","id":"m1","timestamp":"2026-08-01T10:00:00.000Z","message":{"role":"user","content":"plain string"}}
"#;
        let p = write_fixture(&dir, "s.jsonl", body);
        let (_sess, msgs) = parse(&p, Harness::Factory).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].blocks, vec![Block::Text("plain string".into())]);
        fs::remove_dir_all(dir).ok();
    }

    fn tool_msgs() -> Vec<Msg> {
        vec![
            Msg {
                role: Role::Assistant,
                blocks: vec![
                    Block::ToolCall {
                        id: "c1".into(),
                        name: "Read".into(),
                        args: json!({"file_path": "a.rs"}),
                    },
                    Block::ToolCall {
                        id: "c2".into(),
                        name: "Glob".into(),
                        args: json!({"pattern": "*.rs"}),
                    },
                ],
                ts: None,
            },
            Msg {
                role: Role::Tool,
                blocks: vec![Block::ToolResult {
                    call_id: "c1".into(),
                    name: None,
                    content: "r1".into(),
                    is_error: false,
                }],
                ts: None,
            },
            Msg {
                role: Role::Tool,
                blocks: vec![Block::ToolResult {
                    call_id: "c2".into(),
                    name: None,
                    content: "r2".into(),
                    is_error: true,
                }],
                ts: None,
            },
            Msg {
                role: Role::User,
                blocks: vec![Block::Text("next".into())],
                ts: None,
            },
        ]
    }

    fn test_store(dir: &Path) -> Store {
        Store {
            factory: dir.join("fac"),
            pi: dir.join("pi"),
            omp: dir.join("omp"),
            claude: dir.join("claude"),
            codex: dir.join("codex"),
        }
    }

    fn bare_session(dir: &Path, model: &str) -> Session {
        Session {
            harness: Harness::Factory,
            path: dir.join("s.jsonl"),
            id: "x".into(),
            title: "t".into(),
            cwd: "/home/u/app".into(),
            model: model.into(),
            msgs: 0,
            preview: String::new(),
            modified: None,
        }
    }

    fn read_lines(path: &Path) -> Vec<Value> {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn factory_write_groups_consecutive_tool_results() {
        let dir = std::env::temp_dir().join(format!("bs-test-{}", uuid()));
        fs::create_dir_all(&dir).unwrap();
        let store = test_store(&dir);
        let sess = bare_session(&dir, "");
        let out = write_session(&store, Harness::Factory, &sess, &tool_msgs()).unwrap();
        let lines = read_lines(&out);
        // session_start + assistant + ONE grouped user + trailing user
        assert_eq!(lines.len(), 4);
        let grouped = &lines[2]["message"];
        assert_eq!(grouped["role"], "user");
        let content = grouped["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "tool_result");
        assert_eq!(content[0]["tool_use_id"], "c1");
        assert_eq!(content[0]["is_error"], false);
        assert_eq!(content[1]["tool_use_id"], "c2");
        assert_eq!(content[1]["is_error"], true);
        // trailing user message survived the grouping loop
        assert_eq!(lines[3]["message"]["content"][0]["text"], "next");
        // droid rebuilds history from the parentId chain.
        assert!(lines[1].get("parentId").is_none());
        for w in lines[1..].windows(2) {
            assert_eq!(w[1]["parentId"], w[0]["id"]);
        }
        // record ids are dashed uuids
        assert_eq!(lines[1]["id"].as_str().unwrap().len(), 36);
        // tools at the very end of the session: grouping loop must stop at len
        let mut tail = tool_msgs();
        tail.pop();
        let out2 = write_session(&store, Harness::Factory, &sess, &tail).unwrap();
        let lines2 = read_lines(&out2);
        assert_eq!(lines2.len(), 3);
        assert_eq!(lines2[2]["message"]["content"].as_array().unwrap().len(), 2);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn claude_write_groups_tools_and_gates_model() {
        let dir = std::env::temp_dir().join(format!("bs-test-{}", uuid()));
        fs::create_dir_all(&dir).unwrap();
        let store = test_store(&dir);

        let sess = bare_session(&dir, "claude-fable-5");
        let out = write_session(&store, Harness::Claude, &sess, &tool_msgs()).unwrap();
        let lines = read_lines(&out);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0]["message"]["model"], "claude-fable-5");
        assert_eq!(lines[1]["message"]["content"].as_array().unwrap().len(), 2);
        assert_eq!(lines[2]["message"]["content"][0]["text"], "next");

        let mut tail = tool_msgs();
        tail.pop();
        let out2 = write_session(&store, Harness::Claude, &sess, &tail).unwrap();
        let lines2 = read_lines(&out2);
        assert_eq!(lines2.len(), 2);
        assert_eq!(lines2[1]["message"]["content"].as_array().unwrap().len(), 2);

        let sess2 = bare_session(&dir, "");
        let out3 = write_session(&store, Harness::Claude, &sess2, &tool_msgs()).unwrap();
        let lines3 = read_lines(&out3);
        assert!(lines3[0]["message"].get("model").is_none());
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn pi_write_backfills_tool_names_and_gates_model() {
        let dir = std::env::temp_dir().join(format!("bs-test-{}", uuid()));
        fs::create_dir_all(&dir).unwrap();
        let store = test_store(&dir);
        // tool names backfilled from the assistant's ToolCall blocks
        let sess = bare_session(&dir, "gpt-5.6");
        let out = write_session(&store, Harness::Pi, &sess, &tool_msgs()).unwrap();
        let lines = read_lines(&out);
        let results: Vec<&Value> = lines
            .iter()
            .filter(|l| l["message"]["role"] == "toolResult")
            .collect();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["message"]["toolName"], "Read");
        assert_eq!(results[1]["message"]["toolName"], "Glob");
        // model set -> present on assistant message
        let assistant = lines
            .iter()
            .find(|l| l["message"]["role"] == "assistant")
            .unwrap();
        assert_eq!(assistant["message"]["model"], "gpt-5.6");
        // usage must exist: the Pi/OMP renderer dereferences it unconditionally
        let usage = &assistant["message"]["usage"];
        assert_eq!(usage["cacheRead"], 0);
        assert_eq!(usage["input"], 0);
        assert_eq!(usage["cost"]["total"], 0);
        // model empty -> key omitted entirely
        let sess2 = bare_session(&dir, "");
        let out2 = write_session(&store, Harness::Pi, &sess2, &tool_msgs()).unwrap();
        let lines2 = read_lines(&out2);
        let assistant2 = lines2
            .iter()
            .find(|l| l["message"]["role"] == "assistant")
            .unwrap();
        assert!(assistant2["message"].get("model").is_none());
        fs::remove_dir_all(dir).ok();
    }
}
