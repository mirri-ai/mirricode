//! Integration tests for source string parsing and zip extraction (A3).

use std::io::Write;

use mirri_plugin_host::{PluginError, PluginSource, extract_zip, parse_source};

#[test]
fn should_parse_local_path_when_source_uses_local_scheme() {
    let source = parse_source("local:/home/user/my-plugin").unwrap();
    assert_eq!(
        source,
        PluginSource::Local {
            path: "/home/user/my-plugin".to_string()
        }
    );
}

#[test]
fn should_parse_local_relative_path_when_source_uses_local_scheme() {
    let source = parse_source("local:./relative/path").unwrap();
    assert_eq!(
        source,
        PluginSource::Local {
            path: "./relative/path".to_string()
        }
    );
}

#[test]
fn should_parse_github_source_when_repo_has_no_commit() {
    let source = parse_source("github:user/repo").unwrap();
    assert_eq!(
        source,
        PluginSource::Github {
            repo: "user/repo".to_string(),
            commit: None,
            kind: None
        }
    );
}

#[test]
fn should_parse_github_source_when_commit_hash_is_provided() {
    let source = parse_source("github:user/repo@abc123def").unwrap();
    assert_eq!(
        source,
        PluginSource::Github {
            repo: "user/repo".to_string(),
            commit: Some("abc123def".to_string()),
            kind: None
        }
    );
}

#[test]
fn should_parse_github_source_when_branch_name_is_provided() {
    let source = parse_source("github:user/repo@main").unwrap();
    assert_eq!(
        source,
        PluginSource::Github {
            repo: "user/repo".to_string(),
            commit: Some("main".to_string()),
            kind: None
        }
    );
}

#[test]
fn should_parse_zip_source_when_url_uses_https() {
    let source = parse_source("https://example.com/plugin.zip").unwrap();
    assert_eq!(
        source,
        PluginSource::Zip {
            url: "https://example.com/plugin.zip".to_string()
        }
    );
}

#[test]
fn should_parse_zip_source_when_url_uses_http() {
    let source = parse_source("http://example.com/plugin.zip").unwrap();
    assert_eq!(
        source,
        PluginSource::Zip {
            url: "http://example.com/plugin.zip".to_string()
        }
    );
}

// ---------------------------------------------------------------------------
// TS `resolveInstallSource` syntax (A1) — the `source.ts` wire grammar,
// accepted alongside the rust `local:` / `github:` prefixes (superset).
// ---------------------------------------------------------------------------

#[test]
fn should_parse_local_path_when_source_is_a_bare_absolute_path() {
    // TS `resolveInstallSource`: a non-URL, absolute path is a local root.
    let source = parse_source("/home/user/my-plugin").unwrap();
    assert_eq!(
        source,
        PluginSource::Local {
            path: "/home/user/my-plugin".to_string()
        }
    );
}

#[test]
fn should_reject_local_source_when_bare_path_is_relative() {
    // TS throws "Plugin root must be an absolute path" for a relative path.
    let result = parse_source("./relative/path");
    assert!(matches!(result, Err(PluginError::InstallFailed(_))));
    let result = parse_source("relative/path");
    assert!(matches!(result, Err(PluginError::InstallFailed(_))));
}

#[test]
fn should_parse_github_source_when_source_is_a_github_repo_url() {
    let source = parse_source("https://github.com/user/my-plugin").unwrap();
    assert_eq!(
        source,
        PluginSource::Github {
            repo: "user/my-plugin".to_string(),
            commit: None,
            kind: None
        }
    );
}

#[test]
fn should_strip_git_suffix_when_github_url_ends_with_dot_git() {
    // TS `repoRaw.endsWith('.git')` stripping.
    let source = parse_source("https://github.com/user/my-plugin.git").unwrap();
    assert_eq!(
        source,
        PluginSource::Github {
            repo: "user/my-plugin".to_string(),
            commit: None,
            kind: None
        }
    );
}

