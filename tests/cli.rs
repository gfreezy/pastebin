use std::{
    io::Write,
    process::{Command, Output, Stdio},
};

fn run(args: &[&str], source: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_pastebin-js"))
        .args(args)
        .env_remove("PASTEBIN_JS_ALLOW_NET")
        .env_remove("PASTEBIN_USER")
        .env_remove("PASTEBIN_PASS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(source.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn file_and_stdin_keep_result_separate_from_logs() {
    let file = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/runner.js");
    for (args, source) in [
        (vec![file], ""),
        (vec!["-"], include_str!("fixtures/runner.js")),
    ] {
        let result = run(&args, source);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(result.stdout, b"runner result");
        assert!(String::from_utf8_lossy(&result.stderr).contains("[log] runner log"));
    }
}

#[test]
fn json_reports_execution_failure_and_exit_status() {
    let result = run(
        &["--json", "-"],
        "export default () => { console.warn('before'); throw new Error('boom'); }",
    );
    assert_eq!(result.status.code(), Some(1));
    let json: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert!(json["output"].is_null());
    assert!(json["error"].as_str().unwrap().contains("boom"));
    assert_eq!(json["logs"][0], "[warn] before");
    assert!(json["duration_ms"].is_number());
}

#[test]
fn invalid_input_is_distinct_from_script_failure() {
    assert_eq!(run(&[], "").status.code(), Some(2));
    assert_eq!(run(&["--unknown"], "").status.code(), Some(2));
    assert_eq!(run(&["does-not-exist.js"], "").status.code(), Some(2));
    assert!(run(&["--help"], "").status.success());
}

#[test]
fn cidr_arguments_are_validated_and_repeatable() {
    for args in [
        vec!["--allow-cidr"],
        vec!["--allow-cidr", "bad", "-"],
        vec!["--allow-cidr", "198.18.0.0/33", "-"],
    ] {
        assert_eq!(
            run(&args, "export default () => 'ok';").status.code(),
            Some(2)
        );
    }
    let result = run(
        &[
            "--allow-cidr",
            "198.18.0.0/15",
            "--allow-cidr",
            "fd00::1/128",
            "-",
        ],
        "export default () => 'ok';",
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, b"ok");
}
