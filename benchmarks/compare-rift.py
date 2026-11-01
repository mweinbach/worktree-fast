#!/usr/bin/env python3
"""Compare source checkouts on macOS. Only temporary sibling directories and an owned disk image are modified."""
import argparse
import hashlib
import json
import os
import platform
import plistlib
import shutil
import subprocess
import sys
import tempfile
import time
from contextlib import contextmanager
from datetime import datetime, timezone
from pathlib import Path

METHODS = ("git", "rift", "worktree-fast")
RIFT_013_ARM64 = "fb2cddb01786101978926247f04b15bdee00f37ca8e5196237e3d1707a671089"


def run(args, *, cwd=None, env=None):
    p = subprocess.run(args, cwd=cwd, env=env, stdin=subprocess.DEVNULL, capture_output=True)
    if p.returncode:
        raise RuntimeError(f"{args[0]} {args[1]} exited {p.returncode}: {os.fsdecode(p.stderr).strip()}")
    return p


def timed(fn, *args):
    start = time.perf_counter()
    result = fn(*args)
    return time.perf_counter() - start, result


class Fixture:
    def __init__(self, root, source, commit, fast, rift):
        self.root, self.repo, self.works = root, root / "source", root / "workspaces"
        self.fast, self.rift, self.commit = fast, [rift, "--database", root / "rift.sqlite"], commit
        self.works.mkdir()
        self.env = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
        self.env.update(HOME=str(root / "home"), XDG_CACHE_HOME=str(root / "cache"),
                        XDG_CONFIG_HOME=str(root / "config"), XDG_DATA_HOME=str(root / "data"),
                        GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL="/dev/null",
                        GIT_TERMINAL_PROMPT="0", WORKTREE_FAST_TRACE="1")
        Path(self.env["HOME"]).mkdir()
        run(["git", "init", "-q", "--template=", self.repo], env=self.env)
        # Transport copies the pinned objects into this standalone, shallow repo.
        self.call("git", "fetch", "-q", "--depth=1", "--no-tags", "--", source, commit)
        self.call("git", "checkout", "-q", "--detach", commit)
        self.common = self.call("git", "worktree", "list", "--porcelain", "-z").stdout

    def call(self, *args, cwd=None, env=None):
        return run(args, cwd=cwd or self.repo, env=env or self.env)

    def prepare(self, method):
        if method == "rift":
            self.call(*self.rift, "init", "--here")
        elif method == "worktree-fast":
            self.call(self.fast, "prepare", self.commit)

    def add(self, method, path):
        if method == "rift":
            self.call(*self.rift, "create", "--into", path.parent, "--name", path.name, "--copy-all")
        elif method == "git":
            self.call("git", "worktree", "add", "--detach", path, self.commit)
        else:
            out = self.call(self.fast, "add", "--detach", path, self.commit)
            if b"git-worktree-fast: created from APFS checkout cache" not in out.stderr.splitlines():
                raise RuntimeError("worktree-fast did not use its prepared cache")

    def first_status(self, path):
        if self.call("git", "status", "--porcelain=v1", "--untracked-files=all", "-z", cwd=path).stdout:
            raise RuntimeError("Unexpected dirty checkout")

    def verify(self, method, path):
        if self.call("git", "rev-parse", "HEAD", cwd=path).stdout.strip().decode() != self.commit:
            raise RuntimeError("Wrong revision")
        if (path / ".git").is_dir() != (method == "rift"):
            raise RuntimeError("Unexpected Git ownership")
        # Compare working-file bytes/modes through Git with a fresh statless index.
        # The measured first status uses the index actually supplied by each tool.
        with tempfile.TemporaryDirectory() as scratch:
            env = {**self.env, "GIT_INDEX_FILE": str(Path(scratch) / "index")}
            self.call("git", "read-tree", self.commit, cwd=path, env=env)
            self.call("git", "diff", "--quiet", "--no-ext-diff", self.commit, "--", cwd=path, env=env)
        ignored = self.call("git", "ls-files", "-z", "--others", "--ignored", "--exclude-standard", cwd=path).stdout
        expected = b".rift\0" if method == "rift" else b""
        if ignored != expected:
            raise RuntimeError("Unexpected ignored files beyond the tool's own workspace metadata")

    def remove(self, method, path):
        if method == "rift":
            self.call(*self.rift, "remove", path)
            self.call(*self.rift, "gc")
        else:
            self.call("git", "worktree", "remove", "--", path)

    def check_removed(self, method):
        if self.call("git", "worktree", "list", "--porcelain", "-z").stdout != self.common:
            raise RuntimeError("Linked Git registration remains")
        if method == "rift" and self.call(*self.rift, "list").stdout.strip():
            raise RuntimeError("Rift registration remains")
        trash = self.works / ".trash"
        if any(p != trash for p in self.works.iterdir()) or (trash.exists() and any(trash.iterdir())):
            raise RuntimeError("Workspace or Rift trash remains")

    def retire(self, method):
        if method == "worktree-fast":
            self.call(self.fast, "forget", self.commit)
        elif method == "rift":
            self.call(*self.rift, "remove", "-f", self.repo, cwd=self.root)


