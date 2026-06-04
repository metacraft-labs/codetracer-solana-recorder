set shell := ["bash", "-euo", "pipefail", "-c"]

# Default recipe — list available recipes.
default:
  @just --list

build:
  cargo build --locked

build-release:
  cargo build --release --locked

test:
  cargo test --locked
  bash tests/verify-cli-convention-no-silent-skip.sh

t: test

test-rust:
  cargo test --locked

verify-cli-convention:
  bash tests/verify-cli-convention-no-silent-skip.sh

lint:
  cargo fmt --check
  cargo clippy --locked --all-targets -- -D warnings
  bash tests/verify-cli-convention-no-silent-skip.sh

format:
  cargo fmt

fmt: format
# --- M13: Packaging UX Standardization ---
# These recipes implement Repo-Requirements.md §2.8. The OS-packaged
# recorders share a uniform packaging surface: `bump-version` rewrites
# Cargo.toml + packaging/recorder-metadata.yml; `build-package` shells
# out to packaging/build-all.sh with the requested channel selector;
# `verify-package` introspects the produced artifact.
#
# The shebang recipes below let Just hand the recipe body to the
# interpreter verbatim — Just's `{{...}}` interpolation still runs
# (so the `component` / `channel` arguments are substituted) but no
# other token in the body is parsed by Just.

# Bump the version in Cargo.toml + packaging/recorder-metadata.yml.
# Argument: major | minor | patch | MAJOR.MINOR.PATCH
bump-version component:
    #!/usr/bin/env python3
    import re, pathlib
    component = "{{component}}"
    root = pathlib.Path(".").resolve()
    cargo = root / "Cargo.toml"
    metadata = root / "packaging" / "recorder-metadata.yml"

    def current():
        if cargo.exists():
            m = re.search(r'^version\s*=\s*"(\d+\.\d+\.\d+)"', cargo.read_text(), re.MULTILINE)
            if m:
                return m.group(1)
        if metadata.exists():
            m = re.search(r'^\s*version:\s*(\d+\.\d+\.\d+)', metadata.read_text(), re.MULTILINE)
            if m:
                return m.group(1)
        return "0.1.0"

    def bump(c, v):
        if re.match(r'^\d+\.\d+\.\d+$', c):
            return c
        a, b, p = map(int, v.split("."))
        if c == "major":
            return f"{a+1}.0.0"
        if c == "minor":
            return f"{a}.{b+1}.0"
        if c == "patch":
            return f"{a}.{b}.{p+1}"
        raise SystemExit(f"unknown component {c!r}")

    cur = current()
    new = bump(component, cur)
    print(f"[bump-version] {cur} -> {new}")
    if cargo.exists():
        text = cargo.read_text()
        text = re.sub(r'(^version\s*=\s*")\d+\.\d+\.\d+(")', rf'\g<1>{new}\g<2>', text, count=1, flags=re.MULTILINE)
        cargo.write_text(text)
    if metadata.exists():
        text = metadata.read_text()
        text = re.sub(r'(^\s*version:\s*)\d+\.\d+\.\d+', rf'\g<1>{new}', text, count=1, flags=re.MULTILINE)
        metadata.write_text(text)

# Build a release artifact for the given channel.
# Channels: homebrew nix aur deb rpm scoop chocolatey
build-package channel:
    CT_PACKAGING_CHANNELS={{channel}} bash packaging/build-all.sh

