//! Behavioral checks for the config and account commands.
//!
//! These tests do not pin help prose. They check exit status, that secrets
//! given to the process do not come back out, and that file edits stay scoped.

use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

const SECRET: &str = "super-secret-value";

fn command(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mail-mcp"));
    cmd.args(args).current_dir(dir);
    for (key, _) in std::env::vars_os() {
        let Some(key) = key.to_str() else {
            continue;
        };
        if key.starts_with("MAIL_") {
            cmd.env_remove(key);
        }
    }
    cmd
}

fn run(dir: &Path, args: &[&str], stdin: Option<&[u8]>) -> Output {
    spawn(command(dir, args), args, stdin)
}

fn run_with_env(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = command(dir, args);
    for (key, value) in env {
        cmd.env(key, value);
    }
    spawn(cmd, args, None)
}

fn spawn(mut cmd: Command, args: &[&str], stdin: Option<&[u8]>) -> Output {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn mail-mcp");
    if let Some(bytes) = stdin {
        if let Some(mut pipe) = child.stdin.take() {
            let _ = pipe.write_all(bytes);
        }
    } else if let Some(pipe) = child.stdin.take() {
        drop(pipe);
    }
    let start = Instant::now();
    let timeout = Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().expect("wait") {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            if let Some(mut out) = child.stdout.take() {
                let _ = out.read_to_end(&mut stdout);
            }
            if let Some(mut err) = child.stderr.take() {
                let _ = err.read_to_end(&mut stderr);
            }
            return Output {
                status,
                stdout,
                stderr,
            };
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            panic!("mail-mcp did not exit within {timeout:?}: {args:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn combined(output: &Output) -> String {
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    text
}

fn assert_success(output: &Output) -> ExitStatus {
    assert!(
        output.status.success(),
        "status {}\n{}",
        output.status,
        combined(output)
    );
    output.status
}

fn assert_failure(output: &Output) {
    assert!(
        !output.status.success(),
        "expected failure\n{}",
        combined(output)
    );
}

#[test]
fn help_exits_cleanly_and_does_not_print_environment_secrets() {
    let dir = TempDir::new().expect("temp dir");
    let mut cmd = command(dir.path(), &["--help"]);
    cmd.env("MAIL_IMAP_DEFAULT_PASS", SECRET)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = cmd.output().expect("help");
    assert_success(&output);
    assert!(!combined(&output).contains(SECRET));
}

#[test]
fn config_without_action_fails() {
    let dir = TempDir::new().expect("temp dir");
    let output = run(dir.path(), &["config"], None);
    assert_failure(&output);
}

#[test]
fn init_writes_a_file_once_and_refuses_to_overwrite_it() {
    let dir = TempDir::new().expect("temp dir");
    let config = dir.path().join("config.toml");
    let config_arg = config.to_str().expect("utf8 path");
    let created = run(
        dir.path(),
        &["--config", config_arg, "config", "init"],
        None,
    );
    assert_success(&created);
    assert!(!combined(&created).contains(SECRET));
    let first = fs::read(&config).expect("read config");
    let text = String::from_utf8_lossy(&first);
    assert!(text.contains("[accounts]"), "{text}");
    assert!(!text.contains("pass ="), "{text}");
    assert!(!text.contains(SECRET), "{text}");

    let again = run(
        dir.path(),
        &["--config", config_arg, "config", "init"],
        None,
    );
    assert_failure(&again);
    assert!(!combined(&again).contains(SECRET));
    assert_eq!(fs::read(&config).expect("read config again"), first);
}

#[test]
fn add_on_a_pipe_fails_before_creating_a_file() {
    let dir = TempDir::new().expect("temp dir");
    let config = dir.path().join("config.toml");
    let config_arg = config.to_str().expect("utf8 path");
    let output = run(
        dir.path(),
        &["--config", config_arg, "accounts", "add"],
        Some(format!("{SECRET}\n").as_bytes()),
    );
    assert_failure(&output);
    assert!(!config.exists(), "add created a config without a terminal");
    assert!(!combined(&output).contains(SECRET));
}

#[test]
fn list_and_failed_validate_do_not_echo_secrets() {
    let dir = TempDir::new().expect("temp dir");
    let config = dir.path().join("config.toml");
    fs::write(
        &config,
        format!(
            "\
[accounts.work.imap]
host = \"imap.example.com\"
port = 993
secure = true
user = \"work@example.com\"
pass = \"{SECRET}\"

[accounts.work.smtp]
host = \"smtp.example.com\"
port = 587
user = \"work@example.com\"
pass = \"{SECRET}\"
security = \"starttls\"
"
        ),
    )
    .expect("write fixture");
    let config_arg = config.to_str().expect("utf8 path");
    let listed = run(
        dir.path(),
        &["--config", config_arg, "accounts", "list"],
        None,
    );
    assert_success(&listed);
    let text = combined(&listed);
    assert!(text.contains("work"), "{text}");
    assert!(text.contains("imap"), "{text}");
    assert!(text.contains("smtp"), "{text}");
    assert!(!text.contains(SECRET), "{text}");

    let broken = dir.path().join("broken.toml");
    fs::write(&broken, format!("pass = \"{SECRET}\"\n=\n")).expect("write broken");
    let broken_arg = broken.to_str().expect("utf8 path");
    let invalid = run(
        dir.path(),
        &["--config", broken_arg, "config", "validate"],
        None,
    );
    assert_failure(&invalid);
    assert!(!combined(&invalid).contains(SECRET));
}

#[test]
fn remove_deletes_only_the_named_file_account() {
    let dir = TempDir::new().expect("temp dir");
    let config = dir.path().join("config.toml");
    fs::write(
        &config,
        format!(
            "\
[accounts.work.imap]
host = \"imap.example.com\"
port = 993
secure = true
user = \"work@example.com\"
pass = \"{SECRET}\"

[accounts.other.imap]
host = \"imap.example.org\"
port = 993
secure = true
user = \"other@example.org\"
pass = \"kept-secret\"
"
        ),
    )
    .expect("write fixture");
    let before = fs::read(&config).expect("read before");
    let config_arg = config.to_str().expect("utf8 path");

    let invalid = run(
        dir.path(),
        &[
            "--config",
            config_arg,
            "accounts",
            "remove",
            "Not Valid",
            "--yes",
        ],
        None,
    );
    assert_failure(&invalid);
    assert_eq!(fs::read(&config).expect("unchanged"), before);
    assert!(!combined(&invalid).contains(SECRET));

    let removed = run(
        dir.path(),
        &[
            "--config", config_arg, "accounts", "remove", "work", "--yes",
        ],
        None,
    );
    assert_success(&removed);
    let text = fs::read_to_string(&config).expect("read after");
    assert!(!text.contains(SECRET), "{text}");
    assert!(text.contains("other@example.org"), "{text}");
    assert!(text.contains("kept-secret"), "{text}");
    assert!(!combined(&removed).contains(SECRET));
    assert!(!combined(&removed).contains("kept-secret"));
}

#[test]
fn explicit_missing_config_fails_for_validate_and_startup_flags() {
    let dir = TempDir::new().expect("temp dir");
    let config = dir.path().join("missing.toml");
    let config_arg = config.to_str().expect("utf8 path");
    let output = run(
        dir.path(),
        &["--config", config_arg, "config", "validate"],
        None,
    );
    assert_failure(&output);
    assert!(!config.exists());
    assert!(!combined(&output).contains(SECRET));
}

#[test]
fn remove_with_only_a_host_override_deletes_the_file_account() {
    let dir = TempDir::new().expect("temp dir");
    let config = dir.path().join("config.toml");
    fs::write(
        &config,
        format!(
            "\
[accounts.work.imap]
host = \"imap.example.com\"
port = 993
secure = true
user = \"work@example.com\"
pass = \"{SECRET}\"

[accounts.work.smtp]
host = \"smtp.example.com\"
port = 587
user = \"work@example.com\"
pass = \"{SECRET}\"
security = \"starttls\"
"
        ),
    )
    .expect("write fixture");
    let config_arg = config.to_str().expect("utf8 path");
    let removed = run_with_env(
        dir.path(),
        &[
            "--config", config_arg, "accounts", "remove", "work", "--yes",
        ],
        &[("MAIL_IMAP_WORK_HOST", "override.example")],
    );
    assert_success(&removed);
    let text = fs::read_to_string(&config).expect("read after");
    assert!(!text.contains("[accounts.work"), "{text}");
    assert!(!text.contains("work@example.com"), "{text}");
    assert!(!text.contains(SECRET), "{text}");
    let output_text = combined(&removed);
    let stderr = String::from_utf8_lossy(&removed.stderr);
    assert!(output_text.contains("MAIL_IMAP_WORK_HOST"), "{output_text}");
    assert!(!stderr.to_ascii_lowercase().contains("defined"), "{stderr}");
    assert!(!output_text.contains("override.example"), "{output_text}");
    assert!(!output_text.contains(SECRET), "{output_text}");
}

#[test]
fn explicit_missing_config_fails_for_accounts_list() {
    let dir = TempDir::new().expect("temp dir");
    let config = dir.path().join("missing.toml");
    let config_arg = config.to_str().expect("utf8 path");
    let output = run(
        dir.path(),
        &["--config", config_arg, "accounts", "list"],
        None,
    );
    assert_failure(&output);
    assert!(!config.exists());
    assert!(!combined(&output).contains(SECRET));
}
