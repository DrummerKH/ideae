#!/usr/bin/env python3
"""What cargo leaves in target/ and never takes away: every change of a
dependency, a flag or rustc builds a crate again under a new hash, and the
old one stays (our own binary alone is 2 GB a time). A build of a crate
that has been built again since, and not for a week, goes; so does one of a
crate the project no longer has. What is still in use is at worst built once
more. `--dry-run` says what would go; IDEAE_SWEEP_DAYS changes the week."""
import os, re, shutil, sys, time

root = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target")
days = float(os.environ.get("IDEAE_SWEEP_DAYS", "7"))
dry = "--dry-run" in sys.argv
old = time.time() - days * 86400
unit = re.compile(r"^([A-Za-z0-9_-]+?)-([0-9a-f]{16})(?:\..*)?$")
lock = os.path.join(root, "..", "Cargo.lock")
known = {n.replace("-", "_") for n in re.findall(r'^name = "([^"]+)"', open(lock).read(), re.M)} if os.path.exists(lock) else None
session = re.compile(r"^([A-Za-z0-9_]+)-([0-9a-z]{10,})$")


def newest(path):
    try:
        best = os.lstat(path).st_mtime
    except OSError:
        return 0
    if os.path.isdir(path) and not os.path.islink(path):
        for base, dirs, files in os.walk(path):
            for name in dirs + files:
                try:
                    best = max(best, os.lstat(os.path.join(base, name)).st_mtime)
                except OSError:
                    pass
    return best


def size(path):
    if os.path.isdir(path) and not os.path.islink(path):
        return sum(os.lstat(os.path.join(b, f)).st_size for b, _, fs in os.walk(path) for f in fs)
    return os.lstat(path).st_size


def sweep(groups):
    """groups: {(name, hash): [paths]} → paths of the groups to remove."""
    times = {key: max(newest(p) for p in paths) for key, paths in groups.items()}
    latest = {}
    for (name, _), t in times.items():
        latest[name] = max(latest.get(name, 0), t)
    gone = []
    for key, t in times.items():
        name = key[0]
        # Built again since, or a crate the project no longer has.
        if t < old and (t < latest[name] or (known is not None and name not in known)):
            gone += groups[key]
    return gone


def profile_dirs():
    for base, dirs, _ in os.walk(root):
        if ".fingerprint" in dirs:
            yield base
        dirs[:] = [d for d in dirs if d not in (".fingerprint", "deps", "build", "incremental", "examples")]


freed = 0
for profile in profile_dirs():
    groups = {}
    for sub in (".fingerprint", "deps", "build"):
        folder = os.path.join(profile, sub)
        for entry in os.listdir(folder) if os.path.isdir(folder) else []:
            m = unit.match(entry[3:] if sub == "deps" and entry.startswith("lib") and entry.endswith((".rlib", ".rmeta", ".so", ".dylib", ".a")) else entry)
            if m:
                groups.setdefault((m.group(1).replace("-", "_"), m.group(2)), []).append(os.path.join(folder, entry))
    sessions = {}
    folder = os.path.join(profile, "incremental")
    for entry in os.listdir(folder) if os.path.isdir(folder) else []:
        m = session.match(entry)
        if m:
            sessions[(m.group(1), m.group(2))] = [os.path.join(folder, entry)]
    for path in sweep(groups) + sweep(sessions):
        freed += size(path)
        if dry:
            print(path)
        elif os.path.isdir(path) and not os.path.islink(path):
            shutil.rmtree(path, ignore_errors=True)
        else:
            try:
                os.remove(path)
            except OSError:
                pass
if freed > 1e9 or dry:
    print(f"target/: {freed / 1e9:.1f} GB of builds of before {'would go' if dry else 'removed'}", file=sys.stderr)