#[test]
fn should_parse_branch_ref_when_github_url_has_a_tree_segment() {
    let source = parse_source("https://github.com/user/my-plugin/tree/main").unwrap();
    assert_eq!(
        source,
        PluginSource::Github {
            repo: "user/my-plugin".to_string(),
            commit: Some("main".to_string()),
            kind: Some(mirri_plugin_host::GithubRefKind::Branch)
        }
    );
}

#[test]
fn should_parse_sha_ref_when_github_url_has_a_tree_segment_with_full_sha() {
    let sha = "0123456789abcdef0123456789abcdef01234567";
    let source = parse_source(&format!("https://github.com/user/my-plugin/tree/{sha}")).unwrap();
    assert_eq!(
        source,
        PluginSource::Github {
            repo: "user/my-plugin".to_string(),
            commit: Some(sha.to_string()),
            kind: Some(mirri_plugin_host::GithubRefKind::Sha)
        }
    );
}

#[test]
fn should_parse_tag_ref_when_github_url_has_a_releases_tag_segment() {
    let source =
        parse_source("https://github.com/user/my-plugin/releases/tag/v1.2.0").unwrap();
    assert_eq!(
        source,
        PluginSource::Github {
            repo: "user/my-plugin".to_string(),
            commit: Some("v1.2.0".to_string()),
            kind: Some(mirri_plugin_host::GithubRefKind::Tag)
        }
    );
}

#[test]
fn should_parse_sha_ref_when_github_url_has_a_commit_segment() {
    let sha = "0123456789abcdef";
    let source = parse_source(&format!("https://github.com/user/my-plugin/commit/{sha}")).unwrap();
    assert_eq!(
        source,
        PluginSource::Github {
            repo: "user/my-plugin".to_string(),
            commit: Some(sha.to_string()),
            kind: Some(mirri_plugin_host::GithubRefKind::Sha)
        }
    );
}

#[test]
fn should_decode_percent_escapes_when_github_url_ref_contains_them() {
    // TS `decodeURIComponent` per path segment with a raw fallback: a
    // branch named `feature/foo` becomes `feature%2Ffoo` in the URL.
    let source =
        parse_source("https://github.com/user/my-plugin/tree/feature%2Ffoo").unwrap();
    assert_eq!(
        source,
        PluginSource::Github {
            repo: "user/my-plugin".to_string(),
            commit: Some("feature/foo".to_string()),
            kind: Some(mirri_plugin_host::GithubRefKind::Branch)
        }
    );
}

#[test]
fn should_accept_www_github_host_when_github_url_uses_it() {
    let source = parse_source("https://www.github.com/user/my-plugin/tree/main").unwrap();
    assert_eq!(
        source,
        PluginSource::Github {
            repo: "user/my-plugin".to_string(),
            commit: Some("main".to_string()),
            kind: Some(mirri_plugin_host::GithubRefKind::Branch)
        }
    );
}

#[test]
fn should_fall_back_to_zip_source_when_github_url_has_unrecognised_segments() {
    // TS `resolveInstallSource`: `parseGithubUrl` returns undefined for a
    // github.com URL that matches no recognized path, and the URL then
    // falls through to the generic http(s) → zip-url arm.
    let source = parse_source("https://github.com/user/my-plugin/blob/main/readme.md").unwrap();
    assert_eq!(
        source,
        PluginSource::Zip {
            url: "https://github.com/user/my-plugin/blob/main/readme.md".to_string()
        }
    );
}

#[test]
fn should_return_error_when_source_is_empty() {
    assert!(parse_source("").is_err());
    assert!(parse_source("   ").is_err());
}

#[test]
fn should_return_error_when_local_path_is_empty() {
    assert!(parse_source("local:").is_err());
}

#[test]
fn should_return_error_when_github_repo_is_empty() {
    assert!(parse_source("github:").is_err());
}

