//! External credential source tests (Task 6): every behavioral branch is
//! covered through pure functions and the injected runner — the real `az`
//! binary is never executed (CI has none, and no test may depend on the
//! host's Azure login state).

use std::collections::HashMap;

use serde_json::json;

use crate::external::{
    ExternalSourceError, AZURE_CLI, az_args, external_available, obtain_with, parse_az_token,
};

fn metadata_with(resource: Option<&str>) -> HashMap<String, serde_json::Value> {
    let mut metadata = HashMap::new();
    if let Some(resource) = resource {
        metadata.insert("resource_name".to_string(), json!(resource));
    }
    metadata
}

#[test]
fn should_extract_the_access_token_from_an_azure_cli_json_response() {
    // Given a real-shaped `az account get-access-token --output json` stdout
    let stdout = r#"{"accessToken":"eyJabc123","expiresOn":"2026-10-28 12:00:00.000000","subscription":"...","tenant":"...","tokenType":"Bearer"}"#;

    // When the token is parsed
    // Then the accessToken field comes out verbatim
    assert_eq!(
        parse_az_token(stdout).as_deref(),
        Some("eyJabc123")
    );
}

#[test]
fn should_return_none_when_the_azure_cli_output_is_not_valid_token_json() {
    // Given outputs that cannot yield a token
    // Then parsing refuses each one instead of guessing
    assert_eq!(parse_az_token("not json at all"), None);
    assert_eq!(parse_az_token(r#"{"unrelated": true}"#), None);
    assert_eq!(parse_az_token(r#"{"accessToken": 42}"#), None);
    assert_eq!(parse_az_token(r#"{"accessToken": ""}"#), None);
}

#[test]
fn should_build_the_get_access_token_args_when_resource_name_is_present() {
    // Given metadata naming the target resource
    let metadata = metadata_with(Some("https://management.azure.com"));

    // When the az arguments are assembled
    // Then the command asks for a JSON access token scoped to that resource
    assert_eq!(
        az_args(&metadata),
        vec![
            "account",
            "get-access-token",
            "--resource",
            "https://management.azure.com",
            "--output",
            "json",
        ]
    );
}

#[test]
fn should_report_missing_resource_name_through_an_empty_arg_list() {
    // Given metadata without a usable resource_name (absent, or blank)
    // When the az arguments are assembled
    // Then the caller learns there is nothing runnable
    assert!(az_args(&metadata_with(None)).is_empty());
    assert!(az_args(&metadata_with(Some("  "))).is_empty());
}

#[test]
fn should_return_the_token_when_the_runner_succeeds() {
    // Given a runner standing in for a logged-in Azure CLI
    let runner = |args: &[String]| -> Result<String, String> {
        assert_eq!(
            args,
            &[
                "account",
                "get-access-token",
                "--resource",
                "https://management.azure.com",
                "--output",
                "json",
            ]
            .map(String::from)
            .to_vec()
        );
        Ok(r#"{"accessToken":"tok-ok"}"#.to_string())
    };

    // When the external source is obtained
    // Then the parsed token comes back
    assert_eq!(
        obtain_with(&runner, &metadata_with(Some("https://management.azure.com"))).as_deref(),
        Ok("tok-ok")
    );
}

#[test]
fn should_return_re_register_guidance_when_resource_name_is_missing() {
    // Given metadata without a resource_name and a runner that must never run
    let ran = std::cell::Cell::new(false);
    let runner = |_args: &[String]| -> Result<String, String> {
        ran.set(true);
        Ok(r#"{"accessToken":"tok"}"#.to_string())
    };

    // When the external source is obtained
    let err = obtain_with(&runner, &metadata_with(None)).unwrap_err();

    // Then the error names the method and tells the user to re-register
    assert_eq!(err.method_id, AZURE_CLI);
    assert!(err.guidance.contains("resource_name"), "{}", err.guidance);
    assert!(err.guidance.contains("re-register"), "{}", err.guidance);
    assert!(!ran.get());
}

#[test]
fn should_return_az_login_guidance_when_the_runner_fails() {
    // Given a runner standing in for a missing or logged-out Azure CLI
    let runner = |_args: &[String]| -> Result<String, String> {
        Err("az: command not found".to_string())
    };

    // When the external source is obtained
    let err =
        obtain_with(&runner, &metadata_with(Some("https://management.azure.com"))).unwrap_err();

    // Then the error carries the az login action guidance
    assert_eq!(err.method_id, AZURE_CLI);
    assert!(err.guidance.contains("az login"), "{}", err.guidance);
}

#[test]
fn should_return_unreadable_token_guidance_when_the_runner_output_cannot_be_parsed() {
    // Given a runner whose stdout is not token JSON
    let runner = |_args: &[String]| -> Result<String, String> { Ok("garbage".to_string()) };

    // When the external source is obtained
    let err =
        obtain_with(&runner, &metadata_with(Some("https://management.azure.com"))).unwrap_err();

    // Then the error says the token was unreadable
    assert_eq!(err.method_id, AZURE_CLI);
    assert!(err.guidance.contains("unreadable"), "{}", err.guidance);
}

#[test]
#[cfg_attr(not(unix), ignore)]
fn should_report_azure_cli_unavailable_when_az_is_not_the_known_method() {
    // Given a method id the external module does not know
    // When availability is probed
    // Then it reports unavailable without consulting the shell
    assert!(!external_available("not-a-real-method"));
}

#[test]
fn should_equal_error_on_method_id_mismatch_semantics() {
    // Given the error shape external callers branch on
    let err = ExternalSourceError {
        method_id: "azure-cli".to_string(),
        guidance: "run `az login` first".to_string(),
    };

    // When inspected
    // Then it is a plain data error carrying its action guidance
    assert_eq!(err.method_id, "azure-cli");
    assert_eq!(err.guidance, "run `az login` first");
}
