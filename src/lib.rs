//! lvim-fuzzy-plugin-git: git for the lvim fuzzy tools. A library the tools can call directly ([`status`] — the marks of a work tree,
//! the branch, ahead / behind —, [`ops`] — stage, commit, branches, stash, log, diff, push / pull) and a PROCESS plugin (JSON lines
//! on stdin / stdout) for the tools that ask through the plugin host (lfz, the Neovim picker, lvim-files). Everything is the `git`
//! program; the tool draws.
//!
//! The call's context: `{ "call": "status", "dir": "/work/repo/src", "config": { … } }`; `config` is the plugin's settings merged
//! from its defaults, the shared override file and the asking tool's own override. The answers (one instruction in an array):
//!
//! | call | context | answer |
//! |---|---|---|
//! | `status` | `dir` | `{"op":"status","repo":false}` or `{"op":"status","repo":true,"root","branch"?,"oid","upstream"?,"ahead","behind","counts":{staged,changed,untracked,conflicted},"files":{"path":{"kind","staged","unstaged"}}}` |
//! | `branches` | `dir` | `{"op":"branches","branches":[{"name","current","remote","upstream"?,"subject"}]}` |
//! | `log` | `dir`, `path`?, `n`? | `{"op":"log","commits":[{"hash","short","author","date","ago","subject"}]}` |
//! | `diff` | `dir`, `path`?, `staged`?, `untracked`? | `{"op":"text","text":"…"}` |
//! | `show` | `dir`, `rev` | `{"op":"text","text":"…"}` |
//! | `stashes` | `dir` | `{"op":"stashes","stashes":[{"ref","subject"}]}` |
//! | `diff_files` · `changes` · `go` | a TOOL's button (its `[actions.*]` with `call`): the tool's context (`file`, `files`, `dir`, `other_dir`, `arg`) | instructions for the tool: `text` (the diff of two files), `menu` (what is changed; its items call `go`), `cd` + `select` |
//! | `run` | `dir`, `verb`, `paths`?, `message`?, `name`?, `force`?, `untracked`? | `{"op":"done","text":"…"}` |
//!
//! `run` verbs: `stage`, `unstage`, `discard` (`paths` tracked, `untracked`), `stage_all`, `unstage_all`, `commit`, `amend`
//! (`message`), `checkout`, `new_branch`, `delete_branch` (`name`, `force`), `fetch`, `pull`, `push`, `stash_push` (`message`),
//! `stash_pop`, `stash_apply`, `stash_drop` (`name` = the stash), `ignore` (`paths`), `remove` (`paths`), `rename` (`paths` = from, to).
//!
//! A failed call is an error answer (what git said).

pub mod ops;
pub mod runner;
pub mod status;
pub mod ui;

use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use serde_json::{Value, json};

/// The plugin's loop: JSON lines on stdin, answers on stdout. Calls run side by side, each with its own time limit
/// (`config.timeout` seconds; `config.long_timeout` for `run`). Returns when stdin closes or a `quit` event comes.
pub fn run() {
    let out = Arc::new(Mutex::new(std::io::stdout()));
    let inflight = Arc::new(AtomicUsize::new(0));
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(req) = serde_json::from_str::<Value>(&line) else { continue };
        match req["method"].as_str() {
            Some("call") => {
                let id = req["id"].clone();
                let params = req["params"].clone();
                let key = if params["call"] == "run" { "long_timeout" } else { "timeout" };
                let limit = Duration::from_secs(params["config"][key].as_u64().unwrap_or(if key == "timeout" { 30 } else { 300 }).clamp(1, 3600));
                let (tx, rx) = mpsc::channel();
                std::thread::spawn(move || {
                    let _ = tx.send(answer(&params));
                });
                let out = out.clone();
                inflight.fetch_add(1, Ordering::SeqCst);
                let inflight = inflight.clone();
                std::thread::spawn(move || {
                    let reply = match rx.recv_timeout(limit) {
                        // an answer is one instruction or a list of them
                        Ok(Ok(Value::Array(list))) => json!({"id": id, "result": list}),
                        Ok(Ok(ins)) => json!({"id": id, "result": [ins]}),
                        Ok(Err(e)) => json!({"id": id, "error": e}),
                        Err(_) => json!({"id": id, "error": format!("no answer in {} s", limit.as_secs())}),
                    };
                    if let Ok(mut o) = out.lock() {
                        let _ = writeln!(o, "{reply}");
                        let _ = o.flush();
                    }
                    inflight.fetch_sub(1, Ordering::SeqCst);
                });
            }
            Some("event") if req["params"]["event"] == "quit" => break,
            _ => {}
        }
    }
    let until = std::time::Instant::now() + Duration::from_secs(20);
    while inflight.load(Ordering::SeqCst) > 0 && std::time::Instant::now() < until {
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn text<'a>(ctx: &'a Value, key: &str) -> Result<&'a str, String> {
    ctx[key].as_str().ok_or_else(|| format!("no `{key}`"))
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect()).unwrap_or_default()
}

