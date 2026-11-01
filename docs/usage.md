# Using git-worktree-fast

A worktree lets you work on a second branch in another directory while you
leave your current checkout alone. If you often branch from the same commit,
you can prepare that commit once and reuse it for each new worktree.

## Make your first worktree

Start in a Git repository on macOS. Install the command with a recent stable
Rust toolchain and the Apple Command Line Tools.

```sh
cargo install --locked --git https://github.com/mweinbach/worktree-fast.git git-worktree-fast
git worktree-fast --version
```

Pick an unused sibling directory on the same APFS volume as the cache. Save the
commit ID so you can clean up the right cache even if your branch moves later.

```sh
base=$(git rev-parse HEAD)
git worktree-fast prepare "$base"
git worktree-fast add ../login-fix
git -C ../login-fix status --short --branch
git worktree list
```

Git uses the directory name, `login-fix`, as the branch name. If that branch
is new, it starts at your current HEAD. An existing local branch opens at its
own tip and can use a cache prepared for that tip. Git normally refuses to
check out a branch that's already open elsewhere.

Open `../login-fix` in your editor. Your commits go on that branch. To move
the checkout or finish with it, use ordinary Git commands from the original
repository.

```sh
git worktree move ../login-fix ../login-review
git worktree remove ../login-review
git worktree-fast forget "$base"
```

Remove keeps the named branch and its commits. Commit or stash work you want to
keep before removing the directory, including any untracked work. Save any
ignored files you need elsewhere. You can forget the prepared source while
worktrees created from it are still open.

## Choose where a worktree starts

These are independent examples. Use a fresh destination for each add.

| Goal | Command |
| --- | --- |
| Start a new named branch at the exact prepared commit | `git worktree-fast add -b fix/login ../login "$base"` |
| Open an existing branch | `git worktree-fast add ../review release` |
| Inspect or test the exact commit with a detached HEAD | `git worktree-fast add --detach ../test-run "$base"` |

Create a branch before discarding a detached checkout if you want to keep new
commits you made in it.

## Check whether the cache was used

```sh
WORKTREE_FAST_TRACE=1 git worktree-fast add --detach ../cache-check "$base"
git worktree remove ../cache-check
```

A hit prints `git-worktree-fast: created from APFS checkout cache` on stderr.
Use a non-quiet add for this check. If a successful add prints only ordinary
Git output, Git performed the checkout itself. Common reasons are an unprepared
revision, a destination on another volume, a checkout hook, or a sparse
checkout. Run `prepare` again to rebuild an outdated source entry. If Git
cannot discover the command at all, check that `command -v git-worktree-fast`
finds the installed executable.

On macOS the default cache is under `~/Library/Caches/worktree-fast/v1/`.
An absolute `XDG_CACHE_HOME` moves the cache root. For a repository on an
external APFS volume, set it to a directory on that volume for preparation,
creation and cleanup. The CLI manages separate entries for each repository
and commit.

## Set up T3 Code dependencies

The source cache works with eligible Git repositories. The optional package
recipe supports T3 Code commit `15fe2832`. Use Node 26.12.1, pnpm 12.14.0
and the `vp` launcher. A normal frozen install in the demo primes its pnpm
content store. [Pinned T3 revision](https://github.com/pingdotgg/t3code/commit/15fe2832727893cbdd2214383f9f971474849695)

```sh
# From the main checkout of the pinned T3 Code repository
git worktree-fast deps prepare
git worktree add -b feature/search ../feature-search
git worktree-fast setup ../feature-search
```

To use it from T3’s **New Worktree** button, open that demo project's
**Settings → Actions**. Make the first action marked **Run on worktree create**
run `git worktree-fast setup --fallback` and set it to wait for completion.
Use the executable's absolute path if T3's shell cannot find it. T3 creates
the branch and worktree, then runs the action there. On an eligible cache
miss, the action runs the checkout's own frozen setup script.

Registry packages in this mode are shared across checkouts. Treat those files
as read-only. The four local mobile packages follow edits in their own
checkout. If you move a worktree, run `git worktree-fast setup NEW_PATH` once
to refresh pnpm's recorded paths. Remove every worktree using this dependency
store before retiring it.

```sh
git worktree remove ../feature-search
git worktree-fast deps retire
```

The pinned demo's web Vite config uses a URL pathname in a filesystem alias.
For its focused Vitest suite, choose a checkout path without spaces or change
the alias to use Node's `fileURLToPath`. [Pinned Vite config](https://github.com/pingdotgg/t3code/blob/15fe2832727893cbdd2214383f9f971474849695/apps/web/vite.config.ts#L327-L330)

## Give agents private installed packages

An image seed is a reusable disk image. It can hold several independent
checkouts that share one mount. Choose two new sibling paths outside the
source worktree. This example creates two branches and prints two paths,
one per line.

```sh
git worktree-fast image prepare ../review-seed --count 2
git worktree-fast image create ../review-seed ../review-group agent/one agent/two
git worktree list
```

For the pinned T3 demo, add `--t3` to **image prepare** to include a private
frozen dependency installation. Use `--size 32g` if the group needs more
virtual capacity. Building an installed image takes time up front. Each member
then has writable packages isolated from other members.

When everyone is finished, commit the work you want to keep. Remove the group
from outside its mount so the tool can detach the volume and unlink its backing
file. Both branches survive. Retire the seed when you are done making groups.

```sh
git worktree-fast image remove ../review-group
git worktree-fast image retire ../review-seed
```

## Reserve worktrees for a known burst

If you know how many agents will arrive, provision their branches ahead of
time. Each claim consumes one existing slot and prints its checkout path.

```sh
git worktree-fast pool prepare ../workers 4
work=$(git worktree-fast pool claim ../workers)
git -C "$work" status --short --branch

# After the claimed work is finished
git worktree remove "$work"
git worktree-fast pool drain ../workers
git worktree-fast pool retire ../workers
```

Pool preparation starts from the current HEAD. Add `--t3` to `pool prepare`
after `deps prepare` for dependency-ready slots. Claims stay single-use.
Drain closes the pool to new claims and removes unused clean slots and their
unchanged branches. Claimed branches remain yours.

[How it works](how-it-works.md) · [Performance and storage](performance.md) · [Bundling the CLI](bundling.md) · [Project home](../README.md)
