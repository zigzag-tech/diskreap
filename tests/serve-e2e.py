#!/usr/bin/env python3
"""End-to-end for `diskreap serve`: a throwaway $HOME, a fake Benchday daemon on the
ingress socket, and the real binary.

    tests/serve-e2e.py [path/to/diskreap]

The fake daemon speaks the documented ingress wire (4-byte big-endian length +
JSON; hello/serve/publish/intent_partial answered in order; write/write_result for
forwarded intents). It checks the journey the plugin drives: a scan with
instructions, refusals for the wrong writer and a stale selection, decisions with
comments that become rules, a dry run that touches nothing, a confirmed execute
that deletes regenerable data and archives the rest, and a second scan that
honours what was learned.
"""
import json, os, shutil, socket, struct, subprocess, sys, tempfile, threading, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = sys.argv[1] if len(sys.argv) > 1 else os.path.join(ROOT, "target/release/diskreap")
T = tempfile.mkdtemp(prefix="diskreap-serve-e2e-")
H = os.path.join(T, "home")
DAY = 86400
fails = []


def check(cond, what):
    print(("ok   " if cond else "FAIL ") + what)
    if not cond:
        fails.append(what)


def sh(*args, cwd=None):
    subprocess.run(args, cwd=cwd, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def big(path, size, age_days=0, alloc=False):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    if alloc:
        sh("fallocate", "-l", str(size), path)
    else:
        with open(path, "wb") as f:
            f.write(os.urandom(size))
    if age_days:
        t = time.time() - age_days * DAY
        os.utime(path, (t, t))


def age_tree(path, days):
    t = time.time() - days * DAY
    for d, dirs, files in os.walk(path, topdown=False):
        for n in files:
            os.utime(os.path.join(d, n), (t, t), follow_symlinks=False)
        os.utime(d, (t, t))


# ---- fixture -------------------------------------------------------------------
os.makedirs(H)
env = dict(os.environ, HOME=H, XDG_STATE_HOME=os.path.join(T, "xdg"), DISKREAP_STATE=os.path.join(T, "state"),
           TMPDIR=os.path.join(T, "tmp"), DISKREAP_TMP_ROOTS=os.path.join(T, "tmp"),
           GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@t", GIT_COMMITTER_NAME="t", GIT_COMMITTER_EMAIL="t@t")
os.environ.update(env)
os.makedirs(env["TMPDIR"])
for name in ("repo", "keep-me"):
    r = os.path.join(H, name)
    sh("git", "init", "-q", "-b", "main", r)
    open(os.path.join(r, ".gitignore"), "w").write("node_modules/\n")
    open(os.path.join(r, "package.json"), "w").write("{}")
    sh("git", "add", ".", cwd=r)
    sh("git", "commit", "-qm", "i", cwd=r)
    big(os.path.join(r, "node_modules/a/big"), 30 << 20)
    age_tree(r, 10)  # the project too: a project touched today counts as in use
# Personal data, cold for 200 days (cold needs 5 GB: one allocated file plus photos).
w = os.path.join(H, "wedding-photos")
big(os.path.join(w, "IMG_0000.HEIC"), (5 << 30) + (512 << 20), alloc=True)
for n in range(1, 12):
    big(os.path.join(w, f"IMG_{n:04}.HEIC"), 4096)
age_tree(w, 200)
# A protected archive (>= 1 GiB, so the scanner reports it as protected).
big(os.path.join(H, "histo-reader-archive/round1.tar"), (1 << 30) + (64 << 20), alloc=True)
# Dated backups: the newest 3 are kept by diskreap; older ones are pressure-tier `backup`.
for i, day in enumerate(["20260101", "20260201", "20260301", "20260401", "20260501"]):
    big(os.path.join(H, "db-backups", f"app-{day}-0300.dump"), 8 << 20, age_days=200 - i * 30)
os.makedirs(os.path.join(H, ".config/diskreap"))
remote = os.path.join(T, "remote")
script = os.path.join(T, "fake-offload.sh")
open(script, "w").write('#!/bin/sh\nset -e\nmkdir -p "$2"\ncp -a "$1"/. "$2"/ 2>/dev/null || cp -a "$1" "$2"/\n'
                        'echo "$1 -> $2" >>"$3"\nrm -rf "$1"\n')
os.chmod(script, 0o755)
json.dump({"protect_paths": ["~/histo-reader-archive"], "llm": False,
           "archive": {"dest": remote, "script": script, "log": os.path.join(T, "archive.log")}},
          open(os.path.join(H, ".config/diskreap/config.json"), "w"))

# ---- fake daemon ---------------------------------------------------------------
sock_dir = os.path.join(env["XDG_STATE_HOME"], "benchday-daemon")
os.makedirs(sock_dir)
sock_path = os.path.join(sock_dir, "streams-ingress.sock")
srv = socket.socket(socket.AF_UNIX)
srv.bind(sock_path)
srv.listen(1)

lock = threading.Lock()
served, published, intents, answers = [], {}, {}, {}
conn_box = {}


def send(c, obj):
    b = json.dumps(obj).encode()
    c.sendall(struct.pack(">I", len(b)) + b)


def recv(c):
    hdr = b""
    while len(hdr) < 4:
        chunk = c.recv(4 - len(hdr))
        if not chunk:
            raise EOFError
        hdr += chunk
    (n,) = struct.unpack(">I", hdr)
    body = b""
    while len(body) < n:
        chunk = c.recv(n - len(body))
        if not chunk:
            raise EOFError
        body += chunk
    return json.loads(body)


def daemon():
    c, _ = srv.accept()
    conn_box["c"] = c
    epoch = 100
    try:
        while True:
            f = recv(c)
            t = f["type"]
            with lock:
                if t == "hello":
                    send(c, {"type": "hello_ok", "daemon": "testbox", "leases": []})
                elif t == "serve":
                    epoch += 1
                    served.append(f["contract"])
                    send(c, {"type": "serving", "contract": f["contract"], "epoch": epoch})
                elif t == "publish":
                    key = (f["contract"], json.dumps(f["subject"], sort_keys=True))
                    published[key] = f["body"]
                    send(c, {"type": "published", "contract": f["contract"], "key": key[1], "epoch": "1", "seq": "1", "leased": True, "delivered": 1})
                elif t == "intent_partial":
                    p = f["partial"]
                    assert p["status"] == "ok" and p["ttl_ms"] == 0 and p["subject"]["intent_id"] == p["body"]["intent_id"]
                    intents[p["body"]["intent_id"]] = p["body"]
                    send(c, {"type": "intent_published", "contract": f["contract"], "leased": True, "delivered": 1, "epoch": p["epoch"], "seq": p["seq"]})
                elif t == "write_result":
                    answers[f["req"]] = f["partial"]["body"]
                    intents.setdefault(f["partial"]["body"]["intent_id"], f["partial"]["body"])
                else:
                    send(c, {"type": "error", "code": "schema_invalid", "detail": "unknown_frame_type"})
    except EOFError:
        pass


threading.Thread(target=daemon, daemon=True).start()
proc = subprocess.Popen([BIN, "serve"], env=env, stderr=open(os.path.join(T, "serve.log"), "w"))
req_n = [0]


def wait(pred, secs=180, what=""):
    end = time.time() + secs
    while time.time() < end:
        with lock:
            v = pred()
        if v:
            return v
        time.sleep(0.2)
    return None


def write(contract, intent_id, body, principal="user:acct", op="submit"):
    req_n[0] += 1
    req = f"r{req_n[0]}"
    with lock:
        send(conn_box["c"], {"type": "write", "req": req, "contract": contract, "op": op, "intent_id": intent_id,
                             "principal": principal, "payload": {"body": body}})
    return wait(lambda: answers.get(req), 20)


def cands(run):
    return {b["path"]: b for (c, _), b in published.items() if c == "diskreap.candidate/1" and b.get("run") == run and b.get("state") == "present"}


try:
    check(wait(lambda: len(served) == 7, 20) is not None, "serves 4 intent + 3 state contracts")
    with lock:
        for contract in ("diskreap.host/1", "diskreap.candidate/1", "diskreap.policy/1"):
            send(conn_box["c"], {"type": "lease", "contract": contract, "live": True})
    host = wait(lambda: published.get(("diskreap.host/1", json.dumps({"machine": "testbox"}))), 20)
    check(bool(host) and host["archive"] == "configured" and host["total_bytes"] > 0, "host row published on lease")
    rules = wait(lambda: [b for (c, _), b in published.items() if c == "diskreap.policy/1" and b.get("origin") == "config"], 10)
    check(bool(rules) and rules[0]["match"]["path_glob"] == "~/histo-reader-archive", "protect_paths shown as a read-only config rule")

    a = write("diskreap.scan/1", "s0", {"intent_id": "s0", "machine": "elsewhere"})
    check(a and a["status"] == "refused" and a["refusal"] == "wrong_machine", "a scan for another machine is refused")
    a = write("diskreap.scan/1", "s1", {"intent_id": "s1", "machine": "testbox", "full": True, "origin": "agent",
                                        "instructions": {"text": "never touch ~/keep-me; older than 3 days; also polish the silverware"}},
              principal="agent:a_X")
    check(a and a["status"] == "submitted", "an agent may start a read-only scan")
    done = wait(lambda: intents.get("s1") if intents.get("s1", {}).get("status") in ("proposed", "failed") else None, 300)
    check(bool(done) and done["status"] == "proposed", f"scan reaches proposed ({(done or {}).get('detail', '')})")
    done = done or {}
    check(done.get("interpreted", {}).get("min_age_days") == 3 and f"~/keep-me" in done.get("interpreted", {}).get("exclude_globs", []),
          "instructions interpreted: min age and exclusion")
    check(done.get("unparsed") == ["also polish the silverware"] and done.get("interpreter") == "rules", "unparsed clause and interpreter named")
    cs = wait(lambda: cands("s1") or None, 20) or {}
    nm = cs.get(os.path.join(H, "repo/node_modules"))
    check(bool(nm) and nm["tier"] == "safe" and nm["action"] == "delete" and nm["regenerable"], "node_modules: safe, delete")
    if not nm:
        plan = json.load(open(os.path.join(env["DISKREAP_STATE"], "plan.json")))
        print("  plan items:", [(i["cat"], i["path"].replace(H, "~"), i["ok"], i["why"]) for i in plan["items"]][:20], file=sys.stderr)
        print("  candidates:", sorted(p.replace(H, "~") for p in cs), file=sys.stderr)
    check(not any(p.startswith(os.path.join(H, "keep-me")) for p in cs), "nothing under the excluded ~/keep-me")
    wed = cs.get(w)
    check(bool(wed) and wed["personal"] and wed["action"] == "keep" and wed["tier"] == "human", "wedding photos: personal, human, keep")
    hra = cs.get(os.path.join(H, "histo-reader-archive"))
    check(hra is None or hra["tier"] == "protected", "histo-reader-archive is never offered for action")
    old = [b for p, b in cs.items() if b["category"] == "backup"]
    check(len(old) == 2 and all(b["action"] == "archive" and not b["regenerable"] for b in old), "old backups: archive (not regenerable)")

    items = [{"candidate": nm["candidate"], "action": "delete", "comment": "rebuilds in a minute"}] + \
            [{"candidate": b["candidate"], "action": "archive"} for b in old] + \
            [{"candidate": wed["candidate"], "action": "keep", "remember": "path", "comment": "our wedding, keep forever"}]
    a = write("diskreap.decision/1", "d0", {"intent_id": "d0", "machine": "testbox", "run": "s1", "items": items}, principal="agent:a_X")
    check(a and a["refusal"] == "writer_denied", "an agent cannot record decisions")
    a = write("diskreap.decision/1", "d1", {"intent_id": "d1", "machine": "testbox", "run": "s1", "items": items})
    check(a and a["status"] == "recorded" and a["recorded"] == 4 and a["rules_created"] >= 1, "decisions recorded, a rule created")
    rule = wait(lambda: [b for (c, _), b in published.items() if c == "diskreap.policy/1" and b.get("kind") == "protect" and b.get("origin") == "user"], 10)
    check(bool(rule) and rule[0]["match"]["path_glob"] == w and rule[0]["note"] == "our wedding, keep forever", "remembered keep is an enabled protect rule with the comment")
    sel = wait(lambda: (published.get(("diskreap.host/1", json.dumps({"machine": "testbox"}))) or {}).get("selection"), 10) or {}
    check(sel.get("count") == 3 and sel.get("run") == "s1", "host row carries the 3-item selection")

    a = write("diskreap.execute/1", "e0", {"intent_id": "e0", "machine": "testbox", "run": "s1", "expect": {"count": 3, "bytes": sel.get("bytes", 0), "digest": "sha256:" + "0" * 64}})
    check(a and a["refusal"] == "selection_changed", "a stale digest is refused")
    a = write("diskreap.execute/1", "e1a", {"intent_id": "e1a", "machine": "testbox", "run": "s1", "dry_run": True,
                                            "expect": {k: sel.get(k) for k in ("count", "bytes", "digest")}}, principal="agent:a_X")
    check(a and a["refusal"] == "writer_denied", "an agent cannot execute")
    a = write("diskreap.execute/1", "e1", {"intent_id": "e1", "machine": "testbox", "run": "s1", "dry_run": True,
                                           "expect": {k: sel.get(k) for k in ("count", "bytes", "digest")}})
    check(a and a["status"] == "submitted", "dry-run execute accepted")
    e = wait(lambda: intents.get("e1") if intents.get("e1", {}).get("status") in ("done", "failed") else None, 120) or {}
    check(e.get("status") == "done" and e.get("skipped") == 3 and e.get("failed") == 0, f"dry run: 3 skipped, none failed ({e})")
    check(os.path.exists(os.path.join(H, "repo/node_modules")) and len(os.listdir(os.path.join(H, "db-backups"))) == 5, "dry run touched nothing")

    a = write("diskreap.execute/1", "e2", {"intent_id": "e2", "machine": "testbox", "run": "s1",
                                           "expect": {k: sel.get(k) for k in ("count", "bytes", "digest")}})
    e = wait(lambda: intents.get("e2") if intents.get("e2", {}).get("status") in ("done", "failed") else None, 120) or {}
    check(e.get("status") == "done" and e.get("done") == 3, f"execute: 3 done ({e})")
    check(not os.path.exists(os.path.join(H, "repo/node_modules")), "node_modules deleted")
    check(len(os.listdir(os.path.join(H, "db-backups"))) == 3 and os.path.isdir(remote), "old backups archived, then removed")
    check(os.path.exists(os.path.join(w, "IMG_0000.HEIC")) and os.path.exists(os.path.join(H, "histo-reader-archive/round1.tar")),
          "personal data and the protected archive survive")
    check(e.get("avail_before", 0) > 0 and e.get("avail_after", 0) > 0, "before/after free space reported")

    a = write("diskreap.scan/1", "s2", {"intent_id": "s2", "machine": "testbox", "full": True})
    wait(lambda: intents.get("s2") if intents.get("s2", {}).get("status") == "proposed" else None, 300)
    cs2 = cands("s2")
    check(w not in cs2 or cs2[w]["tier"] == "protected", "the remembered protect rule makes the photos protected in the next scan")
    check(json.dumps(published.get(("diskreap.candidate/1", json.dumps({"candidate": nm["candidate"], "run": "s1"}, sort_keys=True)), {})).find('"removed"') >= 0,
          "the previous run's candidates are tombstoned")
finally:
    proc.kill()
    print(open(os.path.join(T, "serve.log")).read()[-2000:], file=sys.stderr) if fails else None
    subprocess.run(["chmod", "-R", "u+w", T])
    shutil.rmtree(T, ignore_errors=True)

print(f"\n{'FAILED: ' + str(len(fails)) if fails else 'all passed'}")
sys.exit(1 if fails else 0)
