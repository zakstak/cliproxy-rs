# Archived benchmark results

[`results/`](results) holds raw output from benchmark and soak runs made before issue
[#45](https://github.com/zakstak/cliproxy-rs/issues/45). Each file name starts with the
date of its run. The files are historical records. They are not budgets, and no CI job
or merge rule compares against them.

The load generators, fake upstreams, soak and idle scripts, budgets file and gate script
that produced them are deleted. To read them, check out a commit before #45.

The prompt-cache tools that used to live here check functional cache behaviour, not
performance. They are now in [`tools/claude-cache/`](../tools/claude-cache).