#[test]
fn should_return_error_when_github_commit_is_empty() {
    assert!(parse_source("github:user/repo@").is_err());
    assert!(parse_source("github:@abc").is_err());
}

#[test]
fn should_return_error_when_source_format_is_unrecognised() {
    let result = parse_source("just-a-name");
    assert!(matches!(result, Err(PluginError::InstallFailed(_))));

    let result = parse_source("ftp://example.com/x");
    assert!(matches!(result, Err(PluginError::InstallFailed(_))));
}

#[test]
fn should_roundtrip_through_json_when_serializing_and_deserializing() {
    let original = PluginSource::Github {
        repo: "user/repo".to_string(),
        commit: Some("abc".to_string()),
        kind: Some(mirri_plugin_host::GithubRefKind::Tag),
    };
    let json = serde_json::to_string(&original).unwrap();
    let deserialized: PluginSource = serde_json::from_str(&json).unwrap();
    assert_eq!(original, deserialized);
}

#[test]
fn should_roundtrip_without_kind_when_github_source_has_no_ref_kind() {
    // A legacy record (or a `github:`-prefixed source) has no `kind`; the
    // JSON round-trip must not fabricate one.
    let original = PluginSource::Github {
        repo: "user/repo".to_string(),
        commit: Some("abc".to_string()),
        kind: None,
    };
    let json = serde_json::to_string(&original).unwrap();
    assert!(!json.contains("kind"), "kind must be skipped when None: {json}");
    let deserialized: PluginSource = serde_json::from_str(&json).unwrap();
    assert_eq!(original, deserialized);
}

#[test]
fn should_serialize_with_type_tag_when_source_is_local() {
    let original = PluginSource::Local {
        path: "/foo".to_string(),
    };
    let json = serde_json::to_string(&original).unwrap();
    assert!(json.contains(r#""type":"local""#));
}

#[test]
fn should_serialize_with_type_tag_when_source_is_zip() {
    let original = PluginSource::Zip {
        url: "https://x.zip".to_string(),
    };
    let json = serde_json::to_string(&original).unwrap();
    assert!(json.contains(r#""type":"zip""#));
}

// ---------------------------------------------------------------------------
// Zip extraction (A3) — the TS `extractZip`/`detectPluginRoot`/permission
// restoration of `archive.ts`, exercised on archives built in-memory with the
// `zip` crate.
// ---------------------------------------------------------------------------

/// One entry of a test archive.
struct TestZipEntry<'a> {
    name: &'a str,
    content: &'a [u8],
    is_dir: bool,
    /// Unix mode (`externalFileAttributes >>> 16`) when set.
    mode: Option<u32>,
}

impl<'a> TestZipEntry<'a> {
    fn file(name: &'a str, content: &'a [u8]) -> Self {
        Self {
            name,
            content,
            is_dir: false,
            mode: None,
        }
    }

    fn dir(name: &'a str) -> Self {
        Self {
            name,
            content: b"",
            is_dir: true,
            mode: None,
        }
    }

    // Only the `#[cfg(unix)]` extraction tests set mode bits.
    #[cfg_attr(not(unix), allow(dead_code))]
    fn with_mode(mut self, mode: u32) -> Self {
        self.mode = Some(mode);
        self
    }
}

/// Build an in-memory zip archive from the given entries.
fn build_zip(entries: &[TestZipEntry]) -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut buf);
        for entry in entries {
            let mut options = zip::write::SimpleFileOptions::default();
            if let Some(mode) = entry.mode {
                options = options.unix_permissions(mode);
            }
            if entry.is_dir {
                writer.add_directory(entry.name, options).unwrap();
            } else {
                writer.start_file(entry.name, options).unwrap();
                writer.write_all(entry.content).unwrap();
            }
        }
        writer.finish().unwrap();
    }
    buf.into_inner()
}

