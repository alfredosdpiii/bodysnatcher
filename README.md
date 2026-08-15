# bodysnatcher

Resume any AI coding-harness session in any other harness.

Factory Droid (`~/.factory`), Pi (`~/.pi`), and OMP (`~/.omp`) each store
sessions as JSONL with their own schema. bodysnatcher converts a session's
full history — messages, tool calls, tool results, thinking blocks — into the
target harness' native format, writes it into the target's session store, and
replaces itself with the harness' native resume command. The target harness
loads the converted history as if it had written it and continues the
conversation from there.

## Install

```sh
cargo install --git https://github.com/alfredosdpiii/bodysnatcher
```

Requires the Rust toolchain (edition 2024). The harnesses you resume into
must be installed separately (`droid`, `pi`, `omp` on PATH).

## Usage

Run inside any project directory:

```sh
bodysnatcher
```

The TUI lists sessions for the current directory across all three harnesses.
Pick one, pick a target, resume.

| Key | Action |
| --- | --- |
| `j`/`k`, arrows | move |
| `g` / `G` | first / last |
| type | filter by title, cwd, or id |
| `Backspace`, `Ctrl-u` | edit / clear filter |
| `Tab` | cycle target harness |
| `Enter` | convert and resume |
| `q`, `Esc`, `Ctrl-c` | quit |

### Headless conversion

```sh
bodysnatcher convert <session.jsonl> --to factory|pi|omp [--from factory|pi|omp] [--sessions-dir <dir>] [--dry-run] [--run]
```

`--from` is auto-detected from the path. `--sessions-dir` redirects output
(default: the target harness' real store). `--run` resumes immediately after
writing.

### Store overrides

```sh
bodysnatcher --factory-dir <dir> --pi-dir <dir> --omp-dir <dir>
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
  - Factory stores tool results inside user messages (`tool_result` blocks);
    Pi/OMP use a separate `toolResult` role. Adapters split and re-group at
    the boundary.
  - OMP requires a fixed-width 256-byte title slot with `source` in
    `auto|user`, and its renderer dereferences `usage` on every assistant
    message. Both are emitted to spec.
  - Pi/OMP user content is a block array; tool results are attributed back to
    their tool names via the call-id map.
- **Resume is native**: `droid -r <id>`, `pi --session <path>`,
  `omp --resume <path>`, via `exec` — no shims, no proxies.

Conversion is non-destructive: the source file is only read; a new file is
written into the target store.

## Development

```sh
cargo test                  # 49 tests
cargo clippy --release
cargo mutants               # mutation-tested: 0 missed
```

## License

MIT
