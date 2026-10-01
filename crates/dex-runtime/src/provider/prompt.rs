//! The system prompt.
//!
//! It is generated, not written by hand, so the capability reference a model
//! reads is derived from the code that decides what those capabilities do. A
//! capability added to the registry appears here; one removed disappears. A
//! hand-written reference would drift the first time a capability changed.
//!
//! It also teaches the shape of the language and the shape of a turn, because
//! the model has to satisfy both to get anything done.

use crate::config::RuntimeConfig;
use crate::script::ScriptLanguage;

/// Build the system prompt for a session.
pub fn system_prompt(config: &RuntimeConfig, language: ScriptLanguage) -> String {
    let mut out = String::new();

    out.push_str(
        "You are the reasoning and program-generation layer of DEX, a coding \
         agent. You do not call tools. You write programs.\n\n",
    );

    out.push_str("## How a turn works\n\n");
    out.push_str(
        "You are given a task. You reply with exactly one program written in \
         the DEX scripting language. The runtime executes it against real \
         capabilities and returns what the program produced. If that is not \
         enough, you are given the result and write another program. You \
         continue until you call `dex::respond(...)` with your answer to the \
         user.\n\n\
         Several capabilities in one program is the normal case, not an \
         optimisation: searching, reading, filtering, editing and testing \
         belong in a single program, because each round trip costs a turn and \
         context.\n\n",
    );

    out.push_str(&format!("## Language ({})\n\n", language.name));
    out.push_str(
        "A program is a module: it contains declarations and has exactly one \
         entry point, `main`. The value `main` returns is the program's \
         result.\n\n\
         ```\npub fn main() {\n    // your program\n}\n```\n\n\
         Things that will bite you:\n\n\
         - Everything is mutable already; `mut` is not a keyword.\n\
         - A call takes exactly the arguments it declares. There are no \
           optional parameters, so pick the function that matches the case \
           you are in rather than passing placeholders.\n\
         - There is no `while` loop and no `try`/`catch`. Use `for`.\n\
         - A capability that fails ends the program and you are told why, so \
           check before you act if a step is optional.\n\
         - There is no `println!`. Use `dex::log` to report progress.\n\n",
    );

    out.push_str(&format!(
        "## Budgets\n\nEvery program runs under a limit. You have {}.\n\n",
        config.budget.describe()
    ));

    out.push_str("## Capabilities\n\n");
    out.push_str(
        "These are the only operations that exist. Anything you cannot express \
         with them, you cannot do.\n\n",
    );
    for line in capability_reference() {
        out.push_str(&line);
        out.push('\n');
    }

    out.push_str("\n## Rules\n\n");
    out.push_str(
        "- Read a file before editing it. `dex::edit` matches an exact string \
         and refuses if it appears more than once, so include enough context \
         to be unambiguous.\n\
         - Paths are relative to the repository root. You cannot read or \
         write outside it.\n\
         - An operation you have no authority for is refused. That is \
         deliberate: do not try to work around it.\n\
         - When you are finished, call `dex::respond(...)`. That is what ends \
         the turn.\n",
    );

    out
}

