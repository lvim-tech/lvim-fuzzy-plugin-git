//! The operations: each one runs the `git` program in the work tree (on this machine or on a server, whatever the [`Repo`]'s runner is) and
//! gives back its text, or its error message (what git printed on stderr). Nothing waits for a person: a prompt fails, an editor is `true`,
//! and a secret is asked for through the machine's prompt (see [`crate::runner`]).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use lvim_fuzzy_plugin::askpass::sh_quote;

pub use crate::runner::{Local, Repo, Runner, SshTarget};

fn fail_text(args: &[&str], o: &crate::runner::Output) -> String {
    let mut m = o.stderr.trim().to_string();
    if m.is_empty() {
        m = String::from_utf8_lossy(&o.stdout).trim().to_string();
    }
    if m.is_empty() { format!("git {} failed ({})", args.first().copied().unwrap_or(""), o.code.map_or("signal".into(), |c| c.to_string())) } else { m }
}

/// What `git` printed on stdout, as bytes; `Err`: its message when it failed. `reading`: a call that only looks; `net`: it may ask for a secret.
pub fn run_bytes(r: &Repo, args: &[&str], input: Option<&[u8]>, reading: bool, net: bool) -> Result<Vec<u8>, String> {
    let o = r.run.git(&r.root, args, input, reading, net)?;
    if o.ok { Ok(o.stdout) } else { Err(fail_text(args, &o)) }
}

