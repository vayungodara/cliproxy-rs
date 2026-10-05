"""Render the binary-only formula from a published release's SHA256SUMS."""

import argparse
import re
from pathlib import Path

TARGETS = (
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-gnu",
)


def render(tag, checksums, template, current_formula=None):
    if not re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+", tag):
        raise ValueError("expected a stable release tag such as v0.1.3")
    version = tag[1:]
    if current_formula is not None:
        versions = set(re.findall(r"/releases/download/v([0-9]+\.[0-9]+\.[0-9]+)/", current_formula))
        if len(versions) != 1:
            raise ValueError("expected one stable version in the current formula")
        current_version = versions.pop()
        if tuple(map(int, current_version.split("."))) > tuple(map(int, version.split("."))):
            print(f"Skipping {tag}; the tap already has v{current_version}.")
            return current_formula
    formula = template.replace("@VERSION@", version)
    for target in TARGETS:
        archive = f"cliproxy-{version}-{target}.tar.gz"
        matches = re.findall(rf"^([0-9a-f]{{64}})  {re.escape(archive)}$", checksums, re.MULTILINE)
        if len(matches) != 1:
            raise ValueError(f"expected exactly one SHA256SUMS entry for {archive}")
        formula = formula.replace(f"@SHA256_{target.upper().replace('-', '_')}@", matches[0])
    if re.search(r"@[A-Z0-9_]+@", formula):
        raise ValueError("unresolved formula placeholder")
    return formula


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("tag")
    parser.add_argument("checksums", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    template = Path(__file__).with_name("cliproxy-rs.rb").read_text()
    current = args.output.read_text() if args.output.exists() else None
    formula = render(args.tag, args.checksums.read_text(), template, current)
    if formula != current:
        args.output.write_text(formula)
