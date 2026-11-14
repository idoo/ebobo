#!/usr/bin/env python3
"""Online saves end to end, without windows: N ebobo processes play through the real room
server and must agree on every save, load and state hash.

    scripts/netsave-e2e.py target/debug/ebobo game.nes 2
    scripts/netsave-e2e.py target/debug/ebobo game.nes 4 --relay-c

Phases (each a full host + guests run; the players' HOME dirs under --base live on between
them, so later phases continue from the files of phase A):
  A  new game; the host saves, a guest saves, the host loads; it runs past one minute, so the
     last-moment file is written, and every player leaves with the moment of leaving on disk
  B  the host continues from the save (--resume save)
  C  the guests are wiped; the host continues from the last moment (--resume last), relayed
     with --relay-c
  D  no flag: the default choice is the last moment
  E  --resume new: a fresh game
"""
import argparse, hashlib, os, re, shutil, subprocess, sys, threading

ROM = ""


def sha16(path):
    with open(path, "rb") as f:
        return hashlib.sha256(f.read()).hexdigest()[:16]


def state_files(home):
    out = {}
    for root, _, files in os.walk(home):
        if os.path.basename(root) != "netplay":
            continue
        for name in files:
            if name.endswith(".state"):
                out[name.split(".", 1)[1]] = sha16(os.path.join(root, name))
    return out


