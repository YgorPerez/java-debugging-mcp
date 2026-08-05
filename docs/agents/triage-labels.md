# Triage Labels

The skills speak in terms of five canonical triage roles. This file maps those roles to the actual label strings used in this repo's issue tracker.

| Label in mattpocock/skills | Label in our tracker | Meaning                                  |
| -------------------------- | -------------------- | ---------------------------------------- |
| `needs-triage`             | `needs-triage`       | Maintainer needs to evaluate this issue  |
| `needs-info`               | `needs-info`         | Waiting on reporter for more information |
| `ready-for-agent`          | `ready-for-agent`    | Fully specified, ready for an AFK agent  |
| `ready-for-human`          | `ready-for-human`    | Requires human implementation            |
| `wontfix`                  | `wontfix`            | Will not be actioned                     |

When a skill mentions a role (e.g. "apply the AFK-ready triage label"), use the corresponding label string from this table.

**All five now exist**, verified against `gh label list` on 2026-08-05. Four did; `needs-info` did not, and
was created (DOC-10) once the audit found it. That mapping had never been checked — the row asserted
`needs-info → needs-info` because the table was written by copying the left column, so a skill reaching that
role would have hit `gh issue edit --add-label needs-info` and **failed** rather than no-opped.

Worth keeping as the reason this file is not just a restatement of the skill's vocabulary: **a mapping table
whose two columns were never compared against reality is a table that only looks like it was checked.**
Re-verify with `gh label list` when you edit the right-hand column, and if a role genuinely has no label
here, say so in the row — do not silently substitute a neighbouring one.