def timing(f, rounds):
    setup = {m: timed(f.prepare, m)[0] if m != "git" else 0 for m in METHODS}
    samples = []
    for n in range(rounds):
        for m in METHODS[n % 3:] + METHODS[:n % 3]:
            path = f.works / "checkout"
            add, _ = timed(f.add, m, path)
            status, _ = timed(f.first_status, path)
            f.verify(m, path)
            remove, _ = timed(f.remove, m, path)
            f.check_removed(m)
            samples.append(dict(method=m, round=n + 1, create_s=add, first_status_s=status,
                                ready_s=add + status, remove_s=remove, lifecycle_s=add + status + remove))
            print(f"Round {n + 1} {m}: ready {add + status:.3f}s, remove {remove:.3f}s", file=sys.stderr, flush=True)
    for m in METHODS:
        f.retire(m)
    return {"rounds": rounds, "preparation_s": setup, "samples": samples}


def plist(args):
    return plistlib.loads(run(args).stdout)


@contextmanager
def image_volume(parent):
    # A failed detach leaves the private directory for recovery instead of
    # letting TemporaryDirectory recurse through a live mounted filesystem.
    root = Path(tempfile.mkdtemp(prefix=".worktree-storage-", dir=parent)).resolve()
    image, mount = root / "measurement.sparseimage", root / "volume"
    mount.mkdir()
    try:
        run(["hdiutil", "create", "-size", "8g", "-type", "SPARSE", "-fs", "APFS",
             "-volname", "WorkspaceMeasurement", "-nospotlight", image])
        run(["hdiutil", "attach", "-nobrowse", "-noautoopen", "-mountpoint", mount, image])
        info = plist(["diskutil", "info", "-plist", mount])
        if info["FilesystemType"] != "apfs" or Path(info["MountPoint"]) != mount or not os.path.ismount(mount):
            raise RuntimeError("Expected private APFS mount")
        yield mount, info["APFSContainerReference"]
    finally:
        # Re-resolve the owned image on retries; never detach a reused device ID.
        for delay in (0, .2, .5, 1):
            time.sleep(delay)
            matches = [i for i in plist(["hdiutil", "info", "-plist"])["images"]
                       if i.get("image-path") and Path(i["image-path"]).resolve() == image]
            if not matches:
                break
            devices = matches[0]["system-entities"]
            mounts = {e["mount-point"] for e in devices if "mount-point" in e}
            if len(matches) != 1 or (mounts and mounts != {str(mount)}):
                raise RuntimeError(f"Unexpected attach location; inspect {root}")
            target = str(mount) if mounts else devices[0]["dev-entry"]
            run_detach = subprocess.run(["hdiutil", "detach", target], capture_output=True)
            if run_detach.returncode == 0:
                break
        if os.path.ismount(mount) or any(i.get("image-path") and Path(i["image-path"]).resolve() == image
                for i in plist(["hdiutil", "info", "-plist"])["images"]):
            raise RuntimeError(f"Could not detach private image; retained {root}")
        if mount.exists():
            mount.rmdir()
        image.unlink(missing_ok=True)
        root.rmdir()


def used(container):
    # Read the block allocator of this dedicated APFS container, not per-file
    # st_blocks or host free space (which counts unrelated apps and snapshots).
    os.sync()
    values = []
    for _ in range(20):
        c, = plist(["diskutil", "apfs", "list", container, "-plist"])["Containers"]
        if c["ContainerReference"] != container:
            raise RuntimeError("Storage container changed")
        values.append(c["CapacityCeiling"] - c["CapacityFree"])
        if len(values) >= 3 and values[-1] == values[-2] == values[-3]:
            return values[-1]
        time.sleep(.2)
    raise RuntimeError("APFS allocation did not settle")


