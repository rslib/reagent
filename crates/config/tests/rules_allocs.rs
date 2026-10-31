//! Allocation counts for `Rules::decide`.
//!
//! `decide` runs on every tool call, and a rule list can be long: the work it
//! does must not allocate per (rule, command part) pair. dhat allows one
//! profiler per process, so every test locks [`SERIAL`] for its whole body.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use dhat::{Alloc, HeapStats, Profiler};

use reagent_config::Layer;
use reagent_config::Settings;
use reagent_config::permissions::{Call, Kind, Lists, Rules};

#[global_allocator]
static ALLOC: Alloc = Alloc;

/// Held by every test for its whole body, since dhat allows only one profiler
/// per process and another test's allocations would land in these numbers.
static SERIAL: Mutex<()> = Mutex::new(());

/// Locks the profiler for one test.
fn serial() -> MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Glob rules in each of the allow, ask and deny lists of the measured rules.
const RULES: usize = 20;

/// A chained command of three parts: a deny or ask rule looks at each part,
/// and so does an allow rule.
const COMMAND: &str = "git status && git log --oneline | head";

/// A chained command that does not parse cleanly, so the loose rules also look
/// for their words in its raw text.
const COMPLEX: &str = "echo $(cat notes) && git status | head";

/// The most blocks one `decide` over [`RULES`] glob rules in each list may
/// allocate, whatever the number of rules: 17 on [`COMMAND`] and 28 on
/// [`COMPLEX`] measured. Joining the parts and tokenising the raw text once
/// per rule instead put them at 348 and 148, and both grew with the rules.
const DECIDE_BLOCKS: u64 = 40;
const _: () = assert!(DECIDE_BLOCKS < 100);

/// Rules with `count` glob rules in each list, none of which matches
/// [`COMMAND`]: every rule of every list is looked at.
fn rules(count: usize) -> Rules {
    let list = |kind: &str| -> Vec<String> {
        (0..count).map(|n| format!("Bash(zz{kind}{n} *)")).collect()
    };
    let layer = Layer {
        path: PathBuf::from("/home/u/.config/reagent/config.toml"),
        settings: Settings {
            permissions: Lists {
                allow: list("a"),
                ask: list("k"),
                deny: list("d"),
                default: Some("allow".into()),
            },
            ..Settings::default()
        },
    };
    Rules::from_layers(&[layer], Path::new("/work"), Some(Path::new("/home/u"))).expect("the rules")
}

/// The blocks one `decide` of `command` allocates under these rules.
fn blocks_per_decide(rules: &Rules, command: &'static str) -> u64 {
    let call = Call {
        tool: "bash",
        main_arg: Some("command"),
        value: Some(command),
        reads: &[],
        writes: &[],
    };
    let profiler = Profiler::builder().testing().build();
    let kind = rules.decide(&call).kind;
    let stats = HeapStats::get();
    drop(profiler);
    assert_eq!(kind, Kind::Allow);
    stats.total_blocks
}

/// Joining a command part for a glob rule, building its word starts and
/// tokenising a complex command all happen once per `decide`, not once per
/// (rule, part): doubling the rules must not move the count.
#[test]
fn perf_rules_decide_does_not_allocate_per_rule() {
    let _serial = serial();
    let few = rules(RULES);
    let many = rules(2 * RULES);
    for command in [COMMAND, COMPLEX] {
        let few_blocks = blocks_per_decide(&few, command);
        let many_blocks = blocks_per_decide(&many, command);
        eprintln!(
            "blocks: {few_blocks} per decide of {command:?} over {} rules, {many_blocks} over {}",
            3 * RULES,
            6 * RULES
        );
        assert!(
            few_blocks <= DECIDE_BLOCKS,
            "{few_blocks} blocks per decide of {command:?} over {} rules, over {DECIDE_BLOCKS}",
            3 * RULES
        );
        assert!(
            many_blocks <= DECIDE_BLOCKS,
            "{many_blocks} blocks per decide of {command:?} over {} rules, over {DECIDE_BLOCKS}",
            6 * RULES
        );
        assert!(
            many_blocks <= few_blocks,
            "{many_blocks} blocks per decide of {command:?} over {} rules against {few_blocks} \
             over {}: it grows with the rule count",
            6 * RULES,
            3 * RULES
        );
    }
}

/// The most blocks one `decide` over [`RULES`] path rules in each list may
/// allocate for [`READS`] paths: 30 measured on macOS, against 370 before each
/// path was folded to lower case once instead of once per (rule, path).
const READ_BLOCKS: u64 = 60;
const _: () = assert!(READ_BLOCKS < 100);