/// What `git` printed on stdout, as text.
pub fn git(r: &Repo, args: &[&str]) -> Result<String, String> {
    run_bytes(r, args, None, false, false).map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// The same for a command that talks to a server (fetch, pull, push …).
pub fn git_net(r: &Repo, args: &[&str]) -> Result<String, String> {
    run_bytes(r, args, None, false, true).map(|b| String::from_utf8_lossy(&b).into_owned())
}

pub fn reading(r: &Repo, args: &[&str]) -> Result<String, String> {
    run_bytes(r, args, None, true, false).map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// The top of the work tree `dir` is in, asked of `run` (`None`: it is not in one, or there is no git).
pub fn root_in(run: &Arc<dyn Runner>, dir: &str) -> Option<String> {
    let o = run.git(dir, &["rev-parse", "--show-toplevel"], None, true, false).ok()?;
    if !o.ok {
        return None;
    }
    let t = String::from_utf8_lossy(&o.stdout).trim().to_string();
    (!t.is_empty()).then_some(t)
}

/// The top of the work tree `dir` (on this machine) is in.
pub fn root_of(dir: &Path) -> Option<PathBuf> {
    let run: Arc<dyn Runner> = Arc::new(Local);
    root_in(&run, &dir.to_string_lossy()).map(PathBuf::from)
}

/// Does the repository have a commit yet?
pub fn has_head(r: &Repo) -> bool {
    run_bytes(r, &["rev-parse", "--verify", "-q", "HEAD"], None, true, false).is_ok()
}

fn with_paths<'a>(head: &[&'a str], paths: &'a [String]) -> Vec<&'a str> {
    let mut v: Vec<&str> = head.to_vec();
    v.push("--");
    v.extend(paths.iter().map(String::as_str));
    v
}

/// Put the paths into the index (new, changed and deleted ones).
pub fn stage(r: &Repo, paths: &[String]) -> Result<(), String> {
    git(r, &with_paths(&["add", "-A"], paths)).map(drop)
}

/// Take the paths out of the index (their changes stay in the work tree).
pub fn unstage(r: &Repo, paths: &[String]) -> Result<(), String> {
    if has_head(r) { git(r, &with_paths(&["restore", "--staged"], paths)).map(drop) } else { git(r, &with_paths(&["rm", "--cached", "-r", "-q"], paths)).map(drop) }
}

/// Give up the changes of the work tree: tracked paths go back to the index, `untracked` ones are deleted.
pub fn discard(r: &Repo, tracked: &[String], untracked: &[String]) -> Result<(), String> {
    if !tracked.is_empty() {
        git(r, &with_paths(&["restore"], tracked))?;
    }
    if !untracked.is_empty() {
        git(r, &with_paths(&["clean", "-f", "-d", "-q"], untracked))?;
    }
    Ok(())
}

pub fn stage_all(r: &Repo) -> Result<(), String> {
    git(r, &["add", "-A"]).map(drop)
}

pub fn unstage_all(r: &Repo) -> Result<(), String> {
    if has_head(r) { git(r, &["restore", "--staged", "."]).map(drop) } else { git(r, &["rm", "--cached", "-r", "-q", "."]).map(drop) }
}

/// Commit the index; `amend`: replace the last commit (an empty message keeps its message).
pub fn commit(r: &Repo, message: &str, amend: bool) -> Result<String, String> {
    let mut a = vec!["commit"];
    if amend {
        a.push("--amend");
    }
    if message.trim().is_empty() {
        if !amend {
            return Err("an empty commit message".into());
        }
        a.push("--no-edit");
    } else {
        a.push("-m");
        a.push(message);
    }
    git(r, &a).map(|s| s.lines().next().unwrap_or("").to_string())
}

/// The message of the last commit (to start an amend from).
pub fn last_message(r: &Repo) -> String {
    reading(r, &["log", "-1", "--format=%B"]).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// One commit of the log.
#[derive(Clone, Debug, PartialEq)]
pub struct Commit {
    pub hash: String,
    pub short: String,
    pub author: String,
    /// `2026-11-22`
    pub date: String,
    /// `3 days ago`
    pub ago: String,
    pub subject: String,
}

/// The last `n` commits (of `path` only, when given).
pub fn log(r: &Repo, path: Option<&str>, n: usize) -> Result<Vec<Commit>, String> {
    let limit = format!("-n{n}");
    let mut a = vec!["log", &limit, "--date=short", "--format=%H%x1f%h%x1f%an%x1f%ad%x1f%ar%x1f%s%x1e"];
    if let Some(p) = path {
        a.push("--");
        a.push(p);
    }
    // a repository without a commit has no log: that is an empty list
    let out = match reading(r, &a) {
        Ok(o) => o,
        Err(_) if !has_head(r) => String::new(),
        Err(e) => return Err(e),
    };
    Ok(out
        .split('\x1e')
        .filter_map(|rec| {
            let f: Vec<&str> = rec.trim_start_matches('\n').split('\x1f').collect();
            (f.len() == 6).then(|| Commit {
                hash: f[0].to_string(),
                short: f[1].to_string(),
                author: f[2].to_string(),
                date: f[3].to_string(),
                ago: f[4].to_string(),
                subject: f[5].to_string(),
            })
        })
        .collect())
}

/// A commit with what it changed (`git show`).
pub fn show(r: &Repo, rev: &str) -> Result<String, String> {
    reading(r, &["show", "--stat", "--patch", "--no-color", rev])
}

/// Who wrote each line of a file, and when.
pub fn blame(r: &Repo, path: &str) -> Result<String, String> {
    reading(r, &["blame", "--date=short", "--", path])
}

/// The difference of `path` (all of the work tree when `None`): from the index to the work tree, or — `staged` — from the last commit
/// to the index. An untracked file is shown as all new.
pub fn diff(r: &Repo, path: Option<&str>, staged: bool, untracked: bool) -> Result<String, String> {
    if untracked {
        let Some(p) = path else { return Ok(String::new()) };
        // exit status 1 means "there is a difference": that is the answer here
        let o = r.run.git(&r.root, &["diff", "--no-index", "--no-color", "--", "/dev/null", p], None, true, false)?;
        return Ok(String::from_utf8_lossy(&o.stdout).into_owned());
    }
    let mut a = vec!["diff", "--no-color"];
    if staged {
        a.push("--cached");
    }
    if let Some(p) = path {
        a.push("--");
        a.push(p);
    }
    reading(r, &a)
}

// ----- hunks (`git add -p`) -----

/// One hunk of a diff: the lines `start..end` of the diff's lines (its `@@` line first).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub start: usize,
    pub end: usize,
}

/// The hunks of a diff text (of one file or several): where each `@@` block is. The lines before a file's first `@@` are its header.
pub fn hunks(lines: &[String]) -> Vec<Hunk> {
    let starts: Vec<usize> = lines.iter().enumerate().filter(|(_, l)| l.starts_with("@@")).map(|(i, _)| i).collect();
    starts
        .iter()
        .enumerate()
        .map(|(k, &s)| {
            let limit = starts.get(k + 1).copied().unwrap_or(lines.len());
            // the next file's header ends this hunk, too
            let end = (s + 1..limit).find(|&i| lines[i].starts_with("diff --git")).unwrap_or(limit);
            Hunk { start: s, end }
        })
        .collect()
}

/// The patch of one hunk: the file's header lines (everything before the file's first `@@`) and the hunk's own lines.
pub fn hunk_patch(lines: &[String], hunks: &[Hunk], idx: usize) -> Option<String> {
    let h = hunks.get(idx)?;
    // the header of this file: from the last `diff --git` line before the hunk to the hunk's first `@@` of that file
    let file_start = (0..=h.start).rev().find(|&i| lines[i].starts_with("diff --git"))?;
    let first_at = (file_start..lines.len()).find(|&i| lines[i].starts_with("@@"))?;
    let mut p = String::new();
    for l in &lines[file_start..first_at] {
        p.push_str(l);
        p.push('\n');
    }
    for l in &lines[h.start..h.end] {
        p.push_str(l);
        p.push('\n');
    }
    Some(p)
}

/// What to do with a hunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HunkDo {
    /// into the index (from a work-tree diff)
    Stage,
    /// out of the index (from a staged diff)
    Unstage,
    /// give up its change in the work tree (from a work-tree diff)
    Discard,
}

