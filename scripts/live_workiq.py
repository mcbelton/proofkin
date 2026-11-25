"""Live Work IQ check against a real tenant (docs/work-iq.md, Testing). It only reads.

A fresh profile signs in to Work IQ and its doctor runs every check (the last is one small
retrieve), and it lists the agents Work IQ offers (only how many is shown). Its agent then
runs a plan with a workiq= step for its own digital worker (real Copilot, on the profile's
GitHub sign-in) and an @workiq step. Then the run's evidence verifies, the step's signed
evidence records the Work IQ context by its hash, and every call in the profile's Work IQ
audit file was a read.

  PROOFKIN_LIVE_WORKIQ=cli      Microsoft's Work IQ CLI, signed in, with its EULA accepted by
                                you (`workiq accept-eula`). Optional: PROOFKIN_LIVE_WORKIQ_CLI_PATH,
                                PROOFKIN_LIVE_WORKIQ_ACCOUNT (an email the CLI signed in).
  PROOFKIN_LIVE_WORKIQ=hosted   Proofkin's own sign-in: PROOFKIN_LIVE_WORKIQ_TENANT and
                                PROOFKIN_LIVE_WORKIQ_CLIENT_ID; enter the device code it prints.
  PROOFKIN_LIVE_WORKIQ_QUERY    what the workiq= step searches for and asks its worker (default:
                                what you have been working on lately).

Needs a built binary (cargo build) and the GitHub CLI signed in (`gh auth login`). Microsoft
365 content is never printed: step outputs are left out of the log and emails are masked.
The profile lives in a temporary PROOFKIN_HOME, deleted afterwards.
Run: $env:PROOFKIN_LIVE_WORKIQ = "cli"; python scripts/live_workiq.py
python scripts/evidence.py --live runs it too when PROOFKIN_LIVE_WORKIQ is set.
"""
import json, os, re, shutil, subprocess, sys, tempfile, threading, time

sys.stdout.reconfigure(encoding="utf-8", errors="replace")

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
EXE = os.path.join(REPO, "target", "debug", "proofkin.exe" if os.name == "nt" else "proofkin")
MODE = os.environ.get("PROOFKIN_LIVE_WORKIQ", "")
QUERY = os.environ.get("PROOFKIN_LIVE_WORKIQ_QUERY", "What have I been working on lately?")
# Read tools only: a call to anything else fails the check that it only read.
READS = {"retrieve", "ask", "fetch", "search_paths", "get_schema", "list_agents"}
results = []


def mask(text):
    """Text with every email address's user part left out."""
    return re.sub(r"[A-Za-z0-9._%+-]+@([A-Za-z0-9-]+(\.[A-Za-z0-9-]+)+)", r"<account>@\1", text)


def check(name, ok, detail=""):
    results.append((name, bool(ok)))
    print(f"{'PASS' if ok else 'FAIL'}: {name}" + ("" if ok else f" {mask(str(detail))}"), flush=True)


def gh_account():
    out = subprocess.run(["gh", "auth", "status", "--json", "hosts"], capture_output=True, text=True)
    hosts = json.loads(out.stdout or "{}").get("hosts", {})
    accounts = [(a["login"], host, a.get("active", False)) for host, accts in hosts.items() for a in accts
                if a.get("state") == "success"]
    accounts.sort(key=lambda a: not a[2])
    return accounts[0][:2] if accounts else None


def values_of(value, key):
    """Every value under `key`, anywhere in `value`."""
    if isinstance(value, dict):
        return [v for k, v in value.items() if k == key] + [f for v in value.values() for f in values_of(v, key)]
    if isinstance(value, list):
        return [f for v in value for f in values_of(v, key)]
    return []


def done_header(line, step=r"\S+", peer=r"\S+"):
    """Whether `line` starts a done step's output: `=== step <id> on <peer>[ · <model>] ===`
    (a failed step's line goes on with `failed: <error>` instead, and has no output)."""
    return re.match(rf"^=== step {step} on {peer}( · .*)? ===$", line) is not None


