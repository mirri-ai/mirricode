---
'@mirri-ai/release': minor
---

web: A cognitive-walkthrough pass over the scheduled-tasks panel — calmer validation, a clearer form footer, honest schedule wording, and tidier task cards.

- Form validation now appears only after you interact with a field or submit: a freshly opened form no longer greets you with red errors on fields you have not touched.
- The four config selectors (workspace / permission mode / model / thinking effort) are separated from the cancel and submit actions, and in edit mode the save bar stays in view while you scroll.
- "Every 1 day / 1 week" now reads as "daily / weekly" in both the schedule sentence and the preview.
- Time pickers offer every minute (00–59) instead of 15-minute steps, so a 09:05 run is no longer forced onto a quarter hour.
- A task that ended on its own reads "Ended · as scheduled", and a task that has never run reads "Never run" instead of "0 runs".
- Task-card metadata (frequency, next run, end date, run count) is clustered on the left as one line instead of being pinned to both card edges, and the prediction block prints its caption once instead of twice.
