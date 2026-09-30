//! The allowlisted test runner.
//!
//! This exists because the acceptance criteria need to run tests, and removing
//! `run_command` removes the only obvious way to do that. A free-form command
//! string would undo the whole point, so this is a table: a name the model may
//! ask for, and for each name the exact subcommands and flags it may use.
//!
//! Nothing here is a shell. Arguments are passed as argv, so there is no
//! quoting to get wrong and no metacharacter to blacklist. The validation is
//! about intent, not about escaping: `--upload-pack` is not a quoting problem,
//! it is a different operation, and it is simply not on the list.

use serde_json::json;
use std::time::Duration;

use super::process;
use super::{CapabilityCtx, CapabilityError};
use crate::capability::CapabilityErrorKind;

/// Authority required to run a test target.
pub const RUN: &str = "testing.run";

/// One runnable tool.
pub struct Target {
    pub name: &'static str,
    pub binary: &'static str,
    /// Subcommands the model may name, e.g. `cargo test`.
    pub subcommands: &'static [&'static str],
    /// Valueless flags permitted before any positional argument.
    pub flags: &'static [&'static str],
    /// Flags permitted with a value, e.g. `--package name`.
    pub valued_flags: &'static [&'static str],
    /// Flags permitted after a bare `--`, passed through to the runner.
    pub passthrough_flags: &'static [&'static str],
}

/// Everything `testing.run` can do. Adding a tool is adding a row here.
pub const TARGETS: &[Target] = &[
    Target {
        name: "cargo",
        binary: "cargo",
        subcommands: &["test", "check", "build", "clippy", "fmt"],
        flags: &[
            "--all",
            "--workspace",
            "--lib",
            "--bins",
            "--tests",
            "--quiet",
            "--release",
            "--locked",
            "--offline",
            "--all-targets",
            "--all-features",
            "--no-default-features",
            "--color=never",
        ],
        valued_flags: &["-p", "--package", "--features", "--profile"],
        passthrough_flags: &["--nocapture", "--test-threads=1", "--ignored", "--exact"],
    },
    Target {
        name: "nextest",
        binary: "cargo",
        subcommands: &["nextest"],
        flags: &["--workspace", "--all-features", "--no-default-features", "--quiet", "--release"],
        valued_flags: &["-p", "--package", "--test-threads"],
        passthrough_flags: &["--nocapture", "--no-capture"],
    },
    Target {
        name: "pytest",
        binary: "pytest",
        subcommands: &[],
        flags: &["-q", "--quiet", "-x", "--verbose", "-v", "--tb=short", "--no-header"],
        valued_flags: &["-k", "--maxfail"],
        passthrough_flags: &["-k", "-k"],
    },
    Target {
        name: "go",
        binary: "go",
        subcommands: &["test", "build", "vet"],
        flags: &["-race", "-count=1", "-short", "-v"],
        valued_flags: &["-timeout", "-run"],
        passthrough_flags: &["-v", "-run"],
    },
    Target {
        name: "npm",
        binary: "npm",
        subcommands: &["test", "run"],
        flags: &["--silent", "--if-present"],
        valued_flags: &["--prefix"],
        passthrough_flags: &[],
    },
    Target {
        name: "pnpm",
        binary: "pnpm",
        subcommands: &["test"],
        flags: &["--silent", "--if-present", "--reporter=default"],
        valued_flags: &["--filter", "--dir"],
        passthrough_flags: &[],
    },
    Target {
        name: "make",
        binary: "make",
        subcommands: &[],
        flags: &["--no-print-directory", "-s"],
        valued_flags: &["-C", "-f"],
        passthrough_flags: &[],
    },
];

pub fn target_names() -> Vec<&'static str> {
    TARGETS.iter().map(|t| t.name).collect()
}

/// `testing.run(name, args?)`
pub async fn run(
    ctx: &CapabilityCtx,
    name: &str,
    args: &[String],
) -> Result<serde_json::Value, CapabilityError> {
    ctx.enter("testing.run")?;
    ctx.authorize(RUN, &ctx.working_dir().to_string_lossy())?;

    let target = TARGETS
        .iter()
        .find(|t| t.name == name)
        .ok_or_else(|| {
            CapabilityError::new(
                CapabilityErrorKind::InvalidArgument,
                format!(
                    "{name:?} is not a runnable test target; available: {}",
                    target_names().join(", ")
                ),
            )
        })?;

    let argv = build_argv(target, args)?;

    let limit: Duration = ctx.budget.limits().command;
    let outcome = process::run(ctx, target.binary, &argv, limit).await?;

    // A failing test suite is a normal outcome the model needs to read, not an
    // error that aborts the program. The exit code is data.
    Ok(json!({
        "target": target.name,
        "argv": argv,
        "exit_code": outcome.exit_code,
        "succeeded": outcome.succeeded(),
        "stdout": outcome.stdout,
        "stderr": outcome.stderr,
        "duration_ms": outcome.duration_ms,
        "truncated": outcome.truncated,
    }))
}

