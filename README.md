# git-worktree-fast

Create ordinary Git worktrees on macOS using an APFS checkout cache. Each worktree has its own branch or detached HEAD, index, and writable source files. Git still owns the history, refs, worktree listing, moves, and removal.

The tool can prepare source files, dependencies, disk images, or complete worktrees ahead of time.

| Command | What it prepares | When to use it |
| --- | --- | --- |
| `prepare` and `add` | A tracked-files-only checkout of a commit | Repeated branches from the same revision |
| `deps prepare` and `setup` | A verified shared pnpm installation for the T3 Code demo | Installing dependencies in worktrees created by Git, T3, or another tool |
| `image prepare` and `image create` | A detached writable-volume template, optionally containing a complete private T3 install | Spinning up one or several isolated worktrees that can be disposed together |
| `pool prepare` and `pool claim` | A fixed set of complete, single-use Git worktrees | Known bursts where you can provision branches before agents arrive |

## Install

With a recent stable Rust toolchain, Git, and the Apple Command Line Tools,
install from GitHub:

```sh
cargo install --locked --git https://github.com/mweinbach/worktree-fast.git git-worktree-fast
```

Or build directly from this checkout:

```sh
cargo install --locked --path .
git worktree-fast --version
```

The executable is `git-worktree-fast`. Git discovers it as `git worktree-fast` when Cargo’s bin directory is on PATH. APFS acceleration requires the cache and destination to be on the same clone-capable volume. Ordinary Git commands and unsupported add forms use the system Git. The package installer integration is specific to the audited demo revision.

## Native Git worktrees

From the repository you want to work in:

```sh
git worktree-fast prepare HEAD
git worktree-fast add -b feature/login ../feature-login HEAD
git worktree list
git -C ../feature-login status

# The prepared source is optional once the worktree exists.
git worktree-fast forget HEAD
git worktree move ../feature-login ../login
git worktree remove ../login
```

Prepare creates a pristine tree from Git objects in a per-repository, per-commit cache. It excludes ignored files, untracked files, local edits and the source index. On a cache hit, Darwin clones that tree and Git registers and publishes the checkout. Files initially share disk blocks and diverge as they are written. Git’s first full status refreshes the independent index. The worktree remains usable after the source cache is removed. A cache miss, active checkout hook, or an unsupported layout uses native Git. To forget a prepared revision after HEAD moves, pass its original commit ID.

## T3 Code and pnpm

The verified dependency recipe targets [T3 Code at 15fe2832](https://github.com/pingdotgg/t3code/commit/15fe2832727893cbdd2214383f9f971474849695). Install its pinned Node and pnpm versions and the `vp` launcher. Start with a populated pnpm content store (a normal frozen install in the demo primes it).

```sh
# Inside the main T3 Code checkout:
git worktree-fast deps prepare
git worktree add -b feature/search ../feature-search
git worktree-fast setup ../feature-search
```

The seed runs T3’s frozen installer once. Setup restores generated dependency directories and connects registry packages to a shared store. Each worktree links its four mutable mobile packages to its own source. Treat registry files in the shared store as read-only; use the image backend to edit installed registry files privately.

T3’s **New Worktree** action already creates the Git branch and chooses its path. In the demo project’s **Settings → Actions**, make the first **Run on worktree create** action the following command and set it to wait for completion. Use the installed binary’s absolute path if it is missing from T3’s terminal PATH.

```sh
git worktree-fast setup --fallback
```

T3 passes the worktree path and project root to the action. On a fresh checkout with no usable seed, the fallback runs the project’s normal `node scripts/setup-worktree.ts` with its frozen-lockfile and setup-marker behavior. Stock Git users can run the same command with a checkout path. After a native `git worktree move`, rerun `git worktree-fast setup NEW_PATH` to refresh pnpm’s absolute paths. Git removes the worktree’s store lease when it removes the checkout. Retiring a generation with live leases is refused.

```sh
git worktree remove ../feature-search
git worktree-fast deps retire
```

The pinned T3 web config uses `URL.pathname` for its fonts alias. If a checkout
path contains a space, Vite can look for a literal `%20` and fail during
testing. This also reproduces with a stock frozen install. A path without
spaces works; using Node’s `fileURLToPath` for that alias fixes the spaced
path. [Pinned Vite config](https://github.com/pingdotgg/t3code/blob/15fe2832727893cbdd2214383f9f971474849695/apps/web/vite.config.ts#L327-L330)

## APFS image worktrees

From the target Git repository, choose two new paths outside its working tree. The example creates two branches with one mount.

```sh
git worktree-fast image prepare ../source-seed --count 2
git worktree-fast image create ../source-seed ../agent-group agent/one agent/two
git worktree list
# Work in the paths printed by create.
git worktree-fast image remove ../agent-group
git worktree-fast image retire ../source-seed
```

Add `--t3` at prepare time to bake the audited project-local T3 installation into the volume. Give larger groups enough virtual capacity with `--size 32g`. Cold image construction and its final writeback can take minutes. Each request clones the detached backing file, attaches the copy and registers real Git worktrees. Member payloads have separate writable files and indexes. Remove the group with the image command so its volume detaches and its backing file is unlinked. Removal keeps the named branches. Use `--discard` only to discard edits in a disposable group.

## Prewarmed workers

From the source repository, `git worktree-fast pool prepare ../workers 8`
creates eight single-use branches and checkouts ahead of time. Add `--t3`
with a prepared dependency store for package-ready checkouts. Each
`git worktree-fast pool claim ../workers` prints a distinct, already-created
checkout. Remove claimed worktrees with ordinary Git. Use `pool drain ../workers`
for unused slots and `pool retire ../workers` when all slots have been removed.
Claim latency excludes provisioning and disposal.

## Test and benchmark

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
cargo build --release --locked
cargo run --release --locked --example bench -- /path/to/repo --bin ./target/release/git-worktree-fast --rounds 3
```

The benchmark prints JSON and uses disposable checkouts. Native Git and cached trials alternate on the same revision. Creation, first full status and physical removal are separate wall times. The one-time cache preparation is reported separately. Source-cache results exclude dependency setup; dependency and image timings depend on the package graph and the host.

[M3 Ultra measurements, including first Git status and removal](benchmarks/README.md)

## License

[MIT](LICENSE).
