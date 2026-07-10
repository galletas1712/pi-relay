use std::fs::OpenOptions;
use std::io::{BufRead, Write};
use std::path::Path;
#[cfg(unix)]
use std::process::Command;
use std::time::Duration;

fn main() {
    let mode = std::env::var("MCP_FIXTURE_MODE").unwrap_or_else(|_| "simple".to_string());
    let marker = std::env::var("MCP_FIXTURE_MARKER").ok();
    if (mode == "exit_then_fail"
        && marker
            .as_deref()
            .is_some_and(|path| Path::new(path).exists()))
        || (mode == "notification_refresh_failure"
            && marker_contains(marker.as_deref(), "REFRESH_FAILED")
            && !marker_contains(marker.as_deref(), "RECOVER"))
    {
        append_marker(marker.as_deref(), "RECONNECT_FAILED\n");
        return;
    }
    if matches!(
        mode.as_str(),
        "slow_reconnect"
            | "startup_timeout_descendant"
            | "missing_after_restart"
            | "cancel_backpressure"
    ) {
        append_marker(
            marker.as_deref(),
            &format!("START {}\n", std::process::id()),
        );
    }

fn write_stdout(line: &str) {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    writeln!(stdout, "{line}").expect("fixture writes response");
    stdout.flush().expect("fixture flushes response");
}
    let stdin = std::io::stdin();
    let mut generation = 1_u32;
    for line in stdin.lock().lines() {
        let line = line.expect("fixture reads stdin");
        let Some(id) = json_number_field(&line, "id") else {
            if line.contains(r#""method":"notifications/cancelled""#) {
                append_marker(marker.as_deref(), "CANCEL\n");
            }
            continue;
        };
        if mode == "oversize_initialize" && line.contains(r#""method":"initialize""#) {
            write_stdout(&format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"protocolVersion":"{}","capabilities":{{"tools":{{"listChanged":true}}}},"serverInfo":{{"name":"fixture","version":"1"}},"instructions":"{}"}}}}"#,
                json_string_field(&line, "protocolVersion").unwrap_or("2025-11-25"),
                "x".repeat(2 * 1024 * 1024 + 1)
            ));
            continue;
        }
        let result = if line.contains(r#""method":"initialize""#) {
            if mode == "slow_reconnect"
                && marker
                    .as_deref()
                    .and_then(|path| std::fs::read_to_string(path).ok())
                    .is_some_and(|contents| contents.matches("START").count() > 1)
            {
                append_marker(marker.as_deref(), "RECONNECT_WAITING\n");
                std::thread::sleep(Duration::from_secs(60));
            }
            if mode == "startup_timeout_descendant" {
                #[cfg(unix)]
                {
                    let descendant = Command::new("sleep")
                        .arg("60")
                        .spawn()
                        .expect("fixture spawns descendant");
                    append_marker(
                        marker.as_deref(),
                        &format!("DESCENDANT {}\n", descendant.id()),
                    );
                }
                std::thread::sleep(Duration::from_secs(60));
            }
            initialize_result(&mode, &line)
        } else if line.contains(r#""method":"tools/list""#) {
            if mode == "notification_refresh_failure"
                && generation == 2
                && !marker_contains(marker.as_deref(), "RECOVER")
            {
                append_marker(marker.as_deref(), "REFRESH_FAILED\n");
                std::process::exit(0);
            }
            if mode == "exit_then_fail" {
                if let Some(marker) = marker.clone() {
                    std::thread::spawn(move || {
                        while !marker_contains(Some(&marker), "EXIT_REQUESTED") {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        append_marker(Some(&marker), "EXITED\n");
                        std::process::exit(0);
                    });
                }
            } else if mode == "slow_reconnect" {
                if let Some(marker) = marker.clone() {
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_millis(100));
                        append_marker(Some(&marker), "EXITED\n");
                        std::process::exit(0);
                    });
                }
            }
            let result = list_result(&mode, generation, &line);
            if mode == "cancel_backpressure" {
                append_marker(marker.as_deref(), "READY_TO_BLOCK\n");
            }
            result
        } else if line.contains(r#""method":"tools/call""#) {
            if mode == "timeout" {
                append_marker(marker.as_deref(), "CALL\n");
                continue;
            }
            if mode == "cancel_backpressure" {
                append_marker(marker.as_deref(), "CALL\n");
                continue;
            }
            if mode == "parallel" {
                let response = format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"result":{{"content":[{{"type":"text","text":"parallel"}}]}}}}"#
                );
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(150));
                    write_stdout(&response);
                });
                continue;
            }
            if line.contains(r#""name":"fail""#) {
                r#"{"content":[{"type":"text","text":"expected failure"}],"isError":true}"#
                    .to_string()
            } else {
                generation = 2;
                write_stdout(r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#);
                append_marker(marker.as_deref(), "NOTIFICATION_SENT\n");
                if mode == "notification_race" {
                    std::thread::sleep(Duration::from_millis(100));
                }
                append_marker(marker.as_deref(), "CALL\n");
                format!(
                    r#"{{"content":[{{"type":"text","text":"{}"}}],"structuredContent":{{"z":1,"a":2}}}}"#,
                    json_string_field(&line, "value").unwrap_or_default()
                )
            }
        } else {
            "{}".to_string()
        };
        write_stdout(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"result":{result}}}"#
        ));
        if mode == "cancel_backpressure" && line.contains(r#""method":"tools/list""#) {
            std::thread::sleep(Duration::from_secs(60));
        }
    }
}

