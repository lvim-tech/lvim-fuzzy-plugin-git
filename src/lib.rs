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

use std::io::{BufRead, Write};
use std::path::Path;
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

/// The answer to one call.
pub fn answer(ctx: &Value) -> Result<Value, String> {
    let call = ctx["call"].as_str().unwrap_or("");
    let cfg = &ctx["config"];
    let dir = text(ctx, "dir")?;
    let run = runner_of(ctx);
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

// ----- shipping inside the tools -----

/// The arguments a tool is started with to BE this plugin: `<tool> --lvim-plugin git`.
pub const PLUGIN_ARGS: [&str; 2] = ["--lvim-plugin", "git"];

/// The plugin's `plugin.toml` as shipped.
pub const MANIFEST: &str = include_str!("../plugin.toml");

/// For a tool's `main`: if it was started to be this plugin (`--lvim-plugin git`), be it (never returns).
pub fn run_if_asked(args: &[String]) {
    if args.first().map(String::as_str) == Some(PLUGIN_ARGS[0]) && args.get(1).map(String::as_str) == Some(PLUGIN_ARGS[1]) {
        // `askpass PROMPT`: asked for a secret by git or ssh
        if args.get(2).map(String::as_str) == Some("askpass") {
            std::process::exit(askpass(args.get(3).map(String::as_str).unwrap_or("")));
        }
        run();
        std::process::exit(0);
    }
}

/// Write the plugin's `plugin.toml` into the shared plugins directory `root`, with `exe` answering as the plugin (see
/// `lvim_fuzzy_plugin::bundled`): a tool calls this only when its config says `[plugins.lvim-fuzzy-plugin-git] install = …`. Silent: a directory that cannot be written just means no git plugin.
pub fn ensure_installed(root: &Path, exe: &Path) {
    let b = lvim_fuzzy_plugin::bundled::Bundled { name: "lvim-fuzzy-plugin-git", version: env!("CARGO_PKG_VERSION"), manifest: MANIFEST };
    let _ = lvim_fuzzy_plugin::bundled::ensure(root, &b, exe, &PLUGIN_ARGS.join(" "));
}

#[cfg(test)]
mod tests {
    use super::*;

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
