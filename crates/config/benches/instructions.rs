//! Gungraun benches: exact Callgrind instruction counts for `Rules::decide`.
//!
//! The rules are built in a `setup` function, so only the decision is counted:
//! 20 glob rules in each of the allow, ask and deny list, and a chained
//! command of three parts, which every rule of every list looks at.
//!
//! Run with `cargo bench -p reagent-config --bench instructions`, or with
//! `cargo bench -p reagent-config --bench instructions -- --test` to run each
//! bench once without Callgrind. `bench/instructions.sh` gates both trees.

// gungraun's benchmark macros expand to std::fs and std::process code on
// Linux, which clippy.toml bans; the ban is for the agent, not for benches.
#![allow(clippy::disallowed_types, clippy::disallowed_methods)]

use std::hint::black_box;
use std::path::{Path, PathBuf};

use gungraun::prelude::*;
use reagent_config::Layer;
use reagent_config::Settings;
use reagent_config::permissions::{Call, Rules};

/// Glob rules in each of the allow, ask and deny lists.
const RULES: usize = 20;

/// A chained command of three parts.
const COMMAND: &str = "git status && git log --oneline | head";

struct DecideCase {
    rules: Rules,
    call: Call<'static>,
}

/// Rules with `RULES` glob rules in each list, none of which matches the
/// command, so every rule of every list is looked at.
fn decide_globs() -> DecideCase {
    let list = |kind: &str| -> Vec<String> {
        (0..RULES).map(|n| format!("Bash(zz{kind}{n} *)")).collect()
    };
    let layer = Layer {
        path: PathBuf::from("/home/u/.config/reagent/config.toml"),
        settings: Settings {
            permissions: reagent_config::permissions::Lists {
                allow: list("a"),
                ask: list("k"),
                deny: list("d"),
                default: Some("allow".into()),
            },
            ..Settings::default()
        },
    };
    DecideCase {
        rules: Rules::from_layers(&[layer], Path::new("/work"), Some(Path::new("/home/u")))
            .expect("the rules"),
        call: Call {
            tool: "bash",
            main_arg: Some("command"),
            value: Some(COMMAND),
            reads: &[],
            writes: &[],
        },
    }
}

#[library_benchmark]
#[bench::globs(setup = decide_globs)]
fn decide(case: DecideCase) -> u8 {
    black_box(case.rules.decide(&case.call).kind as u8)
}

library_benchmark_group!(name = rules, benchmarks = [decide]);

main!(library_benchmark_groups = rules);
