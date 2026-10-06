//! `enzyme workspace plan|apply|recover`: reviewed changes to a workspace program.
//!
//! The program for workspace `<name>` is the one `$ENZYME_HOME/configs/*.enzyme`
//! file that declares it; a new workspace is written to `<name>.enzyme`.
//! Planning and applying are `enzyme_spec::plan`; this module only reads
//! inputs and prints. Unlike `enzyme spec plan`, which previews catalyst
//! generation for a program, these commands change the program file itself.
//! Moving a workspace between files, renaming, or removing it is out of scope.
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Subcommand;
use enzyme_core::config::EnzymePaths;
use enzyme_spec::plan::{Action, ApplyError, ConfigStore, Plan, Receipt};

#[derive(Subcommand)]
pub enum WorkspaceCommand {
    /// Plan replacing a workspace's program with a desired program
    ///
    /// The plan records the program file's current revision, the desired
    /// text and its digest, a unified diff, and a summary of changed
    /// statements. Save it with --json or --out and pass it to `apply`.
    /// Planning never changes the program it plans, but it creates
    /// `configs/.enzyme-apply/` and first completes any interrupted apply.
    Plan {
        /// Workspace name, as declared by `workspace "<name>"`
        name: String,
        /// Desired program text (`-` reads stdin)
        #[arg(long)]
        desired: PathBuf,
        /// Print the plan as JSON (the input `apply --plan` expects)
        #[arg(long)]
        json: bool,
        /// Also save the plan JSON to this file
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Apply a saved plan; refuses stale or altered plans, replays applied ones
    Apply {
        /// Plan JSON from `enzyme workspace plan` (`-` reads stdin)
        #[arg(long)]
        plan: PathBuf,
        /// Print the receipt as JSON
        #[arg(long)]
        json: bool,
    },
    /// Complete interrupted applies; --discard drops journals that cannot be completed
    Recover {
        /// Remove journals whose program was edited mid-apply, leaving the program as is
        #[arg(long)]
        discard: bool,
        /// Print the outcome as JSON
        #[arg(long)]
        json: bool,
    },
}

pub fn run(command: &WorkspaceCommand) -> Result<()> {
    let json = match command {
        WorkspaceCommand::Plan { json, .. }
        | WorkspaceCommand::Apply { json, .. }
        | WorkspaceCommand::Recover { json, .. } => *json,
    };
    let result = match command {
        WorkspaceCommand::Plan {
            name,
            desired,
            json,
            out,
        } => run_plan(name, desired, *json, out.as_deref()),
        WorkspaceCommand::Apply { plan, json } => run_apply(plan, *json),
        WorkspaceCommand::Recover { discard, json } => run_recover(*discard, *json),
    };
    if json {
        if let Err(error) = &result {
            println!("{}", serde_json::to_string(&error_json(error))?);
        }
    }
    result
}

fn store() -> Result<ConfigStore> {
    let user_home = dirs::home_dir().context("cannot resolve home directory")?;
    let enzyme_home = EnzymePaths::enzyme_home()?;
    // Validate with the runtime's environment: built-in kinds and `{home}`.
    let environment = enzyme_core::config::spec_environment(&user_home, &enzyme_home)?;
    Ok(
        ConfigStore::new(enzyme_home.join("configs"), user_home)
            .with_validator(enzyme_spec::plan::resolver_in(environment)),
    )
}

fn read_input(path: &Path) -> Result<String> {
    if path == Path::new("-") {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .context("reading stdin")?;
        return Ok(text);
    }
    std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
}

fn run_plan(name: &str, desired: &Path, json: bool, out: Option<&Path>) -> Result<()> {
    // The same rule `--workspace` applies, so the planned workspace can be indexed.
    EnzymePaths::workspace_root(name).map_err(super::exit::CliFailure::usage)?;
    let desired = read_input(desired)?;
    let plan = store()?.plan(name, &desired)?;
    let body = serde_json::to_string_pretty(&plan)?;
    if let Some(out) = out {
        std::fs::write(out, format!("{body}\n"))
            .with_context(|| format!("writing {}", out.display()))?;
    }
    if json {
        println!("{body}");
        return Ok(());
    }
    print_plan(&plan, out);
    Ok(())
}

fn print_plan(plan: &Plan, out: Option<&Path>) {
    let file = format!("configs/{}", plan.target);
    let new = if plan.base_revision == enzyme_spec::plan::ABSENT {
        " (new file)"
    } else {
        ""
    };
    println!("Workspace \"{}\" → {file}{new}", plan.workspace);
    if plan.is_noop() {
        println!("No changes.");
        return;
    }
    for change in &plan.changes {
        let mark = match change.action {
            Action::Added => '+',
            Action::Removed => '-',
            Action::Changed => '~',
        };
        println!("  {mark} {}", change.summary);
    }
    println!();
    print!("{}", plan.diff);
    println!();
    println!("Plan {}", plan.plan_id);
    match out {
        Some(out) => println!(
            "Apply with: enzyme workspace apply --plan {}",
            out.display()
        ),
        None => println!(
            "Save it with --out <file> or --json, then: enzyme workspace apply --plan <file>"
        ),
    }
}

fn run_apply(plan: &Path, json: bool) -> Result<()> {
    let text = read_input(plan)?;
    let plan: Plan = serde_json::from_str(&text)
        .with_context(|| format!("{} is not a workspace plan", plan.display()))?;
    let receipt = store()?.apply(&plan)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&receipt)?);
    } else {
        print_receipt(&receipt);
    }
    Ok(())
}

