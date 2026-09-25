mod adapters;
mod compact;
mod model;
mod resume;
mod tui;

use clap::{Parser, Subcommand};
use compact::Policy;
use model::{Harness, Session, Store};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "bodysnatcher",
    version,
    about = "Resume any AI coding-harness session in any other harness (Factory Droid, Pi, OMP, Claude Code, Codex)",
    long_about = None
)]
struct Cli {
    /// Extra session directories to scan (harness auto-detected from path)
    #[arg(short = 'd', long = "dir")]
    dirs: Vec<PathBuf>,

    /// Override the Factory Droid sessions dir (default ~/.factory/sessions)
    #[arg(long)]
    factory_dir: Option<PathBuf>,

    /// Override the Pi sessions dir (default ~/.pi/agent/sessions)
    #[arg(long)]
    pi_dir: Option<PathBuf>,

    /// Override the OMP sessions dir (default ~/.omp/agent/sessions)
    #[arg(long)]
    omp_dir: Option<PathBuf>,
    /// Override the Claude Code projects dir (default ~/.claude/projects)
    #[arg(long)]
    claude_dir: Option<PathBuf>,

    /// Override the Codex sessions dir (default ~/.codex/sessions)
    #[arg(long)]
    codex_dir: Option<PathBuf>,

    /// Target model's context window in tokens (default: detected from the
    /// target harness' config). Sessions that fit are never compacted.
    #[arg(long, global = true)]
    context_window: Option<usize>,

    /// Token size to compact oversized sessions down to (default 200000,
    /// capped at 60% of the context window)
    #[arg(long, global = true)]
    budget: Option<usize>,

    /// Never compact, even when the session exceeds the context window
    #[arg(long, global = true)]
    no_compact: bool,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Headless: convert one session file into another harness' format
    Convert {
        /// Source session file (.jsonl)
        file: PathBuf,

        /// Target harness
        #[arg(long)]
        to: Harness,

        /// Source harness (default: detected from the path)
        #[arg(long)]
        from: Option<Harness>,

        /// Target store root; where the converted session is written
        #[arg(long)]
        sessions_dir: Option<PathBuf>,

        /// Only print where the session would be written; write nothing
        #[arg(long)]
        dry_run: bool,

        /// Resume the converted session in the target harness after writing
        #[arg(long)]
        run: bool,
    },
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let mut store = Store::new();
    if let Some(d) = cli.factory_dir {
        store.factory = d;
    }
    if let Some(d) = cli.pi_dir {
        store.pi = d;
    }
    if let Some(d) = cli.omp_dir {
        store.omp = d;
    }
    if let Some(d) = cli.claude_dir {
        store.claude = d;
    }
    if let Some(d) = cli.codex_dir {
        store.codex = d;
    }

    let policy = Policy {
        window: cli.context_window,
        budget: cli.budget,
        disabled: cli.no_compact,
    };

    let res = match cli.cmd {
        None => tui::run(&store, &cli.dirs, policy),
        Some(Cmd::Convert {
            file,
            to,
            from,
            sessions_dir,
            dry_run,
            run,
        }) => convert(&store, &file, to, from, sessions_dir, dry_run, run, policy),
    };

    match res {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("bodysnatcher: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn convert(
    store: &Store,
    file: &Path,
    to: Harness,
    from: Option<Harness>,
    sessions_dir: Option<PathBuf>,
    dry_run: bool,
    run: bool,
    policy: Policy,
) -> std::io::Result<()> {
    let from = from
        .or_else(|| Harness::infer_from_path(file))
        .ok_or_else(|| {
            std::io::Error::other(
                "cannot detect source harness; pass --from factory|pi|omp|claude|codex",
            )
        })?;

    let mut target_store = store.clone();
    if let Some(d) = sessions_dir {
        match to {
            Harness::Factory => target_store.factory = d,
            Harness::Pi => target_store.pi = d,
            Harness::Omp => target_store.omp = d,
            Harness::Claude => target_store.claude = d,
            Harness::Codex => target_store.codex = d,
        }
    }

    let (sess, mut msgs) = adapters::parse(file, from)?;
    if let Some(lim) = policy.limits(to)
        && let Some(c) = compact::compact(&msgs, lim)
    {
        compact::report(&msgs, &c, lim, to);
        msgs = c;
    }
    if dry_run {
        let (out, _) = adapters::session_path(&target_store, to, &sess);
        println!("{}", out.display());
        eprintln!(
            "bodysnatcher: dry run; would write {} session \"{}\" ({} msgs) -> {}",
            from.full(),
            sess.title,
            msgs.len(),
            out.display()
        );
        return Ok(());
    }
    let out = adapters::write_session(&target_store, to, &sess, &msgs)?;
    println!("{}", out.display());
    eprintln!(
        "bodysnatcher: {} session \"{}\" ({} msgs) -> {}",
        from.full(),
        sess.title,
        msgs.len(),
        out.display()
    );

    if run {
        let converted = adapters::summarize(&out, to).unwrap_or(Session {
            harness: to,
            path: out,
            id: String::new(),
            title: sess.title,
            cwd: sess.cwd,
            model: sess.model,
            msgs: msgs.len(),
            preview: sess.preview,
            modified: None,
        });
        let cmd = resume::build(
            &target_store,
            &converted,
            match to {
                Harness::Factory => resume::Target::Factory,
                Harness::Pi => resume::Target::Pi,
                Harness::Omp => resume::Target::Omp,
                Harness::Claude => resume::Target::Claude,
                Harness::Codex => resume::Target::Codex,
            },
            // already compacted above if it needed to be
            Policy {
                disabled: true,
                ..policy
            },
        )?;
        return resume::exec(cmd);
    }
    Ok(())
}
