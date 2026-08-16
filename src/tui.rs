use crate::model::{Harness, Session, Store, rel_age};
use crate::resume::{Target, build, describe, exec};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, List, ListItem, ListState, Paragraph, Wrap};
use std::path::PathBuf;
use std::time::Duration;

fn harness_color(h: Harness) -> Color {
    match h {
        Harness::Factory => Color::Magenta,
        Harness::Pi => Color::Cyan,
        Harness::Omp => Color::Green,
        Harness::Claude => Color::LightRed,
        Harness::Codex => Color::Blue,
    }
}

fn target_color(t: Target) -> Color {
    match t {
        Target::Auto => Color::Yellow,
        Target::Factory => Color::Magenta,
        Target::Pi => Color::Cyan,
        Target::Omp => Color::Green,
        Target::Claude => Color::LightRed,
        Target::Codex => Color::Blue,
    }
}

/// Key press -> abstract action, pure and unit-testable.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    Quit,
    Up,
    Down,
    First,
    Last,
    CycleTarget,
    PopFilter,
    ClearFilter,
    Push(char),
    Resume,
    None,
}

fn key_action(key: &crossterm::event::KeyEvent) -> Action {
    use Action::*;
    match (key.code, key.modifiers) {
        (KeyCode::Char('c'), KeyModifiers::CONTROL)
        | (KeyCode::Esc, _)
        | (KeyCode::Char('q'), _) => Quit,
        (KeyCode::Up, _) | (KeyCode::Char('k'), _) => Up,
        (KeyCode::Down, _) | (KeyCode::Char('j'), _) => Down,
        (KeyCode::Char('g'), m) if m.is_empty() => First,
        (KeyCode::Char('G'), _) => Last,
        (KeyCode::Tab, _) => CycleTarget,
        (KeyCode::Backspace, _) => PopFilter,
        (KeyCode::Char('u'), KeyModifiers::CONTROL) => ClearFilter,
        (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) => Push(c),
        (KeyCode::Enter, _) => Resume,
        _ => None,
    }
}

fn move_up(i: usize) -> usize {
    i.saturating_sub(1)
}

/// Clamp a down-move to the last visible row. Callers guarantee `len >= 1`.
fn move_down(i: usize, len: usize) -> usize {
    (i + 1).min(len - 1)
}

/// Clamp the selected index to the last visible row; None when nothing visible.
fn clamp_selection(selected: Option<usize>, visible_len: usize) -> Option<usize> {
    if visible_len == 0 {
        None
    } else {
        Some(selected.unwrap_or(0).min(visible_len - 1))
    }
}

/// True when a poll found nothing ready (so we redraw instead of blocking).
fn no_event(ready: bool) -> bool {
    !ready
}

/// True when a key event should be skipped (not a press).
fn skip_non_press(kind: KeyEventKind) -> bool {
    kind != KeyEventKind::Press
}

