//! Integration tests for `mirri-os` path access control.

use std::path::PathBuf;

use mirri_os::PathAccess;

/// Build a `PathAccess` rooted at a synthetic workspace root `/workspace`.
fn workspace_access() -> PathAccess {
    PathAccess::new(PathBuf::from("/workspace"))
}

#[test]
fn should_resolve_dotdot_when_normalizing_path_with_dotdot() {
    // Given a workspace root and a path with a `..` component.
    let access = workspace_access();

    // When normalizing.
    let normalized = access.normalize(&PathBuf::from("foo/../bar"));

    // Then the `..` cancels out `foo`, leaving `bar` under the workspace root.
    assert_eq!(normalized, PathBuf::from("/workspace/bar"));
}

#[test]
fn should_collapse_multiple_slashes_when_normalizing() {
    // Given a path with repeated slashes.
    let access = workspace_access();

    // When normalizing.
    let normalized = access.normalize(&PathBuf::from("foo//bar"));

    // Then the repeated slashes collapse into a single separator.
    assert_eq!(normalized, PathBuf::from("/workspace/foo/bar"));
}

#[test]
fn should_resolve_relative_path_when_path_is_relative_to_workspace() {
    // Given a relative path.
    let access = workspace_access();

    // When normalizing.
    let normalized = access.normalize(&PathBuf::from("src/main.rs"));

    // Then it is resolved against the workspace root.
    assert_eq!(normalized, PathBuf::from("/workspace/src/main.rs"));
}

#[test]
fn should_return_true_when_path_is_inside_workspace() {
    // Given a path inside the workspace.
    let access = workspace_access();

    // When checking containment.
    let inside = access.is_within_workspace(&PathBuf::from("/workspace/src/main.rs"));

    // Then it is considered within the workspace.
    assert!(inside);
}

#[test]
fn should_return_false_when_path_is_outside_workspace() {
    // Given a path outside the workspace.
    let access = workspace_access();

    // When checking containment.
    let outside = access.is_within_workspace(&PathBuf::from("/etc/passwd"));

    // Then it is not considered within the workspace.
    assert!(!outside);
}

#[test]
fn should_return_true_when_path_is_workspace_root_itself() {
    // Given the workspace root path itself.
    let access = workspace_access();

    // When checking containment.
    let root_contained = access.is_within_workspace(&PathBuf::from("/workspace"));

    // Then it is considered within the workspace.
    assert!(root_contained);
}

#[test]
fn should_detect_as_sensitive_when_file_is_env_file() {
    // Given a `.env` file inside the workspace.
    let access = workspace_access();

    // When checking sensitivity.
    let sensitive = access.is_sensitive(&PathBuf::from("/workspace/.env"));

    // Then it is flagged as sensitive.
    assert!(sensitive);
}

#[test]
fn should_detect_as_sensitive_when_file_is_env_production() {
    // Given a `.env.production` file inside the workspace.
    let access = workspace_access();

    // When checking sensitivity.
    let sensitive = access.is_sensitive(&PathBuf::from("/workspace/.env.production"));

    // Then it is flagged as sensitive.
    assert!(sensitive);
}

#[test]
fn should_not_detect_as_sensitive_when_file_is_env_example() {
    // Given a `.env.example` file inside the workspace.
    let access = workspace_access();

    // When checking sensitivity.
    let sensitive = access.is_sensitive(&PathBuf::from("/workspace/.env.example"));

    // Then it is not flagged as sensitive.
    assert!(!sensitive);
}

#[test]
fn should_detect_as_sensitive_when_file_is_id_rsa_in_ssh_dir() {
    // Given an SSH private key inside a `.ssh` directory.
    let access = workspace_access();

    // When checking sensitivity.
    let sensitive = access.is_sensitive(&PathBuf::from("/workspace/.ssh/id_rsa"));

    // Then it is flagged as sensitive.
    assert!(sensitive);
}

#[test]
fn should_detect_as_sensitive_when_file_is_pem() {
    // Given a `.pem` certificate file.
    let access = workspace_access();

    // When checking sensitivity.
    let sensitive = access.is_sensitive(&PathBuf::from("/workspace/cert.pem"));

    // Then it is flagged as sensitive.
    assert!(sensitive);
}

#[test]
fn should_detect_as_sensitive_when_file_is_key_file() {
    // Given a `.key` private key file.
    let access = workspace_access();

    // When checking sensitivity.
    let sensitive = access.is_sensitive(&PathBuf::from("/workspace/private.key"));

    // Then it is flagged as sensitive.
    assert!(sensitive);
}

#[test]
fn should_detect_as_sensitive_when_file_is_credentials() {
    // Given a `credentials` file.
    let access = workspace_access();

    // When checking sensitivity.
    let sensitive = access.is_sensitive(&PathBuf::from("/workspace/credentials"));

    // Then it is flagged as sensitive.
    assert!(sensitive);
}

#[test]
fn should_detect_as_sensitive_when_path_is_git_directory() {
    // Given a path that traverses a `.git` directory.
    let access = workspace_access();

    // When checking sensitivity.
    let sensitive = access.is_sensitive(&PathBuf::from("/workspace/.git/config"));

    // Then it is flagged as sensitive because of the `.git` component.
    assert!(sensitive);
}

#[test]
fn should_pass_access_check_when_file_is_normal_in_workspace() {
    // Given a normal source file inside the workspace.
    let access = workspace_access();

    // When checking access.
    let result = access.check_access(&PathBuf::from("/workspace/src/main.rs"));

    // Then access is granted.
    assert!(result.is_ok());
}

#[test]
fn should_fail_access_check_when_file_is_outside_workspace() {
    // Given a file outside the workspace.
    let access = workspace_access();

    // When checking access.
    let result = access.check_access(&PathBuf::from("/etc/passwd"));

    // Then access is denied.
    assert!(result.is_err());
    let message = result.unwrap_err().to_string();
    assert!(
        message.contains("outside the workspace"),
        "unexpected error message: {message}"
    );
}

#[test]
fn should_fail_access_check_when_file_is_sensitive_in_workspace() {
    // Given a sensitive `.env` file inside the workspace.
    let access = workspace_access();

    // When checking access.
    let result = access.check_access(&PathBuf::from("/workspace/.env"));

    // Then access is denied.
    assert!(result.is_err());
    let message = result.unwrap_err().to_string();
    assert!(
        message.contains("sensitive file"),
        "unexpected error message: {message}"
    );
}
