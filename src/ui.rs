//! The tool's buttons: what each `[actions.*]` of `plugin.toml` calls. A button's call gets the tool's context (`dir`, `names` — the marked entries
//! or the one under the cursor —, `dirs`, `remote` / `ssh` on a server, `arg`, and `data`, the note this plugin left itself) and answers with
//! instructions for the tool: `notify`, `reload`, `refresh`, `input`, `confirm`, `menu` (items with `keys`), `text` (with `anchors` and `keys`),
//! `cd`, `close`. Nothing of git is in the tool.

use crate::ops::{self, Repo};
use crate::runner;
use crate::status::{self, Status};
use serde_json::{Value, json};
use std::sync::Arc;

type Ans = Result<Value, String>;

/// A target of a command: an entry of the folder with its path from the work tree's top and what git says of it.
struct Target {
    name: String,
    rel: String,
    dir: bool,
    flags: Vec<&'static str>,
}

impl Target {
    fn has(&self, f: &str) -> bool {
        self.flags.contains(&f)
    }
}

/// The facts about an entry that the buttons' `when` and the commands ask: `unstaged`, `staged`, `untracked`, `conflicted`, `tracked`, `changed`, `file`, `dir`.
pub fn flags_of(st: &Status, base: &str, name: &str, is_dir: bool) -> Vec<&'static str> {
    let m = st.mark(base, name, is_dir);
    let mut f: Vec<&str> = vec![if is_dir { "dir" } else { "file" }];
    if m.is_some_and(|m| m.y != ' ' && m.y != '!') {
        f.push("unstaged");
    }
    if m.is_some_and(|m| m.x != ' ') {
        f.push("staged");
    }
    if m.is_some_and(|m| m.kind == status::Kind::Untracked) {
        f.push("untracked");
    }
    if m.is_some_and(|m| m.kind == status::Kind::Conflict) {
        f.push("conflicted");
    }
    if m.is_none_or(|m| !matches!(m.kind, status::Kind::Untracked | status::Kind::Ignored)) {
        f.push("tracked");
    }
    if m.is_some_and(|m| m.kind != status::Kind::Ignored) {
        f.push("changed");
    }
    f
}

struct Cx {
    repo: Repo,
    base: String,
    st: Status,
    names: Vec<String>,
    dirs: Vec<String>,
    arg: String,
    data: String,
}

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

fn strs(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect()).unwrap_or_default()
}

fn cx(ctx: &Value) -> Result<Cx, String> {
    let dir = ctx["dir"].as_str().ok_or("no folder")?;
    let run = runner_of(ctx);
    let cfg = &ctx["config"];
    let st = status::read_in(run, dir, cfg["ignored"].as_bool().unwrap_or(true), cfg["untracked"].as_str().unwrap_or("normal"))?.ok_or("not in a git work tree")?;
    let repo = st.repo.clone().ok_or("not in a git work tree")?;
    let base = st.base(std::path::Path::new(dir)).unwrap_or_default();
    Ok(Cx { repo, base, st, names: strs(&ctx["names"]), dirs: strs(&ctx["dirs"]), arg: ctx["arg"].as_str().unwrap_or("").to_string(), data: ctx["data"].as_str().unwrap_or("").to_string() })
}

impl Cx {
    fn rel(&self, name: &str) -> String {
        if self.base.is_empty() { name.to_string() } else { format!("{}/{name}", self.base) }
    }

    fn targets(&self) -> Vec<Target> {
        self.names
            .iter()
            .map(|n| {
                let dir = self.dirs.contains(n);
                Target { name: n.clone(), rel: self.rel(n), dir, flags: flags_of(&self.st, &self.base, n, dir) }
            })
            .collect()
    }

    fn rels(&self, f: impl Fn(&Target) -> bool) -> Vec<String> {
        self.targets().iter().filter(|t| f(t)).map(|t| t.rel.clone()).collect()
    }
}

fn notify(level: &str, text: &str) -> Value {
    json!({"op": "notify", "level": level, "title": "git", "text": text})
}

/// The command is done: what it said (or `done`), and the folders read again.
fn done(label: &str, said: &str) -> Ans {
    let line = said.lines().next().unwrap_or("").trim();
    let text = if line.is_empty() { format!("{label}: done") } else { format!("{label}: {line}") };
    Ok(json!([notify("success", &text), {"op": "reload"}]))
}

