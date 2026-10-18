//! Fs-domain routes — the B5-a `hostFolderBrowser` mirror: the host-side
//! folder picker surface (snake_case wire, per `protocol/src/rest/fsBrowse`).

use super::Route;

pub(super) const ROUTES: &[Route] = &[
    // List the directories under an absolute path (home when omitted).
    Route {
        method: "fs.browse",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_fs_browse(params)),
    },
    // The OS user home plus the (honestly empty) recent-roots list.
    Route {
        method: "fs.home",
        aliases: &[],
        handler: |server, _params| Box::pin(server.dispatch_fs_home()),
    },
];
