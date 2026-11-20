# Deploy the fork to Pyre

Pyre owns VM236, the stable client endpoint, deployment slots, and recording
delivery. Follow [Pyre's gateway checkpoint](https://github.com/zakstak/pyre/blob/main/docs/cli-proxy.md)
for the current runtime, install route, verification, and rollback. Run the
owner workflow from T3 **LOCAL** (`codex-direct`).

Build a locked release from an identified `fork` commit on Debian 13 amd64.
Qualify it in a disposable environment with a private auth directory and
external networking blocked. Test client behavior and the recording format
against local upstream fixtures. Hand Pyre the source commit, executable
SHA-256, test results, and any config migration. Pyre verifies the artifact on
the inactive slot before promotion.

The slots use separate OAuth grants. Preserve the schema and rotation behavior
of existing auth files and keep OAuth values on the guest. A release that
changes native recording contents or file layout needs a matching Pyre
publisher and Perch scanner check. Keep the preceding artifact available to
the owner. [Fork build policy](../FORK.md)
