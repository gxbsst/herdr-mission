use std::{fs, path::Path};

#[test]
fn lifecycle_hooks_call_the_prebuilt_rust_reconciler_directly() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest: toml::Value = toml::from_str(
        &fs::read_to_string(root.join("herdr-plugin.toml")).expect("read plugin manifest"),
    )
    .expect("parse plugin manifest");
    let expected = vec!["./target/release/herdr-mission", "reconcile", "--json"];
    assert_eq!(manifest["min_herdr_version"].as_str(), Some("0.8.0"));

    let startup = manifest["startup"]
        .as_array()
        .expect("startup hooks are an array");
    assert_eq!(startup.len(), 1);
    assert_eq!(command(&startup[0]), expected);

    let events = manifest["events"]
        .as_array()
        .expect("event hooks are an array");
    assert_eq!(events.len(), 2);
    for event in events {
        assert_eq!(command(event), expected);
    }

    assert!(!root.join("events/reconcile-delivery.sh").exists());
}

fn command(entry: &toml::Value) -> Vec<&str> {
    entry["command"]
        .as_array()
        .expect("hook command is an array")
        .iter()
        .map(|value| value.as_str().expect("command part is a string"))
        .collect()
}