fn run_recover(discard: bool, json: bool) -> Result<()> {
    let store = store()?;
    let (completed, discarded) = if discard {
        store.discard_unrecoverable()?
    } else {
        (store.recover()?, Vec::new())
    };
    if json {
        let body =
            serde_json::json!({ "ok": true, "completed": completed, "discarded": discarded });
        println!("{}", serde_json::to_string_pretty(&body)?);
        return Ok(());
    }
    for receipt in &completed {
        println!(
            "Completed interrupted apply of plan {} to configs/{}.",
            receipt.plan_id, receipt.target
        );
    }
    for path in &discarded {
        println!(
            "Discarded {}; its program was left as it is.",
            path.display()
        );
    }
    if completed.is_empty() && discarded.is_empty() {
        println!("Nothing to recover.");
    }
    Ok(())
}

fn print_receipt(receipt: &Receipt) {
    let file = format!("configs/{}", receipt.target);
    if receipt.replayed {
        println!(
            "Plan {} was already applied to {file}; nothing written.",
            receipt.plan_id
        );
        return;
    }
    println!(
        "Applied plan {} to {file} ({} change{}).",
        receipt.plan_id,
        receipt.changes.len(),
        if receipt.changes.len() == 1 { "" } else { "s" }
    );
    println!(
        "Index it with: enzyme refresh --workspace {}",
        receipt.workspace
    );
}

fn error_json(error: &anyhow::Error) -> serde_json::Value {
    let (kind, extra) = match error.downcast_ref::<ApplyError>() {
        Some(ApplyError::Stale { expected, actual }) => (
            "stale",
            serde_json::json!({ "expected_revision": expected, "actual_revision": actual }),
        ),
        Some(ApplyError::Altered(_)) => ("altered", serde_json::json!({})),
        Some(ApplyError::Unsupported(_)) => ("unsupported", serde_json::json!({})),
        Some(ApplyError::Unrecoverable(_)) => ("unrecoverable", serde_json::json!({})),
        None => ("invalid", serde_json::json!({})),
    };
    let mut body = serde_json::json!({ "kind": kind, "message": format!("{error:#}") });
    body.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    serde_json::json!({ "ok": false, "error": body })
}
