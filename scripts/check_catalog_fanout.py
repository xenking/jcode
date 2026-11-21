#!/usr/bin/env python3
"""Every Provider-valued MultiProvider slot participates in both refresh modes."""
from pathlib import Path
import re

from guardrail_source import function_bodies, production_code, run


SELF_FIELD = re.compile(r"\bself\s*\.\s*(\w+)")


def bodies(code):
    return {name: code[start:end] for name, start, end in function_bodies(code)}


def provider_fields(code):
    match = re.search(r"\bstruct\s+MultiProvider\s*\{([^}]+)\}", code, re.S)
    if not match:
        raise ValueError("restore MultiProvider or migrate its catalog ownership gate")
    # Field types contain no braces. Only Arc-held Provider runtimes are catalog
    # owners, not active selection state or caches. Names are not an allowlist.
    fields = {}
    for field in re.finditer(r"\b(\w+)\s*:\s*([^,]+(?:,(?!\s*\w+\s*:)[^,]+)*)", match[1]):
        name, kind = field.groups()
        if re.search(r"\bArc\s*<", kind) and re.search(r"\b\w*Provider\b", kind):
            fields[name] = "HashMap" in kind or "BTreeMap" in kind
    if not fields:
        raise ValueError("no Provider-valued slots found; update the field parser, never silently skip catalogs")
    return fields


def violations(root: Path) -> list[str]:
    directory = root / "crates/jcode-base/src/provider"
    main = production_code(directory / "mod.rs")
    refresh_path = directory / "catalog_refresh.rs"
    refresh = production_code(refresh_path) if refresh_path.exists() else main
    entry = bodies(main)
    functions = bodies(refresh)
    fields = provider_fields(main)
    errors = []
    targets = functions.get("catalog_targets")
    if targets is None:
        # Also understand the real pre-extraction implementation, so historical
        # failure names the missing endpoint map, not a newly introduced file.
        old = functions.get("refresh_catalogs", entry.get("prefetch_models", ""))
        seen = set(SELF_FIELD.findall(old))
        for name in fields:
            if re.search(r"\bself\s*\.\s*" + re.escape(name) + r"_provider\s*\(", old):
                seen.add(name)
        for name in fields.keys() - seen:
            errors.append(f"MultiProvider.{name}: missing catalog fanout; include it in catalog_targets for cached AND forced refresh")
        errors.append("replace manual per-provider refresh branches with catalog_targets and the shared join_all consumer")
        return errors

    for name, is_map in fields.items():
        if is_map:
            present = re.search(r"\bself\s*\.\s*" + re.escape(name) + r"\b[\s\S]*?\.\s*iter\s*\(", targets)
        else:
            present = re.search(r"\bcatalog_slot\s*\(\s*&\s*self\s*\.\s*" + re.escape(name) + r"\s*\)", targets)
        if not present:
            errors.append(f"MultiProvider.{name}: add this runtime to catalog_targets using catalog_slot(&self.{name}) or extend all map entries; never limit refresh to the active provider")

    fanout = functions.get("refresh_catalogs", "")
    if not re.search(r"\bjoin_all\s*\(\s*self\s*\.\s*catalog_targets\s*\(\s*\)\s*\.\s*into_iter\s*\(\s*\)\s*\.\s*map\s*\(", fanout):
        errors.append("refresh_catalogs: consume every catalog_targets entry through join_all, not a hand-picked provider subset")
    if not re.search(r"\brefresh_child_catalog\s*\(\s*target\s*\.\s*provider\s*\.\s*as_ref\s*\(\s*\)\s*,\s*force\s*\)", fanout):
        errors.append("refresh_catalogs: forward force to refresh_child_catalog for every target")
    child = functions.get("refresh_child_catalog", "")
    if not re.search(r"\bif\s+force\s*\{[^}]*\.\s*refresh_model_catalog\s*\([^}]*\}\s*else\s*\{[^}]*\.\s*prefetch_models\s*\(", child):
        errors.append("refresh_child_catalog: forced refresh must call refresh_model_catalog, cached prefetch must call prefetch_models")
    for name, mode in (("prefetch_models", "false"), ("refresh_model_catalog", "true")):
        if not re.search(r"\bself\s*\.\s*refresh_catalogs\s*\(\s*" + mode + r"\s*\)", entry.get(name, "")):
            errors.append(f"MultiProvider::{name}: delegate to refresh_catalogs({mode}) so explicit refresh cannot reuse the prefetch path")
    return errors


if __name__ == "__main__":
    raise SystemExit(run(__doc__, violations))
