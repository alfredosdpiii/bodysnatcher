use crate::model::{Harness, Session, Store};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;

/// Pick the resume command for a session, converting first if the target
/// harness differs. Auto = native resume of the session's own harness.
pub fn build(store: &Store, sess: &Session, target: Target) -> std::io::Result<Command> {
    let target = match target {
        Target::Auto => sess.harness,
        Target::Factory => Harness::Factory,
        Target::Pi => Harness::Pi,
        Target::Omp => Harness::Omp,
        Target::Claude => Harness::Claude,
        Target::Codex => Harness::Codex,
    };

    // Converted session: write into target store, resume that instead.
    let (path, id) = if target == sess.harness {
        (sess.path.clone(), sess.id.clone())
    } else {
        let (_, msgs) = crate::adapters::parse(&sess.path, sess.harness)?;
        let out = crate::adapters::write_session(store, target, sess, &msgs)?;
        let out_id = crate::adapters::summarize(&out, target)
            .map(|s| s.id)
            .unwrap_or_default();
        (out, out_id)
    };

    Ok(match target {
        Harness::Factory => Command {
            bin: "droid".into(),
            args: vec!["-r".into(), id.into()],
        },
        Harness::Pi => Command {
            bin: "pi".into(),
            args: vec!["--session".into(), path],
        },
        Harness::Omp => Command {
            bin: "omp".into(),
            args: vec!["--resume".into(), path],
        },
        Harness::Claude => Command {
            bin: "claude".into(),
            args: vec!["--resume".into(), id.into()],
        },
        Harness::Codex => Command {
            bin: "codex".into(),
            args: vec!["resume".into(), id.into()],
        },
    })
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    Auto,
    Factory,
    Pi,
    Omp,
    Claude,
    Codex,
}

impl Target {
    pub fn next(self) -> Self {
        match self {
            Target::Auto => Target::Codex,
            Target::Codex => Target::Claude,
            Target::Claude => Target::Omp,
            Target::Omp => Target::Pi,
            Target::Pi => Target::Factory,
            Target::Factory => Target::Auto,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Target::Auto => "AUTO",
            Target::Factory => "FAC",
            Target::Pi => "PI",
            Target::Omp => "OMP",
            Target::Claude => "CL",
            Target::Codex => "CX",
        }
    }

    pub fn desc(self) -> &'static str {
        match self {
            Target::Auto => "native harness",
            Target::Factory => "Factory Droid",
            Target::Pi => "Pi",
            Target::Omp => "OMP",
            Target::Claude => "Claude Code",
            Target::Codex => "Codex",
        }
    }
}

pub struct Command {
    pub bin: String,
    pub args: Vec<PathBuf>,
}

/// Replace this process with the target harness. Never returns on success.
pub fn exec(cmd: Command) -> std::io::Result<()> {
    let err = std::process::Command::new(&cmd.bin).args(&cmd.args).exec();
    Err(err)
}

/// Human-readable summary of what resume does.
pub fn describe(sess: &Session, target: Target) -> String {
    let t = match target {
        Target::Auto => sess.harness,
        Target::Factory => Harness::Factory,
        Target::Pi => Harness::Pi,
        Target::Omp => Harness::Omp,
        Target::Claude => Harness::Claude,
        Target::Codex => Harness::Codex,
    };

    if t == sess.harness {
        format!(
            "{} -> {} (native)",
            sess.harness.full(),
            match t {
                Harness::Factory => "droid -r",
                Harness::Pi => "pi --session",
                Harness::Omp => "omp --resume",
                Harness::Claude => "claude --resume",
                Harness::Codex => "codex resume",
            }
        )
    } else {
        format!("{} -> {} (convert)", sess.harness.full(), t.full())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn target_cycles() {
        assert_eq!(Target::Auto.next(), Target::Codex);
        assert_eq!(Target::Codex.next(), Target::Claude);
        assert_eq!(Target::Claude.next(), Target::Omp);
        assert_eq!(Target::Factory.next(), Target::Auto);
        assert_eq!(Target::Omp.label(), "OMP");
        assert_eq!(Target::Factory.label(), "FAC");
        assert_eq!(Target::Claude.label(), "CL");
        assert_eq!(Target::Codex.label(), "CX");
        assert_eq!(Target::Omp.desc(), "OMP");
        assert_eq!(Target::Factory.desc(), "Factory Droid");
        assert_eq!(Target::Claude.desc(), "Claude Code");
        assert_eq!(Target::Codex.desc(), "Codex");
        assert_eq!(Target::Auto.desc(), "native harness");
    }

    #[test]
    fn exec_nonexistent_binary_is_err() {
        let res = exec(Command {
            bin: "/definitely/not/a/real/binary".into(),
            args: vec![],
        });
        assert!(res.is_err());
    }
    #[test]
    fn build_native_keeps_path() {
        let dir = std::env::temp_dir().join(format!("bs-r-{}", crate::model::uuid()));
        fs::create_dir_all(&dir).unwrap();
        let sess = Session {
            harness: Harness::Omp,
            path: dir.join("s.jsonl"),
            id: "x".into(),
            title: "t".into(),
            cwd: "/tmp".into(),
            model: String::new(),
            msgs: 0,
            preview: String::new(),
            modified: None,
        };
        let store = Store {
            factory: dir.join("fac"),
            pi: dir.join("pi"),
            omp: dir.join("omp"),
            claude: dir.join("claude"),
            codex: dir.join("codex"),
        };
        let cmd = build(&store, &sess, Target::Auto).unwrap();
        assert_eq!(cmd.bin, "omp");
        assert_eq!(
            cmd.args,
            vec![PathBuf::from("--resume"), dir.join("s.jsonl")]
        );
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn build_native_id_resume_commands() {
        let dir = std::env::temp_dir().join(format!("bs-r-{}", crate::model::uuid()));
        let store = Store {
            factory: dir.join("fac"),
            pi: dir.join("pi"),
            omp: dir.join("omp"),
            claude: dir.join("claude"),
            codex: dir.join("codex"),
        };
        for (harness, target, bin, args) in [
            (
                Harness::Claude,
                Target::Claude,
                "claude",
                vec![PathBuf::from("--resume"), PathBuf::from("session-id")],
            ),
            (
                Harness::Codex,
                Target::Codex,
                "codex",
                vec![PathBuf::from("resume"), PathBuf::from("session-id")],
            ),
        ] {
            let sess = Session {
                harness,
                path: dir.join("s.jsonl"),
                id: "session-id".into(),
                title: "t".into(),
                cwd: "/tmp".into(),
                model: String::new(),
                msgs: 0,
                preview: String::new(),
                modified: None,
            };
            let cmd = build(&store, &sess, target).unwrap();
            assert_eq!(cmd.bin, bin);
            assert_eq!(cmd.args, args);
        }
    }

    #[test]
    fn describe_names_conversion() {
        let sess = Session {
            harness: Harness::Factory,
            path: "/tmp/x.jsonl".into(),
            id: "x".into(),
            title: "t".into(),
            cwd: "/tmp".into(),
            model: String::new(),
            msgs: 0,
            preview: String::new(),
            modified: None,
        };
        assert_eq!(describe(&sess, Target::Omp), "factory -> omp (convert)");
        assert_eq!(
            describe(&sess, Target::Auto),
            "factory -> droid -r (native)"
        );
    }
}