fn unit(label: &str, r: Result<(), String>) -> Ans {
    r.and_then(|_| done(label, ""))
}

fn say(level: &str, text: &str) -> Ans {
    Ok(json!([notify(level, text)]))
}

fn list(items: &[String], more: usize) -> String {
    let mut s: Vec<String> = items.iter().take(8).map(|p| format!("  {p}")).collect();
    if items.len() > 8 {
        s.push(format!("  … and {} more", items.len() - 8));
    }
    let _ = more;
    s.join("\n")
}

fn key(k: &str, label: &str, call: &str) -> Value {
    json!({"key": k, "label": label, "call": call})
}

fn key_confirm(k: &str, label: &str, call: &str, text: &str) -> Value {
    json!({"key": k, "label": label, "call": call, "confirm": text})
}

/// A diff on the whole screen: the hunks are its anchors, the keys stage / unstage / give up the current one, Tab shows the other side.
fn diff_text(path: &str, staged: bool, untracked: bool, text: &str, cur: usize) -> Value {
    let lines: Vec<String> = text.lines().map(str::to_string).collect();
    let hs = ops::hunks(&lines);
    let anchors: Vec<usize> = hs.iter().map(|h| h.start).collect();
    let cur = cur.min(anchors.len().saturating_sub(1));
    let mut keys = vec![key("\t", if staged { "unstaged" } else { "staged" }, "diff_toggle")];
    if !anchors.is_empty() {
        if staged {
            keys.push(key("u", "unstage hunk", "hunk_unstage"));
        } else {
            keys.push(key("s", "stage hunk", "hunk_stage"));
            if !untracked {
                keys.push(key_confirm("d", "discard hunk", "hunk_discard", "This change is given up for good."));
            }
        }
    } else if !staged && untracked {
        keys.push(key("s", "stage file", "hunk_stage"));
    }
    json!({
        "op": "text",
        "title": format!("{} {path}  ·  hunk {}/{}", if staged { "staged diff" } else { "diff" }, cur + 1, anchors.len().max(1)),
        "lines": lines, "diff": true, "anchors": anchors, "current": cur, "keys": keys,
        "data": json!({"path": path, "staged": staged, "untracked": untracked}).to_string(),
    })
}

/// `{"anchor", "data"}` the tool sends back from a key of a text.
fn from_text(c: &Cx) -> (usize, Value) {
    let a: Value = serde_json::from_str(&c.arg).unwrap_or_default();
    let d: Value = serde_json::from_str(a["data"].as_str().unwrap_or("")).unwrap_or_default();
    (a["anchor"].as_u64().unwrap_or(0) as usize, d)
}

fn show_diff(c: &Cx, path: &str, staged: bool, untracked: bool, cur: usize) -> Ans {
    let t = ops::diff(&c.repo, Some(path), staged, untracked)?;
    if t.trim().is_empty() {
        return Ok(json!([{"op": "close"}, notify("info", &format!("{path}: no more changes here")), {"op": "reload"}]));
    }
    Ok(json!([diff_text(path, staged, untracked, &t, cur), {"op": "refresh"}]))
}

/// The pieces of a `git blame` line: the hash, the author, the date, the number and the code.
fn blame_spans(line: &str) -> Vec<(String, &'static str)> {
    let Some(open) = line.find(" (") else { return Vec::new() };
    let Some(close) = line[open..].find(") ").map(|i| open + i + 1).or_else(|| line.ends_with(')').then_some(line.len())) else { return Vec::new() };
    let hash = &line[..open];
    let inside = &line[open + 2..close - 1];
    let toks: Vec<&str> = inside.split_whitespace().collect();
    if toks.len() < 3 {
        return Vec::new();
    }
    let (num, date) = (toks[toks.len() - 1], toks[toks.len() - 2]);
    vec![
        (hash.to_string(), "yellow"),
        (" (".into(), "comment"),
        (toks[..toks.len() - 2].join(" "), "purple"),
        (" ".into(), ""),
        (date.to_string(), "teal"),
        (" ".into(), ""),
        (num.to_string(), "comment"),
        (")".into(), "comment"),
        (line[close..].to_string(), ""),
    ]
}

fn short(h: &str) -> &str {
    &h[..h.len().min(8)]
}

