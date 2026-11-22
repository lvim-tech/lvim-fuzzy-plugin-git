//! The state of a work tree: `git status --porcelain=v2 -z --branch` read into marks per path, the branch, and how far it is
//! ahead of / behind its upstream. The marks of the folders are the heaviest mark of what is inside them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use std::sync::Arc;

use crate::ops;
use crate::runner::{Local, Repo, Runner};

/// What is the matter with a path (the heavier first).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Ignored,
    Untracked,
    Added,
    Renamed,
    Deleted,
    Modified,
    Conflict,
}

impl Kind {
    /// The letter the tools put before the name.
    pub fn letter(self) -> char {
        match self {
            Kind::Ignored => '!',
            Kind::Untracked => '?',
            Kind::Added => 'A',
            Kind::Renamed => 'R',
            Kind::Deleted => 'D',
            Kind::Modified => 'M',
            Kind::Conflict => 'U',
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::Ignored => "ignored",
            Kind::Untracked => "untracked",
            Kind::Added => "added",
            Kind::Renamed => "renamed",
            Kind::Deleted => "deleted",
            Kind::Modified => "modified",
            Kind::Conflict => "conflict",
        }
    }
}

/// The mark of one path (or of a folder: the heaviest of what is in it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mark {
    pub kind: Kind,
    /// Something of it is in the index (it will be in the next commit).
    pub staged: bool,
    /// Something of it is changed in the work tree and not in the index yet.
    pub unstaged: bool,
    /// The two columns of `git status -s`: what is in the index and what is in the work tree (`' '` = nothing).
    pub x: char,
    pub y: char,
}

/// How heavy a column's letter is (the heavier wins when a folder holds several).
fn weight(c: char) -> u8 {
    match c {
        'U' => 6,
        'M' => 5,
        'D' => 4,
        'R' => 3,
        'A' => 2,
        '?' => 1,
        '!' => 0,
        _ => 0,
    }
}

fn heavier(a: char, b: char) -> char {
    if a == ' ' {
        b
    } else if b == ' ' {
        a
    } else if weight(b) > weight(a) {
        b
    } else {
        a
    }
}

impl Mark {
    fn merge(self, o: Mark) -> Mark {
        Mark { kind: self.kind.max(o.kind), staged: self.staged || o.staged, unstaged: self.unstaged || o.unstaged, x: heavier(self.x, o.x), y: heavier(self.y, o.y) }
    }
}

/// The state of a work tree.
#[derive(Clone, Debug, Default)]
pub struct Status {
    /// The top of the work tree (real path).
    pub root: PathBuf,
    /// The branch (`None` when the head is detached).
    pub branch: Option<String>,
    pub oid: String,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    /// By path from the root, `/` separated, no slash at the end (a folder that is untracked or ignored is one entry).
    pub files: HashMap<String, Mark>,
    dirs: HashMap<String, Mark>,
    /// Counters for the bar: in the index, changed in the work tree, untracked, in conflict.
    pub staged: usize,
    pub changed: usize,
    pub untracked: usize,
    pub conflicted: usize,
    /// The operation in progress: `merge`, `rebase`, `cherry-pick` or `revert`.
    pub state: Option<String>,
    /// The work tree, with the runner that reaches it (this machine, or a server over ssh).
    pub repo: Option<Repo>,
}

fn code(c: u8) -> char {
    c as char
}

/// The mark of a `1` / `2` entry's `XY`.
fn mark_of_xy(x: u8, y: u8) -> Mark {
    let staged = x != b'.' && x != b' ';
    let unstaged = y != b'.' && y != b' ';
    // what is told is the work tree's change when there is one, else the index's
    let c = if unstaged { y } else { x };
    let kind = match code(c) {
        'A' | 'C' => Kind::Added,
        'D' => Kind::Deleted,
        'R' => Kind::Renamed,
        _ => Kind::Modified,
    };
    // added to the index and then changed: still an addition
    let kind = if code(x) == 'A' && unstaged && code(y) != 'D' { Kind::Added } else { kind };
    let letter = |c: u8, on: bool| match code(c) {
        _ if !on => ' ',
        'A' | 'C' => 'A',
        'D' => 'D',
        'R' => 'R',
        _ => 'M',
    };
    Mark { kind, staged, unstaged, x: letter(x, staged), y: letter(y, unstaged) }
}

