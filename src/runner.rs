//! Where `git` runs: on this machine ([`Local`]) or on another one over `ssh` ([`Ssh`]). A [`Repo`] is a work tree on one of them; every
//! operation of the plugin goes through its runner, so the marks and the operations work the same on a server.
//!
//! A command that may need a secret (a push to a server, an SSH key with a passphrase) is cut off from the terminal (`setsid`), so
//! nothing can write into the tool's screen, and asks through `GIT_ASKPASS` / `SSH_ASKPASS`: a one-line script that calls
//! `lvim-fuzzy-plugin-git askpass`, which asks with the prompt the machine has (see `lvim_fuzzy_plugin::askpass`).

use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use lvim_fuzzy_plugin::askpass::sh_quote;

/// What a command did.
pub struct Output {
    pub ok: bool,
    pub stdout: Vec<u8>,
    pub stderr: String,
    pub code: Option<i32>,
}

/// Something that runs `git` (and a shell line) in a directory.
pub trait Runner: Send + Sync {
    /// `git args…` in `cwd`; `input` goes to its stdin; `reading`: it only looks (it must not take the index lock); `net`: it may ask for a secret.
    fn git(&self, cwd: &str, args: &[&str], input: Option<&[u8]>, reading: bool, net: bool) -> Result<Output, String>;
    /// A shell line in `cwd`.
    fn shell(&self, cwd: &str, script: &str) -> Result<Output, String>;
    /// Where it runs, for a message (`this machine`, `host`).
    fn place(&self) -> String;
}

/// A work tree: its top, and the runner that reaches it.
#[derive(Clone)]
pub struct Repo {
    pub root: String,
    pub run: Arc<dyn Runner>,
}

impl std::fmt::Debug for Repo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Repo({} on {})", self.root, self.run.place())
    }
}

impl Repo {
    /// A work tree on this machine.
    pub fn local(root: &Path) -> Repo {
        Repo { root: root.to_string_lossy().into_owned(), run: Arc::new(Local) }
    }

    /// A work tree on another machine, reached with `ssh`.
    pub fn ssh(target: SshTarget, root: &str) -> Repo {
        Repo { root: root.to_string(), run: Arc::new(Ssh(target)) }
    }

    pub fn path(&self) -> PathBuf {
        PathBuf::from(&self.root)
    }

    pub fn is_remote(&self) -> bool {
        self.run.place() != "this machine"
    }

    /// The same machine, another directory.
    pub fn at(&self, root: &str) -> Repo {
        Repo { root: root.to_string(), run: self.run.clone() }
    }
}

fn finish(o: std::process::Output) -> Output {
    Output { ok: o.status.success(), code: o.status.code(), stdout: o.stdout, stderr: String::from_utf8_lossy(&o.stderr).into_owned() }
}

/// The directory the askpass script and the ssh control sockets live in.
pub fn cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache"))
        .join("lvim-fuzzy-git")
}

/// The environment that makes `git` / `ssh` ask through the machine's prompt: a script that calls this program as the git plugin's askpass.
pub fn askpass_env() -> Vec<(String, String)> {
    let Ok(exe) = std::env::current_exe() else { return Vec::new() };
    let dir = cache_dir();
    let _ = std::fs::create_dir_all(&dir);
    let script = dir.join("askpass.sh");
    let body = format!("#!/bin/sh\nexec {} askpass \"$@\"\n", sh_quote(&exe.to_string_lossy()));
    if std::fs::read_to_string(&script).ok().as_deref() != Some(body.as_str()) {
        let tmp = dir.join(format!("askpass.sh.{}", std::process::id()));
        if std::fs::write(&tmp, &body).is_ok() {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o700));
            let _ = std::fs::rename(&tmp, &script);
        }
    }
    let s = script.to_string_lossy().into_owned();
    vec![
        ("GIT_ASKPASS".into(), s.clone()),
        ("SSH_ASKPASS".into(), s),
        ("SSH_ASKPASS_REQUIRE".into(), "force".into()),
        ("DISPLAY".into(), std::env::var("DISPLAY").ok().filter(|d| !d.is_empty()).unwrap_or_else(|| ":0".into())),
    ]
}

