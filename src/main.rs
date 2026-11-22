//! The stand-alone program of the plugin (`lvim-fuzzy-plugin install` builds it); lfz and lvc carry the same code.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `lvim-fuzzy-plugin-git askpass PROMPT` is the secret prompt git and ssh call
    if args.first().map(String::as_str) == Some("askpass") {
        std::process::exit(lvim_fuzzy_plugin_git::askpass(args.get(1).map(String::as_str).unwrap_or("")));
    }
    lvim_fuzzy_plugin_git::run();
}