class Agent:
    """A running agent; what its steps output stays out of the log."""

    def __init__(self, name, args, env):
        self.name, self.lines = name, []
        self.p = subprocess.Popen([EXE, "--name", name, *args], cwd=REPO, env=env, stdin=subprocess.PIPE,
                                  stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, bufsize=1,
                                  encoding="utf-8", errors="replace")
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        hidden = False
        for line in self.p.stdout:
            line = line.rstrip("\n")
            self.lines.append(line)
            if hidden:
                hidden = line != "=== end ==="
                continue
            print(f"  [{self.name}] {mask(line)}", flush=True)
            if done_header(line):
                hidden = True
                print(f"  [{self.name}] (the step's output is left out of the log)", flush=True)

    def send(self, cmd):
        print(f"  [{self.name}] << {cmd}", flush=True)
        self.p.stdin.write(cmd + "\n")
        self.p.stdin.flush()

    def wait_for(self, pattern, timeout):
        end = time.time() + timeout
        while time.time() < end:
            for line in self.lines:
                if re.search(pattern, line):
                    return line
            if self.p.poll() is not None:
                return None
            time.sleep(0.2)
        return None

    def stop(self):
        if self.p.poll() is None:
            try:
                self.send("/quit")
                self.p.wait(30)
            except (OSError, subprocess.TimeoutExpired):
                self.p.kill()