# Verify the artifact produced by `build-package <channel>`.
# Set CT_VERIFY_STRICT=1 to fail when a verification tool is missing
# (default: SKIP with a warning so laptop runs don't break).
verify-package channel:
    #!/usr/bin/env python3
    import json, os, re, shutil, subprocess, sys
    from pathlib import Path
    ch = "{{channel}}"
    root = Path(".").resolve()
    pkg = root / "packaging"
    strict = os.environ.get("CT_VERIFY_STRICT") == "1"

    def skip(tool):
        if shutil.which(tool):
            return False
        if strict:
            print(f"::error::tool {tool} required in strict mode")
            sys.exit(1)
        print(f"[verify] SKIP: {tool} not on PATH")
        return True

    if ch == "homebrew":
        formula = next(pkg.glob("homebrew/*.rb"), None)
        if formula is None:
            print("::error::no homebrew formula"); sys.exit(1)
        if not skip("ruby"):
            subprocess.run(["ruby", "-c", str(formula)], check=True, capture_output=True)
        print(f"[verify] homebrew {formula.name} OK")
    elif ch == "nix":
        drv = pkg / "nix" / "default.nix"
        if not drv.exists():
            print("::error::no nix derivation"); sys.exit(1)
        if not skip("nix-instantiate"):
            subprocess.run(["nix-instantiate", "--eval", "-E", f"builtins.pathExists {drv}"], check=True, capture_output=True)
        print(f"[verify] nix {drv.name} OK")
    elif ch == "aur":
        pkgbuild = pkg / "aur" / "PKGBUILD"
        if not pkgbuild.exists():
            print("::error::no PKGBUILD"); sys.exit(1)
        subprocess.run(["bash", "-n", str(pkgbuild)], check=True)
        print(f"[verify] aur {pkgbuild.name} OK")
    elif ch == "deb":
        deb_dir = pkg / "dist" / "deb"
        debs = list(deb_dir.glob("*.deb")) if deb_dir.exists() else []
        if not debs:
            print("[verify] no deb yet — run `just build-package deb` first")
            sys.exit(1 if strict else 0)
        if not skip("dpkg-deb"):
            for d in debs:
                subprocess.run(["dpkg-deb", "-I", str(d)], check=True, capture_output=True)
                print(f"[verify] deb {d.name} OK")
    elif ch == "rpm":
        rpm_dir = pkg / "dist" / "rpm"
        rpms = list(rpm_dir.glob("*.rpm")) if rpm_dir.exists() else []
        if not rpms:
            print("[verify] no rpm yet — run `just build-package rpm` first")
            sys.exit(1 if strict else 0)
        if not skip("rpm"):
            for r in rpms:
                subprocess.run(["rpm", "-qpi", str(r)], check=True, capture_output=True)
                print(f"[verify] rpm {r.name} OK")
    elif ch == "scoop":
        sf = next(pkg.glob("scoop/*.json"), None)
        if sf is None:
            print("::error::no scoop manifest"); sys.exit(1)
        data = json.loads(sf.read_text())
        for k in ("version", "bin"):
            if k not in data:
                print(f"::error::scoop manifest missing {k}"); sys.exit(1)
        print(f"[verify] scoop {sf.name} OK")
    elif ch == "chocolatey":
        import xml.etree.ElementTree as ET
        nuspec = next(pkg.glob("chocolatey/*.nuspec"), None)
        if nuspec is None:
            print("::error::no nuspec"); sys.exit(1)
        tree = ET.parse(nuspec)
        root_el = tree.getroot()
        ns_m = re.match(r"\{.*\}", root_el.tag)
        ns = ns_m.group(0) if ns_m else ""
        meta = root_el.find(f"{ns}metadata")
        for field in ("id", "version", "authors", "description"):
            if meta is None or meta.find(f"{ns}{field}") is None:
                print(f"::error::nuspec missing {field}"); sys.exit(1)
        print(f"[verify] chocolatey {nuspec.name} OK")
    else:
        print(f"::error::unknown channel {ch!r}"); sys.exit(1)

# Per-channel build shortcuts.
build-homebrew:
    just build-package homebrew

build-nix:
    just build-package nix

build-aur:
    just build-package aur

build-deb:
    just build-package deb

build-rpm:
    just build-package rpm

build-scoop:
    just build-package scoop

build-chocolatey:
    just build-package chocolatey

# Per-channel verify shortcuts.
verify-homebrew:
    just verify-package homebrew

verify-nix:
    just verify-package nix

verify-aur:
    just verify-package aur

verify-deb:
    just verify-package deb

verify-rpm:
    just verify-package rpm

verify-scoop:
    just verify-package scoop

verify-chocolatey:
    just verify-package chocolatey