/// The `status` answer.
pub fn status_json(st: &status::Status) -> Value {
    let files: serde_json::Map<String, Value> = st
        .files
        .iter()
        .map(|(p, m)| (p.clone(), json!({"kind": m.kind.name(), "staged": m.staged, "unstaged": m.unstaged, "x": m.x.to_string(), "y": m.y.to_string()})))
        .collect();
    json!({
        "op": "status", "repo": true, "root": st.root, "branch": st.branch, "oid": st.oid, "upstream": st.upstream,
        "ahead": st.ahead, "behind": st.behind, "state": st.state,
        "counts": {"staged": st.staged, "changed": st.changed, "untracked": st.untracked, "conflicted": st.conflicted},
        "files": files,
    })
}

/// The server a call is about, when it says (`ssh`: `{"host", "user"?, "port"?, "key"?}`): git then runs there.
fn runner_of(ctx: &Value) -> Arc<dyn runner::Runner> {
    match ctx["ssh"]["host"].as_str() {
        Some(host) => Arc::new(runner::Ssh(runner::SshTarget {
            host: host.to_string(),
            user: ctx["ssh"]["user"].as_str().map(str::to_string),
            port: ctx["ssh"]["port"].as_u64().map(|p| p as u16),
            key: ctx["ssh"]["key"].as_str().map(str::to_string),
        })),
        None => Arc::new(runner::Local),
    }
}

/// The colour of one of the two letters of a mark (`col` 0: the index, 1: the work tree).
fn letter_colour(c: char, col: u8) -> &'static str {
    match (c, col) {
        (' ', _) => "",
        ('U', _) | ('D', 1) => "red",
        (_, 0) => "green",
        ('M', _) => "yellow",
        ('A', _) => "green",
        ('R', _) => "cyan",
        ('?', _) => "teal",
        ('!', _) => "comment",
        _ => "yellow",
    }
}

