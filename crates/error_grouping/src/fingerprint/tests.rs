use super::options::{
    FrameField, FrameFields, FrameMatcher, FramePolicy, FrameRule, GroupingPolicy, SegmentSelection,
};
use super::*;

fn fingerprint(language: Language, kind: &str, stack: &str) -> Fingerprint {
    fingerprint_with_policy(language, kind, stack, &GroupingPolicy::default())
}

fn fingerprint_with_policy(
    language: Language,
    kind: &str,
    stack: &str,
    policy: &GroupingPolicy,
) -> Fingerprint {
    let trace = language.parse_stack(stack).unwrap();
    parsed(language, &trace, kind, policy).0
}

fn fingerprints(
    language: Language,
    kind: &str,
    first: &str,
    second: &str,
    policy: GroupingPolicy,
) -> (Fingerprint, Fingerprint) {
    (
        fingerprint_with_policy(language, kind, first, &policy),
        fingerprint_with_policy(language, kind, second, &policy),
    )
}

#[test]
fn ignores_common_deployment_and_runtime_noise() {
    let first = fingerprint(
        Language::JavaScript,
        "TypeError",
        "TypeError: user 123 failed\n at load (/release/a/app.js:10:2)\n at node:internal/main/run_main_module:28:49",
    );
    let second = fingerprint(
        Language::JavaScript,
        "TypeError",
        "TypeError: user 999 failed\n at load (C:\\release\\b\\app.js:800:40)\n at node:internal/main/run_main_module:99:1",
    );
    assert_eq!(first, second);
}

#[test]
fn authoritative_kind_and_frame_changes_affect_identity() {
    let stack = "at load (/app.js:1:2)";
    assert_ne!(
        fingerprint(Language::JavaScript, "TypeError", stack),
        fingerprint(Language::JavaScript, "RangeError", stack)
    );
    assert_ne!(
        fingerprint(Language::JavaScript, "TypeError", stack),
        fingerprint(Language::JavaScript, "TypeError", "at save (/app.js:1:2)")
    );
}

#[test]
fn default_policy_has_a_stable_versioned_fingerprint() {
    assert_eq!(
        fingerprint(
            Language::JavaScript,
            "TypeError",
            "TypeError: bad value\n at load (/app/main.js:8:2)"
        )
        .to_string(),
        "eg1_0a2f5bf3a327956dae63ab7149569ff7dd403d80009a90413c0b095191d80626"
    );
}

#[test]
fn parsed_header_without_frames_matches_kind_only_identity() {
    let trace = Language::Java
        .parse_stack("java.lang.RuntimeException: dynamic message")
        .unwrap();
    let policy = GroupingPolicy::default();
    assert_eq!(
        parsed(
            Language::Java,
            &trace,
            "java.lang.RuntimeException",
            &policy
        )
        .0,
        kind_only(Language::Java, "java.lang.RuntimeException", &policy)
    );
}

#[test]
fn java_exception_topology_ignores_causes_nested_under_suppressed_errors() {
    let nested = fingerprint(
        Language::Java,
        "Root",
        "Root: x\n  Suppressed: S: x\n    Caused by: A: x\nCaused by: B: x",
    );
    let linear = fingerprint(
        Language::Java,
        "Root",
        "Root: x\n  Suppressed: S: x\nCaused by: A: x\n  Caused by: B: x",
    );
    assert_ne!(nested, linear);
}

#[test]
fn terminal_cause_affects_identity() {
    let first = fingerprint(
        Language::Java,
        "Root",
        "Root: x\nCaused by: Middle: x\nCaused by: Terminal: x",
    );
    let second = fingerprint(
        Language::Java,
        "Root",
        "Root: x\nCaused by: Middle: x\nCaused by: Other: x",
    );
    assert_ne!(first, second);
}

#[test]
fn terminal_cause_frames_ignore_wrapping_topology() {
    let policy = GroupingPolicy::default().with_segments(SegmentSelection::TerminalCauseFrames);
    let direct = fingerprint_with_policy(
        Language::Java,
        "DatabaseError",
        "DatabaseError: x\n at app.Database.query(Database.java:1)",
        &policy,
    );
    let wrapped = fingerprint_with_policy(
        Language::Java,
        "ServiceError",
        "ServiceError: x\n at app.Service.run(Service.java:1)\nCaused by: DatabaseError: x\n at app.Database.query(Database.java:1)",
        &policy,
    );
    assert_eq!(direct, wrapped);
}

