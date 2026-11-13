# Parity audit (historical)

These files are an item-by-item inventory of CLIProxyAPI v8.0.35 contracts and how
cliproxy-rs covered them at each audit. Since issue
[#45](https://github.com/zakstak/cliproxy-rs/issues/45) they are reference material, not a
gate. A change does not need to move an item to covered, add a `manual.tsv` judgment, rerun
`audit.py` or run a Go comparison to land.

The inventory is still useful for finding a Go contract that a client or an operator may
depend on. The checklist links each item to its Go source. Provider documentation,
first-party clients and verified wire behaviour decide what is correct; see
[AGENTS.md](../../AGENTS.md) for the compatibility policy.

The Go differential harness that `selective-v8.0.67.md` and the receipts refer to is
retired. Its evidence is archived in [`harness/`](../../harness/README.md).