def storage(parent, source, commit, fast, rift, count):
    records = {}
    for m in METHODS:
        with image_volume(parent) as (vol, container):
            empty = used(container)
            # A filesystem root also contains .Trashes/.fseventsd.
            root = vol / "bench"
            root.mkdir()
            f = Fixture(root, source, commit, fast, rift)
            row = {"empty_used_bytes": empty, "baseline_used_bytes": used(container)}
            f.prepare(m)
            row["prepared_used_bytes"] = used(container)
            live = []
            for i in range(count):
                path = f.works / f"checkout-{i + 1}"
                f.add(m, path)
                f.first_status(path)
                f.verify(m, path)
                live.append(used(container))
            row["live_used_bytes"] = live
            for i in reversed(range(count)):
                f.remove(m, f.works / f"checkout-{i + 1}")
            f.check_removed(m)
            row["removed_used_bytes"] = used(container)
            f.retire(m)
            row["retired_used_bytes"] = used(container)
            records[m] = row
            print(f"Storage {m}: prep {row['prepared_used_bytes'] - row['baseline_used_bytes']} bytes, "
                  f"fourth/last total {live[-1] - row['baseline_used_bytes']} bytes above baseline",
                  file=sys.stderr, flush=True)
    return {"workspace_count": count, "unit": "bytes", "methods": records}


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("source", type=Path, help="existing local Git repository (read only)")
    p.add_argument("--revision", default="HEAD")
    p.add_argument("--fast", default="git-worktree-fast")
    p.add_argument("--rift", default="rift", help="Rift v0.0.13 executable")
    p.add_argument("--rounds", type=int, default=3)
    p.add_argument("--copies", type=int, default=4, help="live workspaces per 8 GiB storage image")
    p.add_argument("--phase", choices=("all", "timing", "storage"), default="all")
    a = p.parse_args()
    if platform.system() != "Darwin" or a.rounds < 1 or not 1 <= a.copies <= 8:
        p.error("macOS, positive rounds and 1..8 copies are required")
    source = a.source.resolve(strict=True)
    device_node = run(["df", "-P", source.parent]).stdout.splitlines()[-1].split()[0].decode()
    device = plist(["diskutil", "info", "-plist", device_node])
    if device["FilesystemType"] != "apfs":
        p.error("run from a source with a writable APFS sibling directory")
    fast, rift = [Path(shutil.which(x) or x).resolve(strict=True) for x in (a.fast, a.rift)]
    commit = run(["git", "-C", source, "rev-parse", "--verify", "--end-of-options", a.revision + "^{commit}"]).stdout.strip().decode()
    digest = hashlib.sha256(rift.read_bytes()).hexdigest()
    tracked = run(["git", "-C", source, "ls-tree", "-r", "--name-only", "-z", commit]).stdout.count(b"\0")
    result = {"schema": 1, "commit": commit, "tracked_paths": tracked, "architecture": platform.machine(),
              "machine": run(["sysctl", "-n", "machdep.cpu.brand_string"]).stdout.decode().strip(),
              "macos": platform.mac_ver()[0], "git": run(["git", "--version"]).stdout.decode().strip(),
              "worktree_fast": run([fast, "--version"]).stdout.decode().strip(),
              "rift": {"release": "v0.0.13" if digest == RIFT_013_ARM64 else "unverified", "binary_sha256": digest},
              "measured_utc": datetime.now(timezone.utc).isoformat(),
              "scope": "Source only; clean detached checkouts, same commit, default Git workers and no dependencies.",
              "fidelity": "HEAD, clean full status, statless Git comparison, no extra ignored files except Rift's .rift marker."}
    if a.phase != "storage":
        with tempfile.TemporaryDirectory(prefix=".three-way-", dir=source.parent) as tmp:
            f = Fixture(Path(tmp).resolve(), source, commit, fast, rift)
            result["timing"] = timing(f, a.rounds)
    if a.phase != "timing":
        result["storage"] = storage(source.parent, source, commit, fast, rift, a.copies)
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
