# T3 Code on an M3 Ultra

Rust release build, macOS 27.2 ARM64, APFS, Apple Git 2.54.0. The source workload was T3 Code commit `15fe2832727893cbdd2214383f9f971474849695` with 29,044 tracked paths. These local trials ran on a shared development machine.

## Source checkouts

The included benchmark created detached native Git worktrees and APFS-cached worktrees in alternating order. Each row spans two trials. Before removal it checked the full porcelain status, including untracked files, and verified the commit. No package installation was included.

| Seconds | Checkout returned | Additional first full status | Actual removal |
| --- | ---: | ---: | ---: |
| Native Git | 3.46–4.13 | 0.22–0.26 | 2.27–2.56 |
| Tracked APFS cache | 0.70–0.74 | 4.53–4.78 | 2.24–3.00 |

The cached checkout returned earlier. Git’s subsequent stat-cache refresh made **checkout plus first full status slower** in both trials. Preparing the source cache took 4.06 s and forgetting it took 3.54 s. [Raw benchmark JSON](t3-source-m3-ultra.json)

## Dependency-ready worktrees

With a prepared shared pnpm store, the installed Rust tool restored 40 generated dependency directories and 109 checkout-local mobile package files. The demo’s saved T3 action ran in 0.84 s in a fresh Git worktree. Its command and environment were resolved with the upstream project-action code and executed in a disposable checkout. Native Git move and removal were exercised separately. After the move, `setup` refreshed pnpm metadata in 0.72 s.

In a separate run, an already-prepared two-member private image produced two new, installed T3 branches in 1.65 s. Removing the group, including detaching the image and unlinking it, took 3.02 s. The tests loaded the native PTY, resolved the local contracts package, executed Knip and all five worktree setup tests, and verified that an edit to an installed registry package stayed in its own worktree. One branch gained a commit before disposal and retained it afterwards.

These are single warm request measurements. They exclude building the dependency generation or image. Full-size private image preparation runs the frozen install, verifies the tree and writes the image back to storage before any request can use it.