/// Width for the target column: the rendered line plus one cell of padding.
fn target_col_width(line_width: u16) -> u16 {
    line_width + 1
}
pub fn run(store: &Store, extra_dirs: &[PathBuf]) -> std::io::Result<()> {
    let mut sessions = collect(store, extra_dirs);
    sessions.sort_by_key(|s| std::cmp::Reverse(s.modified.unwrap_or(std::time::UNIX_EPOCH)));
    if sessions.is_empty() {
        eprintln!(
            "bodysnatcher: no sessions found for this directory (scanned ~/.omp, ~/.pi, ~/.factory, ~/.claude, ~/.codex; pass --dir to add more)"
        );
        std::process::exit(1);
    }

    let mut term = ratatui::init();
    let mut list = ListState::default();
    list.select(Some(0));
    let mut filter = String::new();
    let mut target = Target::Auto;
    let mut status = String::new();
    let mut err = false;

    loop {
        let visible = filter_indices(&sessions, &filter);
        list.select(clamp_selection(list.selected(), visible.len()));
        let sel = list
            .selected()
            .and_then(|v| visible.get(v))
            .copied()
            .map(|i| &sessions[i]);

        term.draw(|frame| {
            draw(
                frame, &sessions, &visible, &mut list, sel, &filter, target, &status, err,
            );
        })?;

        if no_event(event::poll(Duration::from_millis(30))?) {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if skip_non_press(key.kind) {
            continue;
        }
        err = false;
        match key_action(&key) {
            Action::Quit => {
                ratatui::restore();
                return Ok(());
            }
            Action::Up => {
                let next = list.selected().map_or(0, move_up);
                list.select(Some(next));
            }
            Action::Down => {
                let next = list.selected().map_or(0, |i| move_down(i, visible.len()));
                list.select(Some(next));
            }
            Action::First => list.select(Some(0)),
            Action::Last => list.select(Some(visible.len().saturating_sub(1))),
            Action::CycleTarget => target = target.next(),
            Action::PopFilter => {
                filter.pop();
            }
            Action::ClearFilter => filter.clear(),
            Action::Push(c) => filter.push(c),
            Action::Resume => {
                let Some(sess) = sel else { continue };
                status = format!("resuming: {} …", describe(sess, target));
                let _ = term.draw(|frame| {
                    draw(
                        frame,
                        &sessions,
                        &visible,
                        &mut list,
                        Some(sess),
                        &filter,
                        target,
                        &status,
                        err,
                    );
                });
                ratatui::restore();
                match build(store, sess, target) {
                    Ok(cmd) => return exec(cmd),
                    Err(e) => {
                        eprintln!("bodysnatcher: {e}");
                        std::process::exit(1);
                    }
                }
            }
            Action::None => {}
        }
    }
}

fn collect(store: &Store, extra: &[PathBuf]) -> Vec<Session> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut out = Vec::new();
    for (h, root) in [
        (Harness::Factory, &store.factory),
        (Harness::Pi, &store.pi),
        (Harness::Omp, &store.omp),
        (Harness::Claude, &store.claude),
        (Harness::Codex, &store.codex),
    ] {
        out.extend(crate::adapters::discover_for_cwd(root, h, &cwd));
    }
    for dir in extra {
        let h = Harness::infer_from_path(dir).unwrap_or(Harness::Omp);
        out.extend(crate::adapters::discover(dir, h));
    }
    out
}