/// What the tool shows of a folder: the marks column (`cells`, with `flags` per entry for the `when` of the buttons), the branch and the counts
/// in the path bar (`bar`), and the folder as a whole (`state`: flags and the work tree's root). A folder in no work tree (or a server git cannot be
/// read on) gets empty answers, so what was shown before is cleared — and nothing is complained about.
fn view(ctx: &Value, run: Arc<dyn runner::Runner>) -> Value {
    let cfg = &ctx["config"];
    let dir = ctx["dir"].as_str().unwrap_or("");
    let (column, remote) = (&ctx["column"], &ctx["remote"]);
    let nothing = json!([
        {"op": "cells", "column": column, "dir": dir, "remote": remote, "cells": {}, "flags": {}},
        {"op": "bar", "bar": "git", "dir": dir, "remote": remote, "segments": []},
        {"op": "state", "dir": dir, "remote": remote, "flags": []},
    ]);
    let ignored = cfg["ignored"].as_bool().unwrap_or(true);
    let untracked = cfg["untracked"].as_str().unwrap_or("normal");
    let Ok(Some(st)) = status::read_in(run, dir, ignored, untracked) else { return nothing };
    let base = st.base(std::path::Path::new(dir)).unwrap_or_default();
    let dirs = strings(&ctx["dirs"]);
    let mut cells = serde_json::Map::new();
    let mut flags = serde_json::Map::new();
    for name in strings(&ctx["names"]) {
        let is_dir = dirs.contains(&name);
        let m = st.mark(&base, &name, is_dir);
        let (a, b) = (m.map_or(' ', |m| m.x), m.map_or(' ', |m| m.y));
        cells.insert(name.clone(), json!([[a.to_string(), letter_colour(a, 0)], [b.to_string(), letter_colour(b, 1)]]));
        let f = ui::flags_of(&st, &base, &name, is_dir);
        flags.insert(name, json!(f));
    }
    // the path bar: the branch, a merge / rebase in progress, ahead / behind, and the counts
    let branch = match &st.branch {
        Some(b) => b.clone(),
        None => format!("({})", st.oid.chars().take(7).collect::<String>()),
    };
    let mut seg: Vec<Value> = vec![json!([format!(" \u{e0a0} {branch}"), "magenta"])];
    if let Some(state) = &st.state {
        seg.push(json!([format!(" {}", state.to_uppercase()), "red bold"]));
    }
    if st.ahead > 0 {
        seg.push(json!([format!(" ↑{}", st.ahead), "green"]));
    }
    if st.behind > 0 {
        seg.push(json!([format!(" ↓{}", st.behind), "red"]));
    }
    for (n, sign, colour) in [(st.staged, "+", "green"), (st.changed, "~", "yellow"), (st.untracked, "?", "teal"), (st.conflicted, "!", "red")] {
        if n > 0 {
            seg.push(json!([format!(" {sign}{n}"), colour]));
        }
    }
    // the folder as a whole
    let mut sf: Vec<&str> = vec!["repo"];
    for (on, f) in [
        (st.staged > 0, "staged"),
        (st.changed + st.untracked + st.conflicted > 0, "unstaged-any"),
        (st.oid == "(initial)", "initial"),
        (st.upstream.is_some(), "upstream"),
        (st.dirty(), "dirty"),
        (st.state.is_some(), "busy"),
    ] {
        if on {
            sf.push(f);
        }
    }
    json!([
        {"op": "cells", "column": column, "dir": dir, "remote": remote, "cells": cells, "flags": flags},
        {"op": "bar", "bar": "git", "dir": dir, "remote": remote, "segments": seg},
        {"op": "state", "dir": dir, "remote": remote, "flags": sf},
    ])
}

