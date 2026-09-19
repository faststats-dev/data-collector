use symbolicator::{JavaScriptMapping, Mapping, ProguardMapping, SourceMap};

macro_rules! fixture {
    ($path:literal) => {
        include_str!(concat!("fixtures/", $path))
    };
}

fn javascript(files: &[(&str, &str)]) -> JavaScriptMapping {
    let mut mapping = JavaScriptMapping::new();
    for (path, json) in files {
        mapping.insert(path, SourceMap::from_slice(json.as_bytes()).unwrap());
    }
    mapping
}

fn assert_mapping(mapping: &dyn Mapping, input: &str, expected: &str) {
    assert_eq!(mapping.apply(input).as_deref(), Some(expected));
    // Parsed mappings are reusable and applying them must not mutate state.
    assert_eq!(mapping.apply(input).as_deref(), Some(expected));
    assert_eq!(mapping.apply(""), None);
}

#[test]
fn javascript_browser_frames() {
    let mapping = javascript(&[("assets/app.js", fixture!("javascript/maps/named.js.map"))]);
    assert_mapping(
        &mapping,
        fixture!("javascript/browser-frames/input.txt"),
        fixture!("javascript/browser-frames/expected.txt"),
    );
}

#[test]
fn javascript_partial_mapping() {
    let mapping = javascript(&[(
        "https://cdn.test/assets/app.js?version=1",
        fixture!("javascript/maps/unnamed.js.map"),
    )]);
    assert_mapping(
        &mapping,
        fixture!("javascript/partial/input.txt"),
        fixture!("javascript/partial/expected.txt"),
    );
}

#[test]
fn javascript_multiple_bundles() {
    let mapping = javascript(&[
        ("first.js", fixture!("javascript/maps/unnamed.js.map")),
        ("second.js", fixture!("javascript/maps/second.js.map")),
    ]);
    assert_mapping(
        &mapping,
        fixture!("javascript/multiple-bundles/input.txt"),
        fixture!("javascript/multiple-bundles/expected.txt"),
    );
}

#[test]
fn javascript_unmapped_frames() {
    let mapping = javascript(&[("assets/app.js", fixture!("javascript/maps/named.js.map"))]);
    assert_eq!(mapping.apply(fixture!("javascript/unmapped.txt")), None);
    assert_eq!(
        JavaScriptMapping::new().apply(fixture!("javascript/browser-frames/input.txt")),
        None
    );
}

#[test]
fn javascript_invalid_mapping() {
    assert!(SourceMap::from_slice(include_bytes!("fixtures/javascript/invalid.js.map")).is_err());
}

#[test]
fn jvm_r8_mapping() {
    let mapping = ProguardMapping::parse(fixture!("jvm/r8/mapping.txt"));
    assert_mapping(
        &mapping,
        fixture!("jvm/r8/input.txt"),
        fixture!("jvm/r8/expected.txt"),
    );
}

#[test]
fn jvm_proguard_mapping() {
    let mapping = ProguardMapping::parse(fixture!("jvm/proguard/mapping.txt"));
    assert_mapping(
        &mapping,
        fixture!("jvm/proguard/input.txt"),
        fixture!("jvm/proguard/expected.txt"),
    );
}

#[test]
fn jvm_split_mapping_files() {
    let parts = [
        include_bytes!("fixtures/jvm/split/base.txt").to_vec(),
        include_bytes!("fixtures/jvm/split/feature.txt").to_vec(),
    ];
    let mapping = ProguardMapping::parse_many_bytes(&parts).unwrap();
    assert_mapping(
        &mapping,
        fixture!("jvm/split/input.txt"),
        fixture!("jvm/split/expected.txt"),
    );
}

#[test]
fn jvm_unmapped_frames() {
    let mapping = ProguardMapping::parse(fixture!("jvm/r8/mapping.txt"));
    assert_eq!(mapping.apply(fixture!("jvm/unmapped.txt")), None);
    assert_eq!(
        ProguardMapping::parse("").apply(fixture!("jvm/r8/input.txt")),
        None
    );
}

#[test]
fn jvm_invalid_utf8() {
    // A bad later file must fail the whole parse, rather than silently returning
    // the classes successfully parsed from earlier files.
    let parts = [
        include_bytes!("fixtures/jvm/split/base.txt").to_vec(),
        include_bytes!("fixtures/jvm/invalid-utf8.txt").to_vec(),
    ];
    assert!(ProguardMapping::parse_many_bytes(&parts).is_err());
}

#[test]
fn r8_inline_rewrite_outline_and_catch_all() {
    let mapping = ProguardMapping::parse(fixture!("jvm/advanced/mapping.txt"));
    assert_mapping(
        &mapping,
        fixture!("jvm/advanced/input.txt"),
        fixture!("jvm/advanced/expected.txt"),
    );
}

#[test]
fn javascript_indexed_sections() {
    let mapping = javascript(&[("bundle.js", fixture!("javascript/indexed/bundle.js.map"))]);
    assert_mapping(
        &mapping,
        fixture!("javascript/indexed/input.txt"),
        fixture!("javascript/indexed/expected.txt"),
    );
}

#[test]
fn react_native_zero_based_offsets_and_logcat() {
    let mapping = symbolicator::ReactNativeMapping::new(
        SourceMap::from_slice(fixture!("javascript/react-native/bundle.js.map").as_bytes())
            .unwrap(),
    );
    assert_mapping(
        &mapping,
        fixture!("javascript/react-native/input.txt"),
        fixture!("javascript/react-native/expected.txt"),
    );
}

#[test]
fn jvm_does_not_invent_overloads_or_apply_unrecognized_metadata() {
    let mapping = ProguardMapping::parse(fixture!("jvm/edge-cases/mapping.txt"));
    assert_mapping(
        &mapping,
        fixture!("jvm/edge-cases/input.txt"),
        fixture!("jvm/edge-cases/expected.txt"),
    );
}

#[test]
fn jvm_range_index_preserves_inline_order_and_rejects_ambiguous_overlaps() {
    let mapping = ProguardMapping::parse(fixture!("jvm/indexed-ranges/mapping.txt"));
    assert_mapping(
        &mapping,
        fixture!("jvm/indexed-ranges/input.txt"),
        fixture!("jvm/indexed-ranges/expected.txt"),
    );
}

#[test]
fn identity_mapping_still_returns_none() {
    let mapping = javascript(&[("bundle.js", fixture!("javascript/identity/bundle.js.map"))]);
    assert_eq!(
        mapping.apply(fixture!("javascript/identity/input.txt")),
        None
    );
}

#[test]
fn removing_all_synthetic_frames_returns_an_empty_replacement() {
    let mapping = ProguardMapping::parse(fixture!("jvm/synthetic-only/mapping.txt"));
    assert_mapping(
        &mapping,
        fixture!("jvm/synthetic-only/input.txt"),
        fixture!("jvm/synthetic-only/expected.txt"),
    );
}