/// Read `git status --porcelain=v2 -z --branch` output.
pub fn parse(out: &[u8]) -> Status {
    let mut st = Status::default();
    let text = String::from_utf8_lossy(out);
    let mut it = text.split('\0').peekable();
    while let Some(tok) = it.next() {
        if tok.is_empty() {
            continue;
        }
        if let Some(h) = tok.strip_prefix("# ") {
            if let Some(v) = h.strip_prefix("branch.oid ") {
                st.oid = v.to_string();
            } else if let Some(v) = h.strip_prefix("branch.head ") {
                st.branch = (v != "(detached)").then(|| v.to_string());
            } else if let Some(v) = h.strip_prefix("branch.upstream ") {
                st.upstream = Some(v.to_string());
            } else if let Some(v) = h.strip_prefix("branch.ab ") {
                let mut p = v.split(' ');
                st.ahead = p.next().and_then(|a| a.trim_start_matches('+').parse().ok()).unwrap_or(0);
                st.behind = p.next().and_then(|b| b.trim_start_matches('-').parse().ok()).unwrap_or(0);
            }
            continue;
        }
        let kind = tok.as_bytes()[0];
        let (path, mark) = match kind {
            b'1' => {
                let f: Vec<&str> = tok.splitn(9, ' ').collect();
                if f.len() < 9 {
                    continue;
                }
                let xy = f[1].as_bytes();
                (f[8].to_string(), mark_of_xy(xy[0], xy[1]))
            }
            b'2' => {
                let f: Vec<&str> = tok.splitn(10, ' ').collect();
                if f.len() < 10 {
                    continue;
                }
                let xy = f[1].as_bytes();
                // the next token is the path it was renamed from
                it.next();
                (f[9].to_string(), mark_of_xy(xy[0], xy[1]))
            }
            b'u' => {
                let f: Vec<&str> = tok.splitn(11, ' ').collect();
                if f.len() < 11 {
                    continue;
                }
                (f[10].to_string(), Mark { kind: Kind::Conflict, staged: false, unstaged: true, x: 'U', y: 'U' })
            }
            b'?' => (tok[2..].to_string(), Mark { kind: Kind::Untracked, staged: false, unstaged: true, x: ' ', y: '?' }),
            b'!' => (tok[2..].to_string(), Mark { kind: Kind::Ignored, staged: false, unstaged: false, x: ' ', y: '!' }),
            _ => continue,
        };
        let path = path.trim_end_matches('/').to_string();
        match mark.kind {
            Kind::Ignored => {}
            Kind::Untracked => st.untracked += 1,
            Kind::Conflict => st.conflicted += 1,
            _ => {
                if mark.staged {
                    st.staged += 1;
                }
                if mark.unstaged {
                    st.changed += 1;
                }
            }
        }
        st.files.insert(path, mark);
    }
    st.fold();
    st
}

impl Status {
    /// The marks of the folders: every folder over a changed path takes the heaviest mark below it.
    fn fold(&mut self) {
        self.dirs.clear();
        for (p, m) in &self.files {
            if m.kind == Kind::Ignored {
                continue;
            }
            let mut end = p.len();
            while let Some(i) = p[..end].rfind('/') {
                end = i;
                let d = &p[..end];
                self.dirs.entry(d.to_string()).and_modify(|e| *e = e.merge(*m)).or_insert(*m);
            }
        }
    }

    /// Where `dir` is in the work tree: its path from the root (`""` is the root itself), `None` when it is not inside.
    pub fn base(&self, dir: &Path) -> Option<String> {
        // a work tree on a server: the paths are only text here
        if self.repo.as_ref().is_some_and(Repo::is_remote) {
            return dir.strip_prefix(&self.root).ok().map(|r| r.to_string_lossy().trim_matches('/').to_string());
        }
        let rel = dir.strip_prefix(&self.root).map(Path::to_path_buf).or_else(|_| dir.canonicalize().and_then(|c| c.strip_prefix(&self.root).map(Path::to_path_buf).map_err(std::io::Error::other))).ok()?;
        Some(rel.to_string_lossy().trim_matches('/').to_string())
    }

    /// The mark of the entry `name` in the folder `base` (see [`Status::base`]); a folder takes the mark of what is in it, and what
    /// is inside an untracked or ignored folder is untracked or ignored too.
    pub fn mark(&self, base: &str, name: &str, is_dir: bool) -> Option<Mark> {
        let rel = if base.is_empty() { name.to_string() } else { format!("{base}/{name}") };
        self.mark_of(&rel, is_dir)
    }

    /// The mark of a path from the root.
    pub fn mark_of(&self, rel: &str, is_dir: bool) -> Option<Mark> {
        if let Some(m) = self.files.get(rel) {
            return Some(*m);
        }
        if is_dir && let Some(m) = self.dirs.get(rel) {
            return Some(*m);
        }
        // inside a folder git listed as one entry
        let mut end = rel.len();
        while let Some(i) = rel[..end].rfind('/') {
            end = i;
            if let Some(m) = self.files.get(&rel[..end])
                && matches!(m.kind, Kind::Untracked | Kind::Ignored)
            {
                return Some(*m);
            }
        }
        None
    }

