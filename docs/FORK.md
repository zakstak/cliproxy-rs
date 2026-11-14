# Working in this fork

This file applies to [zakstak/cliproxy-rs](https://github.com/zakstak/cliproxy-rs), a fork of
[vayungodara/cliproxy-rs](https://github.com/vayungodara/cliproxy-rs). It describes the branch
layout and how changes move between the fork and upstream.

## Branches

| Branch | Role |
| --- | --- |
| `origin/fork` | Integration branch and GitHub default branch. Open pull requests here. CI runs on every push. |
| `origin/master` | A copy of `upstream/master`. Never commit to it directly. It only moves by fast-forward. |
| `upstream/master` | Upstream's default branch. Upstream has no `main` branch. |

The `fork` branch is protected: changes arrive by pull request, and force pushes and deletion are
blocked.

Remotes, as used below:

```sh
git remote add upstream https://github.com/vayungodara/cliproxy-rs.git   # once
git remote -v   # origin = zakstak/cliproxy-rs, upstream = vayungodara/cliproxy-rs
```

## Sync upstream into `origin/master`

Fast-forward the remote branch directly from `upstream/master`. Do not push a local `master`
branch: a local `master` can hold commits that are not meant for publication.

```sh
git fetch upstream
git fetch origin
git push origin upstream/master:refs/heads/master
```

The push is a fast-forward. If Git rejects it as non-fast-forward, someone committed to
`origin/master`. Stop and find out why. Do not force push.

## Integrate upstream into `fork`

Bring upstream changes into `fork` by pull request, so that CI runs on the merged result:

```sh
git switch -c sync/upstream-$(date +%Y%m%d) origin/fork
git merge --no-ff origin/master
# resolve conflicts, then:
git push origin HEAD
gh pr create --repo zakstak/cliproxy-rs --base fork
```

## Contribute a fix upstream

Upstream pull requests contain only generic fixes, never fork-specific files.

1. Develop the fix on `fork` first, in its own pull request, as one or more commits that touch no
   fork-specific files.
2. Create a contribution branch from `upstream/master` and cherry-pick only those commits:

   ```sh
   git fetch upstream
   git switch -c fix/<topic> upstream/master
   git cherry-pick -x <commit>...
   ```

3. Run the checks from [AGENTS.md](../AGENTS.md) on that branch and open the pull request against
   `vayungodara/cliproxy-rs` with base `master`.

## Local builds

This fork builds from source or with the local Dockerfile. It has no release publishing workflow, release installer or distribution packages, and all workspace crates have publishing disabled. Preserve these choices when integrating upstream changes. Version tags follow the `-zak.N` convention in [AGENTS.md](../AGENTS.md); tags do not trigger publication.

## Functional checks only

This fork has no Go/Rust differential harness, benchmark gates, memory soaks, idle or size
gates, or allocation and wall-clock speed bounds in tests ([#45](https://github.com/zakstak/cliproxy-rs/issues/45)).
CI checks formatting, lints, the test suite and the Linux-only build refusal. Upstream
may still carry such gates. When you integrate upstream changes, drop them and keep the
functional tests. [AGENTS.md](../AGENTS.md) states the compatibility policy.

[Features this fork does not adopt](NOT-ADOPTED.md) records the Go features this fork declines
and the reason for each.

## Keep fork-specific changes separate

Fork-specific material includes this file, the `fork` entries in `.github/workflows/ci.yml`, and
any fork-only configuration. Keep it in commits and pull requests of its own, separate from code
fixes, so that fixes cherry-pick cleanly onto `upstream/master`.
