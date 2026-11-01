# Triage Labels

The skills speak in terms of five canonical triage roles. This file maps those roles to the actual
label strings used in this repo's issue tracker.

| Label in mattpocock/skills | Label in our tracker | Meaning                                  |
| -------------------------- | -------------------- | ---------------------------------------- |
| `needs-triage`             | `needs-triage`       | Maintainer needs to evaluate this issue  |
| `needs-info`               | `needs-info`         | Waiting on reporter for more information |
| `ready-for-agent`          | `ready-for-agent`    | Fully specified, ready for an AFK agent  |
| `ready-for-human`          | `ready-for-human`    | Requires human implementation            |
| `wontfix`                  | `wontfix`            | Will not be actioned                     |

When a skill mentions a role (e.g. "apply the AFK-ready triage label"), use the corresponding label
string from this table.

Edit the right-hand column to match whatever vocabulary you actually use.

## Most of these labels do not exist yet

`needs-triage` exists: the website's report bridge puts it on every mirrored report. The other four
are still missing, on the repo and on the `unom` org. The first triage run has to create them with
`mcp__gitea__label_write`
(`method: "create_repo_label"`, `name`, `color` as `#RRGGBB`, optional `description`).

Creating labels is a write to a shared instance, so it falls under the ask-first rule in
`issue-tracker.md` — propose the four, then create them once the user agrees.

Remember the ID trap from `issue-tracker.md`: applying a label needs its **numeric ID** from
`label_read`, not the name in the table above.