    /// Is anything staged, changed or untracked?
    pub fn dirty(&self) -> bool {
        self.staged + self.changed + self.untracked + self.conflicted > 0
    }

    /// The work tree's path of `rel`.
    pub fn abs(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }
}

/// The state of the work tree `dir` is in, on this machine; `Ok(None)`: it is not in one. `ignored`: list ignored paths too; `untracked`:
/// `"normal"`, `"all"` or `"no"`.
pub fn read(dir: &Path, ignored: bool, untracked: &str) -> Result<Option<Status>, String> {
    read_in(Arc::new(Local), &dir.to_string_lossy(), ignored, untracked)
}

/// The same on the machine `run` reaches (a server over ssh): `dir` is a path there.
pub fn read_in(run: Arc<dyn Runner>, dir: &str, ignored: bool, untracked: &str) -> Result<Option<Status>, String> {
    let Some(root) = ops::root_in(&run, dir) else { return Ok(None) };
    let repo = Repo { root: root.clone(), run };
    let mut args = vec!["status", "--porcelain=v2", "-z", "--branch"];
    args.push(match untracked {
        "all" => "--untracked-files=all",
        "no" => "--untracked-files=no",
        _ => "--untracked-files=normal",
    });
    if ignored {
        args.push("--ignored=matching");
    }
    let out = ops::run_bytes(&repo, &args, None, true, false)?;
    let mut st = parse(&out);
    st.root = PathBuf::from(&root);
    st.state = ops::state(&repo);
    st.repo = Some(repo);
    Ok(Some(st))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_is_read_into_marks_folders_and_the_branch() {
        let out = b"# branch.oid 1234abcd\0# branch.head main\0# branch.upstream origin/main\0# branch.ab +2 -1\0\
1 .M N... 100644 100644 100644 aaaa bbbb src/a.rs\0\
1 M. N... 100644 100644 100644 aaaa bbbb src/deep/b.rs\0\
1 AM N... 000000 100644 100644 0000 bbbb c.rs\0\
2 R. N... 100644 100644 100644 aaaa bbbb R100 new.rs\0old.rs\0\
u UU N... 100644 100644 100644 100644 a b c conflict.rs\0\
? scratch/\0\
! target/\0";
        let st = parse(out);
        assert_eq!(st.branch.as_deref(), Some("main"));
        assert_eq!(st.upstream.as_deref(), Some("origin/main"));
        assert_eq!((st.ahead, st.behind), (2, 1));
        let a = st.mark_of("src/a.rs", false).unwrap();
        assert_eq!((a.kind, a.staged, a.unstaged), (Kind::Modified, false, true));
        assert_eq!((a.x, a.y), (' ', 'M'));
        let b = st.mark_of("src/deep/b.rs", false).unwrap();
        assert_eq!((b.kind, b.staged, b.unstaged), (Kind::Modified, true, false));
        // added to the index, then changed: an addition that is partly staged
        let c = st.mark_of("c.rs", false).unwrap();
        assert_eq!((c.kind, c.staged, c.unstaged), (Kind::Added, true, true));
        assert_eq!((c.x, c.y), ('A', 'M'));
        // a folder shows the heaviest letter of each column of what is in it
        let src = st.mark_of("src", true).unwrap();
        assert_eq!((src.x, src.y), ('M', 'M'));
        // the rename is told by its new name, the old name is not a path of its own
        assert_eq!(st.mark_of("new.rs", false).unwrap().kind, Kind::Renamed);
        assert!(st.mark_of("old.rs", false).is_none());
        assert_eq!(st.mark_of("conflict.rs", false).unwrap().kind, Kind::Conflict);
        // the folders hold the heaviest of what is below them
        assert_eq!(st.mark_of("src", true).unwrap().kind, Kind::Modified);
        assert_eq!(st.mark_of("src/deep", true).unwrap().kind, Kind::Modified);
        // an untracked / ignored folder is one entry and what is in it is the same
        assert_eq!(st.mark_of("scratch", true).unwrap().kind, Kind::Untracked);
        assert_eq!(st.mark_of("scratch/x/y.txt", false).unwrap().kind, Kind::Untracked);
        assert_eq!(st.mark_of("target/debug/app", false).unwrap().kind, Kind::Ignored);
        // ignored paths do not make their parents dirty
        assert!(st.mark_of("untouched.rs", false).is_none());
        assert_eq!((st.staged, st.changed, st.untracked, st.conflicted), (3, 2, 1, 1));
        assert!(st.dirty());
    }

    #[test]
    fn a_clean_tree_and_a_detached_head() {
        let st = parse(b"# branch.oid abc\0# branch.head (detached)\0");
        assert!(st.branch.is_none());
        assert!(!st.dirty());
        assert_eq!(Kind::Modified.letter(), 'M');
    }
}
