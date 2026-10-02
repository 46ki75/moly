//! Repository dependency boundaries are part of the architectural contract.
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::Path, process::Command};

#[test]
fn client_sdk_does_not_couple_cli_and_server() -> Result<(), Box<dyn std::error::Error>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = Command::new(env!("CARGO"))
        .args([
            "metadata",
            "--locked",
            "--offline",
            "--no-deps",
            "--format-version",
            "1",
        ])
        .current_dir(root)
        .output()?;
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: Value = serde_json::from_slice(&output.stdout)?;
    let packages = metadata["packages"].as_array().ok_or("missing packages")?;
    let names: BTreeSet<_> = packages.iter().filter_map(|p| p["name"].as_str()).collect();
    assert_eq!(
        names,
        BTreeSet::from([
            "moly",
            "moly-client",
            "moly-protocol",
            "moly-server",
            "moly-provider-openai"
        ])
    );
    for (role, expected) in [
        ("moly", vec!["moly-client"]),
        ("moly-client", vec!["moly-protocol"]),
        ("moly-server", vec!["moly-protocol"]),
        ("moly-provider-openai", vec!["moly-protocol"]),
        ("moly-protocol", vec![]),
    ] {
        let package = packages
            .iter()
            .find(|p| p["name"] == role)
            .ok_or("missing role")?;
        let dependencies = package["dependencies"]
            .as_array()
            .ok_or("missing dependencies")?;
        let local: Vec<_> = dependencies
            .iter()
            .filter(|d| !d["path"].is_null())
            .filter_map(|d| d["name"].as_str())
            .collect();
        assert_eq!(
            local, expected,
            "{role} must preserve role boundaries, including test dependencies"
        );
        let targets = package["targets"].as_array().ok_or("missing targets")?;
        if matches!(role, "moly" | "moly-server" | "moly-provider-openai") {
            assert!(
                targets
                    .iter()
                    .any(|target| target["kind"] == json!(["bin"]) && target["name"] == role)
            );
            assert!(
                targets
                    .iter()
                    .all(|target| target["crate_types"] == json!(["bin"])),
                "application roles must stay binary-only"
            );
        } else {
            assert!(
                targets
                    .iter()
                    .any(|target| target["kind"] == json!(["lib"]))
            );
        }
        if role != "moly-provider-openai" {
            assert!(
                !dependencies.iter().any(|d| d["name"] == "reqwest"),
                "model HTTP belongs behind the Provider protocol, not in Server or Client"
            );
        }
        if role == "moly-protocol" {
            assert!(
                !dependencies
                    .iter()
                    .any(|d| matches!(d["name"].as_str(), Some("tokio" | "interprocess"))),
                "protocol must not become a transport runtime"
            );
        }
    }
    Ok(())
}
