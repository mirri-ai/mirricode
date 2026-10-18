//! Integration tests for `mirri` CLI argument parsing.
//!
//! These tests exercise the [`Cli`](mirri_cli::Cli) struct directly (parsing
//! only); no server is started and no side effects occur.

use clap::Parser;
use mirri_cli::{Cli, Commands};

/// Build the argv `Cli::try_parse_from` expects, mimicking the `mirri` binary
/// invoked with its program name followed by the given arguments.
fn mirri_argv(args: &[&str]) -> Vec<String> {
    std::iter::once("mirri".to_string())
        .chain(args.iter().map(|arg| (*arg).to_string()))
        .collect()
}

#[test]
fn should_parse_the_serve_command_with_default_args_when_no_options_are_provided() {
    let cli = Cli::try_parse_from(mirri_argv(&["serve"])).expect("serve parses with defaults");
    let Commands::Serve {
        host,
        port,
        ipc_path,
        workspace,
        log_level,
        auth_token,
        dangerous_bypass_auth,
        web_dir,
    } = cli.command
    else {
        panic!("expected Commands::Serve");
    };
    assert_eq!(host, "127.0.0.1");
    assert_eq!(port, 58627);
    assert!(ipc_path.is_none(), "IPC should be disabled by default");
    assert!(
        workspace.is_none(),
        "no --workspace should default to unset (the Mirri home's registry decides, \
         falling back to the current directory)"
    );
    assert_eq!(log_level, "info");
    assert!(
        auth_token.is_none(),
        "no explicit token by default (the persistent server.token loads at boot)"
    );
    assert!(
        !dangerous_bypass_auth,
        "auth bypass should be disabled by default"
    );
    assert!(web_dir.is_none(), "web-dir should be disabled by default");
}

#[test]
fn should_parse_the_serve_command_with_custom_port_and_host_when_custom_options_are_provided() {
    let cli = Cli::try_parse_from(mirri_argv(&[
        "serve",
        "--host",
        "0.0.0.0",
        "--port",
        "9999",
        "--ipc-path",
        "/tmp/mirri.sock",
        "--workspace",
        "/tmp/project",
        "--log-level",
        "debug",
    ]))
    .expect("serve parses with custom options");
    let Commands::Serve {
        host: _,
        port,
        ipc_path,
        workspace,
        log_level,
        auth_token,
        dangerous_bypass_auth: _,
        web_dir: _,
    } = cli.command
    else {
        panic!("expected Commands::Serve");
    };
    assert_eq!(port, 9999);
    assert_eq!(ipc_path.as_deref(), Some("/tmp/mirri.sock"));
    assert_eq!(workspace.as_deref(), Some("/tmp/project"));
    assert_eq!(log_level, "debug");
    assert!(
        auth_token.is_none(),
        "auth token should stay unset when --auth-token is omitted"
    );
}

#[test]
fn should_parse_the_serve_command_with_an_auth_token_when_auth_token_is_supplied() {
    let cli = Cli::try_parse_from(mirri_argv(&["serve", "--auth-token", "sup3r-s3cret"]))
        .expect("serve parses with --auth-token");
    let Commands::Serve { auth_token, .. } = cli.command else {
        panic!("expected Commands::Serve");
    };
    assert_eq!(auth_token.as_deref(), Some("sup3r-s3cret"));
}

#[test]
fn should_parse_the_serve_command_with_auth_bypass_when_dangerous_bypass_auth_is_supplied() {
    let cli = Cli::try_parse_from(mirri_argv(&["serve", "--dangerous-bypass-auth"]))
        .expect("serve parses with --dangerous-bypass-auth");
    let Commands::Serve {
        auth_token,
        dangerous_bypass_auth,
        ..
    } = cli.command
    else {
        panic!("expected Commands::Serve");
    };
    assert!(auth_token.is_none(), "bypass leaves the token unset");
    assert!(dangerous_bypass_auth, "bypass flag should be set");
}

#[test]
fn should_reject_unknown_subcommands_when_an_unknown_subcommand_is_given() {
    let error = Cli::try_parse_from(mirri_argv(&["frobnicate"]))
        .expect_err("unknown subcommand must be rejected");
    assert!(
        error.to_string().contains("frobnicate"),
        "error should mention the unknown subcommand: {error}"
    );
}

