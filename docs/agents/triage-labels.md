# Triage Labels

The skills speak in terms of five canonical triage roles. This file maps those roles to the actual label strings used in this repo's issue tracker.

| Label in mattpocock/skills | Label in our tracker | Meaning                                  |
| -------------------------- | -------------------- | ---------------------------------------- |
| `needs-triage`             | `needs-triage`       | Maintainer needs to evaluate this issue  |
| `needs-info`               | ⚠️ **not created**    | Waiting on reporter for more information |
| `ready-for-agent`          | `ready-for-agent`    | Fully specified, ready for an AFK agent  |
| `ready-for-human`          | `ready-for-human`    | Requires human implementation            |
| `wontfix`                  | `wontfix`            | Will not be actioned                     |

When a skill mentions a role (e.g. "apply the AFK-ready triage label"), use the corresponding label string from this table.

**`needs-info` has no label on this repo** — verified against `gh label list` on 2026-08-05; the other four
exist. `gh issue edit --add-label needs-info` therefore **fails** rather than doing nothing, so a skill that
reaches this role stops there. This row said `needs-info` mapped to `needs-info` and had never been checked.

Two ways out, and the choice is the maintainer's: create it once —

```bash
gh label create needs-info --description "Waiting on reporter for more information" --color D876E3
```

— or leave it uncreated deliberately, on the grounds that this tracker has had no external reporter to wait
on, and use a comment plus `needs-triage` instead. Whichever, **do not silently substitute another label**:
the point of this table is that the mapping is checked rather than assumed.

Edit the right-hand column to match whatever vocabulary you actually use, and re-check it against
`gh label list` when you do.
