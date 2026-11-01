# Documentation

Start with the [quickstart](../README.md#try-it-in-a-repository) to make your first cached worktree.

- [Using worktrees](usage.md) covers new and existing branches, moves, cleanup, the T3 setup action, disk images, and prewarmed pools.
- [How it works](how-it-works.md) follows a commit from Git objects to a writable checkout and explains how the tool preserves Git’s status and index behavior.
- [Bundling and distribution](bundling.md) shows how to package the executable, call it from another tool, and hand off work between repositories.
- [Performance](performance.md) compares elapsed time and disk usage for Git, worktree-fast, and Rift, and gives the commands for repeating the measurements.

## Work on the tool locally

From this repository, run the same formatting, lint, and test checks as CI. Use a release build for timing experiments.

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
cargo build --release --locked
```

Try `./target/release/git-worktree-fast --help` for the command list, or put `target/release` on PATH to invoke that build through Git. The [Rust benchmark](../benchmarks/README.md) uses temporary worktrees and checks that Git has removed them at the end of each trial.