/// Validate the model's arguments and produce argv.
///
/// Rejection is by allowlist rather than by pattern, so a newly added flag on a
/// tool is refused until it is deliberately permitted here.
fn build_argv(target: &Target, args: &[String]) -> Result<Vec<String>, CapabilityError> {
    let mut argv = Vec::new();
    let mut after_separator = false;
    let mut index = 0;

    while index < args.len() {
        let arg = &args[index];
        index += 1;

        if after_separator {
            if target.passthrough_flags.contains(&arg.as_str()) {
                argv.push(arg.clone());
                continue;
            }
            return Err(reject(target, arg, "after `--`"));
        }

        if arg == "--" {
            after_separator = true;
            argv.push(arg.clone());
            continue;
        }

        if let Some(stripped) = arg.strip_prefix("--") {
            let (flag, has_inline_value) = match stripped.split_once('=') {
                Some((flag, _)) => (format!("--{flag}"), true),
                None => (arg.clone(), false),
            };
            if target.flags.contains(&flag.as_str()) {
                if has_inline_value && !target.flags.contains(&arg.as_str()) {
                    return Err(reject(target, arg, "with a value"));
                }
                argv.push(arg.clone());
                continue;
            }
            if target.valued_flags.contains(&flag.as_str()) {
                if !has_inline_value {
                    let value = args.get(index).ok_or_else(|| {
                        CapabilityError::invalid(format!("{arg} needs a value"))
                    })?;
                    index += 1;
                    argv.push(flag);
                    argv.push(value.clone());
                } else {
                    argv.push(arg.clone());
                }
                continue;
            }
            return Err(reject(target, arg, "unknown flag"));
        }

        if arg.starts_with('-') && arg.len() > 1 {
            // Short flags, including the single-letter ones the tables allow.
            if target.flags.contains(&arg.as_str()) || target.valued_flags.contains(&arg.as_str()) {
                if target.valued_flags.contains(&arg.as_str()) {
                    let value = args.get(index).ok_or_else(|| {
                        CapabilityError::invalid(format!("{arg} needs a value"))
                    })?;
                    index += 1;
                    argv.push(arg.clone());
                    argv.push(value.clone());
                } else {
                    argv.push(arg.clone());
                }
                continue;
            }
            return Err(reject(target, arg, "unknown flag"));
        }

        // A bare word: either the declared subcommand, or a filter to apply to
        // it. A word that looks like an option was handled above.
        if argv.is_empty() && target.subcommands.contains(&arg.as_str()) {
            argv.push(arg.clone());
            continue;
        }
        if target.subcommands.is_empty() || !argv.is_empty() {
            argv.push(arg.clone());
            continue;
        }
        return Err(reject(target, arg, "unknown subcommand"));
    }

    if argv.is_empty() && !target.subcommands.is_empty() {
        return Err(CapabilityError::invalid(format!(
            "{} needs one of: {}",
            target.name,
            target.subcommands.join(", ")
        )));
    }
    Ok(argv)
}