/// Cut a command off from the controlling terminal (a prompt could not then be written into the tool's screen).
fn detach(c: &mut Command) {
    // SAFETY: setsid is async-signal-safe and touches nothing of this process
    unsafe {
        c.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
}

fn run(mut c: Command, input: Option<&[u8]>) -> Result<Output, String> {
    c.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn().map_err(|e| format!("{e}"))?;
    if let Some(data) = input
        && let Some(mut w) = child.stdin.take()
    {
        let _ = w.write_all(data);
    }
    child.wait_with_output().map(finish).map_err(|e| e.to_string())
}

/// `git` on this machine.
pub struct Local;

impl Runner for Local {
    fn git(&self, cwd: &str, args: &[&str], input: Option<&[u8]>, reading: bool, net: bool) -> Result<Output, String> {
        let mut c = Command::new("git");
        c.arg("-c").arg("core.quotepath=off").arg("-C").arg(cwd).args(args);
        // nothing waits for a person: no prompts of git's own, no editor
        c.env("GIT_TERMINAL_PROMPT", "0").env("GIT_EDITOR", "true").env("GIT_SEQUENCE_EDITOR", "true");
        if reading {
            // looking must not take the index lock: it would make a commit in another terminal fail
            c.env("GIT_OPTIONAL_LOCKS", "0");
        }
        if net {
            c.envs(askpass_env());
            detach(&mut c);
        }
        run(c, input).map_err(|e| format!("git: {e}"))
    }

    fn shell(&self, cwd: &str, script: &str) -> Result<Output, String> {
        let mut c = Command::new("sh");
        c.arg("-c").arg(script).current_dir(cwd);
        run(c, None)
    }

    fn place(&self) -> String {
        "this machine".into()
    }
}

/// A server reached with `ssh`: the host (a name `ssh` knows, or a host name) and what the connection says besides.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshTarget {
    pub host: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub key: Option<String>,
}

/// `git` on a server, over `ssh` (one connection kept for a while by ControlMaster, so each call is quick).
pub struct Ssh(pub SshTarget);

impl Ssh {
    /// `prompt`: the command may ask for a secret (an operation somebody asked for). A command that only looks — the marks, read in the
    /// background — never does: it uses what is there (the ssh-agent, a connection kept open) and fails quietly when that is not enough.
    fn command(&self, remote_line: &str, prompt: bool) -> Command {
        let t = &self.0;
        let dir = cache_dir();
        let _ = std::fs::create_dir_all(&dir);
        // `LVIM_GIT_SSH`: another program than `ssh` (a wrapper, or a stand-in in a test)
        let mut c = Command::new(std::env::var("LVIM_GIT_SSH").ok().filter(|p| !p.is_empty()).unwrap_or_else(|| "ssh".into()));
        c.arg("-o").arg("ControlMaster=auto").arg("-o").arg(format!("ControlPath={}/ctl-%C", dir.display())).arg("-o").arg("ControlPersist=120").arg("-o").arg("ConnectTimeout=20");
        if let Some(p) = t.port {
            c.arg("-p").arg(p.to_string());
        }
        if let Some(k) = &t.key {
            let k = match k.strip_prefix("~/") {
                Some(r) => format!("{}/{r}", std::env::var("HOME").unwrap_or_default()),
                None => k.clone(),
            };
            c.arg("-i").arg(k);
        }
        if !prompt {
            c.arg("-o").arg("BatchMode=yes");
        }
        c.arg(match &t.user {
            Some(u) => format!("{u}@{}", t.host),
            None => t.host.clone(),
        });
        c.arg(remote_line);
        // a passphrase or a password is asked through the machine's prompt, never on the terminal
        if prompt {
            c.envs(askpass_env());
        }
        detach(&mut c);
        c
    }
}

