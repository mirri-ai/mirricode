//! Integration tests for `mirri-os` process execution.

use std::collections::HashMap;

use mirri_os::{ExecOptions, LocalProcess, OsProcess};
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;

#[tokio::test]
async fn should_return_exit_code_0_when_running_echo_command() {
    // Given: a LocalProcess and the system `echo` command
    let process = LocalProcess::new();
    let opts = ExecOptions::default();

    // When: executing `echo hello`
    let mut child = process.exec("/bin/echo", &["hello"], &opts).await.unwrap();

    // Then: the process should exit with code 0
    let code = child.wait().await.unwrap();
    assert_eq!(code, 0);
}

#[tokio::test]
async fn should_capture_stdout_when_command_produces_output() {
    // Given: a LocalProcess ready to run echo
    let process = LocalProcess::new();
    let opts = ExecOptions::default();

    // When: executing `echo hello world`
    let mut child = process
        .exec("/bin/echo", &["hello", "world"], &opts)
        .await
        .unwrap();

    let mut output = String::new();
    if let Some(stdout) = child.stdout() {
        use tokio::io::AsyncReadExt;
        stdout.read_to_string(&mut output).await.unwrap();
    }
    let _code = child.wait().await.unwrap();

    // Then: stdout should contain the echoed text
    assert_eq!(output.trim(), "hello world");
}

#[tokio::test]
async fn should_see_env_var_when_exec_with_custom_env() {
    // Given: a LocalProcess and a custom environment variable
    let process = LocalProcess::new();
    let mut env = HashMap::new();
    env.insert("MIRRI_TEST_VAR".to_string(), "secret-value".to_string());
    let opts = ExecOptions::default();

    // When: running `env` with the custom env set
    let mut child = process
        .exec_with_env("/usr/bin/env", &[], env, &opts)
        .await
        .unwrap();

    let mut output = String::new();
    if let Some(stdout) = child.stdout() {
        use tokio::io::AsyncReadExt;
        stdout.read_to_string(&mut output).await.unwrap();
    }
    let _code = child.wait().await.unwrap();

    // Then: the child environment should contain our variable
    assert!(
        output.contains("MIRRI_TEST_VAR=secret-value"),
        "expected MIRRI_TEST_VAR in env output, got: {output}"
    );
}

#[tokio::test]
async fn should_run_in_correct_directory_when_cwd_is_set() {
    // Given: a temp directory as cwd
    let tmp = TempDir::new().unwrap();
    let process = LocalProcess::new();
    let opts = ExecOptions {
        cwd: Some(tmp.path().to_path_buf()),
        ..Default::default()
    };

    // When: running `pwd` with that cwd
    let mut child = process.exec("/bin/pwd", &[], &opts).await.unwrap();

    let mut output = String::new();
    if let Some(stdout) = child.stdout() {
        use tokio::io::AsyncReadExt;
        stdout.read_to_string(&mut output).await.unwrap();
    }
    let _code = child.wait().await.unwrap();

    // Then: pwd should report the temp directory (resolve symlinks for macOS /tmp)
    let canonical_tmp = std::fs::canonicalize(tmp.path()).unwrap();
    let canonical_pwd = std::fs::canonicalize(output.trim())
        .unwrap_or_else(|_| std::path::PathBuf::from(output.trim()));
    assert_eq!(canonical_pwd, canonical_tmp);
}

#[tokio::test]
async fn should_terminate_when_kill_is_called() {
    // Given: a long-running `sleep 30` process
    let process = LocalProcess::new();
    let opts = ExecOptions::default();
    let mut child = process.exec("/bin/sleep", &["30"], &opts).await.unwrap();
    // When: killing the process
    child.kill().await.unwrap();

    // Then: the process should no longer be alive. We verify by checking that
    // wait returns quickly (kill already reaped it or wait reaps it now).
    let code = child.wait().await.unwrap();
    // A signal-killed process has no exit code, so we expect -1.
    assert_eq!(code, -1);

    // Confirm the PID is no longer a live process (kill(pid, 0) should
    // fail). Unix-only: the liveness re-check rides the nix signal probe.
    #[cfg(unix)]
    {
        let pid = child.pid();
        let nix_pid = nix::unistd::Pid::from_raw(pid as i32);
        let result = nix::sys::signal::kill(nix_pid, None);
        assert!(result.is_err(), "process {pid} should be dead after kill");
    }
}

#[tokio::test]
async fn should_echo_input_when_piping_to_stdin() {
    // Given: a `cat` process with piped stdin
    let process = LocalProcess::new();
    let opts = ExecOptions::default();
    let mut child = process.exec("/bin/cat", &[], &opts).await.unwrap();

    // When: writing to stdin then closing it
    if let Some(stdin) = child.stdin() {
        stdin.write_all(b"piped-content\n").await.unwrap();
    }
    // Drop stdin so cat sees EOF.
    child.take_stdin();

    let mut output = String::new();
    if let Some(stdout) = child.stdout() {
        use tokio::io::AsyncReadExt;
        stdout.read_to_string(&mut output).await.unwrap();
    }
    let _code = child.wait().await.unwrap();

    // Then: cat should echo back what we wrote
    assert_eq!(output, "piped-content\n");
}

#[tokio::test]
async fn should_capture_stderr_when_command_writes_to_stderr() {
    // Given: a LocalProcess ready to run a command that writes to stderr.
    // Using `sh -c 'echo err >&2'` to write to stderr.
    let process = LocalProcess::new();
    let opts = ExecOptions::default();

    // When: executing a command that writes to stderr
    let mut child = process
        .exec("/bin/sh", &["-c", "echo err-msg >&2"], &opts)
        .await
        .unwrap();

    let mut stderr_output = String::new();
    if let Some(stderr) = child.stderr() {
        use tokio::io::AsyncReadExt;
        stderr.read_to_string(&mut stderr_output).await.unwrap();
    }
    let _code = child.wait().await.unwrap();

    // Then: stderr should contain the message
    assert_eq!(stderr_output.trim(), "err-msg");
}