/// The answer to one call.
pub fn answer(ctx: &Value) -> Result<Value, String> {
    let call = ctx["call"].as_str().unwrap_or("");
    if ["diff_files", "changes", "go"].contains(&call) {
        return button_answer(call, ctx);
    }
    if let Some(a) = ui::answer(call, ctx) {
        return a;
    }
    let cfg = &ctx["config"];
    let dir = text(ctx, "dir")?;
    let run = runner_of(ctx);
    if call == "view" {
        return Ok(view(ctx, run));
    }
    if call == "status" {
        let ignored = cfg["ignored"].as_bool().unwrap_or(true);
        let untracked = cfg["untracked"].as_str().unwrap_or("normal");
        return Ok(match status::read_in(run, dir, ignored, untracked)? {
            Some(st) => status_json(&st),
            None => json!({"op": "status", "repo": false}),
        });
    }
    let root = ops::root_in(&run, dir).ok_or_else(|| "not in a git work tree".to_string())?;
    let r = &ops::Repo { root, run };
    Ok(match call {
        "branches" => json!({"op": "branches", "branches": ops::branches(r)?.iter().map(|b| json!({"name": b.name, "current": b.current, "remote": b.remote, "upstream": b.upstream, "subject": b.subject})).collect::<Vec<_>>()}),
        "log" => {
            let n = ctx["n"].as_u64().or(cfg["log"].as_u64()).unwrap_or(200) as usize;
            let commits = ops::log(r, ctx["path"].as_str(), n)?;
            json!({"op": "log", "commits": commits.iter().map(|c| json!({"hash": c.hash, "short": c.short, "author": c.author, "date": c.date, "ago": c.ago, "subject": c.subject})).collect::<Vec<_>>()})
        }
        "diff" => json!({"op": "text", "text": ops::diff(r, ctx["path"].as_str(), ctx["staged"].as_bool().unwrap_or(false), ctx["untracked"].as_bool().unwrap_or(false))?}),
        "show" => json!({"op": "text", "text": ops::show(r, text(ctx, "rev")?)?}),
        "blame" => json!({"op": "text", "text": ops::blame(r, text(ctx, "path")?)?}),
        "stashes" => json!({"op": "stashes", "stashes": ops::stash_list(r)?.iter().map(|(rf, s)| json!({"ref": rf, "subject": s})).collect::<Vec<_>>()}),
        "tags" => json!({"op": "tags", "tags": ops::tags(r)?.iter().map(|t| json!({"name": t.name, "hash": t.hash, "subject": t.subject})).collect::<Vec<_>>()}),
        "submodules" => json!({"op": "submodules", "submodules": ops::submodules(r)?.iter().map(|s| json!({"path": s.path, "hash": s.hash, "state": s.state.to_string(), "describe": s.describe})).collect::<Vec<_>>()}),
        "run" => {
            let paths = strings(&ctx["paths"]);
            let name = ctx["name"].as_str().unwrap_or("");
            let message = ctx["message"].as_str().unwrap_or("");
            let unit = |x: Result<(), String>| x.map(|_| String::new());
            let said = match text(ctx, "verb")? {
                "stage" => unit(ops::stage(r, &paths)),
                "unstage" => unit(ops::unstage(r, &paths)),
                "discard" => unit(ops::discard(r, &paths, &strings(&ctx["untracked"]))),
                "stage_all" => unit(ops::stage_all(r)),
                "unstage_all" => unit(ops::unstage_all(r)),
                "commit" => ops::commit(r, message, false),
                "amend" => ops::commit(r, message, true),
                "checkout" => {
                    let b = ops::branches(r)?.into_iter().find(|b| b.name == name).ok_or_else(|| format!("no branch {name}"))?;
                    unit(ops::checkout(r, &b))
                }
                "new_branch" => unit(ops::new_branch(r, name)),
                "delete_branch" => unit(ops::delete_branch(r, name, ctx["force"].as_bool().unwrap_or(false))),
                "merge" => ops::merge(r, name),
                "rebase" => ops::rebase(r, name),
                "continue" => ops::continue_(r, text(ctx, "state")?),
                "abort" => unit(ops::abort(r, text(ctx, "state")?)),
                "resolve" => unit(ops::resolve(r, &paths, ctx["theirs"].as_bool().unwrap_or(false))),
                "tag" => unit(ops::new_tag(r, name, message)),
                "delete_tag" => unit(ops::delete_tag(r, name)),
                "push_tag" => ops::push_tag(r, name),
                "checkout_tag" => unit(ops::checkout_tag(r, name)),
                "submodule_update" => ops::submodule_update(r, ctx["path"].as_str()),
                "fetch" => ops::fetch(r),
                "pull" => ops::pull(r),
                "push" => ops::push(r),
                "stash_push" => ops::stash_push(r, message, ctx["untracked"].as_bool().unwrap_or(false)),
                "stash_pop" => unit(ops::stash_pop(r, name)),
                "stash_apply" => unit(ops::stash_apply(r, name)),
                "stash_drop" => unit(ops::stash_drop(r, name)),
                "ignore" => unit(paths.iter().try_for_each(|p| ops::ignore(r, p, p.ends_with('/')))),
                "remove" => unit(ops::remove(r, &paths)),
                "rename" => match paths.as_slice() {
                    [a, b] => unit(ops::rename(r, a, b)),
                    _ => Err("rename wants two paths".into()),
                },
                "hunk" => {
                    // `diff` is the text of a diff (of the work tree, or — `staged` — of the index), `index` which hunk of it
                    let lines: Vec<String> = text(ctx, "diff")?.lines().map(str::to_string).collect();
                    let hs = ops::hunks(&lines);
                    let idx = ctx["index"].as_u64().unwrap_or(0) as usize;
                    let patch = ops::hunk_patch(&lines, &hs, idx).ok_or_else(|| "no such hunk".to_string())?;
                    let what = match text(ctx, "do")? {
                        "stage" => ops::HunkDo::Stage,
                        "unstage" => ops::HunkDo::Unstage,
                        "discard" => ops::HunkDo::Discard,
                        d => return Err(format!("no such hunk action: {d}")),
                    };
                    unit(ops::apply_hunk(r, &patch, what))
                }
                v => Err(format!("no such verb: {v}")),
            }?;
            json!({"op": "done", "text": said})
        }
        _ => return Err(format!("no such function: {call}")),
    })
}

