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

    fn without_config() -> Self {
        Self {
            home: TempDir::new().expect("home dir"),
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
    stderr: Receiver<String>,
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
        let stderr = child.stderr.take().expect("stderr");
        let (stderr_sender, stderr_text) = mpsc::channel();
        std::thread::spawn(move || {
            let mut text = Vec::new();
            let _ = BufReader::new(stderr).read_to_end(&mut text);
            let _ = stderr_sender.send(String::from_utf8_lossy(&text).into_owned());
        });
        Self {
            child,
            stdin,
            lines,
            stderr: stderr_text,
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

    /// Close stdin, wait for exit, check the remaining stdout lines, and
    /// return what the server wrote to stderr.
    fn finish(mut self) -> String {
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
        self.stderr
            .recv_timeout(Duration::from_secs(1))
            .expect("stderr closed after exit")
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
fn unrecognized_arguments_warn_on_stderr_and_still_start_the_server() {
    let dirs = Dirs::with_default_config(FILE_ACCOUNT);
    let mut cmd = server(&dirs, &[]);
    cmd.arg("--stdio");
    let mut session = Session::start(cmd);
    session.initialize();
    assert_eq!(session.account_ids(), ["fileacct"]);
    let stderr = session.finish();
    assert!(stderr.contains("--stdio"), "{stderr}");
}

#[test]
fn legacy_dotenv_exports_other_variables_and_supplies_mail_settings() {
    let dirs = Dirs::without_config();
    fs::write(
        dirs.work.path().join(".env"),
        "RUST_LOG=info\n\
         MAIL_IMAP_DOTACCT_HOST=imap.example.invalid\n\
         MAIL_IMAP_DOTACCT_USER=dot@example.invalid\n\
         MAIL_IMAP_DOTACCT_PASS=dot-secret\n",
    )
    .expect(".env file");
    let mut session = Session::start(server(&dirs, &[]));
    session.initialize();
    assert_eq!(session.account_ids(), ["dotacct"]);
    let stderr = session.finish();
    // The startup line is logged at info, which only RUST_LOG from `.env` enables.
    assert!(stderr.contains("starting MCP server"), "{stderr}");
}

/// A `.env` in a parent of the working directory is still found, and
/// `MAIL_DOTENV=false` turns that discovery off entirely.
#[test]
fn mail_dotenv_false_ignores_a_parent_dotenv() {
    let start = |dotenv: Option<&str>| {
        let dirs = Dirs::without_config();
        fs::write(
            dirs.work.path().join(".env"),
            "RUST_LOG=info\n\
             MAIL_IMAP_WRITE_ENABLED=true\n\
             MAIL_IMAP_DOTACCT_HOST=imap.example.invalid\n\
             MAIL_IMAP_DOTACCT_USER=dot@example.invalid\n\
             MAIL_IMAP_DOTACCT_PASS=dot-secret\n",
        )
        .expect(".env file");
        let nested = dirs.work.path().join("nested");
        fs::create_dir(&nested).expect("nested dir");
        let mut env = vec![
            ("MAIL_CONFIG_FILE", "none"),
            ("MAIL_IMAP_ENVACCT_HOST", "imap.example.invalid"),
            ("MAIL_IMAP_ENVACCT_USER", "env@example.invalid"),
            ("MAIL_IMAP_ENVACCT_PASS", "env-secret"),
        ];
        if let Some(value) = dotenv {
            env.push(("MAIL_DOTENV", value));
        }
        let mut cmd = server(&dirs, &env);
        cmd.current_dir(&nested);
        let mut session = Session::start(cmd);
        session.initialize();
        let ids = session.account_ids();
        (ids, session.finish())
    };

    let (ids, stderr) = start(None);
    assert_eq!(
        ids,
        ["dotacct", "envacct"],
        "control: the parent .env is read"
    );
    assert!(stderr.contains("starting MCP server"), "{stderr}");

    for value in ["false", "0", "off"] {
        let (ids, stderr) = start(Some(value));
        assert_eq!(ids, ["envacct"], "MAIL_DOTENV={value} still read .env");
        // RUST_LOG=info comes only from .env, so no info line means it was not exported.
        assert!(!stderr.contains("starting MCP server"), "{stderr}");
    }
}

#[test]
fn invalid_mail_dotenv_is_rejected_at_startup() {
    let dirs = Dirs::without_config();
    let output = server(
        &dirs,
        &[
            ("MAIL_CONFIG_FILE", "none"),
            ("MAIL_DOTENV", "maybe"),
            ("MAIL_IMAP_ENVACCT_HOST", "imap.example.invalid"),
            ("MAIL_IMAP_ENVACCT_USER", "env@example.invalid"),
            ("MAIL_IMAP_ENVACCT_PASS", "env-secret"),
        ],
    )
    .stdin(Stdio::null())
    .output()
    .expect("run mail-mcp");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "startup should fail");
    assert!(output.stdout.is_empty(), "stdout must stay empty");
    assert!(stderr.contains("MAIL_DOTENV"), "{stderr}");
}

/// Under systemd `ProtectHome=true` the home directory cannot be read. The
/// server must still start from the environment, as releases before the
/// config file did.
#[cfg(unix)]
#[test]
fn inaccessible_home_falls_back_to_the_environment() {
    use std::os::unix::fs::PermissionsExt;

    let dirs = Dirs::with_default_config(FILE_ACCOUNT);
    let home = dirs.home.path();
    fs::set_permissions(home, fs::Permissions::from_mode(0o000)).expect("lock home");
    let blocked = fs::metadata(home.join(".mail-mcp").join("config.toml")).is_err();
    if blocked {
        let env = [
            ("MAIL_IMAP_ENVACCT_HOST", "imap.example.invalid"),
            ("MAIL_IMAP_ENVACCT_USER", "env@example.invalid"),
            ("MAIL_IMAP_ENVACCT_PASS", "env-secret"),
        ];
        let mut session = Session::start(server(&dirs, &env));
        session.initialize();
        assert_eq!(session.account_ids(), ["envacct"]);
        let stderr = session.finish();
        assert!(stderr.contains("permission denied"), "{stderr}");
    }
    fs::set_permissions(home, fs::Permissions::from_mode(0o700)).expect("unlock home");
    // Running as root bypasses the permission check, so there is nothing to test.
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

/// Start a server with every HTTPS request sent to a local proxy that never
/// answers, and report whether the server opened a connection to it.
fn startup_contacts_the_network(env: &[(&str, &str)]) -> bool {
    let proxy = std::net::TcpListener::bind("127.0.0.1:0").expect("proxy listener");
    let proxy_url = format!("http://{}", proxy.local_addr().expect("proxy address"));
    let dirs = Dirs::without_config();
    let mut account = vec![
        ("MAIL_CONFIG_FILE", "none"),
        ("MAIL_IMAP_ENVACCT_HOST", "imap.example.invalid"),
        ("MAIL_IMAP_ENVACCT_USER", "env@example.invalid"),
        ("MAIL_IMAP_ENVACCT_PASS", "env-secret"),
    ];
    account.extend_from_slice(env);
    let mut cmd = server(&dirs, &account);
    for key in ["NO_PROXY", "no_proxy", "HTTP_PROXY", "http_proxy"] {
        cmd.env_remove(key);
    }
    for key in ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"] {
        cmd.env(key, &proxy_url);
    }
    let mut session = Session::start(cmd);
    session.initialize();
    assert_eq!(session.account_ids(), ["envacct"]);
    session.finish();
    proxy.set_nonblocking(true).expect("nonblocking proxy");
    proxy.accept().is_ok()
}

#[test]
fn mail_update_check_false_keeps_startup_off_the_network() {
    assert!(
        startup_contacts_the_network(&[]),
        "control: the default startup should ask GitHub for the latest release"
    );
    for value in ["false", "0", "off"] {
        assert!(
            !startup_contacts_the_network(&[("MAIL_UPDATE_CHECK", value)]),
            "MAIL_UPDATE_CHECK={value} still contacted the network"
        );
    }
}

#[test]
fn invalid_mail_update_check_is_rejected_at_startup() {
    let dirs = Dirs::without_config();
    let output = server(
        &dirs,
        &[
            ("MAIL_CONFIG_FILE", "none"),
            ("MAIL_UPDATE_CHECK", "sometimes"),
            ("MAIL_IMAP_ENVACCT_HOST", "imap.example.invalid"),
            ("MAIL_IMAP_ENVACCT_USER", "env@example.invalid"),
            ("MAIL_IMAP_ENVACCT_PASS", "env-secret"),
        ],
    )
    .stdin(Stdio::null())
    .output()
    .expect("run mail-mcp");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "startup should fail");
    assert!(output.stdout.is_empty(), "stdout must stay empty");
    assert!(stderr.contains("MAIL_UPDATE_CHECK"), "{stderr}");
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
