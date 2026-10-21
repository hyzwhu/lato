use std::process::Command;

use serde_json::{Value, json};

#[test]
fn configured_headless_agent_and_doctor_use_the_isolated_application() {
    let home = tempfile::tempdir().unwrap();
    let private_instruction = "PRIVATE_BUSINESS_APPLICATION_INSTRUCTION";
    std::fs::write(
        home.path().join("agent.json"),
        serde_json::to_vec(&json!({
            "mode":"general", "instructions":private_instruction, "tools":[],
        }))
        .unwrap(),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_lato"))
        .env("LATO_HOME", home.path())
        .args(["-p", "reply with hi only"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hi");
    let mut journals = Vec::new();
    for entry in std::fs::read_dir(home.path().join("sessions")).unwrap() {
        let path = entry.unwrap().path().join("events.jsonl");
        if path.is_file() {
            journals.push(std::fs::read_to_string(path).unwrap());
        }
    }
    assert_eq!(journals.len(), 1);
    assert!(journals[0].contains("Lato application configuration v1:"));
    assert!(journals[0].contains(private_instruction));
    assert!(!journals[0].contains("a coding agent"));

    let doctor = Command::new(env!("CARGO_BIN_EXE_lato"))
        .env("LATO_HOME", home.path())
        .args(["doctor", "--json"])
        .output()
        .unwrap();
    let report: Value = serde_json::from_slice(&doctor.stdout).unwrap();
    let check = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "agent_configuration")
        .unwrap();
    assert_eq!(check["status"], "ok");
    assert!(!String::from_utf8_lossy(&doctor.stdout).contains(private_instruction));
}

#[test]
fn malformed_application_fails_headless_instead_of_falling_back_to_coding() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("agent.json"), "{").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_lato"))
        .env("LATO_HOME", home.path())
        .args(["-p", "reply with hi only"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("agent.invalid_configuration"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("hi"));
}
