#!/usr/bin/env python3
"""CI regression guard for the narrow audit exception; never invokes Cargo."""
from copy import deepcopy
import importlib.util
from pathlib import Path

spec = importlib.util.spec_from_file_location("cargo_audit", Path(__file__).with_name("cargo-audit.py"))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


def main():
    wrapper = {"name": "webpki-roots", "version": "0.26.11", "source": module.REGISTRY,
               "checksum": module.WRAPPER_CHECKSUM, "dependencies": ["webpki-roots 1.0.10"]}
    patched = {"name": "webpki-roots", "version": "1.0.10", "source": module.REGISTRY,
               "checksum": "1c1c8e20c8a38fc7d65abfe74eb7280e4aa667e8eec8bc84996d2874810748d9"}
    valid = {"package": [wrapper, patched]}
    if not module.wrapper_exception(valid):
        raise SystemExit("official wrapper fixture rejected")
    cases = []
    for index, key, values in [
        (0, "checksum", ["0" * 64, None]),
        (0, "source", ["git+https://example.test", None]),
        (0, "dependencies", [[], ["webpki-roots"], ["webpki-roots 1.0.9"], ["webpki-roots 1.0.10", "other"], [None], [{}], [[]]]),
        (1, "version", ["1.0.9", "1.0.10-alpha", "1.0.10+build", "2.0.0", "unknown", None]),
        (1, "source", ["path+local", None]),
        (1, "checksum", ["", None]),
    ]:
        for value in values:
            case = deepcopy(valid)
            case["package"][index][key] = value
            cases.append(case)
    cases += [{}, {"package": [wrapper]}, {"package": [patched]},
              {"package": [wrapper, patched, deepcopy(patched)]},
              {"package": [wrapper, patched, deepcopy(wrapper)]},
              {"package": [wrapper, patched, {**patched, "version": "0.25.4"}]},
              {"package": [wrapper, patched, {**patched, "version": "1.0.9"}]},
              {"package": [None]}]
    for number, case in enumerate(cases):
        if module.wrapper_exception(case):
            raise SystemExit(f"unsafe fixture {number} incorrectly permitted")
    print(f"cargo audit exception guard: {len(cases)} unsafe variants rejected")


if __name__ == "__main__":
    main()
