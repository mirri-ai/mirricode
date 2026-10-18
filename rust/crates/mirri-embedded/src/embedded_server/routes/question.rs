//! Question-domain routes — the B4-a blocking-HITL surface (`question.*`).
//! `answer` / `dismiss` mirror mirri-server's handlers wire-for-wire;
//! `listPending` serves the klient `questionRequest[]` shape.

use super::Route;

pub(super) const ROUTES: &[Route] = &[
    // Answer a pending question through the session's gate (`{ok: true}`).
    Route {
        method: "question.answer",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_answer_question(params)),
    },
    // Dismiss a pending question without answering it
    // (`{ok: true, dismissed: true}`).
    Route {
        method: "question.dismiss",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_dismiss_question(params)),
    },
    // The session's pending questions (klient `questionRequest[]`, bare
    // array).
    Route {
        method: "question.listPending",
        aliases: &[],
        handler: |server, params| Box::pin(server.dispatch_question_list_pending(params)),
    },
];