/// The answer to a call a TOOL's button makes (`diff_files`, `changes`, `go`): the context is the tool's — the file under the cursor, the marked
/// files, the folder, the other panel's folder, `arg` — and the answer is instructions for it (`menu`, `text`, `cd`, `select`, `message`).
fn button_answer(call: &str, ctx: &Value) -> Result<Value, String> {
    let path = |k: &str| ctx[k].as_str().map(std::path::PathBuf::from);
    let message = |t: &str| json!([{"op": "message", "text": t}]);
    match call {
        // two files: the two marked, or the one under the cursor and the file of that name in the other panel
        "diff_files" => {
            let files: Vec<std::path::PathBuf> = ctx["files"].as_array().map(|a| a.iter().filter_map(|f| f.as_str().map(Into::into)).collect()).unwrap_or_default();
            let (a, b) = match files.as_slice() {
                [a, b] => (a.clone(), b.clone()),
                [a] => {
                    let Some(other) = path("other_dir") else { return Ok(message("diff: mark two files, or put the other panel on a folder with the same file")) };
                    let b = other.join(a.file_name().unwrap_or_default());
                    if !b.is_file() {
                        return Ok(message(&format!("diff: {} has no {} to compare with (or mark two files)", other.display(), a.file_name().unwrap_or_default().to_string_lossy())));
                    }
                    (a.clone(), b)
                }
                _ => return Ok(message("diff: mark two files, or put the cursor on one and the other panel on a folder with the same file")),
            };
            if !a.is_file() || !b.is_file() {
                return Ok(message("diff: files only"));
            }
            let text = ops::diff_files(&a, &b)?;
            if text.trim().is_empty() {
                return Ok(message("the two files are the same"));
            }
            Ok(json!([{"op": "text", "title": format!("diff {}  {}", a.display(), b.display()), "text": text}]))
        }
        // what is changed or new in the work tree the folder is in: a list to pick from, Enter goes to the file
        "changes" => {
            let dir = path("dir").ok_or("no folder")?;
            let Some(st) = status::read(&dir, false, "all")? else { return Ok(message("not in a git work tree")) };
            let list = ops::changed(&st);
            if list.is_empty() {
                return Ok(message("nothing is changed"));
            }
            // the two letters in the colour of the change, then the path
            let colour = |code: &str| {
                if code == "??" { "teal" } else if code.contains('U') || code.contains('D') { "red" } else if code.contains('M') { "yellow" } else if code.contains('A') { "green" } else if code.contains('R') { "cyan" } else { "comment" }
            };
            let items: Vec<Value> = list
                .iter()
                .map(|(p, code)| json!({"label": format!("{code} {p}"), "call": "go", "arg": st.abs(p).to_string_lossy(), "cells": [[code, colour(code)], [p, ""]]}))
                .collect();
            Ok(json!([{"op": "menu", "title": format!("Changes · {}", list.len()), "items": items}]))
        }
        // Enter on a change: the panel goes to the folder of the file and stands on it
        "go" => {
            let target = ctx["arg"].as_str().map(std::path::PathBuf::from).ok_or("no path")?;
            let (Some(parent), Some(name)) = (target.parent(), target.file_name()) else { return Ok(message("nowhere to go")) };
            Ok(json!([{"op": "cd", "path": parent.to_string_lossy()}, {"op": "select", "name": name.to_string_lossy()}]))
        }
        _ => Err(format!("no such function: {call}")),
    }
}