#[test]
fn generated_symbols_and_asset_hashes_are_deployment_noise() {
    assert_eq!(
        fingerprint(
            Language::Java,
            "Error",
            "at app.Work$$Lambda$12/0x0000000800abc123.run(Work.java:1)"
        ),
        fingerprint(
            Language::Java,
            "Error",
            "at app.Work$$Lambda$99/0x0000000800def456.run(Work.java:9)"
        )
    );
    assert_eq!(
        fingerprint(
            Language::JavaScript,
            "Error",
            "at run (/assets/app.abcdef123456.js:1:2)"
        ),
        fingerprint(
            Language::JavaScript,
            "Error",
            "at run (/assets/app.0123456789ab.js:9:8)"
        )
    );
}

#[test]
fn stable_source_roots_separate_same_named_files() {
    let controller = fingerprint(
        Language::Python,
        "ValueError",
        "Traceback (most recent call last):\n  File \"/srv/app/src/controllers/user.py\", line 1, in load\nValueError: x",
    );
    let model = fingerprint(
        Language::Python,
        "ValueError",
        "Traceback (most recent call last):\n  File \"/opt/app/src/models/user.py\", line 1, in load\nValueError: x",
    );
    assert_ne!(controller, model);
}

#[test]
fn windows_file_identity_is_case_insensitive() {
    assert_eq!(
        fingerprint(
            Language::JavaScript,
            "Error",
            "at run (C:\\App\\Src\\Main.js:1:1)"
        ),
        fingerprint(
            Language::JavaScript,
            "Error",
            "at run (c:\\app\\src\\main.js:2:2)"
        ),
    );
}

#[test]
fn package_roots_separate_same_named_dependency_files() {
    assert_ne!(
        fingerprint(
            Language::JavaScript,
            "Error",
            "at run (/app/node_modules/package-a/lib/index.js:1:1)"
        ),
        fingerprint(
            Language::JavaScript,
            "Error",
            "at run (/app/node_modules/package-b/lib/index.js:1:1)"
        ),
    );
    assert_ne!(
        fingerprint(
            Language::Rust,
            "panic",
            "0: run\n at /repo/crates/alpha/src/lib.rs:1:1"
        ),
        fingerprint(
            Language::Rust,
            "panic",
            "0: run\n at /repo/crates/beta/src/lib.rs:1:1"
        ),
    );
}

#[test]
fn rust_symbol_hashes_and_runtime_frames_are_noise() {
    let noisy = fingerprint(
        Language::Rust,
        "panic",
        "stack backtrace:\n 0: __rustc::rust_begin_unwind\n 1: core::panicking::panic_fmt\n 2: app::main::h0123456789abcdef",
    );
    let application = fingerprint(
        Language::Rust,
        "panic",
        "stack backtrace:\n 0: app::main::hfedcba9876543210",
    );
    assert_eq!(noisy, application);
}

#[test]
fn raw_stack_fallback_separates_different_unparsed_stacks() {
    let policy = GroupingPolicy::default();
    assert_ne!(
        raw_stack(Language::Java, "Error", "first unsupported stack", &policy),
        raw_stack(Language::Java, "Error", "second unsupported stack", &policy)
    );
}

#[test]
fn frames_beyond_the_fixed_limit_do_not_affect_identity() {
    let prefix = "Error: x\n at f0 (/f0.js:1:1)\n at f1 (/f1.js:1:1)\n at f2 (/f2.js:1:1)\n at f3 (/f3.js:1:1)\n at f4 (/f4.js:1:1)\n at f5 (/f5.js:1:1)\n at f6 (/f6.js:1:1)\n at f7 (/f7.js:1:1)";
    let first = format!("{prefix}\n at ignored_a (/a.js:1:1)");
    let second = format!("{prefix}\n at ignored_b (/b.js:1:1)");

    assert_eq!(
        fingerprint(Language::JavaScript, "Error", &first),
        fingerprint(Language::JavaScript, "Error", &second)
    );
}

#[test]
fn frames_within_the_fixed_limit_affect_identity() {
    let suffix = "\n at f1 (/f1.js:1:1)\n at f2 (/f2.js:1:1)\n at f3 (/f3.js:1:1)\n at f4 (/f4.js:1:1)\n at f5 (/f5.js:1:1)\n at f6 (/f6.js:1:1)\n at f7 (/f7.js:1:1)";
    let first = format!("Error: x\n at first (/a.js:1:1){suffix}");
    let second = format!("Error: x\n at second (/b.js:1:1){suffix}");

    assert_ne!(
        fingerprint(Language::JavaScript, "Error", &first),
        fingerprint(Language::JavaScript, "Error", &second)
    );
}

#[test]
fn policy_can_limit_contributing_frames() {
    let policy = GroupingPolicy::default().with_frames(FramePolicy::default().with_max_frames(1));

    let (first, second) = fingerprints(
        Language::JavaScript,
        "Error",
        "Error: x\n at shared (/shared.js:1:1)\n at first (/first.js:1:1)",
        "Error: x\n at shared (/shared.js:1:1)\n at second (/second.js:1:1)",
        policy,
    );
    assert_eq!(first, second);
}

