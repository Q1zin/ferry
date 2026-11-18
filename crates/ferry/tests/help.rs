use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

#[test]
fn help_works_without_ferry_state_in_cli_and_mcp() {
    let directory = std::env::temp_dir().join(format!("ferry-help-{}", std::process::id()));
    std::fs::create_dir(&directory).unwrap();
    let ferry = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ferry"));
        command
            .current_dir(&directory)
            .env_remove("HOME")
            .env_remove("USERPROFILE");
        command
    };
    for (args, expected) in [
        (&["--help"][..], "ferry help"),
        (&["help"][..], "ferry check"),
        (&["help", "check"][..], "ferry check <профиль>"),
        (&["desk", "--help"][..], "ferry desk <устройство>"),
        (&["check", "--help"][..], "ferry check <профиль>"),
        (&["crashes", "--help"][..], "ferry crashes <устройство>"),
        (&["put", "--help"][..], "ferry put <цели>"),
        (&["check", "status", "--help"][..], "ferry check status"),
        (&["help", "workflow", "run"][..], "ferry workflow run"),
    ] {
        let out = ferry().args(args).output().unwrap();
        assert_eq!(out.status.code(), Some(0), "{args:?}");
        assert!(String::from_utf8_lossy(&out.stdout).contains(expected));
    }
    let wrong = ferry().args(["help", "chekc"]).output().unwrap();
    assert_eq!(wrong.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&wrong.stderr).contains("chekc"));
    for args in [
        &["run", "test-node", "--", "echo", "--help"][..],
        &["run", "test-node", "echo", "--help"][..],
    ] {
        let out = ferry().args(args).output().unwrap();
        assert!(!out.status.success(), "treated remote arguments as help");
        assert!(out.stdout.is_empty());
    }

    let mut mcp = ferry()
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = mcp.stdin.take().unwrap();
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"ferry","arguments":{"args":["server","--help"]}}})
    )
    .unwrap();
    drop(input);
    let out = mcp.wait_with_output().unwrap();
    assert!(out.status.success());
    let reply: Value = serde_json::from_slice(&out.stdout).unwrap();
    let result = &reply["result"];
    assert_eq!(result["isError"], false);
    assert_eq!(result["structuredContent"]["exit_code"], 0);
    assert!(
        result["structuredContent"]["stdout"]
            .as_str()
            .unwrap()
            .contains("ferry server [--port")
    );
    std::fs::remove_dir(&directory).unwrap();
}
