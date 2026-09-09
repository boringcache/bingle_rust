"""Retain native compiler diagnostics with credentials and URL queries removed."""

import json
import os
import re
from collections import Counter, defaultdict
from pathlib import Path
from urllib.parse import urlsplit, urlunsplit

destination = Path(os.environ["RUNNER_TEMP"]) / "validation"
source = Path(os.environ["SCCACHE_ERROR_LOG"])
if not source.exists():
    raise SystemExit("sccache did not emit the requested native diagnostic log")
text = source.read_text(errors="replace")
for key, value in os.environ.items():
    if re.search(r"TOKEN|SECRET|PASSWORD|CREDENTIAL|AUTHORIZATION", key, re.IGNORECASE) and len(value) >= 8:
        text = text.replace(value, "[redacted]")


def redact_url(match):
    parts = urlsplit(match.group(0))
    return urlunsplit((parts.scheme, parts.hostname or "", parts.path, "", ""))


text = re.sub(r"https?://[^\s<>\"']+", redact_url, text)
(destination / "sccache-debug.redacted.log").write_text(text)

metadata = json.loads((destination / "cargo-metadata.json").read_text())
members = set(metadata["workspace_members"])
names = {
    target["name"].replace("-", "_")
    for package in metadata["packages"] if package["id"] in members
    for target in package["targets"]
}
counts = defaultdict(Counter)
for name, outcome in re.findall(r"\[([A-Za-z0-9_.-]+)\]: Cache (hit|miss) in", text):
    if name in names:
        counts[name][outcome] += 1
summary = {"workspace_targets": sorted(names), "observed_workspace_cache_lookups": counts,
           "write_error_lines": [line for line in text.splitlines() if "Error executing cache write:" in line]}
(destination / "native-diagnostic-summary.json").write_text(json.dumps(summary, indent=2) + "\n")

build_metadata = []
for build_root in [Path("target"), Path("/var/tmp/bingle_native_target")]:
    if build_root.exists():
        for file in build_root.glob("**/build/bingle_*/output"):
            lines = [line for line in file.read_text().splitlines() if line.startswith(("cargo:rustc-env=VERGEN_BUILD_", "cargo:rustc-env=VERGEN_GIT_SHA="))]
            if lines:
                build_metadata.append({"file": str(file), "lines": lines})
(destination / "build-metadata.json").write_text(json.dumps(build_metadata, indent=2) + "\n")
