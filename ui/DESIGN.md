# Ledger visual system

Ledger is the primary cliproxy-rs dashboard. The accepted layout comes from the round-5 design review. The later sticky phone batch bar is excluded because it obscured keyboard focus.

## Colour and type

`src/app.css` owns the theme tokens. Dark uses a green-black paper (`#0f1311`), a raised surface (`#161b18`) and light ink (`#e6ebe7`). Light uses pale paper (`#f3f4f1`), white surfaces and dark ink (`#171d1a`). The primary control uses ink on paper. Green means ready or successful, amber means limited or waiting, red means failed, and blue means live. Unknown values use neutral ink and explicit text.

Tailwind's default palette is removed. Colour utilities resolve to theme tokens, which change together for Dark, Light or Match system. Text uses `--ink` or `--ink-2`; warning words use `--warn-text`. Never rely on colour alone.

Host Grotesk is self-hosted. Figures are tabular. Body and controls use 14 px text, supporting text uses 12.5–13 px, section titles use 17 px and page titles use 24 px. Monospace is for paths, keys, model IDs, logs and diffs. Labels use sentence case.

## Layout

A top bar holds Activity, Usage, Accounts and Tools. Settings and Sign out stay reachable. On phones, the four main tabs move to the bottom. Settings has its own section navigation. A detail sits beside its list at widths of 1100 px or more; below that it becomes a modal sheet.

Activity puts requests before expanded charts. Its compact/full chart choice stays in the tab. Filters reveal additional controls on phones. Long account names wrap without widening the document.

Activity, Usage and the server log read the server's history store (`/v8/management/history/*`), so they survive restarts and new tabs. Activity shows the last hour; Usage shows requests, failures and tokens per hour or day over 24 hours, 7 days or 30 days. When the store is off, each says so and names `storage.backend` instead of showing zeros.

## Interaction

- Touch controls are at least 44 px through tablet widths. Keyboard focus remains visible, including when content is replaced.
- Rows and charts can open details with the keyboard. Modal sheets contain focus, close with Escape or a backdrop click, and return focus to their opener.
- Success toasts use a persistent status region and clear after four seconds. Failure alerts stay until dismissed and keep retries attached to their original operation.
- Reads retain the last good data after failure and name its age. Missing data displays as unknown, not zero.
- Destructive actions name their target and ask before removing it. Provider keys are managed in Settings rather than through credential-file actions.
- Config edits are reviewed as line-numbered changes. Concurrent edits require reconciliation and earlier versions stay available until explicitly discarded.
- Motion reports state only. `prefers-reduced-motion` disables animation and transitions.

Do not render remote HTML, persist management keys, invent runtime health for config keys, or display a metric the server does not report. Keep fixture controls out of production.