/// The answer to a button's call, `None` when the call is not one of these.
pub fn answer(call: &str, ctx: &Value) -> Option<Ans> {
    const CALLS: &[&str] = &[
        "stage", "unstage", "discard", "do_discard", "remove", "do_remove", "ignore", "rename", "do_rename", "stage_all", "unstage_all", "commit", "amend", "do_commit", "ui_diff", "diff_toggle",
        "hunk_stage", "hunk_unstage", "hunk_discard", "blame", "ui_log", "ui_log_all", "ui_show", "ui_branches", "switch", "new_branch", "do_new_branch", "branch_delete", "merge", "rebase", "merge_run",
        "rebase_run", "continue", "abort", "do_abort", "resolve", "resolve_run", "fetch", "pull", "push", "stash", "do_stash", "ui_stashes", "stash_pop", "stash_apply", "stash_drop", "stash_show",
        "tags", "tag_show", "tag_switch", "tag_push", "tag_delete", "new_tag", "do_tag", "modules", "module_go", "modules_update", "module_update", "modules_update_all", "refresh",
    ];
    if !CALLS.contains(&call) {
        return None;
    }
    if call == "refresh" {
        return Some(Ok(json!([{"op": "refresh"}, {"op": "reload"}])));
    }
    Some(cx(ctx).and_then(|c| run(call, &c, ctx)))
}