/// Apply a hunk's patch (see [`hunk_patch`]).
pub fn apply_hunk(r: &Repo, patch: &str, what: HunkDo) -> Result<(), String> {
    let mut a = vec!["apply", "--recount", "--whitespace=nowarn"];
    match what {
        HunkDo::Stage => a.push("--cached"),
        HunkDo::Unstage => {
            a.push("--cached");
            a.push("--reverse");
        }
        HunkDo::Discard => a.push("--reverse"),
    }
    a.push("-");
    run_bytes(r, &a, Some(patch.as_bytes()), false, false).map(drop)
}

// ----- branches, merge, rebase -----

/// A branch.
#[derive(Clone, Debug, PartialEq)]
pub struct Branch {
    pub name: String,
    pub current: bool,
    /// A branch of a remote (`origin/main`).
    pub remote: bool,
    pub upstream: Option<String>,
    pub subject: String,
}

pub fn branches(r: &Repo) -> Result<Vec<Branch>, String> {
    let out = reading(r, &["for-each-ref", "--format=%(HEAD)%09%(refname)%09%(refname:short)%09%(upstream:short)%09%(contents:subject)", "refs/heads", "refs/remotes"])?;
    Ok(out
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.splitn(5, '\t').collect();
            if f.len() < 5 || f[1].ends_with("/HEAD") {
                return None;
            }
            Some(Branch {
                name: f[2].to_string(),
                current: f[0] == "*",
                remote: f[1].starts_with("refs/remotes/"),
                upstream: (!f[3].is_empty()).then(|| f[3].to_string()),
                subject: f[4].to_string(),
            })
        })
        .collect())
}

/// Switch to a branch; a remote one (`origin/x`) gets a local branch that follows it.
pub fn checkout(r: &Repo, branch: &Branch) -> Result<(), String> {
    if branch.remote { git(r, &["switch", "--track", &branch.name]).map(drop) } else { git(r, &["switch", &branch.name]).map(drop) }
}

pub fn new_branch(r: &Repo, name: &str) -> Result<(), String> {
    git(r, &["switch", "-c", name]).map(drop)
}

pub fn delete_branch(r: &Repo, name: &str, force: bool) -> Result<(), String> {
    git(r, &["branch", if force { "-D" } else { "-d" }, name]).map(drop)
}