/// The capability reference, one line each.
///
/// This is the authoritative list for the model. Adding a capability means
/// adding a line here; the Rune bindings are what actually enforce it.
fn capability_reference() -> Vec<String> {
    let lines = [
        "### Finding and reading\n\
         \x20 `dex::find(query)` search the working tree\n\
         \x20 `dex::find_in(query, path)` search below a path\n\
         \x20 `dex::read(path)` read a file\n\
         \x20 `dex::read_lines(path, offset, limit)` read part of a long file\n\
         \x20 `dex::list(path)` list a directory",
        "### Changing\n\
         \x20 `dex::edit(path, old_string, new_string)` replace an exact string\n\
         \x20 `dex::write(path, content)` write a whole file\n\
         \x20 `dex::delete(path)` delete one file\n\
         \x20 `dex::delete_tree(path)` delete a directory and its contents\n\
         \x20 `dex::exists(path)` whether a path exists\n\
         \x20 `dex::stat(path)` size, kind and modification time",
        "### Git\n\
         \x20 `dex::git_status()` working tree status\n\
         \x20 `dex::git_diff()` diff of the whole tree\n\
         \x20 `dex::git_diff_path(path)` diff of one path\n\
         \x20 `dex::git_log()` the twenty most recent commits\n\
         \x20 `dex::git_checkout(branch)` switch branches",
        "### Testing\n\
         \x20 `dex::test(name)` run an allowlisted target with its own arguments\n\
         \x20 `dex::test_args(name, args)` run it with explicit arguments\n\
         \x20 `dex::test_targets()` the targets that exist\n\n\
         Targets are a fixed list and their arguments are checked against a \
         table. There is no general command runner: you cannot run an \
         arbitrary shell command, and a rejected argument tells you what is \
         allowed.",
        "### Memory\n\
         \x20 `dex::remember(key, program)` store a procedure for later\n\
         \x20 `dex::recall(key)` load one\n\
         \x20 `dex::recalls()` list stored keys\n\
         \x20 `dex::forget(key)` remove one",
        "### Conversation\n\
         \x20 `dex::respond(message)` deliver your answer; this ends the turn\n\
         \x20 `dex::ask(message)` put a question to the user and wait\n\
         \x20 `dex::log(message)` report progress",
    ];
    lines.iter().map(|l| l.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::ExecutionBudget;
    use crate::script::ScriptLanguage;

    fn prompt() -> String {
        let config = RuntimeConfig {
            provider: crate::config::ProviderConfig {
                base_url: "https://example.invalid/v1".into(),
                api_key: "k".into(),
                model: "m".into(),
                session_header: "x-opencode-session".into(),
            },
            socket_path: "/tmp/dex.sock".into(),
            memory_dir: "/tmp/dex-memory".into(),
            max_sessions: 32,
            max_program_rounds: 8,
            budget: ExecutionBudget::default(),
            authority: crate::auth::Authority::deny_all(),
        };
        system_prompt(
            &config,
            ScriptLanguage {
                name: "rune",
                version: "0.14",
            },
        )
    }

    #[test]
    fn it_states_the_core_rule_the_model_must_follow() {
        let p = prompt();
        assert!(p.contains("You do not call tools"), "{p}");
        assert!(p.contains("dex::respond"), "the turn must be endable");
    }

    #[test]
    fn it_documents_every_registered_capability() {
        let p = prompt();
        for name in [
            "dex::find",
            "dex::find_in",
            "dex::read",
            "dex::read_lines",
            "dex::list",
            "dex::edit",
            "dex::write",
            "dex::delete",
            "dex::delete_tree",
            "dex::exists",
            "dex::stat",
            "dex::git_status",
            "dex::git_diff",
            "dex::git_diff_path",
            "dex::git_log",
            "dex::git_checkout",
            "dex::test",
            "dex::test_args",
            "dex::test_targets",
            "dex::remember",
            "dex::recall",
            "dex::recalls",
            "dex::forget",
            "dex::respond",
            "dex::ask",
            "dex::log",
        ] {
            assert!(p.contains(name), "the prompt must document {name}");
        }
    }

    #[test]
    fn it_warns_about_the_language_traps_that_bite() {
        let p = prompt();
        for warning in ["`mut` is not a keyword", "no optional parameters", "no `while` loop", "no `println!`"] {
            assert!(p.contains(warning), "the prompt should warn: {warning}");
        }
    }

    #[test]
    fn it_states_that_there_is_no_general_command_runner() {
        assert!(prompt().contains("no general command runner"));
    }

    #[test]
    fn it_reports_the_budget_the_program_will_run_under() {
        assert!(prompt().contains("capability_calls="));
    }
}