/// The paths a call under test reads.
const READS: [&str; 4] = [
    "/work/build/out.o",
    "/work/src/main.rs",
    "/work/keys/sign",
    "/work/logs/run.log",
];

/// Rules with `count` path globs in each list, none of which matches
/// [`READS`]: a deny and an ask rule look at every path a call reads, and so
/// does the allow rule.
fn path_rules(count: usize) -> Rules {
    let list = |kind: &str| -> Vec<String> {
        (0..count)
            .map(|n| format!("Read(/work/{kind}{n}/**)"))
            .collect()
    };
    let layer = Layer {
        path: PathBuf::from("/home/u/.config/reagent/config.toml"),
        settings: Settings {
            permissions: Lists {
                allow: list("a"),
                ask: list("k"),
                deny: list("d"),
                default: Some("allow".into()),
            },
            ..Settings::default()
        },
    };
    Rules::from_layers(&[layer], Path::new("/work"), Some(Path::new("/home/u"))).expect("the rules")
}

/// How many times a comparison measures one `decide`. dhat counts the blocks
/// of every thread in the process, so one sample can carry a few blocks that
/// are not the call's; the smallest of several is the call's own count.
const SAMPLES: usize = 5;

/// The blocks one `decide` of a call that reads [`READS`] allocates: the
/// smallest count of [`SAMPLES`] measures.
fn blocks_per_read(rules: &Rules, reads: &[PathBuf]) -> u64 {
    let call = Call {
        tool: "read",
        main_arg: Some("path"),
        value: Some("/work/src/main.rs"),
        reads,
        writes: &[],
    };
    (0..SAMPLES)
        .map(|_| {
            let profiler = Profiler::builder().testing().build();
            let kind = rules.decide(&call).kind;
            let stats = HeapStats::get();
            drop(profiler);
            assert_eq!(kind, Kind::Allow);
            stats.total_blocks
        })
        .min()
        .expect("at least one sample")
}

/// A path is made absolute and, on macOS, folded to lower case once per
/// `decide`, not once per (rule, path).
#[test]
fn perf_rules_decide_does_not_allocate_per_path_rule() {
    let _serial = serial();
    let reads: Vec<PathBuf> = READS.iter().map(PathBuf::from).collect();
    let few = path_rules(RULES);
    let many = path_rules(2 * RULES);
    let few_blocks = blocks_per_read(&few, &reads);
    let many_blocks = blocks_per_read(&many, &reads);
    eprintln!(
        "blocks: {few_blocks} per decide over {} path rules for {} paths, {many_blocks} over {}",
        3 * RULES,
        reads.len(),
        6 * RULES
    );
    for (blocks, rules) in [(few_blocks, 3 * RULES), (many_blocks, 6 * RULES)] {
        assert!(
            blocks <= READ_BLOCKS,
            "{blocks} blocks per decide over {rules} path rules, over {READ_BLOCKS}"
        );
    }
    assert!(
        many_blocks <= few_blocks,
        "{many_blocks} blocks per decide over {} path rules against {few_blocks} over {}: it \
         grows with the rule count",
        6 * RULES,
        3 * RULES
    );
}

/// The most blocks one `decide` may allocate for a call no rule covers: 0
/// measured, against 17 when the command was parsed and resolved first.
const UNCOVERED_BLOCKS: u64 = 4;

/// Rules that name another tool: a bash call must not be parsed, since no rule
/// could decide it and the default answers.
fn other_tool_rules() -> Rules {
    let layer = Layer {
        path: PathBuf::from("/home/u/.config/reagent/config.toml"),
        settings: Settings {
            permissions: Lists {
                allow: Vec::new(),
                ask: Vec::new(),
                deny: vec!["WebFetch(https://*)".into()],
                default: Some("deny".into()),
            },
            ..Settings::default()
        },
    };
    Rules::from_layers(&[layer], Path::new("/work"), Some(Path::new("/home/u"))).expect("the rules")
}

#[test]
fn perf_rules_decide_skips_a_call_no_rule_covers() {
    let _serial = serial();
    let rules = other_tool_rules();
    let call = Call {
        tool: "bash",
        main_arg: Some("command"),
        value: Some(COMMAND),
        reads: &[],
        writes: &[],
    };
    let profiler = Profiler::builder().testing().build();
    let kind = rules.decide(&call).kind;
    let stats = HeapStats::get();
    drop(profiler);
    assert_eq!(kind, Kind::Deny);
    eprintln!("blocks: {} for a call no rule covers", stats.total_blocks);
    assert!(
        stats.total_blocks <= UNCOVERED_BLOCKS,
        "{} blocks for a call no rule covers, over {UNCOVERED_BLOCKS}",
        stats.total_blocks
    );
}
