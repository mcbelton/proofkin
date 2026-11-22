#!/usr/bin/env python3
"""Evidence-driven development for proofkin.

Runs the quality gates and the test suite, matches every result against the claims in
evidence/claims.json, and writes an evidence report to target/evidence/:

  report.md    human-readable: gates, every claim with the evidence that proves it
  report.json  the same, machine-readable
  logs/*.log   raw output of each step

Fails (exit 1) when a gate fails, a claim has missing or failing evidence, or a test or
live check isn't tied to any claim. So every claim needs evidence, and every piece of
evidence has to prove a claim.

  python scripts/evidence.py           # fmt, clippy, tests (no credentials needed; used by CI)
  python scripts/evidence.py --live    # also the live internet test (real Copilot, gh sign-in)
  python scripts/evidence.py --no-gates  # skip fmt/clippy, e.g. while iterating
"""
import argparse
import datetime
import json
import os
import platform
import re
import subprocess
import sys

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CLAIMS = os.path.join(REPO, "evidence", "claims.json")
OUT = os.path.join(REPO, "target", "evidence")
LOGS = os.path.join(OUT, "logs")


def run(name, cmd, env=None):
    """Runs `cmd`, saving combined output to logs/<name>.log. Returns (ok, output)."""
    print(f"== {name}: {' '.join(cmd)}", flush=True)
    proc = subprocess.run(cmd, cwd=REPO, env={**os.environ, **(env or {})},
                          stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    out = proc.stdout.decode("utf-8", errors="replace")
    with open(os.path.join(LOGS, f"{name}.log"), "w", encoding="utf-8") as f:
        f.write(out)
    print(f"   {'ok' if proc.returncode == 0 else 'FAILED'} (log: target/evidence/logs/{name}.log)", flush=True)
    return proc.returncode == 0, out


def target_of(path):
    """Cargo test target name from a 'Running' path: lib, bin, or the test file stem."""
    stem = os.path.splitext(os.path.basename(path.replace("\\", "/")))[0]
    if stem == "lib":
        return "lib"
    if stem == "main":
        return "bin"
    return stem


def parse_tests(output):
    """{test id: 'ok' | 'FAILED' | 'ignored'} from `cargo test` output."""
    results, target = {}, "?"
    for line in output.splitlines():
        running = re.match(r"^\s*Running (?:unittests )?(\S+)", line)
        if running:
            target = target_of(running.group(1))
            continue
        if re.match(r"^\s*Doc-tests ", line):
            target = "doc"
            continue
        test = re.match(r"^test (\S+) \.\.\. (ok|FAILED|ignored)\b", line)
        if test:
            results[f"{target}::{short_name(test.group(1))}"] = test.group(2)
    return results


def short_name(name):
    """A test name as cargo prints it, without the `tests` module."""
    name = name.replace("::tests::", "::")
    return name[len("tests::"):] if name.startswith("tests::") else name


def failed_output(output):
    """{test id: its lines of `cargo test` output} for each failed test.

    A section runs from the `---- name stdout ----` line to the next such line, the
    `failures:` list or the next test target.
    """
    sections, target, current = {}, "?", None
    for line in output.splitlines():
        running = re.match(r"^\s*Running (?:unittests )?(\S+)", line)
        if running:
            target, current = target_of(running.group(1)), None
            continue
        if re.match(r"^\s*Doc-tests ", line):
            target, current = "doc", None
            continue
        header = re.match(r"^---- (\S+) stdout ----$", line)
        if header:
            current = sections.setdefault(f"{target}::{short_name(header.group(1))}", [])
            current.append(line)
            continue
        if re.match(r"^failures:\s*$", line):
            current = None
        if current is not None:
            current.append(line)
    return sections


def applies(evidence):
    os_name = evidence.get("os") if isinstance(evidence, dict) else None
    return os_name is None or (os_name == "unix") == (os.name != "nt")


def evidence_id(evidence):
    return evidence["test"] if isinstance(evidence, dict) else evidence


def live_matches(pattern, name):
    return name.startswith(pattern[:-1]) if pattern.endswith("*") else name == pattern


def git(*args):
    try:
        return subprocess.run(["git", *args], cwd=REPO, capture_output=True, text=True).stdout.strip()
    except OSError:
        return ""


def tool_version(cmd):
    try:
        out = subprocess.run(cmd, capture_output=True, text=True)
        return (out.stdout or out.stderr).strip().splitlines()[0]
    except (OSError, IndexError):
        return "not found"


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--live", action="store_true", help="also run scripts/live_test.py (internet, real Copilot)")
    parser.add_argument("--no-gates", action="store_true", help="skip cargo fmt --check and clippy")
    args = parser.parse_args()
    os.makedirs(LOGS, exist_ok=True)
    with open(CLAIMS, encoding="utf-8") as f:
        claims = json.load(f)["claims"]

    gates = []
    if not args.no_gates:
        gates.append(("fmt", *run("fmt", ["cargo", "fmt", "--check"])[:1]))
        gates.append(("clippy", *run("clippy", ["cargo", "clippy", "--all-targets", "--locked", "--", "-D", "warnings"])[:1]))
    tests_ok, test_out = run("test", ["cargo", "test", "--locked", "--no-fail-fast", "--color", "never"])
    gates.append(("tests", tests_ok))
    tests = parse_tests(test_out)
    sections = failed_output(test_out)
    failed_tests = [{"id": t, "output": sections.get(t, [])}
                    for t in dict.fromkeys([*sections, *(t for t, s in tests.items() if s == "FAILED")])]

    live = None
    if args.live:
        built, _ = run("build", ["cargo", "build", "--locked"])
        gates.append(("build", built))
        results_file = os.path.join(OUT, "live_results.json")
        if os.path.exists(results_file):
            os.remove(results_file)
        live_ok, _ = run("live", [sys.executable, os.path.join("scripts", "live_test.py")],
                         env={"P2P_LIVE_RESULTS": results_file})
        gates.append(("live", live_ok))
        live = {}
        if os.path.exists(results_file):
            with open(results_file, encoding="utf-8") as f:
                live = {r["check"]: r["ok"] for r in json.load(f)}

    # Match evidence to claims.
    used_tests, used_live, report_claims = set(), set(), []
    for claim in claims:
        items, proven, pending = [], True, False
        for ev in claim.get("tests", []):
            test_id = evidence_id(ev)
            if not applies(ev):
                items.append({"kind": "test", "id": test_id, "status": "n/a on this OS"})
                continue
            status = tests.get(test_id, "missing")
            used_tests.add(test_id)
            items.append({"kind": "test", "id": test_id, "status": "pass" if status == "ok" else status.lower()})
            proven &= status == "ok"
        for pattern in claim.get("live", []):
            if live is None:
                items.append({"kind": "live", "id": pattern, "status": "not run"})
                pending = True
                continue
            hits = [name for name in live if live_matches(pattern, name)]
            used_live.update(hits)
            ok = bool(hits) and all(live[h] for h in hits)
            items.append({"kind": "live", "id": pattern, "status": "pass" if ok else ("fail" if hits else "missing")})
            proven &= ok
        if not claim.get("tests") and not claim.get("live"):
            proven = False
        status = "unproven" if not proven else ("proven (live not run)" if pending else "proven")
        report_claims.append({**{k: claim[k] for k in ("id", "area", "claim")}, "status": status, "evidence": items})

    orphan_tests = sorted(t for t in tests if t not in used_tests and not t.startswith("doc::"))
    orphan_live = sorted(n for n in (live or {}) if n not in used_live)
    unproven = [c["id"] for c in report_claims if c["status"] == "unproven"]
    gates_ok = all(ok for _, ok in gates)
    ok = gates_ok and not unproven and not orphan_tests and not orphan_live

    meta = {
        "result": "PASS" if ok else "FAIL",
        "date_utc": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%d %H:%M:%SZ"),
        "commit": git("rev-parse", "--short", "HEAD") + (" (uncommitted changes)" if git("status", "--porcelain") else ""),
        "platform": f"{platform.system()} {platform.release()} {platform.machine()}",
        "rustc": tool_version(["rustc", "--version"]),
        "copilot_cli": tool_version(["copilot", "--version"]) if args.live else "not used",
        "live": "run" if args.live else "not run (python scripts/evidence.py --live)",
        "tests_run": len(tests),
        "tests_passed": sum(s == "ok" for s in tests.values()),
        "live_checks": len(live or {}),
        "live_passed": sum(bool(v) for v in (live or {}).values()),
    }
    report = {"meta": meta, "gates": [{"gate": g, "ok": o} for g, o in gates], "claims": report_claims,
              "orphan_tests": orphan_tests, "orphan_live_checks": orphan_live, "failed_tests": failed_tests}
    with open(os.path.join(OUT, "report.json"), "w", encoding="utf-8") as f:
        json.dump(report, f, indent=1)
    with open(os.path.join(OUT, "report.md"), "w", encoding="utf-8") as f:
        f.write(render_markdown(report))

    print()
    for claim in report_claims:
        print(f"  {claim['status']:<22} {claim['id']:<7} {claim['claim'][:90]}")
    for failed in failed_tests:
        print(f"  TEST FAILED            {failed['id']}")
        for line in failed["output"][:30]:
            print(f"    {line}")
    if not tests_ok and not failed_tests:
        print("  TEST FAILED            cargo test did not run its tests; end of its log:")
        for line in test_out.splitlines()[-40:]:
            print(f"    {line}")
    for name in orphan_tests:
        print(f"  ORPHAN TEST            {name} (add it to a claim in evidence/claims.json)")
    for name in orphan_live:
        print(f"  ORPHAN LIVE CHECK      {name} (add it to a claim in evidence/claims.json)")
    for gate, gate_ok in gates:
        if not gate_ok:
            print(f"  GATE FAILED            {gate} (see target/evidence/logs/{gate}.log)")
    print(f"\n{meta['result']}: {len(report_claims) - len(unproven)}/{len(report_claims)} claims proven, "
          f"{meta['tests_passed']}/{meta['tests_run']} tests passed"
          + (f", {meta['live_passed']}/{meta['live_checks']} live checks passed" if live is not None else "")
          + f". Report: {os.path.relpath(os.path.join(OUT, 'report.md'), REPO)}")
    return 0 if ok else 1


def render_markdown(report):
    meta = report["meta"]
    lines = [
        f"# proofkin evidence report: {meta['result']}",
        "",
        "| | |", "| --- | --- |",
        *[f"| {k.replace('_', ' ')} | {v} |" for k, v in meta.items() if k != "result"],
        "",
        "## Gates", "",
        "| Gate | Result |", "| --- | --- |",
        *[f"| {g['gate']} | {'pass' if g['ok'] else '**FAIL**'} |" for g in report["gates"]],
        "",
        "## Claims", "",
        "| Claim | Area | Status | Statement |", "| --- | --- | --- | --- |",
        *[f"| {c['id']} | {c['area']} | {c['status'] if c['status'] != 'unproven' else '**unproven**'} | {c['claim']} |"
          for c in report["claims"]],
        "",
        "## Evidence per claim", "",
    ]
    for claim in report["claims"]:
        lines += [f"### {claim['id']}: {claim['claim']}", ""]
        lines += [f"- {e['kind']} `{e['id']}`: {e['status']}" for e in claim["evidence"]]
        lines.append("")
    if report.get("failed_tests"):
        lines += ["## Failed tests", ""]
        for failed in report["failed_tests"]:
            lines += [f"### `{failed['id']}`", "", "```", *failed["output"][:30], "```", ""]
    if report["orphan_tests"] or report["orphan_live_checks"]:
        lines += ["## Evidence not tied to a claim", ""]
        lines += [f"- test `{t}`" for t in report["orphan_tests"]]
        lines += [f"- live check `{t}`" for t in report["orphan_live_checks"]]
        lines.append("")
    return "\n".join(lines)


if __name__ == "__main__":
    sys.exit(main())