#[test]
fn policy_can_include_runtime_frames() {
    let policy =
        GroupingPolicy::default().with_frames(FramePolicy::default().include_runtime_frames(true));

    let (first, second) = fingerprints(
        Language::Rust,
        "panic",
        "stack backtrace:\n 0: app::main\n 1: core::first",
        "stack backtrace:\n 0: app::main\n 1: core::second",
        policy,
    );
    assert_ne!(first, second);
}

#[test]
fn policy_can_exclude_the_terminal_cause() {
    let policy = GroupingPolicy::default().with_segments(SegmentSelection::Root);

    let (first, second) = fingerprints(
        Language::Java,
        "Root",
        "Root: x\n at app.Root.run(Root.java:1)\nCaused by: First: x",
        "Root: x\n at app.Root.run(Root.java:1)\nCaused by: Second: x",
        policy,
    );
    assert_eq!(first, second);
}

#[test]
fn terminal_cause_frames_group_linkage_errors_from_the_same_call_site() {
    let missing_method = r#"com.destroystokyo.paper.exception.ServerEventException: Could not pass event WorldLoadEvent to Worlds v3.12.4
  at io.papermc.paper.plugin.manager.PaperEventManager.callEvent(PaperEventManager.java:72)
  at io.papermc.paper.plugin.manager.PaperPluginManagerImpl.callEvent(PaperPluginManagerImpl.java:131)
  at org.bukkit.plugin.SimplePluginManager.callEvent(SimplePluginManager.java:627)
  at org.bukkit.event.Event.callEvent(Event.java:45)
  at net.minecraft.server.MinecraftServer.prepareLevel(MinecraftServer.java:912)
  at io.papermc.paper.world.PaperWorldLoader.loadInitialWorlds(PaperWorldLoader.java:137)
Caused by: java.lang.NoSuchMethodError: missing method
  at worlds-3.12.4-all.jar//net.thenextlvl.worlds.level.PaperLevel.createInternal(PaperLevel.java:117)
  at worlds-3.12.4-all.jar//net.thenextlvl.worlds.WorldsPlugin.supplyGlobal(WorldsPlugin.java:177)
  at worlds-3.12.4-all.jar//net.thenextlvl.worlds.level.PaperLevel.createAsync(PaperLevel.java:74)
  at worlds-3.12.4-all.jar//net.thenextlvl.worlds.listener.WorldListener.loadLevel(WorldListener.java:60)"#;
    let missing_field = r#"com.destroystokyo.paper.exception.ServerEventException: Could not pass event WorldLoadEvent to Worlds v3.12.4
  at io.papermc.paper.plugin.manager.PaperEventManager.callEvent(PaperEventManager.java:89)
  at io.papermc.paper.plugin.manager.PaperPluginManagerImpl.callEvent(PaperPluginManagerImpl.java:131)
  at org.bukkit.plugin.SimplePluginManager.callEvent(SimplePluginManager.java:629)
  at org.bukkit.event.Event.callEvent(Event.java:45)
  at net.minecraft.server.MinecraftServer.loadWorld0(MinecraftServer.java:729)
Caused by: java.lang.NoSuchFieldError: missing field
  at worlds-3.12.4-all.jar//net.thenextlvl.worlds.level.PaperLevel.createInternal(PaperLevel.java:124)
  at worlds-3.12.4-all.jar//net.thenextlvl.worlds.WorldsPlugin.supplyGlobal(WorldsPlugin.java:177)
  at worlds-3.12.4-all.jar//net.thenextlvl.worlds.level.PaperLevel.createAsync(PaperLevel.java:74)
  at worlds-3.12.4-all.jar//net.thenextlvl.worlds.listener.WorldListener.loadLevel(WorldListener.java:60)"#;

    let default = fingerprints(
        Language::Java,
        "com.destroystokyo.paper.exception.ServerEventException",
        missing_method,
        missing_field,
        GroupingPolicy::default(),
    );
    assert_ne!(default.0, default.1);

    let terminal_frames = fingerprints(
        Language::Java,
        "com.destroystokyo.paper.exception.ServerEventException",
        missing_method,
        missing_field,
        GroupingPolicy::default().with_segments(SegmentSelection::TerminalCauseFrames),
    );
    assert_eq!(terminal_frames.0, terminal_frames.1);
}

#[test]
fn terminal_cause_frames_fall_back_to_root_frames() {
    let policy = GroupingPolicy::default().with_segments(SegmentSelection::TerminalCauseFrames);
    let (run, stop) = fingerprints(
        Language::Java,
        "java.lang.RuntimeException",
        "java.lang.RuntimeException: failed\n at app.Main.run(Main.java:1)",
        "java.lang.RuntimeException: failed\n at app.Main.stop(Main.java:1)",
        policy,
    );
    assert_ne!(run, stop);
}

