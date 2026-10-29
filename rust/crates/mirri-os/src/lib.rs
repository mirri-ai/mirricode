//! mirri-os: execution environment abstraction layer.
//!
//! Provides filesystem, process, search, glob, path-access, and PTY
//! abstractions that the agent engine uses to interact with the host system.

pub mod file_index;
pub mod fs;
pub mod glob;
pub mod home;
pub mod path_access;
pub mod process;
pub mod pty;
pub mod search;
pub mod shell;
pub mod sync_fs;

pub use fs::{DirEntry, DynFileSystem, DynFsFuture, FileScanResult, FileStat, FileSystem, LocalFs};
pub use glob::{DynGlobFuture, DynGlobMatch, GlobMatch, GlobParams, GlobResult, LocalGlob};
pub use home::{
    mirri_home_dir, os_home_dir, resolve_mirri_home_dir_from, resolve_os_home_dir_from,
};
pub use path_access::PathAccess;
pub use process::{
    ChildProcess, DynChildProcess, DynChildStdin, DynChildStream, DynOsProcess, DynProcessFuture,
    ExecOptions, LocalProcess, OsProcess, ProcessFingerprint, ProcessProbe, probe_process,
};
pub use pty::{DynPtyFuture, LocalPty, PtyConfig, PtyEvent, PtyProvider, PtySession, PtyTerminal};
pub use search::{
    ContentSearch, DynContentSearch, DynSearchFuture, MAX_OUTPUT_BYTES_NOTICE, RipgrepSearch,
    SearchMatch, SearchParams, SearchResult,
};
pub use sync_fs::{LocalSyncFs, SyncFs};