fn filter_indices(sessions: &[Session], filter: &str) -> Vec<usize> {
    let f = filter.to_lowercase();
    sessions
        .iter()
        .enumerate()
        .filter(|(_, s)| {
            f.is_empty()
                || s.title.to_lowercase().contains(&f)
                || s.cwd.to_lowercase().contains(&f)
                || s.id.to_lowercase().contains(&f)
        })
        .map(|(i, _)| i)
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn draw(
    frame: &mut ratatui::Frame,
    sessions: &[Session],
    visible: &[usize],
    list: &mut ListState,
    sel: Option<&Session>,
    filter: &str,
    target: Target,
    status: &str,
    err: bool,
) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(frame.area());

    let header = Line::from(vec![
        Span::styled(
            "bodysnatcher",
            Style::new()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  cross-harness session stealer - factory/pi/omp/claude/codex"),
        Span::styled(
            format!("  {} sessions", sessions.len()),
            Style::new().fg(Color::DarkGray),
        ),
    ]);
    frame.render_widget(Paragraph::new(header), outer[0]);

    let input = Line::from(vec![
        Span::styled(
            " search ",
            Style::new().fg(Color::Black).bg(Color::DarkGray),
        ),
        Span::raw(if filter.is_empty() { "▏" } else { filter }),
        Span::styled("▕", Style::new().fg(Color::DarkGray)),
    ]);
    let target_line = Line::from(vec![
        Span::styled(
            format!(" TARGET: {} ", target.label()),
            Style::new()
                .fg(Color::Black)
                .bg(target_color(target))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" ({})", target.desc()),
            Style::new().fg(Color::DarkGray),
        ),
    ]);
    let tw = target_col_width(target_line.width() as u16);
    let row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(10), Constraint::Length(tw)])
        .split(outer[1]);
    frame.render_widget(Paragraph::new(input), row[0]);
    frame.render_widget(
        Paragraph::new(target_line).alignment(Alignment::Right),
        row[1],
    );

    let mid = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(outer[2]);

    let items: Vec<ListItem> = visible
        .iter()
        .map(|&i| {
            let s = &sessions[i];
            let age = s.modified.map(rel_age).unwrap_or_else(|| "?".into());
            let line = Line::from(vec![
                Span::styled(
                    format!(" {} ", s.harness.label()),
                    Style::new()
                        .fg(Color::Black)
                        .bg(harness_color(s.harness))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::raw(s.title.clone()),
                Span::styled(format!("  {age}"), Style::new().fg(Color::DarkGray)),
            ]);
            ListItem::new(line)
        })
        .collect();
    let list_widget = List::new(items)
        .block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(Color::DarkGray))
                .title(" sessions "),
        )
        .highlight_style(
            Style::new()
                .fg(target_color(target))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▐ ");
    frame.render_stateful_widget(list_widget, mid[0], list);

    let detail = match sel {
        None => Text::raw("no session selected"),
        Some(s) => {
            let mut lines = vec![
                Line::from(vec![
                    Span::styled(
                        format!(" {} ", s.harness.label()),
                        Style::new()
                            .fg(Color::Black)
                            .bg(harness_color(s.harness))
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" "),
                    Span::styled(s.title.clone(), Style::new().add_modifier(Modifier::BOLD)),
                ]),
                Line::raw(""),
            ];
            for (k, v) in [
                ("id", s.id.clone()),
                ("cwd", s.cwd.clone()),
                ("model", s.model.clone()),
                ("msgs", s.msgs.to_string()),
                (
                    "modified",
                    s.modified.map(rel_age).unwrap_or_else(|| "?".into()),
                ),
                ("file", s.path.display().to_string()),
            ] {
                lines.push(Line::from(vec![
                    Span::styled(format!("{k:>9}  "), Style::new().fg(Color::DarkGray)),
                    Span::raw(v),
                ]));
            }
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "- preview -",
                Style::new()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ));
            for l in wrap(&s.preview, mid[1].width.saturating_sub(4) as usize) {
                lines.push(Line::raw(l));
            }
            Text::from(lines)
        }
    };
    let detail_widget = Paragraph::new(detail)
        .block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(Color::DarkGray))
                .title(" detail "),
        )
        .wrap(Wrap { trim: true });
    frame.render_widget(detail_widget, mid[1]);

    let hints = Line::from(vec![
        Span::styled(
            " /jk move ",
            Style::new().fg(Color::Black).bg(Color::DarkGray),
        ),
        Span::styled(
            " enter resume ",
            Style::new().fg(Color::Black).bg(Color::Green),
        ),
        Span::styled(
            " tab target ",
            Style::new().fg(Color::Black).bg(Color::DarkGray),
        ),
        Span::styled(
            " type filter ",
            Style::new().fg(Color::Black).bg(Color::DarkGray),
        ),
        Span::styled(
            " q quit ",
            Style::new().fg(Color::Black).bg(Color::DarkGray),
        ),
    ]);
    let footer = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(10), Constraint::Length(50)])
        .split(outer[3]);
    frame.render_widget(Paragraph::new(hints), footer[0]);

    let status_style = if err {
        Style::new()
            .fg(Color::Black)
            .bg(Color::Red)
            .add_modifier(Modifier::BOLD)
    } else if status.is_empty() {
        Style::new().fg(Color::DarkGray)
    } else {
        Style::new().fg(Color::Black).bg(Color::Yellow)
    };
    let status_line = if status.is_empty() {
        Line::from(vec![
            Span::styled(" resume: ", Style::new().fg(Color::DarkGray)),
            Span::styled(
                sel.map(|s| describe(s, target)).unwrap_or_default(),
                Style::new().fg(Color::DarkGray),
            ),
        ])
    } else {
        Line::from(vec![Span::styled(format!(" {status} "), status_style)])
    };
    frame.render_widget(
        Paragraph::new(status_line).alignment(Alignment::Right),
        footer[1],
    );
}