#[test]
fn should_report_the_crate_version_for_dash_dash_version_when_version_flag_is_passed() {
    let error = Cli::try_parse_from(mirri_argv(&["--version"]))
        .expect_err("--version terminates parsing with a version display");
    assert!(
        error.to_string().contains(env!("CARGO_PKG_VERSION")),
        "--version should print the crate version: {error}"
    );
}

#[test]
fn should_parse_serve_command_when_serve_subcommand_given() {
    let cli = Cli::try_parse_from(mirri_argv(&["serve"])).expect("serve parses with defaults");
    assert!(
        matches!(cli.command, Commands::Serve { .. }),
        "expected Commands::Serve, got {:?}",
        cli.command
    );
}

#[test]
fn should_parse_migrate_command_when_migrate_subcommand_given() {
    let cli = Cli::try_parse_from(mirri_argv(&["migrate"])).expect("migrate parses with defaults");
    let Commands::Migrate { workspace, dry_run } = cli.command else {
        panic!("expected Commands::Migrate");
    };
    assert!(
        workspace.is_none(),
        "workspace should default to unset (global-only migration)"
    );
    assert!(!dry_run, "dry-run should default to false");
}

#[test]
fn should_parse_dry_run_flag_when_dry_run_given() {
    let cli = Cli::try_parse_from(mirri_argv(&[
        "migrate",
        "--dry-run",
        "--workspace",
        "/tmp/x",
    ]))
    .expect("migrate parses with --dry-run");
    let Commands::Migrate { workspace, dry_run } = cli.command else {
        panic!("expected Commands::Migrate");
    };
    assert_eq!(workspace.as_deref(), Some("/tmp/x"));
    assert!(dry_run, "dry-run flag should be set");
}

#[test]
fn should_parse_session_list_command_when_session_list_subcommand_given() {
    let cli = Cli::try_parse_from(mirri_argv(&["session-list", "--workspace", "/tmp/proj"]))
        .expect("session-list parses with --workspace");
    let Commands::SessionList { workspace } = cli.command else {
        panic!("expected Commands::SessionList");
    };
    assert_eq!(workspace, "/tmp/proj");
}

#[test]
fn should_parse_config_show_command_when_config_show_subcommand_given() {
    let cli = Cli::try_parse_from(mirri_argv(&["config-show"]))
        .expect("config-show parses with defaults");
    let Commands::ConfigShow { workspace } = cli.command else {
        panic!("expected Commands::ConfigShow");
    };
    assert_eq!(workspace, ".");
}

#[test]
fn should_reject_the_auth_subcommand_when_the_managed_login_is_removed() {
    let error = Cli::try_parse_from(mirri_argv(&["auth", "login"]))
        .expect_err("the removed auth subcommand no longer parses");
    assert!(
        error.to_string().contains("unrecognized subcommand"),
        "the error must be clap's unrecognized-subcommand spelling: {error}"
    );
}

#[test]
fn should_parse_print_command_when_print_subcommand_given() {
    let cli = Cli::try_parse_from(mirri_argv(&["print", "hello world"]))
        .expect("print parses with a prompt");
    let Commands::Print {
        prompt,
        model,
        provider,
    } = cli.command
    else {
        panic!("expected Commands::Print");
    };
    assert_eq!(prompt, "hello world");
    assert!(model.is_none(), "--model should default to unset");
    assert!(provider.is_none(), "--provider should default to unset");
}

#[test]
fn should_parse_print_command_with_model_and_provider_when_flags_are_given() {
    let cli = Cli::try_parse_from(mirri_argv(&[
        "print",
        "hi",
        "--model",
        "gpt-test",
        "--provider",
        "openai",
    ]))
    .expect("print parses with --model and --provider");
    let Commands::Print {
        prompt,
        model,
        provider,
    } = cli.command
    else {
        panic!("expected Commands::Print");
    };
    assert_eq!(prompt, "hi");
    assert_eq!(model.as_deref(), Some("gpt-test"));
    assert_eq!(provider.as_deref(), Some("openai"));
}

