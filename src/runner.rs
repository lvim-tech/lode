// lode/runner: running somebody's own code — a connection hook, or a script.
//
// Two callers, one implementation, because they are the same act seen from two
// sides: a hook is a script the engine runs at a moment it chose, and a script
// is a hook somebody ran themselves.
//
// WHAT A CHILD GETS, AND WHAT IT DOES NOT. The environment carries CONTEXT — the
// driver, the host, the port, the database, which event this is — and never a
// credential. A hook that needs a password fetches it the same way the engine
// does, from the keyring, under its own name. Putting a resolved secret into a
// child's environment would publish it to `ps`, to `/proc/<pid>/environ`, and to
// every other process the child starts, and the convenience is not worth any of
// that.
//
// LUA IS NOT EMBEDDED, and that is a limitation rather than a decision. An
// in-process Lua would need `mlua`, which is not in this lock file and cannot be
// fetched here, so `lua` is run as an interpreter like `python3` and `bash` are.
// The difference is real and worth knowing: an embedded Lua could be given a
// sandbox and an API onto the open connection, and an interpreter cannot be
// given either. Nothing here pretends otherwise.
//
// Output is streamed as it arrives, with the stream it came from NAMED. Merging
// stdout and stderr loses which, and which is most of what a failing script is
// telling you.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
#[cfg(unix)]
use std::os::unix::process::CommandExt as _;

/// How long a hook or a script may run before it is stopped.
///
/// A hook with no limit is a connection that never opens and never says why,
/// and the default has to be short enough that somebody notices rather than
/// assumes the database is slow.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lang {
    Bash,
    Python,
    Lua,
}

impl Lang {
    pub fn label(self) -> &'static str {
        match self {
            Lang::Bash => "bash",
            Lang::Python => "python",
            Lang::Lua => "lua",
        }
    }

    /// The interpreters to look for, in order. Several per language because a
    /// machine with `lua5.4` and no `lua` is an ordinary machine, and refusing
    /// to run there would be refusing over a symlink.
    pub fn candidates(self) -> &'static [&'static str] {
        match self {
            Lang::Bash => &["bash", "sh"],
            Lang::Python => &["python3", "python"],
            Lang::Lua => &["lua", "lua5.4", "lua5.3", "luajit"],
        }
    }

    /// The first candidate that exists on PATH, or an error NAMING what was
    /// looked for. "lua: not found" sends somebody to install Lua; "could not
    /// run the script" sends them nowhere.
    pub fn interpreter(self) -> anyhow::Result<String> {
        for c in self.candidates() {
            if which(c) {
                return Ok(c.to_string());
            }
        }
        Err(anyhow::anyhow!(
            "no {} interpreter on PATH — looked for {}",
            self.label(),
            self.candidates().join(", ")
        ))
    }
}

/// Is `prog` on PATH? Written out because the alternative is a dependency for
/// four lines.
fn which(prog: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else { return false };
    std::env::split_paths(&path).any(|dir| {
        let p = dir.join(prog);
        // Executable by somebody. A file that is there and not runnable is the
        // same to us as a file that is not there, and saying "not found" about
        // it would send somebody looking in the wrong place — so `metadata`
        // rather than `exists`.
        std::fs::metadata(&p).map(|m| m.is_file()).unwrap_or(false)
    })
}

/// What a run produced.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// `None` when the process was killed — by the timeout, or by a cancel.
    pub code: Option<i32>,
    pub ms: u64,
    /// True when this ended because it ran out of time.
    pub timed_out: bool,
}

impl Outcome {
    /// A run is a success only on exit status zero. Not on "it produced output",
    /// and not on "it did not crash": a hook that fails has to be able to STOP
    /// a connection, and the only thing it can say with is its exit status.
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }

    pub fn why(&self) -> String {
        if self.timed_out {
            return "it ran out of time and was stopped".into();
        }
        match self.code {
            Some(0) => "it succeeded".into(),
            Some(c) => format!("it exited with status {c}"),
            None => "it was stopped before it finished".into(),
        }
    }
}

/// One line of output on its way to a client.
pub struct Line {
    /// `"stdout"` or `"stderr"`, kept apart because which one a message came
    /// from is most of what a failing script is telling you.
    pub stream: &'static str,
    pub text: String,
}

