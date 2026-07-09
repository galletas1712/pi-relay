use std::path::PathBuf;
use std::process::Command;

fn main() {
    let source = PathBuf::from("tests/support/fake_mcp_server.rs");
    println!("cargo:rerun-if-changed={}", source.display());
    let extension = if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        ".exe"
    } else {
        ""
    };
    let executable = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR is set"))
        .join(format!("fake_mcp_server{extension}"));
    let status = Command::new(std::env::var_os("RUSTC").expect("RUSTC is set"))
        .arg("--edition=2021")
        .arg(&source)
        .arg("-o")
        .arg(&executable)
        .status()
        .expect("compile Rust fake MCP server");
    assert!(status.success(), "Rust fake MCP server failed to compile");
    println!(
        "cargo:rustc-env=AGENT_MCP_FAKE_SERVER={}",
        executable.display()
    );
}
