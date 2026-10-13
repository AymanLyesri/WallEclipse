use std::process::Command;

fn bin() -> String {
    env!("CARGO_BIN_EXE_walleclipse-rs").to_string()
}

#[test]
fn help_lists_all_subcommands() {
    let out = Command::new(bin())
        .arg("--help")
        .output()
        .expect("run --help");
    assert!(out.status.success(), "status: {:?}", out.status);
    let text = String::from_utf8_lossy(&out.stdout);
    for want in [
        "run daemon",
        "--once",
        "set MON WS PATH",
        "preload PATH",
        "reload",
        "transition [name]",
        "current|list",
    ] {
        assert!(text.contains(want), "missing {want:?} in:\n{text}");
    }
}

#[test]
fn unknown_command_exits_2() {
    let out = Command::new(bin())
        .arg("bogus-command-xyz")
        .output()
        .expect("run unknown");
    assert_eq!(out.status.code(), Some(2), "stdout: {:?}", String::from_utf8_lossy(&out.stdout));
}
