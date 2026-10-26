"""Build distribution archives once; QA and publishing consume these exact files."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import tomllib
import zipfile

ROOT = Path(__file__).resolve().parent.parent
BINARIES = {"linux-x86_64": "libmistlib.so", "windows-x86_64": "mistlib.dll", "macos-aarch64": "libmistlib.dylib"}


def sha256(path):
    with open(path, "rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def validate_version(value):
    core, separator, suffix = value.partition("-")
    if not re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", core):
        raise ValueError("invalid release version")
    if separator and any(not re.fullmatch(r"[0-9A-Za-z-]+", part) or
                         (part.isdigit() and len(part) > 1 and part.startswith("0")) for part in suffix.split(".")):
        raise ValueError("invalid prerelease version")
    return value


class _DefaultBump(str):
    """Distinguish the default minor bump from an explicit minor argument."""


_DEFAULT_BUMP = _DefaultBump("minor")


def next_version(current: str, channel: str, bump: str = _DEFAULT_BUMP) -> str:
    validate_version(current)
    if channel not in ("beta", "stable"):
        raise ValueError("channel must be beta or stable")
    if bump not in ("minor", "patch", "major"):
        raise ValueError("bump must be minor, patch or major")
    core, _, suffix = current.partition("-")
    if suffix:
        if not re.fullmatch(r"beta\.[0-9]+", suffix):
            raise ValueError("only beta.N prereleases are part of the beta/stable cycle")
        if bump is not _DEFAULT_BUMP:
            raise ValueError("explicit bump is not allowed during a beta cycle; finish with stable first")
        return core if channel == "stable" else f"{core}-beta.{int(suffix.split('.')[1]) + 1}"
    if channel == "stable":
        raise ValueError("already stable; start a beta first")
    major, minor, patch = map(int, core.split("."))
    if bump == "major":
        major, minor, patch = major + 1, 0, 0
    elif bump == "minor":
        minor, patch = minor + 1, 0
    else:
        patch += 1
    return f"{major}.{minor}.{patch}-beta.1"


def semver_key(value):
    """SemVer precedence for validated release versions (numeric identifiers first)."""
    core, _, suffix = validate_version(value).partition("-")
    identifiers = tuple((0, int(part)) if part.isdigit() else (1, part)
                        for part in suffix.split(".")) if suffix else ()
    return (*map(int, core.split(".")), not bool(suffix), identifiers)


def npm_dist_tag(version: str) -> str:
    _, separator, suffix = validate_version(version).partition("-")
    if not separator:
        return "latest"
    first = suffix.split(".")[0]
    return first if re.fullmatch(r"[a-z]+", first) else "next"


def version():
    versions = {tomllib.loads((ROOT / crate / "Cargo.toml").read_text())["package"]["version"]
                for crate in ("mistlib-core", "mistlib-native", "mistlib-wasm")}
    if len(versions) != 1:
        raise ValueError("crate versions disagree")
    return validate_version(versions.pop())


def build(args):
    out = Path(args.out).resolve()
    out.mkdir(parents=True, exist_ok=True)
    release_version = version()
    commit = os.environ.get("GITHUB_SHA") or subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    manifest = {"schema": 1, "version": release_version, "commit": commit, "files": {}}
    with tempfile.TemporaryDirectory(prefix="mistlib-package-") as temp:
        staging = Path(temp)
        if args.kind == "native":
            binary = Path(args.binary).resolve()
            if not binary.is_file() or binary.stat().st_size == 0:
                raise ValueError("missing native binary")
            archive = out / f"mistlib-native-{args.target}-v{release_version}.zip"
            with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as target:
                target.write(binary, BINARIES[args.target])
                for name in ("LICENSE", "README.md"):
                    target.write(ROOT / name, name)
            manifest.update({"id": f"native-{args.target}", "binary": BINARIES[args.target], "binarySha256": sha256(binary)})
        else:
            pkg = staging / "pkg"
            shutil.copytree(args.pkg, pkg)
            # Never ship the previous build's tgz or incidental output.
            for item in pkg.iterdir():
                if item.name not in {"package.json", "mistlib_wasm.js", "mistlib_wasm_bg.wasm", "mistlib_wasm.d.ts", "snippets"}:
                    if item.is_dir():
                        shutil.rmtree(item)
                    else:
                        item.unlink()
            for name in ("LICENSE", "README.md"):
                shutil.copyfile(ROOT / name, pkg / name)
            metadata = json.loads((pkg / "package.json").read_text())
            if metadata["version"] != release_version:
                raise ValueError("WASM package is stale; rebuild it before packaging")
            metadata.update({"name": "@tik-choco/mistlib", "version": release_version,
                             "repository": {"type": "git", "url": "git+https://github.com/tik-choco-lab/mistlib.git"},
                             "homepage": "https://github.com/tik-choco-lab/mistlib"})
            (pkg / "package.json").write_text(json.dumps(metadata, indent=2) + "\n")
            result = subprocess.check_output(["npm", "pack", "--json", "--ignore-scripts", "--pack-destination", str(out)], cwd=pkg, text=True)
            packed = json.loads(result)[0]
            required = {"mistlib_wasm.js", "mistlib_wasm_bg.wasm", "mistlib_wasm.d.ts", "README.md", "LICENSE", "package.json"}
            if not required.issubset({file["path"] for file in packed["files"]}):
                raise ValueError("npm archive omits required files")
            tarball = out / packed["filename"]
            manifest["files"][tarball.name] = sha256(tarball)
            archive = out / f"mistlib-wasm-v{release_version}.zip"
            with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as target:
                for file in sorted(pkg.rglob("*")):
                    if file.is_file():
                        target.write(file, file.relative_to(staging))
                for name in ("LICENSE", "README.md"):
                    target.write(ROOT / name, name)
            manifest.update({"id": "wasm", "package": tarball.name})
        manifest["files"][archive.name] = sha256(archive)
    (out / f"candidate-{manifest['id']}.json").write_text(json.dumps(manifest, indent=2) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="kind", required=True)
    native = sub.add_parser("native")
    native.add_argument("--binary", required=True)
    native.add_argument("--target", choices=BINARIES, required=True)
    native.add_argument("--out", required=True)
    wasm = sub.add_parser("wasm")
    wasm.add_argument("--pkg", required=True)
    wasm.add_argument("--out", required=True)
    show_version = sub.add_parser("version")
    show_version.add_argument("value", nargs="?")
    next_release = sub.add_parser("next-version")
    next_release.add_argument("current")
    next_release.add_argument("channel", choices=("beta", "stable"))
    next_release.add_argument("--bump", choices=("minor", "patch", "major"), default=_DEFAULT_BUMP)
    compare = sub.add_parser("compare")
    compare.add_argument("left")
    compare.add_argument("right")
    dist_tag = sub.add_parser("npm-dist-tag")
    dist_tag.add_argument("value")
    args = parser.parse_args()
    if args.kind == "version":
        print(validate_version(args.value) if args.value is not None else version())
    elif args.kind in ("next-version", "compare", "npm-dist-tag"):
        try:
            if args.kind == "next-version":
                print(next_version(args.current, args.channel, args.bump))
            elif args.kind == "compare":
                left, right = semver_key(args.left), semver_key(args.right)
                print((left > right) - (left < right))
            else:
                print(npm_dist_tag(args.value))
        except ValueError as error:
            sys.exit(f"error: {error}")
    else:
        build(args)


if __name__ == "__main__":
    main()