/// Greedy word wrap to `width` chars.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(10);
    let mut out = Vec::new();
    for para in text.split('\n') {
        if para.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut cur = String::new();
        for word in para.split_whitespace() {
            if !cur.is_empty() && cur.chars().count() + 1 + word.chars().count() > width {
                out.push(std::mem::take(&mut cur));
            }
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.push_str(word);
        }
        if !cur.is_empty() {
            out.push(cur);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use crossterm::event::KeyEvent;

    #[test]
    fn key_actions_map() {
        let k = |code, mods| KeyEvent::new(code, mods);
        let n = KeyModifiers::NONE;
        let ctrl = KeyModifiers::CONTROL;
        assert_eq!(key_action(&k(KeyCode::Char('q'), n)), Action::Quit);
        assert_eq!(key_action(&k(KeyCode::Esc, n)), Action::Quit);
        assert_eq!(key_action(&k(KeyCode::Char('c'), ctrl)), Action::Quit);
        assert_eq!(key_action(&k(KeyCode::Up, n)), Action::Up);
        assert_eq!(key_action(&k(KeyCode::Char('k'), n)), Action::Up);
        assert_eq!(key_action(&k(KeyCode::Down, n)), Action::Down);
        assert_eq!(key_action(&k(KeyCode::Char('j'), n)), Action::Down);
        assert_eq!(key_action(&k(KeyCode::Char('g'), n)), Action::First);
        assert_eq!(key_action(&k(KeyCode::Char('G'), n)), Action::Last);
        assert_eq!(key_action(&k(KeyCode::Tab, n)), Action::CycleTarget);
        assert_eq!(key_action(&k(KeyCode::Backspace, n)), Action::PopFilter);
        assert_eq!(
            key_action(&k(KeyCode::Char('u'), ctrl)),
            Action::ClearFilter
        );
        assert_eq!(key_action(&k(KeyCode::Char('x'), n)), Action::Push('x'));
        assert_eq!(key_action(&k(KeyCode::Enter, n)), Action::Resume);
        // guards: ctrl-g is neither First (guard fails) nor Push (ctrl held)
        assert_eq!(key_action(&k(KeyCode::Char('g'), ctrl)), Action::None);
        assert_eq!(key_action(&k(KeyCode::Char('x'), ctrl)), Action::None);
        assert_eq!(key_action(&k(KeyCode::F(1), n)), Action::None);
    }

    #[test]
    fn move_helpers_clamp() {
        assert_eq!(move_up(0), 0);
        assert_eq!(move_up(3), 2);
        assert_eq!(move_down(0, 5), 1);
        assert_eq!(move_down(4, 5), 4);
        assert_eq!(move_down(0, 1), 0);
    }

    #[test]
    fn clamp_selection_bounds() {
        assert_eq!(clamp_selection(None, 0), None);
        assert_eq!(clamp_selection(Some(3), 0), None);
        assert_eq!(clamp_selection(None, 3), Some(0));
        assert_eq!(clamp_selection(Some(0), 3), Some(0));
        assert_eq!(clamp_selection(Some(2), 3), Some(2));
        assert_eq!(clamp_selection(Some(9), 3), Some(2));
    }

    #[test]
    fn no_event_inverts_poll() {
        assert!(no_event(false));
        assert!(!no_event(true));
    }

    #[test]
    fn skip_non_press_flags_kinds() {
        assert!(!skip_non_press(KeyEventKind::Press));
        assert!(skip_non_press(KeyEventKind::Release));
        assert!(skip_non_press(KeyEventKind::Repeat));
    }

    #[test]
    fn target_col_width_adds_padding() {
        assert_eq!(target_col_width(0), 1);
        assert_eq!(target_col_width(10), 11);
    }

    #[test]
    fn colors_map_per_harness() {
        assert_eq!(harness_color(Harness::Factory), Color::Magenta);
        assert_eq!(harness_color(Harness::Pi), Color::Cyan);
        assert_eq!(harness_color(Harness::Omp), Color::Green);
        assert_eq!(harness_color(Harness::Claude), Color::LightRed);
        assert_eq!(harness_color(Harness::Codex), Color::Blue);
        assert_eq!(target_color(Target::Auto), Color::Yellow);
        assert_eq!(target_color(Target::Factory), Color::Magenta);
        assert_eq!(target_color(Target::Pi), Color::Cyan);
        assert_eq!(target_color(Target::Omp), Color::Green);
        assert_eq!(target_color(Target::Claude), Color::LightRed);
        assert_eq!(target_color(Target::Codex), Color::Blue);
    }

    #[test]
    fn draw_renders_session_title() {
        use ratatui::backend::TestBackend;
        let sess = Session {
            harness: Harness::Factory,
            path: "/x".into(),
            id: "id1".into(),
            title: "Fix auth".into(),
            cwd: "/tmp".into(),
            model: String::new(),
            msgs: 3,
            preview: "p".into(),
            modified: None,
        };
        let mut state = ListState::default();
        state.select(Some(0));
        let backend = TestBackend::new(80, 24);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|f| {
            draw(
                f,
                &[sess],
                &[0],
                &mut state,
                None,
                "",
                Target::Auto,
                "",
                false,
            );
        })
        .unwrap();
        let text: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("Fix auth"));
    }

    #[test]
    fn collect_scans_current_cwd_store_and_extra_dirs() {
        let dir = std::env::temp_dir().join(format!("bs-c-{}", crate::model::uuid()));
        let body = r#"{"type":"session","version":3,"id":"t1","timestamp":"2026-07-01T09:00:00.000Z","cwd":"/x"}
{"type":"message","id":"m1","parentId":null,"timestamp":"2026-07-01T09:00:01.000Z","message":{"role":"user","content":"hello"}}
"#;
        let cwd = std::env::current_dir().unwrap();
        // Store root: one workspace matching the cwd (discovered), one that must be skipped.
        let store_root = dir.join("pi");
        let cwd_ws = store_root.join(crate::model::slug_for(Harness::Pi, &cwd.to_string_lossy()));
        std::fs::create_dir_all(&cwd_ws).unwrap();
        std::fs::write(cwd_ws.join("2026-01-01T00-00-00-000Z_x.jsonl"), body).unwrap();
        let other_ws = store_root.join("--definitely-not-cwd--");
        std::fs::create_dir_all(&other_ws).unwrap();
        std::fs::write(other_ws.join("2026-01-02T00-00-00-000Z_z.jsonl"), body).unwrap();
        // Extra dirs are scanned fully regardless of cwd.
        let extra = dir.join("extra");
        let ews = extra.join("--home-u-x--");
        std::fs::create_dir_all(&ews).unwrap();
        std::fs::write(ews.join("2026-01-03T00-00-00-000Z_y.jsonl"), body).unwrap();

        let store = Store {
            factory: dir.join("f"),
            pi: store_root,
            omp: dir.join("o"),
            claude: dir.join("cl"),
            codex: dir.join("cx"),
        };
        let found = collect(&store, &[extra]);
        // cwd-matching pi session + extra session; the non-matching ws is skipped.
        assert_eq!(found.len(), 2);
        let harnesses: Vec<Harness> = found.iter().map(|s| s.harness).collect();
        assert!(harnesses.contains(&Harness::Pi));
        // non-harness extra dirs default to Omp
        assert!(harnesses.contains(&Harness::Omp));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn wraps_long_lines() {
        // width clamps to 10; a 10-char line fits, an 11-char line wraps
        assert_eq!(
            wrap("one two three four", 9),
            vec!["one two".to_string(), "three four".to_string()]
        );
        assert_eq!(
            wrap("aaaaa bbbbb ccccc", 10),
            vec![
                "aaaaa".to_string(),
                "bbbbb".to_string(),
                "ccccc".to_string()
            ]
        );
        assert_eq!(
            wrap("a\n\nb", 20),
            vec!["a".to_string(), "".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn filter_matches_title_cwd_id() {
        let mk = |title: &str, cwd: &str, id: &str| Session {
            harness: Harness::Omp,
            path: "/tmp/x".into(),
            id: id.into(),
            title: title.into(),
            cwd: cwd.into(),
            model: String::new(),
            msgs: 0,
            preview: String::new(),
            modified: None,
        };
        let sessions = vec![
            mk("Fix auth", "/home/u/app", "aaa"),
            mk("Write parser", "/home/u/lib", "bbb"),
        ];
        assert_eq!(filter_indices(&sessions, ""), vec![0, 1]);
        assert_eq!(filter_indices(&sessions, "auth"), vec![0]);
        assert_eq!(filter_indices(&sessions, "/lib"), vec![1]);
        assert_eq!(filter_indices(&sessions, "bbb"), vec![1]);
        assert_eq!(filter_indices(&sessions, "zzz"), Vec::<usize>::new());
    }
}
