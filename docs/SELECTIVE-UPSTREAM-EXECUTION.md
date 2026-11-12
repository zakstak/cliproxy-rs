# Selective upstream execution

Zak approved implementation of the recommended core batch after reviewing the
research and parallelization proposal. The requested team is Opus at high effort
for implementation, a dedicated Sol reviewer for each lane, and the parent agent
as orchestrator. Ember supplies additional compute.

The [research plan](SELECTIVE-UPSTREAM-PLAN.md) and
[official Codex audit](research/codex-official-2026-11-11.md) remain the scope and
evidence baseline. Their awaiting-approval statements describe the research
snapshot; this execution record supersedes them for foundation, core, and
existing-behavior audit tasks. Conditional, deferred, and follow-up features
remain outside this approval. No release, deployment, provider-account traffic,
or restoration of removed providers or plugins is authorized.

## Initial assignments

| Lane | Initial work | Execution host | Integration gate |
| --- | --- | --- | --- |
| Harness | H1, #67 | Ember | Dedicated Sol review and isolated acceptance receipt |
| Claude | C4, #72, followed by C3, #71 | Workstation | H1 plus dedicated Sol review |
| Grok | X1, #84, followed by X2, #85 | Workstation | H1 plus dedicated Sol review |
| Credentials | A1, #89 | Ember | H1 plus dedicated Sol review |

Implementations may be authored concurrently in isolated checkouts. Foundation
acceptance precedes integration of dependent features. Keep changes separated by
issue where practical. Review findings return to the implementation lane; each
review or re-check is a fresh review task against an exact commit.

After H1 is accepted, expand toward six lanes as build capacity permits. Add
Muse/catalog, server boundaries, and Codex work as slots become available.
Credential work continues through A3 then A4. Claude stop reasons precede R1;
R1 precedes R2 and FP2. Logging precedes decompression bounds. N1 and FP1 precede
FP3. Cross-provider translation changes follow the affected provider changes.
FP5 can qualify documented current capabilities, but any new conditional tool
feature still needs its separate approval.

## Delivery and validation

Use draft PRs targeting `zakstak/cliproxy-rs`, base `fork`, for reviewable slices.
The orchestrator owns integration and final acceptance. Do not merge into `fork`
or close an issue before its review and acceptance gates are complete. Keep the
frozen Go reference and historical fixture provenance; add selective evidence
instead of declaring blanket parity.

Tests use private HOME/auth directories, synthetic credentials, loopback mocks,
and network isolation. Never run the production configuration. Use targeted
tests while iterating and control build concurrency and storage. Record actual
toolchain, missing prerequisites, skipped gates, and exact tested commits.
Disabled GitHub Actions do not count as successful checks.