/// The git plugin as the prompt of a secret: `git` and `ssh` call it (through the script of [`runner::askpass_env`]) with the question as the
/// argument, and read the answer from stdout. Returns the exit status.
pub fn askpass(prompt: &str) -> i32 {
    let title = if prompt.to_lowercase().contains("passphrase") { "lvim — SSH key" } else { "lvim — git" };
    match lvim_fuzzy_plugin::askpass::ask(&json!({}), title, prompt) {
        Ok(secret) => {
            println!("{secret}");
            0
        }
        Err(_) => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_buttons_of_the_tool_diff_two_files_list_the_changes_and_go_to_one() {
        let d = std::env::temp_dir().join(format!("lvim-git-buttons-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("one")).unwrap();
        std::fs::create_dir_all(d.join("two")).unwrap();
        std::fs::write(d.join("one/f.txt"), "a\nb\n").unwrap();
        std::fs::write(d.join("two/f.txt"), "a\nc\n").unwrap();
        std::fs::write(d.join("same.txt"), "x\n").unwrap();
        std::fs::write(d.join("same2.txt"), "x\n").unwrap();
        let s = |v: &Value| v.to_string();
        // the file under the cursor and the one of that name in the other panel
        let r = answer(&json!({"call": "diff_files", "files": [d.join("one/f.txt")], "other_dir": d.join("two"), "dir": d.join("one")})).unwrap();
        assert_eq!(r[0]["op"], "text");
        assert!(s(&r[0]["text"]).contains("+c") && s(&r[0]["text"]).contains("-b"), "{r}");
        // the whole file, not only the changes: a line that is the same in both is there too
        let many: String = (1..=40).map(|i| format!("line {i}\n")).collect();
        std::fs::write(d.join("one/long.txt"), &many).unwrap();
        std::fs::write(d.join("two/long.txt"), many.replace("line 20\n", "line twenty\n")).unwrap();
        let r = answer(&json!({"call": "diff_files", "files": [d.join("one/long.txt")], "other_dir": d.join("two"), "dir": d.join("one")})).unwrap();
        let text = s(&r[0]["text"]);
        assert!(text.contains("line 1\\n") && text.contains("line 40") && text.contains("twenty"), "{text}");
        // two marked files
        let r = answer(&json!({"call": "diff_files", "files": [d.join("same.txt"), d.join("same2.txt")], "dir": d})).unwrap();
        assert_eq!(r[0]["op"], "message");
        assert!(s(&r[0]["text"]).contains("same"));
        // nothing to compare with: said, not an error
        let r = answer(&json!({"call": "diff_files", "files": [d.join("one/f.txt")], "other_dir": d.join("nowhere"), "dir": d})).unwrap();
        assert_eq!(r[0]["op"], "message");
        // the changes of a work tree
        let g = |a: &[&str]| assert!(std::process::Command::new("git").arg("-C").arg(&d).args(a).output().unwrap().status.success());
        g(&["init", "-q", "-b", "main"]);
        g(&["config", "user.email", "t@example.org"]);
        g(&["config", "user.name", "T"]);
        g(&["add", "same.txt"]);
        g(&["commit", "-q", "-m", "one"]);
        std::fs::write(d.join("same.txt"), "changed\n").unwrap();
        let r = answer(&json!({"call": "changes", "dir": d.join("one")})).unwrap();
        assert_eq!(r[0]["op"], "menu");
        let items = r[0]["items"].as_array().unwrap();
        assert!(items.iter().any(|i| i["label"].as_str().unwrap() == " M same.txt"), "{r}");
        assert!(items.iter().any(|i| i["label"].as_str().unwrap().starts_with("??") && i["label"].as_str().unwrap().contains("one/f.txt")), "{r}");
        assert_eq!(items[0]["call"], "go");
        // Enter: the folder and the name
        let arg = items.iter().find(|i| i["label"].as_str().unwrap().contains("one/f.txt")).unwrap()["arg"].as_str().unwrap().to_string();
        let r = answer(&json!({"call": "go", "arg": arg})).unwrap();
        assert_eq!(r[0]["op"], "cd");
        assert_eq!(r[1], json!({"op": "select", "name": "f.txt"}));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn calls_are_answered_as_json() {
        let d = std::env::temp_dir().join(format!("lvim-git-plugin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let g = |a: &[&str]| assert!(std::process::Command::new("git").arg("-C").arg(&d).args(a).output().unwrap().status.success());
        g(&["init", "-q", "-b", "main"]);
        g(&["config", "user.email", "t@example.org"]);
        g(&["config", "user.name", "T"]);
        g(&["config", "commit.gpgsign", "false"]);
        std::fs::write(d.join("a.txt"), "a\n").unwrap();
        let ask = |v: Value| answer(&v);
        let dir = d.to_string_lossy().to_string();
        let s = ask(json!({"call": "status", "dir": dir, "config": {}})).unwrap();
        assert_eq!(s["repo"], true);
        assert_eq!(s["files"]["a.txt"]["kind"], "untracked");
        ask(json!({"call": "run", "dir": dir, "verb": "stage", "paths": ["a.txt"]})).unwrap();
        ask(json!({"call": "run", "dir": dir, "verb": "commit", "message": "one"})).unwrap();
        let l = ask(json!({"call": "log", "dir": dir})).unwrap();
        assert_eq!(l["commits"][0]["subject"], "one");
        assert!(ask(json!({"call": "nope", "dir": dir})).is_err());
        let outside = ask(json!({"call": "status", "dir": "/proc", "config": {}})).unwrap();
        assert_eq!(outside["repo"], false);
        let _ = std::fs::remove_dir_all(&d);
    }
}