/// Run `body` under `lang`, or `command` through the shell when `lang` is None.
///
/// Every line of output is sent on `out` as it arrives. The future resolves when
/// the process has ended, so a caller that must not proceed until it is done —
/// a PreConnect hook — simply awaits it.
pub async fn run(
    lang: Option<Lang>,
    source: &str,
    env: &BTreeMap<String, String>,
    timeout: Duration,
    out: tokio::sync::mpsc::UnboundedSender<Line>,
) -> anyhow::Result<Outcome> {
    let started = Instant::now();
    let mut cmd = match lang {
        // The body goes in on STDIN, not as an argument and not through a
        // temporary file. An argument has a length limit and would appear in
        // `ps`; a temporary file is a file somebody has to clean up and a path
        // somebody else can read or replace between writing and running.
        Some(l) => {
            let prog = l.interpreter()?;
            let mut c = tokio::process::Command::new(&prog);
            if l == Lang::Lua {
                // Lua's `-` is not universal; `/dev/stdin` is what works on the
                // interpreters this looks for.
                c.arg("/dev/stdin");
            } else {
                c.arg("-");
            }
            c
        }
        None => {
            // A hook declared as a COMMAND is a command line, so it goes through
            // a shell: `pg_dump … | gzip > x` is the kind of thing somebody
            // writes there, and splitting it on spaces ourselves would break it
            // and be a worse shell than the shell.
            let mut c = tokio::process::Command::new("sh");
            c.arg("-c").arg(source);
            c
        }
    };
    cmd.envs(env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // A child that outlives this process is a child nobody will ever stop.
        .kill_on_drop(true);
    // Its OWN process group, so a timeout can stop everything the script
    // started and not merely the interpreter. Without this, `bash -` running
    // `sleep 30` forks: killing bash leaves `sleep` holding the output pipes,
    // and the engine waits the full thirty seconds for a timeout it set at four
    // hundred milliseconds. Found by the test that asserts a timeout is quick.
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("could not start it: {e}"))?;

    if lang.is_some() {
        use tokio::io::AsyncWriteExt;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(source.as_bytes()).await?;
            // Closing stdin is what tells the interpreter the script is over.
            drop(stdin);
        }
    } else {
        drop(child.stdin.take());
    }

    let mut tasks = Vec::new();
    if let Some(o) = child.stdout.take() {
        tasks.push(pump(o, "stdout", out.clone()));
    }
    if let Some(e) = child.stderr.take() {
        tasks.push(pump(e, "stderr", out.clone()));
    }

    let pid = child.id();
    let waited = tokio::time::timeout(timeout, child.wait()).await;
    let (code, timed_out) = match waited {
        Ok(status) => (status?.code(), false),
        Err(_) => {
            kill_group(pid);
            let _ = child.kill().await;
            (None, true)
        }
    };
    // Drain what the pipes still hold BEFORE reporting the end: output written
    // just before exiting is usually the reason for the exit.
    for t in tasks {
        let _ = t.await;
    }
    Ok(Outcome {
        code,
        ms: started.elapsed().as_millis() as u64,
        timed_out,
    })
}

/// Kill the whole process group this child leads.
///
/// A negative pid is the group, which is the point: the script's own children
/// are in it, and they are what hold the output pipes open after the
/// interpreter is gone.
#[cfg(unix)]
fn kill_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        // SAFETY: `kill` with a group id this process created. A failure means
        // the group is already gone, which is the outcome being asked for.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn kill_group(_pid: Option<u32>) {}

