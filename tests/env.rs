//! Environment layering: computed values win, layers override each other, and
//! interpolation never silently produces an empty string.

use magictree::env::build;
use magictree::manifest::is_reserved_env;
use std::collections::BTreeMap;

fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

#[test]
fn app_values_override_workspace_values() {
    let plan = build(
        map(&[("MAGICTREE_SLUG", "main")]),
        &map(&[("LOG_LEVEL", "info"), ("REGION", "eu")]),
        &map(&[("LOG_LEVEL", "debug")]),
    )
    .expect("build");

    assert_eq!(plan.vars.get("LOG_LEVEL"), Some(&"debug".to_string()));
    assert_eq!(plan.vars.get("REGION"), Some(&"eu".to_string()));
}

#[test]
fn declared_layers_cannot_shadow_computed_values() {
    let error = build(
        map(&[("MAGICTREE_SLUG", "main")]),
        &BTreeMap::new(),
        &map(&[("MAGICTREE_SLUG", "spoofed")]),
    )
    .expect_err("must refuse");
    assert!(error.to_string().contains("MAGICTREE_SLUG"), "{error}");
}

#[test]
fn values_interpolate_from_computed_and_earlier_values() {
    let plan = build(
        map(&[("MAGICTREE_PORT_api", "21001")]),
        &map(&[(
            "DATABASE_URL",
            "postgres://localhost:${MAGICTREE_PORT_api}/app",
        )]),
        &map(&[("API_BASE", "http://localhost:${MAGICTREE_PORT_api}")]),
    )
    .expect("build");

    assert_eq!(
        plan.vars.get("DATABASE_URL"),
        Some(&"postgres://localhost:21001/app".to_string())
    );
    assert_eq!(
        plan.vars.get("API_BASE"),
        Some(&"http://localhost:21001".to_string())
    );
}

#[test]
fn unknown_interpolation_is_an_error_not_an_empty_string() {
    let error = build(
        map(&[("MAGICTREE_SLUG", "main")]),
        &map(&[("BROKEN", "value-${NOT_SET}")]),
        &BTreeMap::new(),
    )
    .expect_err("must refuse");
    assert!(error.to_string().contains("NOT_SET"), "{error}");
}

#[test]
fn dotenv_output_quotes_only_what_needs_it() {
    let plan = build(
        map(&[("SIMPLE", "value"), ("SPACED", "two words")]),
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .expect("build");

    let rendered = plan.dotenv();
    assert!(rendered.contains("SIMPLE=value\n"));
    assert!(rendered.contains("SPACED=\"two words\"\n"));
    assert!(plan.export().contains("export SIMPLE=value"));
}

#[test]
fn explain_attributes_each_key_to_its_layer() {
    let plan = build(
        map(&[("COMPUTED_KEY", "c")]),
        &map(&[("WORKSPACE_KEY", "w")]),
        &map(&[("APP_KEY", "a")]),
    )
    .expect("build");

    let text = plan.explain();
    assert!(text.contains("computed"));
    assert!(text.contains("workspace"));
    assert!(text.contains("app"));
}

#[test]
fn reserved_keys_are_named_as_such() {
    assert!(is_reserved_env("MAGICTREE_PORT_web"));
    assert!(is_reserved_env("COMPOSE_PROJECT_NAME"));
    assert!(!is_reserved_env("DATABASE_URL"));
    assert!(!is_reserved_env("PORT"));
}
