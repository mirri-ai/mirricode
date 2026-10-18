//! `mirri doctor` — validate the user's config.toml against the rust
//! engine's real parsing behavior (syntax, structure, unknown keys as hard
//! errors; advisory diagnostics and unknown experimental flags as warnings).
//!
//! Read-only: never writes files, never spawns processes, never performs
//! network requests, and never applies environment-variable config
//! overrides — the verdict matches what the engine would do with the file
//! in a clean environment.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::commands::mirri_data_dir;

/// Entry point from `main.rs`. Returns `Ok(())` for exit code 0; prints the
/// failure report and terminates with exit code 1 on hard errors (the only
/// process::exit call, kept at the boundary so the core stays testable).
pub async fn run_doctor(config: Option<String>) -> Result<()> {
    let home = mirri_data_dir()?;
    let code = doctor_impl(&home, config);
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

/// Pure-ish core (only file reads): resolve the target path, validate, print
/// the report, and return the exit code.
fn doctor_impl(home: &Path, config: Option<String>) -> i32 {
    let (path, explicit) = match config {
        Some(input) => (resolve_input_path(&input), true),
        None => (home.join("config.toml"), false),
    };

    if !path.exists() {
        let (status, message, code) = if explicit {
            ("ERROR", "File does not exist.", 1)
        } else {
            (
                "SKIP",
                "File does not exist; built-in defaults will apply.",
                0,
            )
        };
        print_report(
            &path,
            &format!("{status} config.toml  {}", path.display()),
            &[message.to_string()],
            code,
        );
        return code;
    }

    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) => {
            print_report(
                &path,
                &format!("ERROR config.toml  {}", path.display()),
                &[format!("Failed to read {}: {err}", path.display())],
                1,
            );
            return 1;
        }
    };

    let report = mirri_core::config::validate_config_str(
        &text,
        &path.display().to_string(),
        &mirri_embedded::embedded_server::engine_flag_registry_ids(),
    );

    if report.has_errors() {
        let mut lines = Vec::new();
        for issue in &report.issues {
            let label = match issue.status {
                mirri_core::config::CheckStatus::Warn => "WARN",
                _ => "ERROR",
            };
            let location = if issue.path.is_empty() {
                String::new()
            } else {
                format!("{}: ", render_path(&issue.path))
            };
            lines.push(format!("{label} {}", location));
            lines.push(format!("  {}", issue.message));
        }
        print_report(
            &path,
            &format!("ERROR config.toml  {}", path.display()),
            &lines,
            1,
        );
        1
    } else {
        let mut lines = Vec::new();
        if report.issues.is_empty() {
            lines.push(format!("OK config.toml  {}", path.display()));
        } else {
            lines.push(format!(
                "OK config.toml (with warnings)  {}",
                path.display()
            ));
            for issue in &report.issues {
                lines.push(format!("WARN  {}", issue.message));
            }
        }
        lines.push(String::new());
        lines.push("All checked config files are valid.".to_string());
        print_report(&path, "Mirri doctor", &lines, 0);
        0
    }
}

/// Resolve a user-supplied path: absolute passes through, relative joins the
/// current directory.
fn resolve_input_path(input: &str) -> PathBuf {
    let path = PathBuf::from(input);
    if path.is_absolute() {
        path
    } else {
        PathBuf::from(".").join(path)
    }
}

/// Render a check path (`hooks` `[0]` `pattern` → `hooks[0].pattern`).
fn render_path(path: &[String]) -> String {
    let mut out = String::new();
    for segment in path {
        if !out.is_empty() && !segment.starts_with('[') {
            out.push('.');
        }
        out.push_str(segment);
    }
    out
}

/// Print the report to the right stream: success to stdout, failure to
/// stderr (US-5.3).
fn print_report(_path: &Path, header: &str, lines: &[String], code: i32) {
    let mut text = String::from("Mirri doctor\n\n");
    text.push_str(header);
    text.push('\n');
    for line in lines {
        text.push_str(line);
        text.push('\n');
    }
    text.push('\n');
    if code == 0 {
        print!("{text}");
    } else {
        eprint!("{text}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write(path: &std::path::Path, text: &str) {
        fs::write(path, text).expect("write should succeed");
    }

    #[tokio::test]
    async fn should_skip_with_zero_exit_when_default_config_is_missing() {
        // Given a data dir with no config.toml and the env pointing at it
        let dir = tempdir().expect("tempdir should succeed");
        // When running the doctor against the (missing) default path
        let code = run_doctor_with_env(dir.path(), None).await;
        // Then the exit code is 0 and the report says skipped
        assert_eq!(code, 0);
    }

    #[tokio::test]
    async fn should_error_with_nonzero_exit_when_explicit_path_is_missing() {
        // Given an explicit config path that does not exist
        let dir = tempdir().expect("tempdir should succeed");
        let missing = dir.path().join("nope.toml");
        // When running the doctor against it
        let code =
            run_doctor_with_env(dir.path(), Some(missing.to_string_lossy().into_owned())).await;
        // Then the exit code is 1
        assert_eq!(code, 1);
    }

    #[tokio::test]
    async fn should_pass_when_config_is_valid() {
        // Given a config the engine accepts without warnings
        let dir = tempdir().expect("tempdir should succeed");
        write(&dir.path().join("config.toml"), "telemetry = false\n");
        // When running the doctor against the default path
        let code = run_doctor_with_env(dir.path(), None).await;
        // Then the exit code is 0
        assert_eq!(code, 0);
    }

    #[tokio::test]
    async fn should_fail_when_config_has_an_unknown_key() {
        // Given a config with a misspelled top-level key
        let dir = tempdir().expect("tempdir should succeed");
        write(&dir.path().join("config.toml"), "defualt_model = \"x\"\n");
        // When running the doctor
        let code = run_doctor_with_env(dir.path(), None).await;
        // Then the exit code is 1
        assert_eq!(code, 1);
    }

    /// Test seam: run the doctor with MIRRICODE_HOME pointed at `home`,
    /// capturing the exit decision instead of calling process::exit.
    async fn run_doctor_with_env(home: &std::path::Path, config: Option<String>) -> i32 {
        // The run_doctor internals are factored as
        // `doctor_impl(home: &Path, config: Option<String>) -> i32` so tests
        // avoid env mutation entirely.
        doctor_impl(home, config)
    }
}
