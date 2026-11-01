# T3 Code on an M3 Ultra

Rust release build, macOS 27.2 ARM64, APFS, Apple Git 2.54.0. The source workload was T3 Code commit `15fe2832727893cbdd2214383f9f971474849695` with 29,044 tracked paths. These local trials ran on a shared development machine.

## Source checkouts

Version 0.1.1 was measured against stock Git in five alternating trials per
command and configuration. Each trial created a fresh detached worktree, asked
Git for its first full porcelain status including untracked files, verified HEAD,
then physically removed the checkout. The cache is warm and each row is the
median in seconds. Combined times are the medians of the individual trial
totals. Both implementations used the same Git settings.

| Git configuration | Implementation | Add | First status | Add + status | Add + status + remove |
| --- | --- | ---: | ---: | ---: | ---: |
| Default | Native Git | 3.38 | 0.36 | 3.80 | 6.27 |
| Default | APFS cache | 1.21 | 0.25 | 1.50 | 3.68 |
| Four checkout workers | Native Git | 1.83 | 1.10 | 2.83 | 5.65 |
| Four checkout workers | APFS cache | 1.33 | 0.26 | 1.58 | 3.88 |

**Checkout plus first status was 2.53× faster than default Git and 1.79×
faster than four-worker Git** on this host. In both cases the worktrees were
removed with the same native Git command. Preparing the cache took 4.20 s
with default settings and 2.96 s with four workers. Forgetting it afterwards
took 2.60 s and 3.32 s, respectively. An ordinary add without a prepared eligible revision uses stock
Git. [Default trials](t3-source-m3-ultra.json) · [Four-worker trials](t3-source-four-workers-m3-ultra.json)

In 0.1.0 a cached add returned in 0.70–0.74 s but its first status took another
4.53–4.78 s. The new version verifies the source-cache tree, then fills the
new index with the clone’s actual file metadata before Git publishes the
checkout. In a separate full-size trace, Git read all 29,044 index entries
and reported zero files needing a content scan during first status.
[0.1.0 trials](t3-source-v0.1.0-m3-ultra.json) · [Git Trace2 counters](t3-source-first-status.json)

To repeat the speed checks after building the release binary:

```sh
cargo run --release --locked --example bench -- /path/to/t3code --bin ./target/release/git-worktree-fast --rounds 5 --min-speedup 2
cargo run --release --locked --example bench -- /path/to/t3code --bin ./target/release/git-worktree-fast --rounds 5 --checkout-workers 4 --min-speedup 1.5
```

The optional threshold uses median native add-plus-status divided by median
cached add-plus-status. The example emits all timings even if the threshold
is missed. Small repositories and other hardware can have different results.

### Eight concurrent ordinary worktrees

Eight separate add processes each created a detached native Git worktree on
host APFS. No pool or image was used. Two batches per method ran in alternating
order. Timings are wall time for all eight, with each worktree's HEAD verified
outside the timed status phase.

| Batch median | Native Git | APFS cache |
| --- | ---: | ---: |
| Add and first full status across eight | 15.72 s | 6.55 s |
| Including physical removal of all eight | 30.96 s | 15.52 s |

The warm add-plus-status speedup was **2.40×**. To reproduce, add
`--jobs 8 --rounds 2` to the example benchmark. Both methods used stock
`git worktree remove` for teardown. [Eight-worktree trials](t3-source-eight-m3-ultra.json)

### Git and T3 compatibility

Compatibility tests compare native and cached output, exit status, `ORIG_HEAD`
and reflogs. They also cover edits before first status, staging, SHA-1 and
SHA-256 repositories, CRLF checkout, symlinks, cache corruption, and native
lock/move/repair/prune/remove. A full-size T3 run used its existing-branch
add form, executed the demo's saved setup action, loaded the native PTY and
passed all five focused T3 tests. The branch retained a new commit after
ordinary Git removed the checkout.

## Dependency-ready worktrees

With a prepared shared pnpm store, the installed Rust tool restored 40 generated dependency directories and 109 checkout-local mobile package files. The demo’s saved T3 action ran in 0.84 s in a fresh Git worktree. Its command and environment were resolved with the upstream project-action code and executed in a disposable checkout. Native Git move and removal were exercised separately. After the move, `setup` refreshed pnpm metadata in 0.72 s.

In a separate run, an already-prepared two-member private image produced two new, installed T3 branches in 1.65 s. Removing the group, including detaching the image and unlinking it, took 3.02 s. The tests loaded the native PTY, resolved the local contracts package, executed Knip and all five worktree setup tests, and verified that an edit to an installed registry package stayed in its own worktree. One branch gained a commit before disposal and retained it afterwards.

These are single warm request measurements. They exclude building the dependency generation or image. Full-size private image preparation runs the frozen install, verifies the tree and writes the image back to storage before any request can use it.