impl Runner for Ssh {
    fn git(&self, cwd: &str, args: &[&str], input: Option<&[u8]>, reading: bool, _net: bool) -> Result<Output, String> {
        let prompt = !reading;
        let quoted: Vec<String> = args.iter().map(|a| sh_quote(a)).collect();
        let opt = if reading { "GIT_OPTIONAL_LOCKS=0 " } else { "" };
        let line = format!("cd {} && GIT_TERMINAL_PROMPT=0 GIT_EDITOR=true GIT_SEQUENCE_EDITOR=true {opt}git -c core.quotepath=off {}", sh_quote(cwd), quoted.join(" "));
        run(self.command(&line, prompt), input).map_err(|e| format!("ssh {}: {e}", self.0.host))
    }

    fn shell(&self, cwd: &str, script: &str) -> Result<Output, String> {
        // a shell line is a look at the repository too (`state`, `ignore` asks for a prompt only as part of an operation, and one is open by then)
        run(self.command(&format!("cd {} && {script}", sh_quote(cwd)), false), None).map_err(|e| format!("ssh {}: {e}", self.0.host))
    }

    fn place(&self) -> String {
        self.0.host.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ops, status};

    #[test]
    fn a_server_is_reached_with_ssh_and_the_same_operations_work_there() {
        let d = std::env::temp_dir().join(format!("lvim-git-ssh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let work = d.join("work");
        std::fs::create_dir_all(&work).unwrap();
        // a stand-in for ssh: it runs the last argument (the line for the server) with sh, here
        let fake = d.join("ssh");
        std::fs::write(&fake, "#!/bin/sh\nfor a; do last=$a; done\nexec sh -c \"$last\"\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // SAFETY: only this test reads LVIM_GIT_SSH
        unsafe { std::env::set_var("LVIM_GIT_SSH", &fake) };
        let g = |a: &[&str]| assert!(Command::new("git").arg("-C").arg(&work).args(a).output().unwrap().status.success());
        g(&["init", "-q", "-b", "main"]);
        g(&["config", "user.email", "t@example.org"]);
        g(&["config", "user.name", "T"]);
        g(&["config", "commit.gpgsign", "false"]);
        std::fs::write(work.join("a with space.txt"), "one\n").unwrap();

        let run: Arc<dyn Runner> = Arc::new(Ssh(SshTarget { host: "server".into(), user: Some("me".into()), port: Some(2222), key: None }));
        let root = work.canonicalize().unwrap().to_string_lossy().into_owned();
        let st = status::read_in(run.clone(), &root, true, "normal").unwrap().unwrap();
        assert_eq!(st.mark_of("a with space.txt", false).unwrap().kind, status::Kind::Untracked);
        let r = st.repo.clone().unwrap();
        assert!(r.is_remote());
        // the marks of a folder on a server: its path is only text
        assert_eq!(st.base(Path::new(&root)).as_deref(), Some(""));
        ops::stage(&r, &["a with space.txt".into()]).unwrap();
        ops::commit(&r, "it's a message", false).unwrap();
        assert_eq!(ops::log(&r, None, 5).unwrap()[0].subject, "it's a message");
        ops::ignore(&r, "out", true).unwrap();
        assert_eq!(std::fs::read_to_string(work.join(".gitignore")).unwrap(), "/out/\n");
        // stdin goes through too (a hunk)
        std::fs::write(work.join("a with space.txt"), "one\ntwo\n").unwrap();
        let text = ops::diff(&r, Some("a with space.txt"), false, false).unwrap();
        let lines: Vec<String> = text.lines().map(str::to_string).collect();
        let hs = ops::hunks(&lines);
        ops::apply_hunk(&r, &ops::hunk_patch(&lines, &hs, 0).unwrap(), ops::HunkDo::Stage).unwrap();
        assert!(ops::diff(&r, Some("a with space.txt"), true, false).unwrap().contains("+two"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
