#!/usr/bin/env python3
"""Rooms end to end, without windows: ebobo processes play through the real room server.

    scripts/netroom-e2e.py target/debug/ebobo game.nes [--phases SJRHO] [--base DIR]

Phases (each a fresh set of processes):
  S  two devices with two players each: the game starts with controllers [0,0,1,1]
  J  a third device joins a running two-player game: everyone goes back to the room and the
     game continues, from the host's state, with three players
  R  a guest quits a running game and joins again with the same code: the game continues
  H  saves for the host only: the host's save counts, the guest's does not
  O  saves off: nobody's save counts and no file is written
A phase passes when every process ran its games, exited with 0, and the processes of the last
game printed the same state hashes.
"""
import argparse, os, re, shutil, subprocess, sys, threading, time

ENV = dict(os.environ, SDL_VIDEODRIVER="dummy", SDL_AUDIODRIVER="dummy", SDL_RENDER_DRIVER="software")


class Proc:
    def __init__(self, binary, base, tag, args, secs):
        home = os.path.join(base, tag)
        os.makedirs(home, exist_ok=True)
        env = dict(ENV, HOME=home, XDG_CONFIG_HOME=os.path.join(home, "cfg"), XDG_DATA_HOME=os.path.join(home, "data"))
        self.tag, self.home, self.lines = tag, home, []
        self.shot = os.path.join(base, tag + ".png")
        cmd = [binary, *args, "--windowed", "--exit-after", str(secs), "--screenshot", self.shot, "--screenshot-size", "512x480"]
        self.p = subprocess.Popen(cmd, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        threading.Thread(target=lambda: self.lines.extend(self.p.stdout), daemon=True).start()

    def text(self):
        return "".join(self.lines)

    def wait_for(self, pattern, secs, count=1):
        end = time.time() + secs
        while time.time() < end:
            found = re.findall(pattern, self.text())
            if len(found) >= count:
                return found
            if self.p.poll() is not None:
                break
            time.sleep(0.2)
        return None

    def games(self):
        """One entry per game this process played: its controllers and its frame hashes."""
        out = []
        for part in re.split(r"(?=ebobo: game started)", self.text())[1:]:
            slots = re.search(r"game started slots=\[(.*?)\]", part)
            out.append({
                "slots": [int(x) for x in re.findall(r"PeerId\((\d+)\)", slots.group(1))] if slots else [],
                "frames": dict(re.findall(r"frame (\d+) hash ([0-9a-f]+)", part)),
                "saves": re.findall(r"net save by slot (\d+)", part),
                "resumed": "ebobo: resumed" in part,
            })
        return out

    def state_files(self):
        found = []
        for root, _, files in os.walk(self.home):
            if os.path.basename(root) == "netplay":
                found += [f for f in files if f.endswith(".state")]
        return found


def finish(procs, secs):
    problems = []
    for pr in procs:
        try:
            pr.p.wait(timeout=secs)
        except subprocess.TimeoutExpired:
            pr.p.kill()
            problems.append(f"{pr.tag} did not exit")
    time.sleep(0.5)
    for pr in procs:
        if pr.p.returncode != 0 or "panicked" in pr.text():
            problems.append(f"{pr.tag} exit code {pr.p.returncode}")
        other = [l.strip() for l in pr.lines if not re.search(r"frame \d+ hash|input delay|update:|gamepad", l)]
        print(f"  {pr.tag}: rc={pr.p.returncode} games={[(g['slots'], len(g['frames']), g['saves']) for g in pr.games()]}")
        for l in other[:14]:
            print(f"     | {l[:150]}")
    return problems


def same_hashes(procs, problems, least=2):
    last = [pr.games()[-1]["frames"] if pr.games() else {} for pr in procs]
    common = set(last[0])
    for f in last[1:]:
        common &= set(f)
    if len(common) < least:
        problems.append(f"only {len(common)} common hash frames in the last game")
    for frame in sorted(common, key=int):
        if len({f[frame] for f in last}) != 1:
            problems.append(f"frame {frame}: hashes differ")
            break


def host(binary, base, rom, args, secs):
    h = Proc(binary, base, "host", ["--host", rom, "--name", "Host", *args], secs)
    code = h.wait_for(r"ebobo: room (\w+)", 30)
    return h, (code[0] if code else None)


def phase_seats(binary, base, rom):
    h, code = host(binary, base, rom, ["--autostart", "2", "--seats", "2"], 40)
    if not code:
        return finish([h], 5) + ["no room code"]
    g = Proc(binary, base, "g1", ["--join", code, "--name", "G1", "--seats", "2"], 32)
    problems = finish([h, g], 100)
    for pr in (h, g):
        slots = pr.games()[-1]["slots"] if pr.games() else None
        if slots != [0, 0, 1, 1]:
            problems.append(f"{pr.tag}: controllers {slots}, expected [0, 0, 1, 1] (its last frame: {pr.shot})")
    same_hashes([h, g], problems)
    return problems


def phase_join(binary, base, rom):
    h, code = host(binary, base, rom, ["--autostart", "2"], 75)
    if not code:
        return finish([h], 5) + ["no room code"]
    g1 = Proc(binary, base, "g1", ["--join", code, "--name", "G1"], 68)
    if not h.wait_for(r"frame 300 hash", 40):
        return finish([h, g1], 5) + ["the two-player game did not run"]
    g2 = Proc(binary, base, "g2", ["--join", code, "--name", "G2"], 45)
    problems = finish([h, g1, g2], 140)
    for pr in (h, g1):
        if "back to the room" not in pr.text():
            problems.append(f"{pr.tag} never went back to the room")
        if len(pr.games()) != 2:
            problems.append(f"{pr.tag} played {len(pr.games())} games, expected 2 (its last frame: {pr.shot})")
    for pr in (h, g1, g2):
        last = pr.games()[-1] if pr.games() else None
        if not last or len(set(last["slots"])) != 3:
            problems.append(f"{pr.tag}: the last game has controllers {last and last['slots']}, expected three devices")
        elif not last["resumed"]:
            problems.append(f"{pr.tag}: the three-player game did not start from the host's state")
    same_hashes([h, g1, g2], problems)
    return problems


def phase_return(binary, base, rom):
    h, code = host(binary, base, rom, ["--autostart", "2"], 80)
    if not code:
        return finish([h], 5) + ["no room code"]
    g1 = Proc(binary, base, "g1", ["--join", code, "--name", "G1"], 16)
    if not h.wait_for(r"frame 300 hash", 40):
        return finish([h, g1], 5) + ["the two-player game did not run"]
    g1.p.wait(timeout=60)
    time.sleep(3)
    back = Proc(binary, base, "g1", ["--join", code, "--name", "G1"], 40)
    problems = finish([h, back], 140)
    if len(h.games()) != 2:
        problems.append(f"host played {len(h.games())} games, expected 2 (its last frame: {h.shot})")
    last = back.games()[-1] if back.games() else None
    if not last or not last["resumed"]:
        problems.append(f"the returning player did not continue the host's game (its last frame: {back.shot})")
    same_hashes([h, back], problems)
    return problems


def phase_saves(binary, base, rom, policy):
    h, code = host(binary, base, rom, ["--autostart", "2", "--saves", policy, "--resume", "new", "--script", "400=save"], 45)
    if not code:
        return finish([h], 5) + ["no room code"]
    g = Proc(binary, base, "g1", ["--join", code, "--name", "G1", "--script", "700=save"], 37)
    problems = finish([h, g], 110)
    want = ["0"] if policy == "host" else []
    for pr in (h, g):
        saves = pr.games()[-1]["saves"] if pr.games() else None
        if saves != want:
            problems.append(f"{pr.tag}: saves by controllers {saves}, expected {want}")
        if policy == "off" and pr.state_files():
            problems.append(f"{pr.tag}: files written with saves off: {pr.state_files()}")
    same_hashes([h, g], problems)
    return problems


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("binary")
    ap.add_argument("rom")
    ap.add_argument("--phases", default="SJRHO")
    ap.add_argument("--base", default="/tmp/ebobo-netroom")
    a = ap.parse_args()
    phases = {
        "S": ("two players on each of two devices", lambda b: phase_seats(a.binary, b, a.rom)),
        "J": ("a third device joins a running game", lambda b: phase_join(a.binary, b, a.rom)),
        "R": ("a guest quits and comes back", lambda b: phase_return(a.binary, b, a.rom)),
        "H": ("saves for the host only", lambda b: phase_saves(a.binary, b, a.rom, "host")),
        "O": ("saves off", lambda b: phase_saves(a.binary, b, a.rom, "off")),
    }
    failed = False
    for key in a.phases:
        title, run = phases[key]
        base = os.path.join(a.base, key)
        shutil.rmtree(base, ignore_errors=True)
        print(f"== {key}: {title}")
        problems = run(base)
        for p in problems:
            print(f"  PROBLEM: {p}")
        print(f"  {'FAILED' if problems else 'ok'}")
        failed |= bool(problems)
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
