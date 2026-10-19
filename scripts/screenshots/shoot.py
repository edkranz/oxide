"""Renders the README screenshots into assets/screenshots (macOS).

    cargo build --release && python3 scripts/screenshots/shoot.py

Each scene in scenes.json opens a sandboxed oxide (its own session and settings) on files
from this repo, picks menu items through System Events, and saves the last frame oxide
drew (OXIDE_SNAPSHOT) inside a window frame. Clicking menus needs Accessibility access for
your terminal. Close any running oxide first: the sandbox must be the primary instance.
"""
import json, os, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
OUT = os.path.join(REPO, "assets", "screenshots")


def offset(text, line, col=0):
    """Char offset of 1-based `line` and 0-based `col`."""
    return sum(len(l) + 1 for l in text.split("\n")[: line - 1]) + col


def session_tab(t):
    path = os.path.join(REPO, t["file"])
    text = open(path).read()
    if "word" in t:
        col = text.split("\n")[t["line"] - 1].index(t["word"])
        anchor = offset(text, t["line"], col)
        head = anchor + len(t["word"])
    else:
        anchor = head = offset(text, t.get("line", 1), t.get("col", 0))
    return {"path": path, "text": None, "anchor": anchor, "head": head, "scroll": [0.0, float(t.get("top", 0))]}


def click(pid, menu, item):
    subprocess.run(["osascript", "-e", f'''tell application "System Events" to tell (first process whose unix id is {pid})
        set frontmost to true
        click menu item "{item}" of menu 1 of menu bar item "{menu}" of menu bar 1
    end tell'''], check=True, capture_output=True)


def shoot(name, scene, frame_tool):
    with tempfile.TemporaryDirectory() as tmp:
        data, config = os.path.join(tmp, "data"), os.path.join(tmp, "config")
        os.makedirs(data)
        os.makedirs(os.path.join(config, "oxide"))
        session = {"windows": [{"tabs": [session_tab(t) for t in scene["tabs"]], "active": 0}]}
        json.dump(session, open(os.path.join(data, "session.json"), "w"))
        open(os.path.join(config, "oxide", "settings.toml"), "w").write(
            f'theme = "{scene.get("theme", "base16-ocean.dark")}"\ncaret_blink = false\n')
        frame = os.path.join(tmp, "frame.bmp")
        env = dict(os.environ, OXIDE_FOREGROUND="1", OXIDE_DATA_DIR=data, XDG_CONFIG_HOME=config,
                   OXIDE_SNAPSHOT=frame)
        proc = subprocess.Popen([os.path.join(REPO, "target/release/oxide")], env=env)
        try:
            time.sleep(2.5)
            for action in scene.get("menu", []):
                click(proc.pid, *action.split(":"))
                time.sleep(0.6)
            time.sleep(0.8)
        finally:
            proc.terminate()
            proc.wait()
        raw = os.path.join(tmp, "frame.png")
        subprocess.run(["sips", "-s", "format", "png", frame, "--out", raw], check=True, capture_output=True)
        title = os.path.basename(scene["tabs"][0]["file"])
        out = os.path.join(OUT, name + ".png")
        subprocess.run([frame_tool, raw, title, out], check=True)
        print(out)


def main():
    scenes = json.load(open(os.path.join(HERE, "scenes.json")))
    wanted = sys.argv[1:] or list(scenes)
    frame_tool = os.path.join(REPO, "target", "screenshot-frame")
    subprocess.run(["swiftc", "-O", os.path.join(HERE, "frame.swift"), "-o", frame_tool], check=True)
    os.makedirs(OUT, exist_ok=True)
    for name in wanted:
        shoot(name, scenes[name], frame_tool)


main()
