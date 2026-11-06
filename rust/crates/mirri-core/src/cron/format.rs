//! LLM-facing rendering for the cron domain (B2 T9): the `<cron-fire>`
//! envelope the scheduler wraps every injected prompt in — the verbatim
//! port of TS `app/cron/format.ts` (`renderCronFireXml`).
//!
//! The envelope is load-bearing for consumers: the web client peels it
//! back with `extractCronPrompt` (the `<prompt>\n…\n</prompt>` body) with a
//! line-level `<cron-fire …>`/`</cron-fire>` fallback, so the attribute
//! spelling (`jobId`, `coalescedCount` — camelCase) and the line structure
//! must match the TS renderer exactly. The prompt itself is never escaped
//! — it is wrapped verbatim.

use super::CronFire;

/// The TS `renderCronFireXml` attribute escaping: `&` and `"` only (the
/// value never reaches a real XML parser as an attribute — the consumers
/// peel the envelope textually).
fn escape_attr(value: &str) -> String {
    value.replace('&', "&amp;").replace('"', "&quot;")
}

/// Render the `<cron-fire>` envelope around a fired prompt (TS
/// `renderCronFireXml`, `app/cron/format.ts`):
///
/// ```text
/// <cron-fire jobId="…" cron="…" recurring="…" coalescedCount="…" stale="…">
/// <prompt>
/// {prompt verbatim}
/// </prompt>
/// </cron-fire>
/// ```
///
/// Attribute values are `&`/`"`-escaped; the prompt is wrapped verbatim
/// (never escaped — a prompt carrying markup stays readable).
pub fn render_cron_fire_envelope(fire: &CronFire) -> String {
    let job_id = if fire.task_id.is_empty() {
        "unknown".to_string()
    } else {
        escape_attr(&fire.task_id)
    };
    let cron = if fire.cron.is_empty() {
        "unknown".to_string()
    } else {
        escape_attr(&fire.cron)
    };
    let recurring = if fire.recurring { "true" } else { "false" };
    let coalesced_count = fire.coalesced_count.to_string();
    let stale = if fire.stale { "true" } else { "false" };
    // S3 (F.2): the maxed-out mark rides the envelope's terminal-signal
    // slot — the same presentation a stale finale uses, so consumers
    // treat "this is the last delivery" uniformly.
    let maxed_out = if fire.maxed_out { "true" } else { "false" };
    // C.2a (stage-1 batch-2): the lateness mark rides only LATE fires —
    // an on-time or run-now fire keeps the envelope exactly as before,
    // so consumers that never read the key see zero change.
    let late_attr = match fire.late_by_ms {
        Some(ms) => format!(" lateByMs=\"{ms}\""),
        None => String::new(),
    };
    [
        format!(
            "<cron-fire jobId=\"{job_id}\" cron=\"{cron}\" recurring=\"{recurring}\" \
             coalescedCount=\"{coalesced_count}\" stale=\"{stale}\" maxedOut=\"{maxed_out}\"\
             {late_attr}>"
        ),
        "<prompt>".to_string(),
        fire.prompt.clone(),
        "</prompt>".to_string(),
        "</cron-fire>".to_string(),
    ]
    .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fire() -> CronFire {
        CronFire {
            task_id: "cron-01J8Z3".into(),
            prompt: "Check the build status.\nReport failures.".into(),
            cron: "*/5 * * * *".into(),
            recurring: true,
            coalesced_count: 3,
            stale: false,
            maxed_out: false,
            readonly: false,
            scheduled_at: None,
            late_by_ms: None,
            scope: crate::cron::TaskScope::Session,
            ended: false,
        }
    }

    #[test]
    fn should_wrap_the_prompt_verbatim_in_the_cron_fire_envelope() {
        // Given a fired cron task, when rendered
        let xml = render_cron_fire_envelope(&fire());
        // Then the opening tag matches the TS shape (camelCase attributes)
        // and the prompt rides verbatim inside the <prompt> body — the
        // exact lines the web client's extractCronPrompt peels.
        assert_eq!(
            xml,
            "<cron-fire jobId=\"cron-01J8Z3\" cron=\"*/5 * * * *\" \
             recurring=\"true\" coalescedCount=\"3\" stale=\"false\" maxedOut=\"false\">\n\
             <prompt>\n\
             Check the build status.\nReport failures.\n\
             </prompt>\n\
             </cron-fire>"
        );
    }

    #[test]
    fn should_escape_attributes_when_the_cron_expression_carries_markup() {
        // Given a cron expression with an ampersand and a quote
        let mut one = fire();
        one.cron = "5 & \"o'clock\"".into();
        let xml = render_cron_fire_envelope(&one);
        // Then the attribute is escaped (& and " only — the TS caliber)…
        assert!(
            xml.contains("cron=\"5 &amp; &quot;o'clock&quot;\""),
            "the attribute escaping is & and \" only: {xml}"
        );
        // …and the prompt is never escaped, even when it names the closing
        // tag (verbatim wrapping semantics).
        let mut tricky = fire();
        tricky.prompt = "contains </cron-fire> and <prompt> tags".into();
        let xml = render_cron_fire_envelope(&tricky);
        assert!(
            xml.contains("contains </cron-fire> and <prompt> tags"),
            "the prompt stays verbatim: {xml}"
        );
    }

    #[test]
    fn should_carry_the_late_by_ms_attribute_when_the_fire_is_late() {
        // Given a late fire (C.2a), when rendered
        let mut late = fire();
        late.late_by_ms = Some(7_200_000);
        let xml = render_cron_fire_envelope(&late);
        // Then the lateness mark rides the opening tag
        assert!(
            xml.contains("lateByMs=\"7200000\""),
            "the late fire carries its lateness: {xml}"
        );
    }

    #[test]
    fn should_omit_the_late_by_ms_attribute_when_the_fire_is_on_time() {
        // Given an on-time fire (late_by_ms None — the default), when
        // rendered
        let xml = render_cron_fire_envelope(&fire());
        // Then the envelope carries no lateness key at all (an on-time
        // fire is byte-identical to the pre-C.2a shape)
        assert!(
            !xml.contains("lateByMs"),
            "an on-time fire carries no lateness mark: {xml}"
        );
    }

    #[test]
    fn should_spell_the_booleans_and_fall_back_when_the_identity_is_empty() {
        // Given a stale one-shot fire with no cron expression
        let empty = CronFire {
            task_id: "c1".into(),
            prompt: "p".into(),
            cron: String::new(),
            recurring: false,
            coalesced_count: 1,
            stale: true,
            maxed_out: false,
            readonly: false,
            scheduled_at: None,
            late_by_ms: None,
            scope: crate::cron::TaskScope::Session,
            ended: false,
        };
        let xml = render_cron_fire_envelope(&empty);
        // Then the booleans are lowercase words and the empty cron falls
        // back to "unknown" (the TS stringAttr caliber).
        assert!(xml.contains("recurring=\"false\""), "{xml}");
        assert!(xml.contains("stale=\"true\""), "{xml}");
        assert!(xml.contains("cron=\"unknown\""), "{xml}");
    }
}