def main():
    if MODE not in ("cli", "hosted"):
        sys.exit("set PROOFKIN_LIVE_WORKIQ to cli or hosted (see the top of scripts/live_workiq.py)")
    if MODE == "hosted" and not (os.environ.get("PROOFKIN_LIVE_WORKIQ_TENANT")
                                 and os.environ.get("PROOFKIN_LIVE_WORKIQ_CLIENT_ID")):
        sys.exit("PROOFKIN_LIVE_WORKIQ=hosted needs PROOFKIN_LIVE_WORKIQ_TENANT and PROOFKIN_LIVE_WORKIQ_CLIENT_ID")
    account = gh_account()
    if not account:
        sys.exit("no GitHub CLI account signed in; run: gh auth login")
    home = tempfile.mkdtemp(prefix="p2p-live-workiq-")
    env = {**os.environ, "PROOFKIN_HOME": home, "PROOFKIN_SECRET_STORE": "file"}
    for var in ("PROOFKIN_TEST_WORKIQ_URL", "PROOFKIN_TEST_ENTRA_URL"):
        env.pop(var, None)

    def cli(*args, timeout=300, shown=False, quiet=False):
        """Runs a command; `shown` prints its output as it comes (a device code to enter), `quiet`
        prints none of it."""
        p = subprocess.Popen([EXE, *args], env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                             stderr=subprocess.STDOUT, text=True, encoding="utf-8", errors="replace")
        lines = []

        def read():
            for line in p.stdout:
                lines.append(line)
                if shown:
                    print(f"  {mask(line.rstrip())}", flush=True)
        reader = threading.Thread(target=read, daemon=True)
        reader.start()
        try:
            p.wait(timeout)
        except subprocess.TimeoutExpired:
            p.kill()
            p.wait()
        reader.join(5)
        out = "".join(lines)
        if not shown and not quiet:
            print("\n".join(f"  {mask(line)}" for line in out.strip().splitlines()), flush=True)
        return p.returncode, out

    agent = None
    try:
        print(f"== the profile 'owner': Copilot through the GitHub CLI, then Work IQ ({MODE})")
        code, out = cli("auth", "login", "copilot", "--profile", "owner", "--gh-user", account[0],
                        "--host", account[1])
        check("workiq: the owner signed in to Copilot for its worker", code == 0, out)
        if MODE == "cli":
            args = ["auth", "login", "workiq", "--cli", "--profile", "owner"]
            if os.environ.get("PROOFKIN_LIVE_WORKIQ_CLI_PATH"):
                args += ["--cli-path", os.environ["PROOFKIN_LIVE_WORKIQ_CLI_PATH"]]
            if os.environ.get("PROOFKIN_LIVE_WORKIQ_ACCOUNT"):
                args += ["--account", os.environ["PROOFKIN_LIVE_WORKIQ_ACCOUNT"]]
            code, out = cli(*args)
        else:
            code, out = cli("auth", "login", "workiq", "--tenant", os.environ["PROOFKIN_LIVE_WORKIQ_TENANT"],
                            "--client-id", os.environ["PROOFKIN_LIVE_WORKIQ_CLIENT_ID"], "--device-code",
                            "--profile", "owner", timeout=900, shown=True)
        check(f"workiq: signed in to Work IQ ({MODE})", code == 0 and "Work IQ signed in" in out, out)

        print("== the doctor: every check; the last is one small retrieve")
        code, out = cli("workiq", "doctor", "--profile", "owner")
        failed = [line.strip() for line in out.splitlines() if "FAILED" in line or "fix (" in line]
        check("workiq: the doctor passes every check, with one retrieve",
              code == 0 and "Work IQ is ready." in out, failed)

        print("== the agents Work IQ offers (only how many is shown)")
        code, out = cli("workiq", "agents", "--profile", "owner", quiet=True)
        listed = [] if "lists no agents" in out else [line for line in out.splitlines() if line.strip()]
        print(f"  {len(listed)} agent(s) listed", flush=True)
        check("workiq: proofkin workiq agents reads the agents list Work IQ gives",
              code == 0, out.strip().splitlines()[-1:] if code != 0 else "")

        print("== a digital worker for the owner, on its Copilot sign-in, started with it")
        cli("worker", "create", "helper", "--owner", "owner", "--providers", "copilot", "--start-with-owner")
        plan = os.path.join(home, "workiq-live.plan")
        with open(plan, "w", encoding="utf-8") as f:
            f.write(f"find @helper workiq=all {QUERY} Answer in two sentences, without quoting anything.\n"
                    "ask @workiq In one sentence: what is my next meeting about?\n")

        print("== the owner's agent runs the plan: the workiq= step on its worker's real Copilot")
        agent = Agent("owner", ["--network", "local", "--listen", "127.0.0.1:0"], env)
        started = agent.wait_for(r"^started digital worker helper", 60)
        connected = agent.wait_for(r"^\+ helper", 180)
        check("workiq: the owner's agent started its digital worker", started and connected,
              "\n".join(agent.lines[-20:]))
        agent.send(f"/plan {plan}")
        finished = agent.wait_for(r"^=== plan finished", 1500)
        failures = [line for line in agent.lines if re.match(r"^=== step \S+.* failed: ", line)]
        check("workiq: a workiq= step for the owner's own worker ran on real Copilot with what Work IQ found",
              any(done_header(line, "find", "helper") for line in agent.lines), failures or finished)
        check("workiq: an @workiq step got Microsoft 365 Copilot's answer",
              any(done_header(line, "ask", "workiq") for line in agent.lines), failures or finished)
        agent.stop()
        agent = None

        print("== the run's evidence")
        _, out = cli("runs", "list", "--profile", "owner")
        found = re.search(r"^\s+(\S+)\s", out, re.M)
        run_id = found.group(1) if found else "none"
        code, out = cli("runs", "verify", run_id, "--profile", "owner")
        verified = re.search(r"all (\d+) signed records verify", out)
        check("workiq: the run's signed evidence verifies",
              code == 0 and verified and int(verified.group(1)) >= 1, "no signed record to verify" if code == 0 else out)
        record_path = os.path.join(home, "profiles", "owner", "runs", run_id, "run.json")
        record = json.load(open(record_path, encoding="utf-8")) if os.path.exists(record_path) else {}
        kept = record.get("workiq", {}).get("find", {})
        signed = [c for found in values_of(record, "context") if isinstance(found, list) for c in found
                  if isinstance(c, dict) and c.get("source") == "workiq"]
        check("workiq: the step's signed evidence records the Work IQ context by its hash, not its content",
              kept.get("sha256") and [c.get("sha256") for c in signed] == [kept["sha256"]]
              and all(set(c) == {"source", "sha256", "results", "labels"} for c in signed),
              f"Work IQ kept {kept.get('kept')} result(s) and withheld {kept.get('withheld')}; "
              "if it kept none, set PROOFKIN_LIVE_WORKIQ_QUERY to something in your Microsoft 365")

        audit_path = os.path.join(home, "profiles", "owner", "workiq-audit.jsonl")
        entries = [json.loads(line) for line in open(audit_path, encoding="utf-8") if line.strip()] \
            if os.path.exists(audit_path) else []
        print(f"  audited calls: {sorted((e['tool'], e['purpose'], e['outcome']) for e in entries)}")
        check("workiq: it only read: every audited Work IQ call was a read tool",
              entries and all(e["tool"] in READS for e in entries), sorted({e["tool"] for e in entries}))
    finally:
        if agent:
            agent.stop()
        cli("auth", "logout", "workiq", "--profile", "owner")
        shutil.rmtree(home, ignore_errors=True)

    passed = sum(ok for _, ok in results)
    print(f"\n== {passed}/{len(results)} Work IQ checks passed")
    if os.environ.get("P2P_LIVE_RESULTS"):
        with open(os.environ["P2P_LIVE_RESULTS"], "w") as f:
            json.dump([{"check": name, "ok": ok} for name, ok in results], f, indent=1)
    sys.exit(0 if passed == len(results) else 1)


if __name__ == "__main__":
    main()
