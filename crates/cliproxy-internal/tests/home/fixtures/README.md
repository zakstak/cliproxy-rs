# Home test inputs

`inflight_inputs.json` retains 22 historical scenarios from the former
`go_auth_golden.json`, recorded from CLIProxyAPI commit `6fecc6e`. The recorded
`frames` outputs were removed. The Rust tests count aggregate groups independently
and check complete partitions, schema fields, timestamps, safe details, and
configured limits.

The command, dispatch, JWT, log, release, and alias tests construct their protocol
inputs and expected fields directly. The old Go output fixtures and their writers
were removed in issue #198. Git history retains their provenance.
