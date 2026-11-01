# Share the tool or a finished branch

To give a teammate the command, send a release binary and its license. To hand over the code you wrote in a worktree, commit the changes and send a Git bundle. If both people already have access to the same remote, pushing the branch works as usual.

## Package the command for macOS

Run these commands from the root of this repository on the Mac that will build the archive. Cargo uses the dependency versions in `Cargo.lock`. The resulting executable runs without a Rust installation on the receiving Mac. It calls the system Git, and image operations use macOS’s `hdiutil`.

```sh
cargo build --release --locked --target-dir target --bin git-worktree-fast

repo=$(pwd -P)
bin_dir="$repo/target/release"
dist="$repo/target/dist"
version=$("$bin_dir/git-worktree-fast" --version | awk '{print $2}')
arch=$(xcrun lipo -archs "$bin_dir/git-worktree-fast")
archive="git-worktree-fast-$version-macos-$arch.tar.gz"

mkdir -p "$dist"
COPYFILE_DISABLE=1 tar -czf "$dist/$archive" \
  -C "$bin_dir" git-worktree-fast -C "$repo" LICENSE
(cd "$dist" && shasum -a 256 "$archive" > "$archive.sha256")
```

The two files in `target/dist/` are the assets to share. A native Apple silicon build produces an `arm64` archive. For a single download that includes Intel support, build both slices as shown below. Keep the executable’s name exactly `git-worktree-fast` so Git can discover it as `git worktree-fast`. You can also install directly from the Git repository. The crate manifest sets `publish = false` for Cargo registries. [Install from source](../README.md) · [Cargo build options](https://doc.rust-lang.org/cargo/commands/cargo-build.html)

### Verify the archive as a recipient

On the build machine, continue in the same shell. On a receiving Mac, set `dist` to the download directory and `archive` to the downloaded filename. Check the checksum, extract into a fresh directory, and try the binary through Git.

```sh
(cd "$dist" && shasum -a 256 -c "$archive.sha256")
tools_dir=$(mktemp -d)
tar -xzf "$dist/$archive" -C "$tools_dir"
PATH="$tools_dir:/usr/bin:/bin" git worktree-fast --version
```

For a persistent installation, copy the executable into a directory on your PATH.

```sh
mkdir -p "$HOME/.local/bin"
install -m 0755 "$tools_dir/git-worktree-fast" "$HOME/.local/bin/git-worktree-fast"
export PATH="$HOME/.local/bin:$PATH"
git worktree-fast --version
```

Add the PATH line to your shell profile if you want it in future sessions. The arm64 archive recipe was tested by extracting the binary, invoking it through Git, preparing a cache and creating a writable feature branch.

### Build one download for Apple silicon and Intel

From this repository on macOS, install the two Rust targets, build each slice, and combine them. The release binary keeps the same name on both architectures.

```sh
rustup target add aarch64-apple-darwin x86_64-apple-darwin
for target in aarch64-apple-darwin x86_64-apple-darwin; do
  cargo build --release --locked --target-dir target --target "$target" --bin git-worktree-fast
done

bin_dir="$PWD/target/universal-macos"
mkdir -p "$bin_dir"
xcrun lipo -create target/aarch64-apple-darwin/release/git-worktree-fast \
  target/x86_64-apple-darwin/release/git-worktree-fast -output "$bin_dir/git-worktree-fast"
xcrun lipo "$bin_dir/git-worktree-fast" -verify_arch arm64
xcrun lipo "$bin_dir/git-worktree-fast" -verify_arch x86_64
```

On Apple silicon with Rosetta installed, run both slices to verify that they launch.

```sh
arch -arm64 "$bin_dir/git-worktree-fast" --version
arch -x86_64 "$bin_dir/git-worktree-fast" --version
```

Use this `bin_dir` and `archive="git-worktree-fast-$version-macos-universal.tar.gz"` with the tar and checksum commands above. Both slices of the extracted v0.1.1 archive were verified on Apple silicon, with the Intel slice running under Rosetta. If you sign your releases, sign the combined executable before archiving it, then calculate the checksum of the final archive. [Apple’s universal binary guide](https://developer.apple.com/documentation/apple-silicon/building-a-universal-macos-binary)

## Use the binary in an app or CI job

A CI job can prepend the verified extraction directory to PATH for that job. From your project checkout, `git worktree-fast prepare HEAD` and `git worktree-fast add ../worker` then use the bundled version.

A macOS app can include the binary at `Contents/Helpers/git-worktree-fast`. Resolve that path from the running app bundle, set the child’s working directory to the target repository, and pass each argument separately. For example, the helper’s argument list for a new branch can be `["add", "-b", "feature/login", "../login"]`. The child process needs Git on PATH. The optional T3 dependency recipe also requires the project’s Node, pnpm and Vite+ tools. [Usage guide](usage.md)

For a Developer ID distribution, sign the final helper before signing the enclosing app. Apply Apple’s notarization workflow to the deliverable you intend to distribute and generate checksums after signing. The archive recipe above produces a local build. [Apple’s guide to nested code](https://developer.apple.com/library/archive/technotes/tn2206/_index.html) · [Notarizing macOS software](https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution)

## Hand off a feature branch offline

Commit your work in the feature checkout. From its root, export the branch and its reachable Git objects to a file outside the checkout.

```sh
bundle="$PWD/../feature-login.bundle"
git bundle create "$bundle" refs/heads/feature/login
```

For a handoff into an empty repository, the sender needs the history and objects reachable from the exported branch. From a shallow or partial clone, fetch the missing history or objects first.

Transfer the file. From the receiving repository, point `bundle` at the transferred file and fetch into an unused local branch.

```sh
bundle="$PWD/../feature-login.bundle"
git bundle verify "$bundle"
git fetch "$bundle" refs/heads/feature/login:refs/heads/review/feature-login
git worktree add ../review-login review/feature-login
```

This handoff was verified by fetching into a second, empty repository and checking out the same commit with ordinary Git. A bundle carries committed history. Your staging area, untracked edits and installed packages stay on the sender’s machine. A linked checkout’s `.git` file points into its original repository, and worktree-fast caches and image seeds have local ownership records. Recreate those on the receiving Mac when you want accelerated checkouts or an installed T3 environment there. [Git bundle reference](https://git-scm.com/docs/git-bundle) · [How the caches work](how-it-works.md)