fn pump<R>(r: R, stream: &'static str, out: tokio::sync::mpsc::UnboundedSender<Line>) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(r).lines();
        while let Ok(Some(text)) = lines.next_line().await {
            if out.send(Line { stream, text }).is_err() {
                break;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sink() -> (
        tokio::sync::mpsc::UnboundedSender<Line>,
        tokio::sync::mpsc::UnboundedReceiver<Line>,
    ) {
        tokio::sync::mpsc::unbounded_channel()
    }

    async fn collect(mut rx: tokio::sync::mpsc::UnboundedReceiver<Line>) -> Vec<(String, String)> {
        let mut out = Vec::new();
        while let Ok(l) = rx.try_recv() {
            out.push((l.stream.to_string(), l.text));
        }
        // Anything still in flight.
        while let Some(l) = rx.recv().await {
            out.push((l.stream.to_string(), l.text));
        }
        out
    }

    #[tokio::test]
    async fn a_shell_command_runs_and_its_two_streams_stay_apart() {
        let (tx, rx) = sink();
        let o = run(
            None,
            "echo hello; echo trouble >&2",
            &BTreeMap::new(),
            DEFAULT_TIMEOUT,
            tx,
        )
        .await
        .unwrap();
        assert!(o.ok(), "{}", o.why());
        let lines = collect(rx).await;
        assert!(lines.contains(&("stdout".into(), "hello".into())), "{lines:?}");
        assert!(lines.contains(&("stderr".into(), "trouble".into())), "{lines:?}");
    }

    /// A command line goes through a shell, so a pipe is a pipe. Splitting on
    /// spaces ourselves would break this and be a worse shell than the shell.
    #[tokio::test]
    async fn a_command_line_is_a_command_line() {
        let (tx, rx) = sink();
        let o = run(None, "printf 'a\\nb\\n' | wc -l", &BTreeMap::new(), DEFAULT_TIMEOUT, tx)
            .await
            .unwrap();
        assert!(o.ok());
        let lines = collect(rx).await;
        assert_eq!(lines[0].1.trim(), "2");
    }

    /// Exit status is the ONLY thing a hook can stop a connection with, so it
    /// has to survive exactly.
    #[tokio::test]
    async fn a_non_zero_exit_is_a_failure_and_keeps_its_number() {
        let (tx, _rx) = sink();
        let o = run(None, "exit 3", &BTreeMap::new(), DEFAULT_TIMEOUT, tx).await.unwrap();
        assert!(!o.ok());
        assert_eq!(o.code, Some(3));
        assert!(o.why().contains("status 3"), "{}", o.why());
    }

    /// The environment carries CONTEXT. The test that matters is the other
    /// half — see `a_secret_is_not_in_the_environment` in server.rs.
    #[tokio::test]
    async fn the_environment_reaches_the_child() {
        let (tx, rx) = sink();
        let mut env = BTreeMap::new();
        env.insert("LODE_DRIVER".into(), "postgres".into());
        let o = run(None, "echo \"$LODE_DRIVER\"", &env, DEFAULT_TIMEOUT, tx)
            .await
            .unwrap();
        assert!(o.ok());
        assert_eq!(collect(rx).await[0].1, "postgres");
    }

    /// A hook with no limit is a connection that never opens and never says
    /// why. The timeout kills the child and SAYS that is what happened, rather
    /// than reporting a plain failure.
    ///
    /// The `ms` assertion is the one that found a real bug: the first version
    /// killed only the interpreter, so `bash -` running `sleep 30` left `sleep`
    /// holding the output pipes and this took thirty seconds to return a
    /// four-hundred-millisecond timeout.
    #[tokio::test]
    async fn a_run_that_overstays_is_stopped_and_says_so() {
        let (tx, _rx) = sink();
        let o = run(
            None,
            "sleep 30",
            &BTreeMap::new(),
            Duration::from_millis(300),
            tx,
        )
        .await
        .unwrap();
        assert!(!o.ok());
        assert!(o.timed_out);
        assert!(o.why().contains("ran out of time"), "{}", o.why());
        assert!(o.ms < 10_000, "it should have been stopped, not waited for");
    }

    /// And the half that the shell-level test caught: a script that STARTS
    /// something must not outlive its own timeout through its child.
    #[tokio::test]
    async fn a_timeout_stops_what_the_script_started_too() {
        let (tx, rx) = sink();
        let started = Instant::now();
        let o = run(
            Some(Lang::Bash),
            "sleep 30",
            &BTreeMap::new(),
            Duration::from_millis(300),
            tx,
        )
        .await
        .unwrap();
        assert!(o.timed_out);
        // Draining the pipes must not wait for the orphan: the whole group goes.
        let waited = started.elapsed();
        assert!(
            waited < Duration::from_secs(5),
            "took {waited:?} — the script's child was still holding the pipes"
        );
        let _ = collect(rx).await;
    }

    /// Output written just before exiting is usually the reason for the exit,
    /// so the pipes are drained before the end is reported.
    #[tokio::test]
    async fn the_last_thing_written_before_exiting_is_not_lost() {
        let (tx, rx) = sink();
        let o = run(None, "echo final; exit 1", &BTreeMap::new(), DEFAULT_TIMEOUT, tx)
            .await
            .unwrap();
        assert!(!o.ok());
        let lines = collect(rx).await;
        assert!(lines.iter().any(|(_, t)| t == "final"), "{lines:?}");
    }

    /// The body goes in on stdin: not as an argument (length limits, and it
    /// would show in `ps`) and not through a temporary file (a path somebody
    /// else can read or replace between writing and running).
    #[tokio::test]
    async fn a_python_script_runs_from_stdin_when_python_is_there() {
        if Lang::Python.interpreter().is_err() {
            eprintln!("skipped: no python");
            return;
        }
        let (tx, rx) = sink();
        let o = run(
            Some(Lang::Python),
            "import sys\nprint('from python')\nsys.exit(0)\n",
            &BTreeMap::new(),
            DEFAULT_TIMEOUT,
            tx,
        )
        .await
        .unwrap();
        assert!(o.ok(), "{}", o.why());
        assert_eq!(collect(rx).await[0].1, "from python");
    }

    #[tokio::test]
    async fn a_lua_script_runs_when_lua_is_there() {
        if Lang::Lua.interpreter().is_err() {
            eprintln!("skipped: no lua");
            return;
        }
        let (tx, rx) = sink();
        let o = run(
            Some(Lang::Lua),
            "print('from lua')",
            &BTreeMap::new(),
            DEFAULT_TIMEOUT,
            tx,
        )
        .await
        .unwrap();
        assert!(o.ok(), "{}", o.why());
        assert_eq!(collect(rx).await[0].1, "from lua");
    }

    /// A missing interpreter names what was looked for. "lua: not found" sends
    /// somebody to install Lua; "could not run the script" sends them nowhere.
    #[test]
    fn a_missing_interpreter_names_what_was_looked_for() {
        // Nothing is on an empty PATH, so every language is missing.
        let before = std::env::var_os("PATH");
        std::env::set_var("PATH", "/nonexistent-for-this-test");
        let e = Lang::Lua.interpreter().unwrap_err().to_string();
        if let Some(p) = before {
            std::env::set_var("PATH", p);
        }
        assert!(e.contains("lua"), "{e}");
        assert!(e.contains("luajit"), "the whole list, so the fix is obvious: {e}");
    }
}