def run(binary, base, players, secs, host_args=(), guest_args=None, relay=False):
    env0 = dict(os.environ, SDL_VIDEODRIVER="dummy", SDL_AUDIODRIVER="dummy", SDL_RENDER_DRIVER="software")
    procs, logs = [], []

    def spawn(i, args):
        home = os.path.join(base, f"p{i}")
        os.makedirs(home, exist_ok=True)
        env = dict(env0, HOME=home, XDG_CONFIG_HOME=os.path.join(home, "cfg"), XDG_DATA_HOME=os.path.join(home, "data"))
        # The picture of the last frame tells why a lobby never started (who is not ready).
        shot = os.path.join(base, f"p{i}.png")
        cmd = [binary, *args, "--windowed", "--exit-after", str(secs if i == 0 else secs - 6), "--screenshot", shot, "--screenshot-size", "512x480"]
        if relay:
            cmd.append("--relay")
        p = subprocess.Popen(cmd, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        lines = []
        logs.append(lines)
        t = threading.Thread(target=lambda: lines.extend(p.stdout), daemon=True)
        procs.append((p, t))
        return p, lines, t

    host, hlines, ht = spawn(0, ["--host", ROM, "--autostart", str(players), "--name", "Host", *host_args])
    code = None
    for line in host.stdout:
        hlines.append(line)
        m = re.search(r"ebobo: room (\w+)", line)
        if m:
            code = m.group(1)
            break
    if not code:
        host.kill()
        return None, ["host printed no room code: " + "".join(hlines)[-400:]]
    ht.start()
    for i in range(1, players):
        extra = (guest_args or {}).get(i, ())
        _, _, t = spawn(i, ["--join", code, "--name", f"G{i}", *extra])
        t.start()
    for p, t in procs:
        try:
            p.wait(timeout=secs + 90)
        except subprocess.TimeoutExpired:
            p.kill()
        t.join(timeout=5)

    problems, res = [], []
    for i, lines in enumerate(logs):
        text = "".join(lines)
        rc = procs[i][0].returncode
        # NES controller of this process: where its peer id sits in `slots` (guests get their
        # ids in the order the server saw them, not in spawn order).
        game = re.search(r"game started slots=\[(.*?)\] me=PeerId\((\d+)\)", text)
        ids = re.findall(r"PeerId\((\d+)\)", game.group(1)) if game else []
        r = {
            "started": "game started" in text,
            "slot": str(ids.index(game.group(2))) if game and game.group(2) in ids else None,
            "frames": dict(re.findall(r"frame (\d+) hash ([0-9a-f]+)", text)),
            "saves": re.findall(r"net save by slot (\d+) sha ([0-9a-f]+)", text),
            "loads": re.findall(r"net load by slot (\d+)", text),
            "resumed": re.findall(r"resumed (\w+) sha ([0-9a-f]+)", text),
            "last": re.findall(r"last moment sha ([0-9a-f]+)", text),
            "left": re.findall(r"last moment on leave sha ([0-9a-f]+)", text),
            "files": state_files(os.path.join(base, f"p{i}")),
        }
        res.append(r)
        other = [l.strip() for l in lines if not re.search(r"frame \d+ hash|game started|input delay|update:|room |net save|net load|resumed|last moment", l)]
        print(f"  p{i}: rc={rc} frames={len(r['frames'])} saves={r['saves']} loads={r['loads']} resumed={r['resumed']} last={len(r['last'])} files={r['files']}")
        for l in other[:6]:
            print(f"     | {l}")
        if rc != 0 or "panicked" in text:
            problems.append(f"p{i} exit code {rc}")
        if not r["started"]:
            problems.append(f"p{i} never started the game (its last frame: {os.path.join(base, f'p{i}.png')})")
    common = set(res[0]["frames"])
    for r in res[1:]:
        common &= set(r["frames"])
    if len(common) < 3:
        problems.append(f"only {len(common)} common hash frames")
    for f in sorted(common, key=int):
        if len({r["frames"][f] for r in res}) != 1:
            problems.append(f"hash mismatch at frame {f}")
            break
    for key in ("saves", "loads", "resumed"):
        if any(r[key] != res[0][key] for r in res):
            problems.append(f"{key} differ between peers")
    for r in res[1:]:
        n = min(len(r["last"]), len(res[0]["last"]))
        if r["last"][:n] != res[0]["last"][:n]:
            problems.append("last-moment checkpoints differ between peers")
    # Only a phase that saved wrote the file everywhere (a guest wiped before it has none).
    if res[0]["saves"] and any(r["files"].get("save.state") != res[0]["files"].get("save.state") for r in res):
        problems.append("save files differ between peers")
    return res, problems


def main():
    global ROM
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("binary", help="the ebobo binary under test")
    ap.add_argument("rom", help="a .nes file every player starts with")
    ap.add_argument("players", type=int)
    ap.add_argument("--base", default="/tmp/ebobo-save-e2e")
    ap.add_argument("--long", type=int, default=110, help="seconds of phase A")
    ap.add_argument("--short", type=int, default=32)
    ap.add_argument("--phases", default="ABCDE")
    ap.add_argument("--relay-c", action="store_true")
    a = ap.parse_args()
    ROM = os.path.abspath(a.rom)
    ok = True

    def phase(name, expect, **kw):
        nonlocal ok
        print(f"=== phase {name}")
        res, problems = run(a.binary, a.base, a.players, **kw)
        if res:
            problems += expect(res)
        for p in problems:
            print(f"  FAIL: {p}")
        print(f"  {'PASS' if not problems else 'FAIL'}: phase {name}")
        ok &= not problems
        return res

    saved = last = None
    if "A" in a.phases:
        shutil.rmtree(a.base, ignore_errors=True)

        def expect_a(res):
            out = []
            r = res[0]
            want = ["0", res[1]["slot"]]
            if [s for s, _ in r["saves"]] != want:
                out.append(f"expected saves by slots {want} (the host, then the first guest), got {r['saves']}")
            if r["loads"] != ["0"]:
                out.append(f"expected one load by slot 0, got {r['loads']}")
            for i, p in enumerate(res):
                if len(p["left"]) != 1 or p["files"].get("last.state") != p["left"][0]:
                    out.append(f"p{i}: last-moment file is not the moment of leaving ({p['left']})")
            if r["resumed"]:
                out.append("a new game printed resumed")
            if r["saves"] and r["files"].get("save.state") != r["saves"][-1][1]:
                out.append("save file is not the last save")
            if not r["last"]:
                out.append("no last-moment file after a minute")
            return out

        res = phase("A", expect_a, secs=a.long, host_args=["--resume", "new", "--script", "700=save,1700=load"], guest_args={1: ["--script", "1200=save"]})
    host_files = state_files(os.path.join(a.base, "p0"))
    saved, last = host_files.get("save.state"), host_files.get("last.state")
    print(f"host files: save={saved} last={last}")

    def expect_resumed(kind, sha):
        def check(res):
            want = [(kind, sha)] if kind else []
            return [] if res[0]["resumed"] == want else [f"expected resumed {want}, got {res[0]['resumed']}"]
        return check

    if "B" in a.phases:
        phase("B", expect_resumed("save", saved), secs=a.short, host_args=["--resume", "save"])
    if "C" in a.phases:
        for i in range(1, a.players):
            shutil.rmtree(os.path.join(a.base, f"p{i}"), ignore_errors=True)
        phase("C", expect_resumed("last", last), secs=a.short, host_args=["--resume", "last"], relay=a.relay_c)
    if "D" in a.phases:
        phase("D", expect_resumed("last", last), secs=a.short)
    if "E" in a.phases:
        phase("E", expect_resumed(None, None), secs=a.short, host_args=["--resume", "new"])
    print("ALL PASS" if ok else "SOME FAILED")
    sys.exit(0 if ok else 1)


main()
