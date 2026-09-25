# bodysnatcher

Resume any AI coding-harness session in any other harness.

Factory Droid (`~/.factory`), Pi (`~/.pi`), OMP (`~/.omp`), Claude Code
(`~/.claude`), and Codex (`~/.codex`) each store sessions as JSONL with their
own schema. bodysnatcher converts a session's full history — messages, tool
calls, tool results, thinking blocks — into the
target harness' native format, writes it into the target's session store, and
replaces itself with the harness' native resume command. The target harness
loads the converted history as if it had written it and continues the
conversation from there.

![Pick a Codex session in the bodysnatcher TUI and resume it in Claude Code](assets/demo.gif)

## Install

```sh
cargo install --git https://github.com/alfredosdpiii/bodysnatcher
```

Requires the Rust toolchain (edition 2024). The harnesses you resume into
must be installed separately (`droid`, `pi`, `omp`, `claude`, `codex` on PATH).

## Usage

Run inside any project directory:

```sh
bodysnatcher
```

The TUI lists sessions for the current directory across all five harnesses.
Pick one, pick a target, resume.

| Key | Action |
| --- | --- |
| `j`/`k`, arrows, `Ctrl-j`/`Ctrl-k` | move |
| `g` / `G` | first / last |
| `/`, or any other letter | search by title, cwd, or id (while searching, every letter is typed) |
| `Backspace`, `Ctrl-u` | edit / clear search |
| `Esc` | leave search, or quit |
| `Tab` | cycle target harness |
| `Enter` | convert and resume |
| `q`, `Ctrl-c` | quit |

### Headless conversion

```sh
bodysnatcher convert <session.jsonl> --to factory|pi|omp|claude|codex [--from factory|pi|omp|claude|codex] [--sessions-dir <dir>] [--dry-run] [--run]
```

`--from` is auto-detected from the path. `--sessions-dir` redirects output
(default: the target harness' real store). `--dry-run` prints the destination
without writing. `--run` resumes immediately after writing.

### Oversized sessions

Sessions the source harness already compacted are read the way that harness
reloads them: its latest compaction summary plus the messages it kept, not the
full pre-compaction history still sitting in the file (Claude
`compact_boundary`, Pi/OMP `compaction`, Factory `compaction_state`, Codex
`compacted`).

A harness compacts a session by feeding it to its model, so a session larger
than the model's context window can never be compacted once it lands. When an
imported (or natively resumed) session exceeds the target model's context
window, bodysnatcher writes a compacted copy instead: older history is folded
into checkpoint messages (user requests, assistant outcomes, tools used, files
touched, errors) and the most recent turns are kept verbatim, until the whole
transcript fits the landing budget. Sessions that fit are written untouched.

```sh
bodysnatcher --context-window 1000000   # target model's window (default: detected)
bodysnatcher --budget 200000            # landing size (default 200k, max 60% of window)
bodysnatcher --no-compact               # never compact
```

The window is detected from the target harness' config where possible (Codex
`model_context_window` / models cache, Pi `models.json`, Factory custom model
`maxContextLimit`, Claude `[1m]` models or recent turns that used more than
200k tokens); otherwise 272k for Codex and 200k for the rest. The flags also
apply to `convert`.

### Store overrides

```sh
bodysnatcher --factory-dir <dir> --pi-dir <dir> --omp-dir <dir> --claude-dir <dir> --codex-dir <dir>
bodysnatcher -d <extra-session-dir>   # scan additional directories
```

## How it works

- **Canonical model**: sessions parse into `Msg { role, blocks, ts }` where
  blocks are `Text | Thinking | ToolCall | ToolResult`. Adapters convert to
  and from each native schema.
- **Structure survives**: tool calls, tool results, and thinking are written
  as native blocks — not flattened prose — so the resumed transcript renders
  and reads like the original.
- **Format quirks handled**:
  - Factory and Claude store tool results inside user messages
    (`tool_result` blocks); Pi/OMP use a separate `toolResult` role. Adapters
    split and re-group at the boundary.
  - Codex uses date-partitioned rollout files, string-encoded function-call
    arguments, and flat function-call outputs.
  - OMP requires a fixed-width 256-byte title slot with `source` in
    `auto|user`, and its renderer dereferences `usage` on every assistant
    message. Both are emitted to spec.
  - Pi/OMP user content is a block array; tool results are attributed back to
    their tool names via the call-id map.
- **Resume is native**: `droid -r <id>`, `pi --session <path>`,
  `omp --resume <path>`, `claude --resume <id>`, `codex resume <id>`, via `exec`.

Conversion is non-destructive: the source file is only read; a new file is
written into the target store.

## Development

```sh
cargo test
cargo clippy --release -- -D warnings
cargo mutants --jobs 2 --timeout 30 -- -- --test-threads=1
```

Writer-loop mutations that alter index increments or loop comparisons are
expected to time out; they create non-terminating loops rather than surviving
behavioral mutants.

## License

MIT