fn run(call: &str, c: &Cx, ctx: &Value) -> Ans {
    let r = &c.repo;
    let state = ops::state(r);
    let first = || c.targets().into_iter().next().ok_or_else(|| "nothing under the cursor".to_string());
    match call {
        "stage" => {
            let p = c.rels(|t| t.has("unstaged"));
            unit("add", ops::stage(r, &p))
        }
        "unstage" => {
            let p = c.rels(|t| t.has("staged"));
            unit("unstage", ops::unstage(r, &p))
        }
        "discard" => {
            let tracked = c.rels(|t| t.has("unstaged") && !t.has("untracked"));
            let untracked = c.rels(|t| t.has("untracked"));
            let all: Vec<String> = tracked.iter().chain(&untracked).cloned().collect();
            let mut text = format!("The changes of these are given up for good:\n{}", list(&all, 0));
            if !untracked.is_empty() {
                text.push_str("\nUntracked ones are deleted.");
            }
            Ok(json!([{"op": "confirm", "title": "Discard changes", "text": text, "then": "do_discard", "data": json!({"tracked": tracked, "untracked": untracked}).to_string()}]))
        }
        "do_discard" => {
            let d: Value = serde_json::from_str(&c.arg).unwrap_or_default();
            unit("discard", ops::discard(r, &strs(&d["tracked"]), &strs(&d["untracked"])))
        }
        "remove" => {
            let p = c.rels(|t| t.has("tracked"));
            let text = format!("These are removed from the folder and from git:\n{}", list(&p, 0));
            Ok(json!([{"op": "confirm", "title": "git rm", "text": text, "then": "do_remove", "data": json!(p).to_string()}]))
        }
        "do_remove" => {
            let p: Vec<String> = strs(&serde_json::from_str::<Value>(&c.arg).unwrap_or_default());
            unit("rm", ops::remove(r, &p))
        }
        "ignore" => {
            let ts: Vec<Target> = c.targets().into_iter().filter(|t| t.has("untracked")).collect();
            for t in &ts {
                ops::ignore(r, &t.rel, t.dir)?;
            }
            done("ignore", &format!("{} added to .gitignore", ts.len()))
        }
        "rename" => {
            let t = first()?;
            Ok(json!([{"op": "input", "prompt": "git mv — the new name", "value": t.name, "then": "do_rename", "data": json!({"from": t.rel, "base": c.base}).to_string()}]))
        }
        "do_rename" => {
            let name = c.arg.trim();
            if name.is_empty() || name.contains('/') {
                return say("warn", "a name in the same folder (no /)");
            }
            let d: Value = serde_json::from_str(&c.data).unwrap_or_default();
            let base = d["base"].as_str().unwrap_or("");
            let to = if base.is_empty() { name.to_string() } else { format!("{base}/{name}") };
            unit("mv", ops::rename(r, d["from"].as_str().unwrap_or(""), &to))
        }
        "stage_all" => unit("add all", ops::stage_all(r)),
        "unstage_all" => unit("unstage all", ops::unstage_all(r)),
        "commit" => Ok(json!([{"op": "input", "prompt": "Commit message", "value": "", "then": "do_commit", "data": "commit"}])),
        "amend" => Ok(json!([{"op": "input", "prompt": "Amend the last commit — its message", "value": ops::last_message(r), "then": "do_commit", "data": "amend"}])),
        "do_commit" => {
            let said = ops::commit(r, c.arg.trim(), c.data == "amend")?;
            done(if c.data == "amend" { "amend" } else { "commit" }, &said)
        }
        "ui_diff" => {
            let t = first()?;
            let (untracked, staged) = (t.has("untracked"), !t.has("unstaged") && t.has("staged"));
            show_diff(c, &t.rel, staged, untracked, 0)
        }
        "diff_toggle" => {
            let (_, d) = from_text(c);
            show_diff(c, d["path"].as_str().unwrap_or(""), !d["staged"].as_bool().unwrap_or(false), false, 0)
        }
        "hunk_stage" | "hunk_unstage" | "hunk_discard" => {
            let (idx, d) = from_text(c);
            let (path, staged, untracked) = (d["path"].as_str().unwrap_or("").to_string(), d["staged"].as_bool().unwrap_or(false), d["untracked"].as_bool().unwrap_or(false));
            let text = ops::diff(r, Some(&path), staged, untracked)?;
            let lines: Vec<String> = text.lines().map(str::to_string).collect();
            let hs = ops::hunks(&lines);
            // a new file has no hunks to choose: the whole file goes in
            if call == "hunk_stage" && untracked {
                ops::stage(r, std::slice::from_ref(&path))?;
                return Ok(json!([{"op": "close"}, notify("success", &format!("add: {path}")), {"op": "reload"}]));
            }
            let patch = ops::hunk_patch(&lines, &hs, idx).ok_or("no such hunk")?;
            let what = match call {
                "hunk_stage" => ops::HunkDo::Stage,
                "hunk_unstage" => ops::HunkDo::Unstage,
                _ => ops::HunkDo::Discard,
            };
            ops::apply_hunk(r, &patch, what)?;
            show_diff(c, &path, staged, untracked, idx)
        }
        "blame" => {
            let t = first()?;
            let text = ops::blame(r, &t.rel)?;
            let lines: Vec<Value> = text
                .lines()
                .map(|l| {
                    let sp = blame_spans(l);
                    if sp.is_empty() { json!(l) } else { json!(sp) }
                })
                .collect();
            Ok(json!([{"op": "text", "title": format!("blame {}", t.rel), "lines": lines}]))
        }
        "ui_log" | "ui_log_all" => {
            let path = (call == "ui_log").then(|| c.targets().first().map(|t| t.rel.clone()).unwrap_or_else(|| c.base.clone())).filter(|p| !p.is_empty());
            let n = ctx["config"]["log"].as_u64().unwrap_or(200).max(300) as usize;
            let commits = ops::log(r, path.as_deref(), n)?;
            let items: Vec<Value> = commits
                .iter()
                .map(|m| {
                    json!({
                        "label": format!("{} {} {} {}", m.short, m.date, m.author, m.subject), "call": "ui_show", "arg": format!("{}\t{}", m.hash, m.subject), "enter": "show",
                        "cells": [[m.short, "blue"], [m.date, "teal"], [m.author.chars().take(14).collect::<String>(), "purple"], [m.subject, ""]],
                    })
                })
                .collect();
            Ok(json!([{"op": "menu", "title": path.map_or("Log".to_string(), |p| format!("Log · {p}")), "items": items}]))
        }
        "ui_show" => {
            let (hash, subject) = c.arg.split_once('\t').unwrap_or((&c.arg, ""));
            let t = ops::show(r, hash)?;
            Ok(json!([{"op": "text", "title": format!("{} {subject}", short(hash)), "text": t, "diff": true}]))
        }
        "ui_branches" => {
            let items: Vec<Value> = ops::branches(r)?
                .iter()
                .map(|b| {
                    let colour = if b.current { "green" } else if b.remote { "orange" } else { "blue" };
                    json!({
                        "label": format!("{} {} {}", b.name, b.upstream.clone().unwrap_or_default(), b.subject), "call": "switch", "arg": b.name, "enter": "switch",
                        "keys": [
                            key_confirm("d", "delete", "branch_delete", "Delete this branch (its commits that are in no other branch are lost)?"),
                            key("n", "new", "new_branch"), key("m", "merge", "merge_run"), key("r", "rebase", "rebase_run"),
                        ],
                        "cells": [[if b.current { "●" } else { " " }, "green"], [b.name, colour], [b.upstream.as_deref().map(|u| format!("→ {u}")).unwrap_or_default(), "comment"], [b.subject, ""]],
                    })
                })
                .collect();
            Ok(json!([{"op": "menu", "title": "Branches", "items": items}]))
        }
        "switch" => {
            let b = ops::branches(r)?.into_iter().find(|b| b.name == c.arg).ok_or_else(|| format!("no branch {}", c.arg))?;
            if b.current {
                return say("info", &format!("already on {}", b.name));
            }
            ops::checkout(r, &b)?;
            done("switch", &format!("on {}", b.name))
        }
        "new_branch" => Ok(json!([{"op": "input", "prompt": "New branch (switched to)", "value": "", "then": "do_new_branch"}])),
        "do_new_branch" => {
            let name = c.arg.trim();
            if name.is_empty() {
                return Ok(json!([]));
            }
            ops::new_branch(r, name)?;
            done("new branch", &format!("on {name}"))
        }
        "branch_delete" => {
            let b = ops::branches(r)?.into_iter().find(|b| b.name == c.arg).ok_or("no such branch")?;
            if b.current || b.remote {
                return say("warn", if b.current { "the branch you are on stays" } else { "only local branches are deleted here" });
            }
            ops::delete_branch(r, &b.name, true)?;
            done("branch -D", &format!("{} deleted", b.name))
        }
        "merge" | "rebase" => {
            let (go, title) = if call == "merge" { ("merge_run", "Merge into the current branch") } else { ("rebase_run", "Rebase the current branch onto") };
            let items: Vec<Value> = ops::branches(r)?
                .iter()
                .filter(|b| !b.current)
                .map(|b| json!({"label": format!("{} {}", b.name, b.subject), "call": go, "arg": b.name, "enter": call, "cells": [[b.name, if b.remote { "orange" } else { "blue" }], [b.subject, ""]]}))
                .collect();
            Ok(json!([{"op": "menu", "title": title, "items": items}]))
        }
        "merge_run" => {
            let said = ops::merge(r, &c.arg)?;
            done(&format!("merge {}", c.arg), &said)
        }
        "rebase_run" => {
            let said = ops::rebase(r, &c.arg)?;
            done(&format!("rebase {}", c.arg), &said)
        }
        "continue" => {
            let st = state.ok_or("nothing in progress")?;
            let said = ops::continue_(r, &st)?;
            done(&format!("{st} --continue"), &said)
        }
        "abort" => {
            let st = state.ok_or("nothing in progress")?;
            Ok(json!([{"op": "confirm", "title": "Give up", "text": format!("Abort the {st} in progress? Its changes are lost."), "then": "do_abort", "data": st}]))
        }
        "do_abort" => unit(&format!("{} --abort", c.arg), ops::abort(r, &c.arg)),
        "resolve" => {
            let paths = c.rels(|t| t.has("conflicted"));
            let item = |label: &str, how: u8| json!({"label": label, "call": "resolve_run", "arg": json!({"paths": paths, "how": how}).to_string(), "enter": "resolve", "cells": [[label, ""]]});
            Ok(json!([{"op": "menu", "title": "Resolve the conflict", "items": [
                item("Use ours (this branch's side)", 0), item("Use theirs (the other side)", 1), item("Mark as resolved (the file as it is now)", 2)]}]))
        }
        "resolve_run" => {
            let d: Value = serde_json::from_str(&c.arg).unwrap_or_default();
            let (paths, how) = (strs(&d["paths"]), d["how"].as_u64().unwrap_or(2));
            match how {
                0 | 1 => unit("resolve", ops::resolve(r, &paths, how == 1)),
                _ => unit("resolve", ops::stage(r, &paths)),
            }
        }
        "fetch" => done("fetch", &ops::fetch(r)?),
        "pull" => done("pull", &ops::pull(r)?),
        "push" => done("push", &ops::push(r)?),
        "stash" => Ok(json!([{"op": "input", "prompt": "Stash the changes — a message (or none)", "value": "", "then": "do_stash"}])),
        "do_stash" => done("stash", &ops::stash_push(r, c.arg.trim(), true)?),
        "ui_stashes" => {
            let items: Vec<Value> = ops::stash_list(r)?
                .iter()
                .map(|(rf, s)| {
                    json!({
                        "label": format!("{rf} {s}"), "call": "stash_pop", "arg": rf, "enter": "pop",
                        "keys": [key("a", "apply", "stash_apply"), key_confirm("d", "drop", "stash_drop", "Drop this stash for good?"), key("v", "show", "stash_show")],
                        "cells": [[rf, "yellow"], [s, ""]],
                    })
                })
                .collect();
            Ok(json!([{"op": "menu", "title": "Stashes", "items": items}]))
        }
        "stash_pop" => unit("stash pop", ops::stash_pop(r, &c.arg)),
        "stash_apply" => unit("stash apply", ops::stash_apply(r, &c.arg)),
        "stash_drop" => unit("stash drop", ops::stash_drop(r, &c.arg)),
        "stash_show" => Ok(json!([{"op": "text", "title": format!("stash {}", c.arg), "text": ops::show(r, &c.arg)?, "diff": true}])),
        "tags" => {
            let items: Vec<Value> = ops::tags(r)?
                .iter()
                .map(|t| {
                    json!({
                        "label": format!("{} {} {}", t.name, t.hash, t.subject), "call": "tag_show", "arg": t.name, "enter": "show",
                        "keys": [key("c", "switch", "tag_switch"), key("p", "push", "tag_push"), key("d", "delete", "tag_delete")],
                        "cells": [[t.name, "yellow"], [t.hash, "comment"], [t.subject, ""]],
                    })
                })
                .collect();
            Ok(json!([{"op": "menu", "title": "Tags", "items": items}]))
        }
        "tag_show" => Ok(json!([{"op": "text", "title": format!("tag {}", c.arg), "text": ops::show(r, &c.arg)?, "diff": true}])),
        "tag_switch" => {
            ops::checkout_tag(r, &c.arg)?;
            done("switch", &format!("at {}", c.arg))
        }
        "tag_push" => done("push tag", &ops::push_tag(r, &c.arg)?),
        "tag_delete" => unit("tag -d", ops::delete_tag(r, &c.arg)),
        "new_tag" => Ok(json!([{"op": "input", "prompt": "New tag at the head — a name (and, after a space, a message: an annotated tag)", "value": "", "then": "do_tag"}])),
        "do_tag" => {
            let v = c.arg.trim();
            let (name, msg) = v.split_once(' ').map_or((v, ""), |(n, m)| (n, m.trim()));
            if name.is_empty() {
                return Ok(json!([]));
            }
            ops::new_tag(r, name, msg)?;
            done("tag", &format!("{name} at the head"))
        }
        "modules" => {
            let items: Vec<Value> = ops::submodules(r)?
                .iter()
                .map(|s| {
                    let mark = match s.state {
                        '-' => "not initialized",
                        '+' => "other commit",
                        'U' => "conflict",
                        _ => "",
                    };
                    json!({
                        "label": format!("{} {} {} {mark}", s.path, short(&s.hash), s.describe), "call": "module_go", "arg": s.path, "enter": "go",
                        "keys": [key("u", "update", "module_update"), key("U", "update all", "modules_update_all")],
                        "cells": [[s.path, "blue"], [short(&s.hash), "yellow"], [s.describe, "comment"], [mark, "red"]],
                    })
                })
                .collect();
            Ok(json!([{"op": "menu", "title": "Submodules", "items": items}]))
        }
        "module_go" => {
            if r.is_remote() {
                return say("info", "a submodule of a server: open it in the panel");
            }
            Ok(json!([{"op": "cd", "path": r.path().join(&c.arg)}]))
        }
        "module_update" => done("submodule update", &ops::submodule_update(r, Some(&c.arg))?),
        "modules_update" => {
            let p = first().map(|t| t.rel).ok();
            done("submodule update", &ops::submodule_update(r, p.as_deref())?)
        }
        "modules_update_all" => done("submodule update", &ops::submodule_update(r, None)?),
        _ => Err(format!("no such function: {call}")),
    }
}