#[test]
fn terminal_cause_frames_use_cause_kind_when_frames_are_missing() {
    let policy = GroupingPolicy::default().with_segments(SegmentSelection::TerminalCauseFrames);
    let (first, second) = fingerprints(
        Language::Java,
        "java.lang.RuntimeException",
        "java.lang.RuntimeException: failed\nCaused by: java.lang.NoSuchMethodError",
        "java.lang.RuntimeException: failed\nCaused by: java.lang.NoSuchFieldError",
        policy,
    );
    assert_ne!(first, second);
}

#[test]
fn policy_can_select_frame_identity_fields() {
    let policy = GroupingPolicy::default()
        .with_frames(FramePolicy::default().with_fields(FrameFields::FUNCTION));

    let (first, second) = fingerprints(
        Language::JavaScript,
        "Error",
        "at load (/first.js:1:1)",
        "at load (/second.js:1:1)\n at load (/third.js:1:1)",
        policy,
    );
    assert_eq!(first, second);
}

#[test]
fn policy_can_exclude_frames() {
    let policy = GroupingPolicy::default()
        .with_frames(FramePolicy::default().with_fields(FrameFields::NONE));

    let (first, second) = fingerprints(
        Language::JavaScript,
        "Error",
        "at load (/first.js:1:1)",
        "at save (/second.js:1:1)",
        policy,
    );
    assert_eq!(first, second);
}

#[test]
fn policy_can_preserve_duplicate_frames() {
    let policy = GroupingPolicy::default()
        .with_frames(FramePolicy::default().deduplicate_adjacent_frames(false));

    let (first, second) = fingerprints(
        Language::JavaScript,
        "Error",
        "at load (/app.js:1:1)",
        "at load (/app.js:1:1)\n at load (/app.js:2:2)",
        policy,
    );
    assert_ne!(first, second);
}

#[test]
fn policy_can_exclude_frames_with_custom_matchers() {
    let exclusions = [
        FrameRule::new(FrameField::Function, FrameMatcher::prefix("")),
        FrameRule::new(FrameField::Module, FrameMatcher::prefix("")),
        FrameRule::new(FrameField::File, FrameMatcher::prefix("")),
        FrameRule::new(FrameField::Function, FrameMatcher::exact("exact")),
        FrameRule::new(FrameField::Function, FrameMatcher::prefix("vendor")),
        FrameRule::new(FrameField::Function, FrameMatcher::suffix("suffix")),
        FrameRule::new(FrameField::Function, FrameMatcher::contains("middle")),
    ];
    let policy = GroupingPolicy::default().with_frames(
        FramePolicy::default()
            .with_max_frames(1)
            .with_exclusions(exclusions),
    );

    let (filtered, expected) = fingerprints(
        Language::JavaScript,
        "Error",
        "at exact (/a.js:1:1)\n at vendorLoad (/b.js:1:1)\n at ends_suffix (/c.js:1:1)\n at has_middle_value (/d.js:1:1)\n at keep (/e.js:1:1)",
        "at keep (/e.js:1:1)",
        policy,
    );
    assert_eq!(filtered, expected);
}

#[test]
fn policy_changes_that_do_not_change_evidence_preserve_identity() {
    let input = "at load (/app.js:1:1)";
    let default = fingerprint_with_policy(
        Language::JavaScript,
        "Error",
        input,
        &GroupingPolicy::default(),
    );
    let root_only = fingerprint_with_policy(
        Language::JavaScript,
        "Error",
        input,
        &GroupingPolicy::default().with_segments(SegmentSelection::Root),
    );

    assert_eq!(default, root_only);
}

#[test]
fn legitimate_hex_filenames_are_not_treated_as_asset_hashes() {
    let values = fingerprints(
        Language::JavaScript,
        "Error",
        "at run (/assets/deadbeef.js:1:1)",
        "at run (/assets/cafebabe.js:1:1)",
        GroupingPolicy::default(),
    );
    assert_ne!(values.0, values.1);
}

#[test]
fn java_shared_frame_elisions_match_expanded_traces() {
    let elided = "Root: bad\n at app.Root.run(Root.java:1)\n at app.Shared.call(Shared.java:2)\nCaused by: Cause: bad\n at app.Cause.fail(Cause.java:3)\n ... 1 more";
    let expanded = "Root: bad\n at app.Root.run(Root.java:1)\n at app.Shared.call(Shared.java:2)\nCaused by: Cause: bad\n at app.Cause.fail(Cause.java:3)\n at app.Shared.call(Shared.java:2)";
    assert_eq!(
        fingerprint(Language::Java, "Root", elided),
        fingerprint(Language::Java, "Root", expanded)
    );
}
