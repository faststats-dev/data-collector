//! Differential tests: expected output comes from independent JS consumers and
//! the official ProGuard/R8 retracers, never from this crate.
use std::{fs, path::PathBuf};
use symbolicator::{JavaScriptMapping, Mapping, ProguardMapping, ReactNativeMapping, SourceMap};

fn file(case: &str, name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/generated")
        .join(case)
        .join(name);
    fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "{}: {error}. Run tests/fixtures-generator/generate.sh",
            path.display()
        )
    })
}

fn check(case: &str, mapping: &dyn Mapping) {
    let input = file(case, "input.txt");
    let expected = file(case, "expected.txt");
    for (input, expected) in [
        (input.clone(), expected.clone()),
        (input.replace('\n', "\r\n"), expected.replace('\n', "\r\n")),
        (
            input.trim_end_matches('\n').to_owned(),
            expected.trim_end_matches('\n').to_owned(),
        ),
    ] {
        assert_eq!(
            mapping.apply(&input).as_deref(),
            Some(expected.as_str()),
            "{case}"
        );
    }
}

macro_rules! javascript_fixture {
    ($name:ident) => {
        #[test]
        fn $name() {
            let mut mapping = JavaScriptMapping::new();
            mapping.insert(
                "bundle.js",
                SourceMap::from_slice(file(stringify!($name), "bundle.js.map").as_bytes()).unwrap(),
            );
            check(stringify!($name), &mapping);
            assert_eq!(
                mapping
                    .apply(&file(stringify!($name), "firefox-input.txt"))
                    .as_deref(),
                Some(file(stringify!($name), "firefox-expected.txt").as_str()),
            );
        }
    };
}
javascript_fixture!(rollup);
javascript_fixture!(rolldown);
javascript_fixture!(webpack);
javascript_fixture!(metro);
javascript_fixture!(rollup_indexed);
javascript_fixture!(rolldown_indexed);
javascript_fixture!(rollup_root);
javascript_fixture!(rolldown_typescript);
javascript_fixture!(webpack_development);
javascript_fixture!(webpack_nosources);

#[test]
fn hermes() {
    let map = SourceMap::from_slice(file("hermes", "bundle.js.map").as_bytes()).unwrap();
    check("hermes", &ReactNativeMapping::new(map));
}

#[test]
fn r8() {
    check("r8", &ProguardMapping::parse(&file("r8", "mapping.txt")));
}

#[test]
fn proguard() {
    check(
        "proguard",
        &ProguardMapping::parse(&file("proguard", "mapping.txt")),
    );
}

#[test]
fn r8_exceptions() {
    check(
        "r8_exceptions",
        &ProguardMapping::parse(&file("r8_exceptions", "mapping.txt")),
    );
}

#[test]
fn proguard_exceptions() {
    check(
        "proguard_exceptions",
        &ProguardMapping::parse(&file("proguard_exceptions", "mapping.txt")),
    );
}
