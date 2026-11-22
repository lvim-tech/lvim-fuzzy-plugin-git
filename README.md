# lvim-fuzzy-plugin-git

git for the lvim fuzzy tools (`lvim-fuzzy-commander` — lvc —, `lvim-fuzzy-cli` — lfz —, the Neovim picker, lvim-files): the state of a
work tree and the operations on it, on the `git` program (no library, no `.so`). The tools draw; this plugin knows git.

so: `[plugins.lvim-fuzzy-plugin-git] install = "bundled"` (or the path of a program that answers as the plugin).


## Installation

A plugin is a program of its own — no tool carries it. `./scripts/install.sh` builds it (`cargo build --release`), puts the program in
`~/.local/bin/` and writes its `plugin.toml` (with `exec` pointing at that program) into the shared plugins directory
`~/.config/lvim-plugins/lvim-fuzzy-plugin-git/`; lvc, lfz and the Neovim picker find it there. The old `install = "bundled"` lines of a tool's
config are gone (a tool refuses to start with one).

## Where it is used

| App | What for |
|---|---|
| **lvc** | the marks column, the branch and counts in the path bar, the whole `x` group (stage, commit, diff and hunks, log, branches, stash, tags …) |
| lfz, the Neovim picker | not used (the plugin is not loaded there) |

## Library

- `status::read(dir, ignored, untracked)` — `git status --porcelain=v2 -z --branch` read into a [`Status`]: the branch, the upstream, ahead /
  behind, a mark per path (`x` the index and `y` the work tree, as in `git status -s`, a kind, staged / unstaged), the marks of the folders
  (the heaviest below them), the counters. `Status::mark(base, name, is_dir)` answers for one entry of a folder.
- `ops` — on a `Repo` (a work tree and the runner that reaches it): `stage`, `unstage`, `discard`, `stage_all`, `unstage_all`, `commit`
  (`amend`), `log`, `show`, `diff`, `blame`, the hunks of a diff (`hunks`, `hunk_patch`, `apply_hunk`: stage / unstage / discard one — `git add -p`),
  `branches`, `checkout`, `new_branch`, `delete_branch`, `merge`, `rebase`, `state` (a merge / rebase / cherry-pick / revert in progress),
  `continue_`, `abort`, `resolve` (ours / theirs), `tags` (`new_tag`, `delete_tag`, `push_tag`, `checkout_tag`), `submodules`
  (`submodule_update`), `fetch`, `pull` (fast-forward only), `push` (sets the upstream of a new branch), `stash_*`, `ignore`, `remove`, `rename`.
  Each gives the text of the result or git's own message. Nothing waits for a person: no prompt of git's own, no editor.
- `runner` — where git runs: `Local`, or `Ssh` (a server: `ssh host -- cd … && git …`, one connection kept by ControlMaster; the same
  operations work there). A command that may need a secret (a push, a key with a passphrase) is cut off from the terminal and asks through
  `lvim_fuzzy_plugin::askpass`: `ward` / `pinentry` / `$SSH_ASKPASS`).

## Plugin calls

`status`, `branches`, `log`, `diff`, `show`, `blame`, `stashes`, `tags`, `submodules` and `run` (verbs: `stage unstage discard stage_all unstage_all commit
amend checkout new_branch delete_branch merge rebase continue abort resolve tag delete_tag push_tag checkout_tag submodule_update fetch pull push
stash_push stash_pop stash_apply stash_drop ignore remove rename hunk`); a call may carry `ssh` (`{host, user?, port?, key?}`) to run on a server
— see the table in `src/lib.rs`.

## Its buttons

`plugin.toml` places them: the group `x` (git), the sub-groups (`files`, `commit`, `inspect`, `branch`, `sync`, `stash`, `tags`, `modules`, `in progress`) and
every button, each an `[actions.*]` with `builtin = "git-…"` — the operation lvc does for the button. Move any in lvc's config
(`[plugins.lvim-fuzzy-plugin-git.keys] stage = "x a z"`) or under `[keys]` of `~/.config/lvim-plugins/lvim-fuzzy-plugin-git.toml`.

## Settings

Override any key in `~/.config/lvim-plugins/lvim-fuzzy-plugin-git.toml` (every tool) or under `[plugins.lvim-fuzzy-plugin-git]` in one tool's
config: `timeout`, `long_timeout`, `ignored`, `untracked`, `log` (see `plugin.toml`).

## License

BSD-3-Clause