#[test]
fn should_parse_the_web_command_with_default_args_when_no_options_are_provided() {
    let cli = Cli::try_parse_from(mirri_argv(&["web"])).expect("web parses with defaults");
    let Commands::Web {
        host,
        port,
        workspace,
        log_level,
        auth_token,
        dangerous_bypass_auth,
        web_dir,
        no_open,
    } = cli.command
    else {
        panic!("expected Commands::Web");
    };
    assert_eq!(host, "127.0.0.1");
    assert_eq!(port, 58627);
    assert!(
        workspace.is_none(),
        "workspace should default to unset (resolved at dispatch: cwd, falling back to the \
         user home when the packaged app inherits the filesystem root)"
    );
    assert_eq!(log_level, "info");
    assert!(auth_token.is_none(), "no explicit token by default");
    assert!(
        !dangerous_bypass_auth,
        "auth bypass should be disabled by default"
    );
    assert!(
        web_dir.is_none(),
        "web-dir should default to unset (probed from the executable)"
    );
    assert!(
        !no_open,
        "--no-open should default to unset (a no-op for the Rust binary)"
    );
}

#[test]
fn should_parse_the_web_command_with_the_desktop_spawn_argv_when_packaged_flags_are_given() {
    // The Desktop app spawns: `mirri web --no-open --port 58827 --log-level error`
    let cli = Cli::try_parse_from(mirri_argv(&[
        "web",
        "--no-open",
        "--port",
        "58827",
        "--log-level",
        "error",
    ]))
    .expect("web parses the Desktop spawn argv");
    let Commands::Web {
        host,
        port,
        log_level,
        no_open,
        ..
    } = cli.command
    else {
        panic!("expected Commands::Web");
    };
    assert_eq!(host, "127.0.0.1");
    assert_eq!(port, 58827);
    assert_eq!(log_level, "error");
    assert!(
        no_open,
        "--no-open should be accepted for argv compatibility"
    );
}

#[test]
fn should_parse_the_web_command_with_custom_options_when_all_options_are_provided() {
    let cli = Cli::try_parse_from(mirri_argv(&[
        "web",
        "--host",
        "0.0.0.0",
        "--port",
        "9999",
        "--workspace",
        "/tmp/project",
        "--dangerous-bypass-auth",
        "--web-dir",
        "/tmp/web-dist",
        "--auth-token",
        "sup3r-s3cret",
    ]))
    .expect("web parses with custom options");
    let Commands::Web {
        host,
        port,
        workspace,
        web_dir,
        auth_token,
        dangerous_bypass_auth,
        ..
    } = cli.command
    else {
        panic!("expected Commands::Web");
    };
    assert_eq!(host, "0.0.0.0");
    assert_eq!(port, 9999);
    assert_eq!(workspace.as_deref(), Some("/tmp/project"));
    assert_eq!(web_dir.as_deref(), Some("/tmp/web-dist"));
    assert_eq!(auth_token.as_deref(), Some("sup3r-s3cret"));
    assert!(dangerous_bypass_auth, "auth bypass flag should be set");
}

#[test]
fn should_reject_print_command_when_prompt_is_missing() {
    let error = Cli::try_parse_from(mirri_argv(&["print"]))
        .expect_err("print without a prompt must be rejected");
    assert!(
        error.to_string().contains("<PROMPT>"),
        "error should mention the missing prompt: {error}"
    );
}

#[test]
fn should_parse_doctor_command_with_no_options_when_doctor_subcommand_given() {
    let cli = Cli::try_parse_from(mirri_argv(&["doctor"])).expect("doctor parses with defaults");
    let Commands::Doctor { config } = cli.command else {
        panic!("expected Commands::Doctor");
    };
    assert!(
        config.is_none(),
        "--config should default to unset (default path)"
    );
}

#[test]
fn should_parse_doctor_config_path_when_config_flag_is_given() {
    let cli = Cli::try_parse_from(mirri_argv(&["doctor", "--config", "/tmp/candidate.toml"]))
        .expect("doctor parses with --config");
    let Commands::Doctor { config } = cli.command else {
        panic!("expected Commands::Doctor");
    };
    assert_eq!(config.as_deref(), Some("/tmp/candidate.toml"));
}
