#!/usr/bin/env python3
"""Turns the build artifacts of one version into the files of a GitHub Release.

    python3 scripts/latest_json.py <artifacts dir> <out dir> <version> [--repo OWNER/NAME]
    python3 scripts/latest_json.py --self-test

<artifacts dir> holds the downloaded artifacts, one directory per target
(`lazurio-agent-desktop-<target triple>/…`). <out dir> receives the installers
and update packages under names without spaces, their `.sig` files and the
`latest.json` the app's updater reads from the newest stable release:

    https://github.com/<repo>/releases/latest/download/latest.json

Every platform of the app needs its update package and signature; a missing
one is an error, so a release never announces an update it cannot deliver.
"""
import json
import shutil
import sys
import tempfile
from datetime import datetime, timezone
from pathlib import Path

REPO = "Lazurio/lazurio-agent-desktop"
PREFIX = "Lazurio-Agent-Desktop"

# target triple -> (updater platform key, update package suffix, arch label)
TARGETS = {
    "aarch64-apple-darwin": ("darwin-aarch64", ".app.tar.gz", "aarch64"),
    "x86_64-apple-darwin": ("darwin-x86_64", ".app.tar.gz", "x86_64"),
    "x86_64-pc-windows-msvc": ("windows-x86_64", "-setup.exe", "x64"),
}
# Installers people download by hand, next to the update packages.
INSTALLERS = {".dmg", ".msi"}


def one(files, what):
    if len(files) != 1:
        raise SystemExit(f"expected exactly one {what}, found {len(files)}: {files}")
    return files[0]


def build(artifacts: Path, out: Path, version: str, repo: str = REPO, notes: str = "") -> dict:
    out.mkdir(parents=True, exist_ok=True)
    base = f"https://github.com/{repo}/releases/download/v{version}"
    platforms = {}
    for triple, (platform, suffix, arch) in TARGETS.items():
        root = artifacts / f"lazurio-agent-desktop-{triple}"
        if not root.is_dir():
            raise SystemExit(f"missing artifacts for {triple} in {artifacts}")
        files = [p for p in root.rglob("*") if p.is_file()]
        package = one([p for p in files if p.name.endswith(suffix)], f"{suffix} for {triple}")
        signature = one(
            [p for p in files if p.name == package.name + ".sig"], f"signature for {triple}"
        )
        name = f"{PREFIX}_{version}_{arch}{suffix}"
        shutil.copyfile(package, out / name)
        shutil.copyfile(signature, out / (name + ".sig"))
        platforms[platform] = {
            "signature": signature.read_text().strip(),
            "url": f"{base}/{name}",
        }
        for installer in files:
            if installer.suffix in INSTALLERS:
                shutil.copyfile(installer, out / f"{PREFIX}_{version}_{arch}{installer.suffix}")
    manifest = {
        "version": version,
        "notes": notes or f"Lazurio Agent Desktop {version}",
        "pub_date": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "platforms": platforms,
    }
    (out / "latest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    return manifest


def self_test() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        art = Path(tmp) / "artifacts"
        for triple, (_, suffix, _) in TARGETS.items():
            d = art / f"lazurio-agent-desktop-{triple}"
            sub = d / ("nsis" if "windows" in triple else "macos")
            sub.mkdir(parents=True)
            name = "Lazurio Agent Desktop" + ("_0.2.0_x64" if "windows" in triple else "") + suffix
            (sub / name).write_bytes(b"package")
            (sub / (name + ".sig")).write_text(f"sig-{triple}\n")
            extra = d / ("msi" if "windows" in triple else "dmg")
            extra.mkdir()
            (extra / ("x.msi" if "windows" in triple else "x.dmg")).write_bytes(b"installer")
        out = Path(tmp) / "out"
        m = build(art, out, "0.2.0")
        assert set(m["platforms"]) == {"darwin-aarch64", "darwin-x86_64", "windows-x86_64"}
        win = m["platforms"]["windows-x86_64"]
        assert win["signature"] == "sig-x86_64-pc-windows-msvc", win
        assert win["url"].endswith("/v0.2.0/Lazurio-Agent-Desktop_0.2.0_x64-setup.exe"), win
        names = sorted(p.name for p in out.iterdir())
        assert all(" " not in n for n in names), names
        assert "Lazurio-Agent-Desktop_0.2.0_aarch64.dmg" in names, names
        assert "Lazurio-Agent-Desktop_0.2.0_x64.msi" in names, names
        assert json.loads((out / "latest.json").read_text())["version"] == "0.2.0"
        # A missing signature must stop the release.
        (art / "lazurio-agent-desktop-x86_64-apple-darwin/macos/Lazurio Agent Desktop.app.tar.gz.sig").unlink()
        try:
            build(art, Path(tmp) / "out2", "0.2.0")
        except SystemExit:
            pass
        else:
            raise AssertionError("missing signature was accepted")
    print("latest_json self-test ok")


if __name__ == "__main__":
    if sys.argv[1:] == ["--self-test"]:
        self_test()
        sys.exit(0)
    args = sys.argv[1:]
    repo = REPO
    if "--repo" in args:
        i = args.index("--repo")
        repo = args[i + 1]
        del args[i : i + 2]
    if len(args) != 3:
        raise SystemExit(__doc__)
    manifest = build(Path(args[0]), Path(args[1]), args[2], repo)
    print(json.dumps({k: v for k, v in manifest.items() if k != "platforms"}))
    print("platforms:", ", ".join(manifest["platforms"]))