fn reject(target: &Target, arg: &str, why: &str) -> CapabilityError {
    CapabilityError::new(
        CapabilityErrorKind::InvalidArgument,
        format!(
            "{why}: {arg:?} is not permitted for `{}`. Allowed subcommands: [{}]; flags: [{}]; passthrough after --: [{}]",
            target.name,
            target.subcommands.join(", "),
            target.flags.join(" "),
            target.passthrough_flags.join(" "),
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Authority;
    use crate::budget::{BudgetMeter, ExecutionBudget};
    use crate::capability::PathGuard;
    use crate::events::EventSink;
    use crate::memory::MemoryStore;
    use dex_protocol::{CallId, SessionId};
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    fn target(name: &str) -> &'static Target {
        TARGETS.iter().find(|t| t.name == name).expect("target")
    }

    fn argv(name: &str, args: &[&str]) -> Result<Vec<String>, CapabilityError> {
        build_argv(target(name), &args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn a_plain_test_invocation_is_allowed() {
        assert_eq!(argv("cargo", &["test"]).unwrap(), vec!["test"]);
    }

    #[test]
    fn common_cargo_flags_are_allowed() {
        assert_eq!(
            argv("cargo", &["test", "--workspace", "--lib", "-p", "dex-protocol"]).unwrap(),
            vec!["test", "--workspace", "--lib", "-p", "dex-protocol"]
        );
    }

    #[test]
    fn a_test_filter_after_the_subcommand_is_allowed() {
        assert_eq!(
            argv("cargo", &["test", "auth::login"]).unwrap(),
            vec!["test", "auth::login"]
        );
    }

    #[test]
    fn passthrough_flags_after_a_separator_are_allowed() {
        assert_eq!(
            argv("cargo", &["test", "--", "--nocapture"]).unwrap(),
            vec!["test", "--", "--nocapture"]
        );
    }

    #[test]
    fn an_unknown_subcommand_is_refused() {
        let err = argv("cargo", &["publish"]).expect_err("must refuse");
        assert_eq!(err.kind, CapabilityErrorKind::InvalidArgument);
    }

    #[test]
    fn cargo_run_is_refused_because_it_executes_arbitrary_code() {
        // `cargo run` would give the model back a general code-execution
        // primitive, which is exactly what the capability model excludes.
        assert!(argv("cargo", &["run"]).is_err());
    }

    #[test]
    fn flags_outside_the_allowlist_are_refused() {
        for flag in [
            "--config",
            "--manifest-path",
            "--target-dir",
            "--out-dir",
            "--target",
            "--unit-graph",
        ] {
            assert!(
                argv("cargo", &["test", flag]).is_err(),
                "{flag} should have been refused"
            );
        }
    }

    #[test]
    fn git_style_option_injection_is_refused_for_every_target() {
        for name in target_names() {
            assert!(
                argv(name, &["--upload-pack=touch /tmp/pwn"]).is_err(),
                "{name} should refuse --upload-pack"
            );
        }
    }

    #[test]
    fn shell_metacharacters_are_inert_because_there_is_no_shell() {
        // Not a quoting problem: argv is passed through untouched, so these
        // become a literal (nonsensical) argument rather than a command.
        let built = argv("pytest", &["-k", "auth && rm -rf /"]).unwrap();
        assert_eq!(built, vec!["-k", "auth && rm -rf /"]);
    }

    #[test]
    fn a_valued_flag_without_a_value_is_refused() {
        assert!(argv("cargo", &["test", "-p"]).is_err());
    }

    #[test]
    fn a_target_needing_a_subcommand_says_so() {
        let err = argv("cargo", &[]).expect_err("must refuse");
        assert!(err.message.contains("needs one of"), "got {}", err.message);
    }

    #[test]
    fn a_tool_with_no_subcommand_accepts_flags_alone() {
        assert!(argv("pytest", &["-q"]).is_ok());
    }

    #[test]
    fn an_unknown_target_lists_the_available_ones() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = CapabilityCtx::new(
            PathGuard::new(dir.path()).expect("guard"),
            Arc::new(Authority::parse("testing.run=*").expect("authority")),
            BudgetMeter::new(ExecutionBudget::default()),
            EventSink::new(SessionId::new()),
            Arc::new(MemoryStore::new(dir.path().join("memory"))),
            CancellationToken::new(),
            CallId(1),
        );
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let err = runtime
            .block_on(run(&ctx, "nonesuch", &[]))
            .expect_err("must refuse");
        assert!(err.message.contains("cargo"), "got {}", err.message);
    }

    #[tokio::test]
    async fn running_a_target_needs_authority() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = CapabilityCtx::new(
            PathGuard::new(dir.path()).expect("guard"),
            Arc::new(Authority::deny_all()),
            BudgetMeter::new(ExecutionBudget::default()),
            EventSink::new(SessionId::new()),
            Arc::new(MemoryStore::new(dir.path().join("memory"))),
            CancellationToken::new(),
            CallId(1),
        );
        assert_eq!(
            run(&ctx, "cargo", &["test".into()]).await.unwrap_err().kind,
            CapabilityErrorKind::PermissionDenied
        );
    }

    #[tokio::test]
    async fn a_real_command_runs_and_reports_its_exit_code() {
        // `make` with no target list is permitted and is a genuine subprocess,
        // so this exercises the full spawn/stream/collect path.
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = CapabilityCtx::new(
            PathGuard::new(dir.path()).expect("guard"),
            Arc::new(Authority::parse("testing.run=*").expect("authority")),
            BudgetMeter::new(ExecutionBudget {
                command: Duration::from_secs(30),
                ..ExecutionBudget::default()
            }),
            EventSink::new(SessionId::new()),
            Arc::new(MemoryStore::new(dir.path().join("memory"))),
            CancellationToken::new(),
            CallId(1),
        );
        std::fs::write(
            dir.path().join("Makefile"),
            "all:\n\t@echo dex-test-ok\n",
        )
        .expect("write");

        let result = run(&ctx, "make", &[]).await.expect("run");
        assert_eq!(result["succeeded"], true);
        assert!(result["stdout"].as_str().unwrap().contains("dex-test-ok"));
    }
}