fn initialize_result(mode: &str, line: &str) -> String {
    let protocol = if mode == "unsupported_protocol" {
        "2099-01-01"
    } else {
        json_string_field(line, "protocolVersion").unwrap_or("2025-11-25")
    };
    let capabilities = match mode {
        "missing_tools" => "{}",
        "unsolicited_list_changed" => r#"{"tools":{"listChanged":false}}"#,
        _ => r#"{"tools":{"listChanged":true}}"#,
    };
    format!(
        r#"{{"protocolVersion":"{protocol}","capabilities":{capabilities},"serverInfo":{{"name":"fixture","version":"1"}}}}"#
    )
}

fn list_result(mode: &str, generation: u32, line: &str) -> String {
    if mode == "duplicate" {
        return r#"{"tools":[{"name":"read","description":"first","inputSchema":{"type":"object"}},{"name":"read","description":"conflicting","inputSchema":{"type":"object"}}]}"#.to_string();
    }
    if matches!(mode, "normal" | "notification_race") {
        if !line.contains(r#""cursor":"page-2""#) {
            return format!(
                r#"{{"tools":[{{"name":"echo","description":"Echo arguments v{generation}","inputSchema":{{"type":"object","properties":{{"value":{{"type":"string"}}}}}}}}],"nextCursor":"page-2"}}"#
            );
        }
        if mode == "normal" {
            return r#"{"tools":[{"name":"fail","description":"Return an MCP error","inputSchema":{"type":"object"}}]}"#.to_string();
        }
        return r#"{"tools":[]}"#.to_string();
    }
    if mode == "many_tools" {
        let tools = (0..300)
            .map(|index| {
                format!(
                    r#"{{"name":"tool_{index}","description":"fixture tool","inputSchema":{{"type":"object"}}}}"#
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        return format!(r#"{{"tools":[{tools}]}}"#);
    }
    if mode == "missing_after_restart"
        && std::env::var("MCP_FIXTURE_MARKER")
            .ok()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .is_some_and(|contents| contents.matches("START").count() > 1)
    {
        return r#"{"tools":[{"name":"different","description":"fixture tool","inputSchema":{"type":"object"}}]}"#.to_string();
    }
    if mode == "notification_refresh_failure"
        && marker_contains(
            std::env::var("MCP_FIXTURE_MARKER").ok().as_deref(),
            "RECOVER",
        )
    {
        return r#"{"tools":[{"name":"read","description":"fixture tool v2","inputSchema":{"type":"object"}}]}"#.to_string();
    }
    let name = match mode {
        "timeout" | "cancel_backpressure" | "parallel" => "echo",
        "missing_raw" => "different",
        _ => "read",
    };
    format!(
        r#"{{"tools":[{{"name":"{name}","description":"fixture tool","inputSchema":{{"type":"object"}}}}]}}"#
    )
}

fn json_number_field<'a>(line: &'a str, field: &str) -> Option<&'a str> {
    let rest = line.split_once(&format!(r#""{field}":"#))?.1;
    let end = rest
        .find(|character: char| !character.is_ascii_digit() && character != '-')
        .unwrap_or(rest.len());
    (end > 0).then_some(&rest[..end])
}

fn json_string_field<'a>(line: &'a str, field: &str) -> Option<&'a str> {
    let rest = line.split_once(&format!(r#""{field}":""#))?.1;
    let end = rest.find('"')?;
    Some(&rest[..end])
}

fn append_marker(path: Option<&str>, value: &str) {
    let Some(path) = path else {
        return;
    };
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("fixture opens marker");
    file.write_all(value.as_bytes())
        .expect("fixture appends marker");
}

fn marker_contains(path: Option<&str>, value: &str) -> bool {
    path.and_then(|path| std::fs::read_to_string(path).ok())
        .is_some_and(|contents| contents.contains(value))
}