fn plugin_manifest(name: &str) -> Vec<u8> {
    format!(r#"{{"name":"{name}","version":"1.0.0"}}"#).into_bytes()
}

/// Given a zip whose entry escapes the destination with `..`, when the
/// archive is extracted, then the extraction fails with the TS traversal
/// error text and nothing lands outside the destination.
#[test]
fn should_reject_path_traversal_when_zip_entry_escapes_with_parent_dir() {
    let zip = build_zip(&[TestZipEntry::file("../evil.txt", b"boom")]);

    let dest = tempfile::tempdir().unwrap();
    let result = extract_zip(&zip, dest.path());
    assert!(matches!(
        result,
        Err(PluginError::InstallFailed(ref message))
            if message == "Path traversal detected in zip entry: ../evil.txt"
    ), "unexpected result: {result:?}");
    assert!(
        !dest
            .path()
            .parent()
            .expect("tempdir has a parent")
            .join("evil.txt")
            .exists(),
        "the escaped file must not be written"
    );
}

/// Given a zip entry with an absolute path, when the archive is extracted,
/// then the extraction fails with the TS traversal error text (the TS
/// `path.resolve` containment check rejects it).
#[test]
fn should_reject_path_traversal_when_zip_entry_is_absolute() {
    let zip = build_zip(&[TestZipEntry::file("/etc/passwd", b"root")]);

    let dest = tempfile::tempdir().unwrap();
    let result = extract_zip(&zip, dest.path());
    assert!(matches!(
        result,
        Err(PluginError::InstallFailed(ref message))
            if message == "Path traversal detected in zip entry: /etc/passwd"
    ), "unexpected result: {result:?}");
}

/// Given a zip entry that climbs past the destination (`a/../../evil.txt`),
/// when the archive is extracted, then the extraction fails — the lexical
/// resolution runs out of destination components and escapes it.
#[test]
fn should_reject_path_traversal_when_zip_entry_climbs_above_the_destination() {
    let zip = build_zip(&[TestZipEntry::file("a/../../evil.txt", b"boom")]);

    let dest = tempfile::tempdir().unwrap();
    let result = extract_zip(&zip, dest.path());
    assert!(matches!(
        result,
        Err(PluginError::InstallFailed(ref message))
            if message == "Path traversal detected in zip entry: a/../../evil.txt"
    ), "unexpected result: {result:?}");
    assert!(
        !dest
            .path()
            .parent()
            .expect("tempdir has a parent")
            .join("evil.txt")
            .exists(),
        "the escaped file must not be written"
    );
}

/// Given a zip entry whose `..` stays inside the destination (`a/../ok.txt`),
/// when the archive is extracted, then the entry lands in the lexically
/// resolved location — the TS `path.resolve` containment allows it.
#[test]
fn should_allow_lexically_normalized_entries_when_they_stay_inside_the_destination() {
    let zip = build_zip(&[TestZipEntry::file("a/../ok.txt", b"fine")]);

    let dest = tempfile::tempdir().unwrap();
    let root = extract_zip(&zip, dest.path()).expect("in-bounds entries extract");
    assert_eq!(root, dest.path());
    assert_eq!(
        std::fs::read(dest.path().join("ok.txt")).expect("normalized entry reads"),
        b"fine"
    );
    assert!(
        !dest.path().join("a").exists(),
        "the `a` leg of the normalized entry must not materialize"
    );
}

/// Given a zip whose entries all live under one child directory carrying the
/// manifest, when the archive is extracted, then the root detection promotes
/// the single child directory (the TS `detectPluginRoot`).
#[test]
fn should_promote_single_child_directory_when_it_carries_the_manifest() {
    let zip = build_zip(&[
        TestZipEntry::dir("my-plugin/"),
        TestZipEntry::file("my-plugin/mirri.plugin.json", &plugin_manifest("single-child")),
        TestZipEntry::file("my-plugin/README.md", b"readme"),
    ]);

    let dest = tempfile::tempdir().unwrap();
    let root = extract_zip(&zip, dest.path()).expect("extraction succeeds");
    assert_eq!(root, dest.path().join("my-plugin"));
    assert!(root.join("mirri.plugin.json").is_file());
    assert!(root.join("README.md").is_file());
    // The extraction directory itself carries no manifest.
    assert!(!dest.path().join("mirri.plugin.json").exists());
}

/// Given a zip carrying the manifest at the archive root, when the archive is
/// extracted, then the root detection leaves the extraction directory as the
/// plugin root.
#[test]
fn should_keep_the_extraction_dir_when_the_manifest_is_at_the_archive_root() {
    let zip = build_zip(&[
        TestZipEntry::file("mirri.plugin.json", &plugin_manifest("root-level")),
        TestZipEntry::file("README.md", b"readme"),
    ]);

    let dest = tempfile::tempdir().unwrap();
    let root = extract_zip(&zip, dest.path()).expect("extraction succeeds");
    assert_eq!(root, dest.path());
}

/// Given a zip with several top-level children and no rooted manifest, when
/// the archive is extracted, then the root detection does not promote (the TS
/// `detectPluginRoot` only promotes a **single** child carrying a manifest).
#[test]
fn should_not_promote_when_zip_has_multiple_top_level_children() {
    let zip = build_zip(&[
        TestZipEntry::file("a/file.txt", b"a"),
        TestZipEntry::file("b/file.txt", b"b"),
    ]);

    let dest = tempfile::tempdir().unwrap();
    let root = extract_zip(&zip, dest.path()).expect("extraction succeeds");
    assert_eq!(root, dest.path());
}

/// Given a zip whose single child directory carries no manifest, when the
/// archive is extracted, then the root detection falls back to the extraction
/// directory (the TS `hasManifest` gate on the child).
#[test]
fn should_not_promote_a_child_without_a_manifest() {
    let zip = build_zip(&[
        TestZipEntry::dir("assets/"),
        TestZipEntry::file("assets/logo.png", b"png"),
    ]);

    let dest = tempfile::tempdir().unwrap();
    let root = extract_zip(&zip, dest.path()).expect("extraction succeeds");
    assert_eq!(root, dest.path());
}

/// Given a zip entry with unix mode bits, when the archive is extracted, then
/// the file's permissions restore to `mode & 0o777` — and a zero mode leaves
/// the defaults (the TS `restoreFilePermissions`).
#[cfg(unix)]
#[test]
fn should_restore_unix_permissions_when_zip_entry_carries_mode_bits() {
    use std::os::unix::fs::PermissionsExt;

    let zip = build_zip(&[
        TestZipEntry::file("tool.sh", b"#!/bin/sh").with_mode(0o100755),
        TestZipEntry::file("plain.txt", b"hello").with_mode(0),
    ]);

    let dest = tempfile::tempdir().unwrap();
    extract_zip(&zip, dest.path()).expect("extraction succeeds");

    let exec_mode = std::fs::metadata(dest.path().join("tool.sh"))
        .expect("tool.sh exists")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(exec_mode, 0o755, "the mode bits restore to 755");
    let plain_mode = std::fs::metadata(dest.path().join("plain.txt"))
        .expect("plain.txt exists")
        .permissions()
        .mode()
        & 0o777;
    assert_ne!(plain_mode, 0, "a zero mode leaves the default permissions");
}

/// Given a zip entry carrying a symlink unix mode (the `S_IFLNK` type bit),
/// when the archive is extracted, then the entry materializes as a **regular
/// file** holding the link-target bytes — never an on-disk symlink — matching
/// the TS yauzl flow (which has no symlink concept).
#[cfg(unix)]
#[test]
fn should_materialize_symlink_entries_as_regular_files_when_extracting() {
    let zip = build_zip(&[
        TestZipEntry::file("plugin/.bin/link", b"plugin/target").with_mode(0o120777),
    ]);

    let dest = tempfile::tempdir().unwrap();
    extract_zip(&zip, dest.path()).expect("extraction succeeds");

    let link_path = dest.path().join("plugin/.bin/link");
    let metadata = std::fs::symlink_metadata(&link_path).expect("entry exists");
    assert!(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        "the symlink-mode entry must be a regular file, not a symlink"
    );
    assert_eq!(
        std::fs::read(&link_path).expect("entry reads"),
        b"plugin/target",
        "the link-target bytes are the file content"
    );
}

/// Given a zip whose recognisable plugin layout is a single child directory,
/// when a full `install_from_source` zip flow runs (the extraction step
/// only — no network), then the returned root is the promoted child and the
/// archive itself never lands on disk.
#[tokio::test]
async fn should_return_the_detected_root_when_installing_a_zip_source() {
    let zip = build_zip(&[
        TestZipEntry::dir("demo/"),
        TestZipEntry::file("demo/mirri.plugin.json", &plugin_manifest("zip-demo")),
        TestZipEntry::file("demo/README.md", b"zip readme"),
    ]);
    let zip_path = tempfile::NamedTempFile::new().expect("zip temp file");
    std::fs::write(zip_path.path(), &zip).expect("zip bytes");

    // Serve the archive from a local HTTP endpoint and install from it.
    let (addr, guard) = serve_bytes(zip_path.path().to_path_buf());
    let target = tempfile::tempdir().expect("install target");
    let url = format!("http://{addr}/plugin.zip");
    let source = PluginSource::Zip { url };
    let (root, capture) = mirri_plugin_host::install_from_source(
        &source,
        target.path(),
        &mirri_plugin_host::ReqwestGithubResolver::new(),
    )
    .await
    .expect("zip install succeeds");
    guard.abort();

    assert_eq!(root, target.path().join("demo"));
    assert!(root.join("mirri.plugin.json").is_file());
    assert!(root.join("README.md").is_file());
    // The promoted child is the plugin root; the download never persisted.
    assert!(capture.github_ref.is_none());
    assert!(capture.installed_sha.is_none());
    assert!(!target.path().join("plugin.zip").exists());
}

/// Serve `bytes` over a one-shot local HTTP endpoint (the manager-level zip
/// URL installs never touch the public network). Returns the bound address
/// and a guard whose `abort()` stops the server task.
fn serve_bytes(bytes: std::path::PathBuf) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = std_listener.local_addr().expect("local addr");
    std_listener.set_nonblocking(true).expect("nonblocking");
    let listener = tokio::net::TcpListener::from_std(std_listener).expect("tokio listener");
    let handle = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                continue;
            };
            // Read the request head (best-effort; the response does not
            // depend on it).
            let mut head = [0u8; 4096];
            let _ = socket.read(&mut head).await;
            let body = std::fs::read(&bytes).unwrap_or_default();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.write_all(&body).await;
            let _ = socket.shutdown().await;
        }
    });
    (addr, handle)
}

/// Given a zip built with `zip -r plugin.zip .`-style dot-relative entries
/// (`./` directory entries and `./file` names), when the archive is
/// extracted, then it succeeds — the TS containment check permits entries
/// resolving to the destination itself, and `./` file entries land at the
/// root.
#[test]
fn should_extract_dot_relative_entries_when_zip_uses_zip_r_style_names() {
    let zip = build_zip(&[
        TestZipEntry::dir("./"),
        TestZipEntry::file("./mirri.plugin.json", &plugin_manifest("dot-plugin")),
        TestZipEntry::file("./README.md", b"readme"),
    ]);

    let dest = tempfile::tempdir().unwrap();
    let root = extract_zip(&zip, dest.path()).expect("dot-relative entries extract");
    assert_eq!(root, dest.path());
    assert!(root.join("mirri.plugin.json").is_file());
    assert!(root.join("README.md").is_file());
}