/// Merge a branch into the current one (no editor: git's own message). A merge with conflicts is left in progress: [`state`] says so,
/// the conflicted files are marked `U`, and [`continue_`] or [`abort`] end it.
pub fn merge(r: &Repo, name: &str) -> Result<String, String> {
    git(r, &["merge", "--no-edit", name]).map(|s| s.lines().last().unwrap_or("").to_string())
}

/// Rebase the current branch onto another.
pub fn rebase(r: &Repo, onto: &str) -> Result<String, String> {
    git(r, &["rebase", onto]).map(|s| s.lines().last().unwrap_or("").to_string())
}

/// The operation that is in progress in the work tree: `merge`, `rebase`, `cherry-pick` or `revert` (`None`: none).
pub fn state(r: &Repo) -> Option<String> {
    let script = "gd=$(git rev-parse --git-dir) && for f in MERGE_HEAD rebase-merge rebase-apply CHERRY_PICK_HEAD REVERT_HEAD; do if [ -e \"$gd/$f\" ]; then echo $f; break; fi; done";
    let o = r.run.shell(&r.root, script).ok()?;
    let f = String::from_utf8_lossy(&o.stdout).trim().to_string();
    match f.as_str() {
        "MERGE_HEAD" => Some("merge".into()),
        "rebase-merge" | "rebase-apply" => Some("rebase".into()),
        "CHERRY_PICK_HEAD" => Some("cherry-pick".into()),
        "REVERT_HEAD" => Some("revert".into()),
        _ => None,
    }
}

/// End the operation in progress by giving it up.
pub fn abort(r: &Repo, state: &str) -> Result<(), String> {
    git(r, &[state, "--abort"]).map(drop)
}

/// Go on with the operation in progress (after the conflicts were resolved and staged).
pub fn continue_(r: &Repo, state: &str) -> Result<String, String> {
    git(r, &[state, "--continue"]).map(|s| s.lines().last().unwrap_or("").to_string())
}

/// Resolve conflicted paths by taking one side (`ours` / `theirs`), and stage them.
pub fn resolve(r: &Repo, paths: &[String], theirs: bool) -> Result<(), String> {
    git(r, &with_paths(&["checkout", if theirs { "--theirs" } else { "--ours" }], paths))?;
    stage(r, paths)
}

// ----- tags -----

/// A tag.
#[derive(Clone, Debug, PartialEq)]
pub struct Tag {
    pub name: String,
    pub hash: String,
    pub subject: String,
}

pub fn tags(r: &Repo) -> Result<Vec<Tag>, String> {
    let out = reading(r, &["for-each-ref", "--sort=-creatordate", "--format=%(refname:short)%09%(objectname:short)%09%(contents:subject)", "refs/tags"])?;
    Ok(out
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.splitn(3, '\t').collect();
            (f.len() == 3).then(|| Tag { name: f[0].to_string(), hash: f[1].to_string(), subject: f[2].to_string() })
        })
        .collect())
}

/// A tag at the head; with a message it is an annotated one.
pub fn new_tag(r: &Repo, name: &str, message: &str) -> Result<(), String> {
    if message.trim().is_empty() { git(r, &["tag", name]).map(drop) } else { git(r, &["tag", "-a", name, "-m", message]).map(drop) }
}

pub fn delete_tag(r: &Repo, name: &str) -> Result<(), String> {
    git(r, &["tag", "-d", name]).map(drop)
}

pub fn push_tag(r: &Repo, name: &str) -> Result<String, String> {
    git_net(r, &["push", "origin", &format!("refs/tags/{name}")])
}

/// Look at a tag: the head is detached there.
pub fn checkout_tag(r: &Repo, name: &str) -> Result<(), String> {
    git(r, &["switch", "--detach", name]).map(drop)
}

// ----- submodules -----

/// A submodule: its path, the commit it is at, and its state: `' '` in step, `'-'` not initialized, `'+'` at another commit than the
/// parent wants, `'U'` in conflict.
#[derive(Clone, Debug, PartialEq)]
pub struct Submodule {
    pub path: String,
    pub hash: String,
    pub state: char,
    pub describe: String,
}

