import json
import subprocess
from pathlib import Path

root = Path(__file__).resolve().parent.parent
metadata = json.loads(subprocess.check_output(["cargo", "metadata", "--locked", "--offline", "--format-version", "1", "--filter-platform", "x86_64-pc-windows-msvc"], cwd=root))
active = {node["id"] for node in metadata["resolve"]["nodes"]}
version=next(p["version"] for p in metadata["packages"] if p["name"]=="file-recovery")
sections = [f"THIRD-PARTY LICENSE NOTICES\nFile Recovery {version}\n\nDependency copyrights and licenses remain with their respective owners.\nThis inventory includes build and test dependencies from Cargo.lock.\n"]
missing = []
for package in sorted(metadata["packages"], key=lambda p: p["name"]):
    if package["id"] not in active or package["name"] == "file-recovery":
        continue
    path = Path(package["manifest_path"]).parent
    candidates = sorted({p for p in path.iterdir() if p.is_file() and (p.name.upper().startswith(("LICENSE", "LICENCE", "COPYING", "NOTICE", "COPYRIGHT")))})
    if package.get("license_file"):
        candidates = sorted(set(candidates + [path / package["license_file"]]))
    if not candidates:
        missing.append(package["name"])
    sections.append(f"\n{'=' * 72}\n{package['name']} {package['version']}\nLicense: {package.get('license') or 'see license file'}\nSource: {package.get('repository') or 'https://crates.io/crates/' + package['name']}\n")
    for file in candidates:
        sections.append(f"\n--- {file.name} ---\n{file.read_text(encoding='utf-8', errors='replace')}\n")
if missing:
    raise SystemExit("Missing license texts: " + ", ".join(missing))
output = root / "assets" / "third-party-notices.txt"
output.write_text("".join(sections), encoding="utf-8")
print(f"Collected {len(sections) - 1} notice sections ({output.stat().st_size} bytes)")
