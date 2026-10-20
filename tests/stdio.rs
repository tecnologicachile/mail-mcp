//! Behavioral checks for the MCP server over stdio.
//!
//! Each test runs the built binary with a private `HOME`, so the default
//! config file is the one the test writes, and talks JSON-RPC over its pipes.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;

const TIMEOUT: Duration = Duration::from_secs(15);

const FILE_ACCOUNT: &str = "\
[settings]
smtp_write_enabled = true

[accounts.fileacct.imap]
host = \"imap.example.invalid\"
port = 993
secure = true
user = \"file@example.invalid\"
pass = \"file-secret\"
";

struct Dirs {
    home: TempDir,
    work: TempDir,
}

impl Dirs {
    fn with_default_config(text: &str) -> Self {
        let home = TempDir::new().expect("home dir");
        let config_dir = home.path().join(".mail-mcp");
        fs::create_dir_all(&config_dir).expect("config dir");
        fs::write(config_dir.join("config.toml"), text).expect("config file");
        Self {
            home,
            work: TempDir::new().expect("work dir"),
        }
    }
}

fn server(dirs: &Dirs, env: &[(&str, &str)]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mail-mcp"));
    cmd.current_dir(dirs.work.path())
        .env("HOME", dirs.home.path())
        .env_remove("USERPROFILE")
        .env_remove("RUST_LOG");
    for (key, _) in std::env::vars_os() {
        if key.to_str().is_some_and(|key| key.starts_with("MAIL_")) {
            cmd.env_remove(key);
        }
    }
    for (key, value) in env {
        cmd.env(key, value);
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

/// A running server whose stdout lines arrive on a channel.
struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
}

impl Session {
    fn start(mut cmd: Command) -> Self {
        let mut child = cmd.spawn().expect("spawn mail-mcp");
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("stdout");
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        if let Some(stderr) = child.stderr.take() {
            std::thread::spawn(move || {
                let mut sink = Vec::new();
                let _ = BufReader::new(stderr).read_to_end(&mut sink);
            });
        }
        Self {
            child,
            stdin,
            lines,
        }
    }

    fn send(&mut self, message: &Value) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{message}").expect("write request");
        stdin.flush().expect("flush request");
    }

    /// Send a request and return its response. Every stdout line read on the
    /// way must be a JSON-RPC 2.0 message.
    fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let line = self
                .lines
                .recv_timeout(remaining)
                .unwrap_or_else(|_| panic!("no response to {method} within {TIMEOUT:?}"));
            let message = json_rpc(&line);
            if message.get("id") == Some(&json!(id)) {
                assert!(message.get("error").is_none(), "{method} failed: {line}");
                return message;
            }
        }
    }

    fn initialize(&mut self) {
        self.request(
            1,
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "stdio-test", "version": "0"}
            }),
        );
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    }

    fn account_ids(&mut self) -> Vec<String> {
        let response = self.request(
            2,
            "tools/call",
            json!({"name": "list_all_accounts", "arguments": {}}),
        );
        let result = &response["result"];
        let envelope = match result.get("structuredContent") {
            Some(structured) => structured.clone(),
            None => {
                let text = result["content"][0]["text"].as_str().expect("tool text");
                serde_json::from_str(text).expect("tool text is JSON")
            }
        };
        envelope["data"]["accounts"]
            .as_array()
            .expect("accounts array")
            .iter()
            .map(|account| account["account_id"].as_str().expect("id").to_owned())
            .collect()
    }

    /// Close stdin, wait for exit, and check the remaining stdout lines.
    fn finish(mut self) {
        drop(self.stdin.take());
        let deadline = Instant::now() + TIMEOUT;
        while self.child.try_wait().expect("wait").is_none() {
            if Instant::now() > deadline {
                let _ = self.child.kill();
                panic!("mail-mcp did not exit after stdin closed");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        while let Ok(line) = self.lines.recv_timeout(Duration::from_secs(1)) {
            json_rpc(&line);
        }
    }
}

fn json_rpc(line: &str) -> Value {
    let message: Value = serde_json::from_str(line)
        .unwrap_or_else(|err| panic!("stdout line is not JSON ({err}): {line}"));
    assert_eq!(
        message["jsonrpc"], "2.0",
        "stdout line is not JSON-RPC: {line}"
    );
    message
}

#[test]
fn stdio_stdout_carries_only_json_rpc_and_loads_the_default_file() {
    let dirs = Dirs::with_default_config(FILE_ACCOUNT);
    let mut session = Session::start(server(&dirs, &[]));
    session.initialize();
    assert_eq!(session.account_ids(), ["fileacct"]);
    session.finish();
}

#[test]
fn mail_config_file_none_ignores_the_default_file() {
    let dirs = Dirs::with_default_config(FILE_ACCOUNT);
    let env = [
        ("MAIL_CONFIG_FILE", "none"),
        ("MAIL_IMAP_ENVACCT_HOST", "imap.example.invalid"),
        ("MAIL_IMAP_ENVACCT_USER", "env@example.invalid"),
        ("MAIL_IMAP_ENVACCT_PASS", "env-secret"),
    ];
    let mut session = Session::start(server(&dirs, &env));
    session.initialize();
    assert_eq!(session.account_ids(), ["envacct"]);
    session.finish();
}

#[test]
fn transport_in_the_default_file_does_not_leave_stdio_clients_waiting() {
    let text = FILE_ACCOUNT.replace(
        "[settings]\n",
        "[settings]\ntransport = \"http\"\nhttp_port = 0\n",
    );
    let dirs = Dirs::with_default_config(&text);
    let mut child = server(&dirs, &[]).spawn().expect("spawn mail-mcp");
    drop(child.stdin.take());
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait") {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("mail-mcp kept running with transport in the default file");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    child
        .stdout
        .take()
        .expect("stdout")
        .read_to_string(&mut stdout)
        .expect("read stdout");
    child
        .stderr
        .take()
        .expect("stderr")
        .read_to_string(&mut stderr)
        .expect("read stderr");
    assert!(!status.success(), "startup should fail");
    assert!(stdout.is_empty(), "stdout must stay empty: {stdout}");
    assert!(stderr.contains("unknown key"), "{stderr}");
}

#[test]
fn transport_still_comes_from_the_environment() {
    let dirs = Dirs::with_default_config(FILE_ACCOUNT);
    let output = server(
        &dirs,
        &[("MAIL_MCP_TRANSPORT", "http"), ("MAIL_MCP_HTTP_PORT", "0")],
    )
    .args(["config", "validate"])
    .stdin(Stdio::null())
    .output()
    .expect("validate");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(stdout.contains("transport: http"), "{stdout}");
}