pub fn submodules(r: &Repo) -> Result<Vec<Submodule>, String> {
    let out = reading(r, &["submodule", "status"])?;
    Ok(out
        .lines()
        .filter_map(|l| {
            let mut c = l.chars();
            let state = c.next()?;
            let rest = c.as_str();
            let mut f = rest.splitn(3, ' ');
            let hash = f.next()?.to_string();
            let path = f.next()?.to_string();
            let describe = f.next().unwrap_or("").trim_matches(|c| c == '(' || c == ')').to_string();
            Some(Submodule { path, hash, state, describe })
        })
        .collect())
}

/// Bring submodules to the commit the parent wants (initializing them first): one of them, or all.
pub fn submodule_update(r: &Repo, path: Option<&str>) -> Result<String, String> {
    let mut a = vec!["submodule", "update", "--init", "--recursive"];
    if let Some(p) = path {
        a.push("--");
        a.push(p);
    }
    git_net(r, &a).map(|s| s.trim().to_string())
}

// ----- the server -----

pub fn fetch(r: &Repo) -> Result<String, String> {
    git_net(r, &["fetch", "--all", "--prune"])
}

pub fn pull(r: &Repo) -> Result<String, String> {
    git_net(r, &["pull", "--ff-only"]).map(|s| s.lines().last().unwrap_or("").to_string())
}

/// Push the branch; one that has no upstream yet is pushed to `origin` and set to follow it.
pub fn push(r: &Repo) -> Result<String, String> {
    match git_net(r, &["push"]) {
        Ok(s) => Ok(s),
        Err(e) if e.contains("no upstream branch") || e.contains("--set-upstream") => {
            let b = reading(r, &["rev-parse", "--abbrev-ref", "HEAD"])?.trim().to_string();
            git_net(r, &["push", "--set-upstream", "origin", &b])
        }
        Err(e) => Err(e),
    }
}

// ----- stash, ignore, rm, mv -----

pub fn stash_push(r: &Repo, message: &str, untracked: bool) -> Result<String, String> {
    let mut a = vec!["stash", "push"];
    if untracked {
        a.push("--include-untracked");
    }
    if !message.trim().is_empty() {
        a.push("-m");
        a.push(message);
    }
    git(r, &a).map(|s| s.trim().to_string())
}

/// The stashes: `(stash@{0}, its subject)`.
pub fn stash_list(r: &Repo) -> Result<Vec<(String, String)>, String> {
    let out = reading(r, &["stash", "list", "--format=%gd%x09%s"])?;
    Ok(out.lines().filter_map(|l| l.split_once('\t').map(|(a, b)| (a.to_string(), b.to_string()))).collect())
}

pub fn stash_pop(r: &Repo, which: &str) -> Result<(), String> {
    git(r, &["stash", "pop", which]).map(drop)
}

pub fn stash_apply(r: &Repo, which: &str) -> Result<(), String> {
    git(r, &["stash", "apply", which]).map(drop)
}

pub fn stash_drop(r: &Repo, which: &str) -> Result<(), String> {
    git(r, &["stash", "drop", which]).map(drop)
}

/// Add a path to `.gitignore` of the work tree (a folder with a slash at the end), once.
pub fn ignore(r: &Repo, rel: &str, dir: bool) -> Result<(), String> {
    let line = format!("/{}{}", rel.trim_matches('/'), if dir { "/" } else { "" });
    let q = sh_quote(&line);
    // the file may not end with a newline: it gets one before the new line
    let script = format!("touch .gitignore && grep -qxF -- {q} .gitignore || {{ [ -z \"$(tail -c1 .gitignore)\" ] || echo >> .gitignore; printf '%s\\n' {q} >> .gitignore; }}");
    let o = r.run.shell(&r.root, &script)?;
    if o.ok { Ok(()) } else { Err(format!(".gitignore: {}", o.stderr.trim())) }
}

/// Remove tracked paths from the work tree and the index (also ones with changes: the tool asks first).
pub fn remove(r: &Repo, paths: &[String]) -> Result<(), String> {
    git(r, &with_paths(&["rm", "-r", "-q", "-f"], paths)).map(drop)
}

