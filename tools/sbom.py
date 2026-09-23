#!/usr/bin/env python3
"""CycloneDX 1.5 JSON for what a package ships.

Walks the locked dependency graph from one package (default `schwab-mcp`,
whose binaries the release publishes) through normal and build
dependencies only: dev-dependencies and unrelated workspace members are not
part of the shipped artifact, so they are not in its bill of materials.
"""
import argparse
import datetime
import json
import pathlib
import subprocess
import tomllib
import uuid


def purl(pkg):
    return f"pkg:cargo/{pkg['name']}@{pkg['version']}"


def shipped(metadata, root_name):
    """Package ids reachable from `root_name` over normal/build edges."""
    packages = {p["id"]: p for p in metadata["packages"]}
    nodes = {n["id"]: n for n in metadata["resolve"]["nodes"]}
    roots = [p["id"] for p in metadata["packages"]
             if p["name"] == root_name and p["id"] in metadata["workspace_members"]]
    if len(roots) != 1:
        raise SystemExit(f"workspace package {root_name!r} not found")
    seen, stack, edges = set(), list(roots), {}
    while stack:
        current = stack.pop()
        if current in seen:
            continue
        seen.add(current)
        deps = []
        for dep in nodes[current]["deps"]:
            kinds = {k["kind"] for k in dep["dep_kinds"]}
            if kinds & {None, "build"}:
                deps.append(dep["pkg"])
                stack.append(dep["pkg"])
        edges[current] = sorted(set(deps))
    return roots[0], packages, seen, edges


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=pathlib.Path)
    parser.add_argument("--package", default="schwab-mcp")
    args = parser.parse_args()

    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--format-version", "1", "--locked"], text=True))
    lock = tomllib.loads(pathlib.Path("Cargo.lock").read_text())
    checksums = {(p["name"], p["version"]): p["checksum"]
                 for p in lock["package"] if "checksum" in p}

    root, packages, ids, edges = shipped(metadata, args.package)

    def component(pkg, kind):
        entry = {"type": kind, "bom-ref": purl(pkg), "name": pkg["name"],
                 "version": pkg["version"], "purl": purl(pkg)}
        checksum = checksums.get((pkg["name"], pkg["version"]))
        if checksum:
            entry["hashes"] = [{"alg": "SHA-256", "content": checksum}]
        if pkg.get("license"):
            entry["licenses"] = [{"expression": pkg["license"]}]
        if pkg.get("repository"):
            entry["externalReferences"] = [{"type": "vcs", "url": pkg["repository"]}]
        return entry

    bom = {
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "serialNumber": f"urn:uuid:{uuid.uuid4()}",
        "version": 1,
        "metadata": {
            "timestamp": datetime.datetime.now(datetime.timezone.utc).isoformat(),
            "tools": {"components": [{"type": "application", "name": "tools/sbom.py"}]},
            "component": component(packages[root], "application"),
        },
        "components": [component(packages[i], "library")
                       for i in sorted(ids - {root}, key=lambda i: purl(packages[i]))],
        "dependencies": [{"ref": purl(packages[i]),
                          "dependsOn": [purl(packages[d]) for d in edges[i]]}
                         for i in sorted(ids, key=lambda i: purl(packages[i]))],
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(bom, indent=2) + "\n")


if __name__ == "__main__":
    main()
