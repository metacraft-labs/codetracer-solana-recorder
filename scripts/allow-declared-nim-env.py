#!/usr/bin/env python3
"""Authorize only the exact locked Nim sibling environment in owning native CI.

Uses real Git objects/filesystem and the declared direnv executable. No mocks,
source substitutions, global trust, compiler bypass or changed test commands.
"""
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import tomllib

root = Path(__file__).resolve().parent.parent
sdk = root.parent / "codetracer-trace-format-nim"
if sdk.is_symlink() or not sdk.is_dir() or (sdk / ".git").is_symlink() or not (sdk / ".git").is_dir():
    raise RuntimeError("declared Nim CI sibling must be a direct primary checkout")
def identity(path):
    info = path.lstat()
    return info.st_dev, info.st_ino, stat.S_IMODE(info.st_mode)
def file_binding(path):
    return identity(path), hashlib.sha256(path.read_bytes()).hexdigest()
source_bindings = {path: file_binding(path) for path in (root / "repro.lock", Path(__file__).resolve())}
directory_bindings = {path: identity(path) for path in (sdk, sdk / ".git")}
git = Path(os.environ["SOLANA_CI_GIT"])
direnv = Path(os.environ["SOLANA_CI_DIRENV"])
principals = {}
for tool in (git, direnv):
    resolved = tool.resolve(strict=True)
    info = resolved.stat()
    if not str(resolved).startswith("/nix/store/") or not stat.S_ISREG(info.st_mode) or info.st_uid != 0 or info.st_mode & 0o222 or not info.st_mode & 0o111:
        raise RuntimeError("CI metadata/trust tool differs from declared immutable package")
    principals[tool] = (str(resolved), file_binding(resolved), os.readlink(tool) if tool.is_symlink() else None)
# Metadata reads need no network. Isolate them from inherited storage, config,
# filters and Git template authority; preserve original env for real direnv.
env = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
env.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull)
def read(*args):
    return subprocess.check_output([str(git), "--git-dir=" + str(sdk / ".git"), "--work-tree=" + str(sdk), *args], cwd=sdk, env=env)
lock = tomllib.loads((root / "repro.lock").read_text())
selected = [d for d in lock["lock"]["deps"] if d["name"] == "codetracer-trace-format-nim"]
if len(selected) != 1 or selected[0]["path"] != "../codetracer-trace-format-nim":
    raise RuntimeError("owning lock has no unique declared Nim sibling")
revision = selected[0]["revision"]
if re.fullmatch("[0-9a-f]{40}", revision) is None or selected[0]["integrity"] != "git-sha1:" + revision:
    raise RuntimeError("declared Nim revision/integrity authority is invalid")
if read("rev-parse", "HEAD").decode().strip() != revision:
    raise RuntimeError("Nim checkout differs from owning declared revision")
def snapshot():
    tree = {}
    for row in read("ls-tree", "-r", "-z", revision).split(b"\0"):
        if not row:
            continue
        meta, name = row.split(b"\t", 1)
        mode, kind, oid = meta.split()
        if kind != b"blob" or mode not in (b"100644", b"100755", b"120000"):
            raise RuntimeError("unsupported tracked source kind in declared Nim checkout")
        tree[name] = (mode, oid)
    index = {}
    for row in read("ls-files", "--stage", "-z").split(b"\0"):
        if not row:
            continue
        meta, name = row.split(b"\t", 1)
        mode, oid, stage = meta.split()
        if stage != b"0":
            raise RuntimeError("Nim index contains unresolved source stages")
        index[name] = (mode, oid)
    if tree != index:
        raise RuntimeError("Nim complete tracked index differs from declared tree")
    actual = {}
    for name, (mode, oid) in tree.items():
        relative = Path(os.fsdecode(name))
        if relative.is_absolute() or ".." in relative.parts:
            raise RuntimeError("tracked source escapes declared Nim root")
        file = sdk / relative
        for ancestor in list(file.parents)[:len(relative.parts) - 1]:
            if ancestor.is_symlink() or not ancestor.is_dir():
                raise RuntimeError("Nim source ancestor is not a direct directory")
        info = file.lstat()
        if mode == b"120000":
            if not stat.S_ISLNK(info.st_mode):
                raise RuntimeError("tracked Nim link changed kind")
            data = os.fsencode(os.readlink(file))
        else:
            if not stat.S_ISREG(info.st_mode) or bool(info.st_mode & 0o111) != (mode == b"100755"):
                raise RuntimeError("tracked Nim regular source kind/mode changed")
            data = file.read_bytes()
        actual_oid = hashlib.sha1(b"blob " + str(len(data)).encode() + b"\0" + data).hexdigest().encode()
        if actual_oid != oid:
            raise RuntimeError("tracked Nim source bytes differ from declared Git object")
        actual[os.fsdecode(name)] = {"mode": stat.S_IMODE(info.st_mode), "kind": mode.decode(), "sha256": hashlib.sha256(data).hexdigest()}
    envrc = sdk / ".envrc"
    if envrc.is_symlink() or not envrc.is_file() or ".envrc" not in actual:
        raise RuntimeError("declared Nim environment is not a tracked regular file")
    return actual
def authority_guard():
    if any(file_binding(path) != binding for path, binding in source_bindings.items()):
        raise RuntimeError("owning lock or trust script changed")
    for path, binding in directory_bindings.items():
        if path.is_symlink() or not path.is_dir() or identity(path) != binding:
            raise RuntimeError("declared SDK directory identity changed")
    for tool, binding in principals.items():
        resolved = tool.resolve(strict=True)
        actual = (str(resolved), file_binding(resolved), os.readlink(tool) if tool.is_symlink() else None)
        if actual != binding:
            raise RuntimeError("declared metadata/trust executable principal changed")
before = snapshot()
authority_guard()
subprocess.run([str(direnv), "allow", str(sdk)], check=True, cwd=sdk)
authority_guard()
if read("rev-parse", "HEAD").decode().strip() != revision or snapshot() != before:
    raise RuntimeError("Nim declared source changed during scoped authorization")
print(json.dumps({"declaredRevision": revision, "trackedSourceCount": len(before), "envrcSHA": before[".envrc"]["sha256"], "sourceGuard": True, "completeTrackedSourceSHA": hashlib.sha256(json.dumps(before, sort_keys=True).encode()).hexdigest(), "authorizedOnly": str(sdk)}))