/// Rename a tracked path (the history follows it).
pub fn rename(r: &Repo, from: &str, to: &str) -> Result<(), String> {
    git(r, &["mv", "--", from, to]).map(drop)
}

/// The last commit's one-liner, for the bar.
pub fn head_line(r: &Repo) -> String {
    reading(r, &["log", "-1", "--format=%h %s"]).map(|s| s.trim().to_string()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status;
    use std::process::Command;

    fn repo(name: &str) -> Repo {
        let d = std::env::temp_dir().join(format!("lvim-git-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let g = |a: &[&str]| {
            let o = Command::new("git").arg("-C").arg(&d).args(a).output().unwrap();
            assert!(o.status.success(), "git {a:?}: {}", String::from_utf8_lossy(&o.stderr));
        };
        g(&["init", "-q", "-b", "main"]);
        g(&["config", "user.email", "t@example.org"]);
        g(&["config", "user.name", "T"]);
        g(&["config", "commit.gpgsign", "false"]);
        Repo::local(&d.canonicalize().unwrap())
    }

    fn st(r: &Repo) -> status::Status {
        status::read(&r.path(), true, "normal").unwrap().unwrap()
    }

    fn write(r: &Repo, name: &str, text: &str) {
        std::fs::write(r.path().join(name), text).unwrap();
    }

    fn rm(r: &Repo) {
        let _ = std::fs::remove_dir_all(r.path());
    }

    #[test]
    fn stage_commit_unstage_discard_and_the_marks_follow() {
        let r = repo("flow");
        write(&r, "a.txt", "one\n");
        std::fs::create_dir_all(r.path().join("sub")).unwrap();
        write(&r, "sub/b.txt", "two\n");
        let s = st(&r);
        assert_eq!(s.mark_of("a.txt", false).unwrap().kind, status::Kind::Untracked);
        assert_eq!(s.branch.as_deref(), Some("main"));

        stage(&r, &["a.txt".into()]).unwrap();
        let m = st(&r).mark_of("a.txt", false).unwrap();
        assert_eq!((m.kind, m.staged), (status::Kind::Added, true));
        // nothing to commit with an empty message
        assert!(commit(&r, "  ", false).is_err());
        commit(&r, "first", false).unwrap();
        assert_eq!(log(&r, None, 10).unwrap()[0].subject, "first");
        assert!(st(&r).mark_of("a.txt", false).is_none());

        // a change, staged and unstaged again
        write(&r, "a.txt", "one\nmore\n");
        assert!(diff(&r, Some("a.txt"), false, false).unwrap().contains("+more"));
        stage(&r, &["a.txt".into()]).unwrap();
        assert!(diff(&r, Some("a.txt"), true, false).unwrap().contains("+more"));
        unstage(&r, &["a.txt".into()]).unwrap();
        let m = st(&r).mark_of("a.txt", false).unwrap();
        assert_eq!((m.kind, m.staged, m.unstaged), (status::Kind::Modified, false, true));

        // discard: the tracked file goes back, the untracked folder is deleted
        discard(&r, &["a.txt".into()], &["sub".into()]).unwrap();
        assert_eq!(std::fs::read_to_string(r.path().join("a.txt")).unwrap(), "one\n");
        assert!(!r.path().join("sub").exists());
        assert!(!st(&r).dirty());

        // amend keeps the message when none is given
        write(&r, "c.txt", "c\n");
        stage_all(&r).unwrap();
        commit(&r, "", true).unwrap();
        assert_eq!(log(&r, None, 10).unwrap().len(), 1);
        assert_eq!(last_message(&r), "first");
        assert!(show(&r, "HEAD").unwrap().contains("c.txt"));
        assert!(blame(&r, "a.txt").unwrap().contains("one"));
        unstage_all(&r).unwrap();
        rm(&r);
    }

    #[test]
    fn branches_stash_ignore_and_rename() {
        let r = repo("branch");
        write(&r, "a.txt", "x\n");
        stage_all(&r).unwrap();
        commit(&r, "init", false).unwrap();
        new_branch(&r, "topic").unwrap();
        let b = branches(&r).unwrap();
        assert!(b.iter().any(|b| b.name == "topic" && b.current));
        let main = b.iter().find(|b| b.name == "main").unwrap().clone();
        checkout(&r, &main).unwrap();
        delete_branch(&r, "topic", false).unwrap();
        assert!(!branches(&r).unwrap().iter().any(|b| b.name == "topic"));

        write(&r, "a.txt", "changed\n");
        assert!(stash_push(&r, "wip", false).unwrap().contains("Saved"));
        assert_eq!(stash_list(&r).unwrap().len(), 1);
        assert_eq!(std::fs::read_to_string(r.path().join("a.txt")).unwrap(), "x\n");
        stash_pop(&r, "stash@{0}").unwrap();
        assert_eq!(std::fs::read_to_string(r.path().join("a.txt")).unwrap(), "changed\n");
        discard(&r, &["a.txt".into()], &[]).unwrap();

        std::fs::create_dir_all(r.path().join("out")).unwrap();
        write(&r, "out/x.o", "");
        ignore(&r, "out", true).unwrap();
        ignore(&r, "out", true).unwrap();
        assert_eq!(std::fs::read_to_string(r.path().join(".gitignore")).unwrap(), "/out/\n");
        // a file without a final newline gets one before the new line
        write(&r, ".gitignore", "/out/");
        ignore(&r, "junk.log", false).unwrap();
        assert_eq!(std::fs::read_to_string(r.path().join(".gitignore")).unwrap(), "/out/\n/junk.log\n");
        assert_eq!(st(&r).mark_of("out", true).unwrap().kind, status::Kind::Ignored);

        rename(&r, "a.txt", "b.txt").unwrap();
        assert_eq!(st(&r).mark_of("b.txt", false).unwrap().kind, status::Kind::Renamed);
        remove(&r, &["b.txt".into()]).unwrap();
        assert!(!r.path().join("b.txt").exists());
        // pushing with no remote says why
        assert!(push(&r).is_err());
        rm(&r);
    }

    #[test]
    fn an_untracked_file_is_diffed_as_all_new_and_an_empty_repo_has_no_log() {
        let r = repo("untr");
        write(&r, "n.txt", "hello\n");
        let d = diff(&r, Some("n.txt"), false, true).unwrap();
        assert!(d.contains("+hello") && d.contains("new file"));
        assert!(log(&r, None, 5).unwrap().is_empty());
        rm(&r);
    }

    #[test]
    fn a_hunk_is_staged_unstaged_and_discarded_by_itself() {
        let r = repo("hunks");
        let base: String = (1..=40).map(|i| format!("line {i}\n")).collect();
        write(&r, "f.txt", &base);
        stage_all(&r).unwrap();
        commit(&r, "base", false).unwrap();
        // two changes far apart: two hunks
        let changed = base.replace("line 2\n", "line 2 changed\n").replace("line 38\n", "line 38 changed\n");
        write(&r, "f.txt", &changed);
        let text = diff(&r, Some("f.txt"), false, false).unwrap();
        let lines: Vec<String> = text.lines().map(str::to_string).collect();
        let hs = hunks(&lines);
        assert_eq!(hs.len(), 2);

        // stage the second one only
        let patch = hunk_patch(&lines, &hs, 1).unwrap();
        apply_hunk(&r, &patch, HunkDo::Stage).unwrap();
        let staged = diff(&r, Some("f.txt"), true, false).unwrap();
        assert!(staged.contains("line 38 changed") && !staged.contains("line 2 changed"));
        let work = diff(&r, Some("f.txt"), false, false).unwrap();
        assert!(work.contains("line 2 changed") && !work.contains("line 38 changed"));
        let m = st(&r).mark_of("f.txt", false).unwrap();
        assert_eq!((m.x, m.y), ('M', 'M'));

        // unstage it again, from the staged diff
        let slines: Vec<String> = staged.lines().map(str::to_string).collect();
        let shs = hunks(&slines);
        assert_eq!(shs.len(), 1);
        apply_hunk(&r, &hunk_patch(&slines, &shs, 0).unwrap(), HunkDo::Unstage).unwrap();
        assert!(diff(&r, Some("f.txt"), true, false).unwrap().is_empty());

        // discard the first one: the file keeps the second change only
        let text = diff(&r, Some("f.txt"), false, false).unwrap();
        let lines: Vec<String> = text.lines().map(str::to_string).collect();
        let hs = hunks(&lines);
        apply_hunk(&r, &hunk_patch(&lines, &hs, 0).unwrap(), HunkDo::Discard).unwrap();
        let now = std::fs::read_to_string(r.path().join("f.txt")).unwrap();
        assert!(now.contains("line 2\n") && now.contains("line 38 changed"));
        rm(&r);
    }

    #[test]
    fn merge_with_a_conflict_is_resolved_continued_or_aborted() {
        let r = repo("merge");
        write(&r, "a.txt", "base\n");
        stage_all(&r).unwrap();
        commit(&r, "base", false).unwrap();
        new_branch(&r, "topic").unwrap();
        write(&r, "a.txt", "topic\n");
        stage_all(&r).unwrap();
        commit(&r, "topic change", false).unwrap();
        let main = branches(&r).unwrap().into_iter().find(|b| b.name == "main").unwrap();
        checkout(&r, &main).unwrap();
        write(&r, "a.txt", "main\n");
        stage_all(&r).unwrap();
        commit(&r, "main change", false).unwrap();

        assert!(state(&r).is_none());
        assert!(merge(&r, "topic").is_err());
        assert_eq!(state(&r).as_deref(), Some("merge"));
        let s = st(&r);
        assert_eq!(s.mark_of("a.txt", false).unwrap().kind, status::Kind::Conflict);
        assert_eq!(s.state.as_deref(), Some("merge"));
        // give it up, and do it again taking the other side's file
        abort(&r, "merge").unwrap();
        assert!(state(&r).is_none());
        assert!(merge(&r, "topic").is_err());
        resolve(&r, &["a.txt".into()], true).unwrap();
        continue_(&r, "merge").unwrap();
        assert!(state(&r).is_none());
        assert_eq!(std::fs::read_to_string(r.path().join("a.txt")).unwrap(), "topic\n");
        rm(&r);
    }

    #[test]
    fn rebase_tags_and_submodules() {
        let r = repo("tags");
        write(&r, "a.txt", "1\n");
        stage_all(&r).unwrap();
        commit(&r, "one", false).unwrap();
        new_tag(&r, "v1", "").unwrap();
        new_tag(&r, "v2", "release two").unwrap();
        let t = tags(&r).unwrap();
        assert_eq!(t.len(), 2);
        assert!(t.iter().any(|t| t.name == "v2" && t.subject == "release two"));
        delete_tag(&r, "v1").unwrap();
        assert_eq!(tags(&r).unwrap().len(), 1);
        checkout_tag(&r, "v2").unwrap();
        assert!(st(&r).branch.is_none());

        // a rebase of a branch onto main
        let main = branches(&r).unwrap().into_iter().find(|b| b.name == "main").unwrap();
        checkout(&r, &main).unwrap();
        new_branch(&r, "side").unwrap();
        write(&r, "s.txt", "s\n");
        stage_all(&r).unwrap();
        commit(&r, "side", false).unwrap();
        checkout(&r, &main).unwrap();
        write(&r, "m.txt", "m\n");
        stage_all(&r).unwrap();
        commit(&r, "main two", false).unwrap();
        let side = branches(&r).unwrap().into_iter().find(|b| b.name == "side").unwrap();
        checkout(&r, &side).unwrap();
        rebase(&r, "main").unwrap();
        let l = log(&r, None, 10).unwrap();
        assert_eq!(l.iter().map(|c| c.subject.as_str()).collect::<Vec<_>>(), ["side", "main two", "one"]);

        // no submodules here
        assert!(submodules(&r).unwrap().is_empty());
        rm(&r);
    }
}
