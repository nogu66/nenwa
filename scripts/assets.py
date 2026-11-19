#!/usr/bin/env python3
"""Draws the README diagrams into assets/ (a light and a dark version of each). The banner is a separate PNG.

    python3 scripts/assets.py

The images are plain SVG with system fonts, so GitHub shows them without loading anything.
"""
from html import escape
from pathlib import Path

SANS = "-apple-system, 'Segoe UI', Helvetica, Arial, sans-serif"
MONO = "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace"

THEMES = {
    "light": dict(bg="#faf9f5", fg="#141413", muted="#6b6a64", card="#f0eee6", line="#d5d2c5", term="#141413", term_fg="#faf9f5", term_muted="#b0aea5"),
    "dark": dict(bg="#141413", fg="#faf9f5", muted="#b0aea5", card="#1f1e1c", line="#3d3c37", term="#0c0c0b", term_fg="#faf9f5", term_muted="#8f8d85"),
}
ORANGE, BLUE, GREEN = "#d97757", "#6a9bcc", "#788c5d"


def text(x, y, s, size=16, fill="#000", weight=400, family=SANS, anchor="start"):
    return f'<text x="{x}" y="{y}" font-family="{family}" font-size="{size}" font-weight="{weight}" fill="{fill}" text-anchor="{anchor}">{escape(s)}</text>'


def rect(x, y, w, h, fill, stroke="none", r=10, sw=1.5, dash=""):
    d = f' stroke-dasharray="{dash}"' if dash else ""
    return f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="{r}" fill="{fill}" stroke="{stroke}" stroke-width="{sw}"{d}/>'


def arrow(x1, y1, x2, y2, color, dash=""):
    d = f' stroke-dasharray="{dash}"' if dash else ""
    return f'<line x1="{x1}" y1="{y1}" x2="{x2}" y2="{y2}" stroke="{color}" stroke-width="2"{d} marker-end="url(#a{color[1:]})"/>'


def svg(w, h, body, t, title, border=False):
    markers = "".join(
        f'<marker id="a{c[1:]}" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse"><path d="M0 0L10 5L0 10z" fill="{c}"/></marker>'
        for c in (ORANGE, BLUE, GREEN, t["muted"])
    )
    return (
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}" width="{w}" height="{h}" role="img" aria-label="{escape(title)}">'
        f"<title>{escape(title)}</title><defs>{markers}</defs>"
        f'{rect(1, 1, w - 2, h - 2, t["bg"], t["line"] if border else "none", r=24)}{"".join(body)}</svg>\n'
    )


def chip_w(s, size=15):
    return round(len(s) * size * 0.62) + 36


def chip(x, y, s, t, color, size=15):
    w = chip_w(s, size)
    return (
        rect(x, y, w, 34, t["card"], t["line"], r=17)
        + f'<circle cx="{x + 16}" cy="{y + 17}" r="4" fill="{color}"/>'
        + text(x + 26, y + 22, s, size, t["fg"], 500)
    ), w


def terminal(x, y, w, h, name, lines, t):
    out = [rect(x, y, w, h, t["term"], t["line"], r=12)]
    out += [f'<circle cx="{x + 20 + i * 16}" cy="{y + 20}" r="5" fill="{c}"/>' for i, c in enumerate((ORANGE, BLUE, GREEN))]
    out.append(text(x + w - 18, y + 25, name, 13, t["term_muted"], 500, anchor="end"))
    for i, (s, color) in enumerate(lines):
        out.append(text(x + 20, y + 62 + i * 26, s, 14, color or t["term_fg"], 400, MONO))
    return out


def machine(x, name, agents, daemon_left, t):
    """One computer: three agent boxes and its daemon. Returns the drawing and the daemon's (x, y, w, h)."""
    b = [rect(x, 60, 400, 340, "none", t["line"], r=16, dash="6 6"), text(x + 20, 92, name, 15, t["muted"], 600)]
    ax, dx = (x + 216, x + 24) if daemon_left else (x + 24, x + 250)
    dy, dw, dh = 116, 126, 164
    b.append(rect(dx, dy, dw, dh, t["card"], t["fg"], r=12))
    b.append(text(dx + dw / 2, dy + dh / 2 - 4, "nenwa", 18, t["fg"], 700, anchor="middle"))
    b.append(text(dx + dw / 2, dy + dh / 2 + 18, "daemon", 14, t["muted"], anchor="middle"))
    for i, a in enumerate(agents):
        y = 116 + i * 60
        b.append(rect(ax, y, 160, 44, t["card"], t["line"]))
        b.append(text(ax + 80, y + 28, a, 14, t["fg"], 500, anchor="middle"))
        near, far = (ax - 4, dx + dw + 4) if daemon_left else (ax + 164, dx - 4)
        b.append(arrow(near, y + 15, far, y + 15, ORANGE))
        b.append(arrow(far, y + 30, near, y + 30, BLUE))
    return b, (dx, dy, dw, dh)


def how_it_works(t):
    left, (lx, ly, lw, lh) = machine(40, "Your computer", ["Claude Code", "Codex", "Cursor, Pi, …"], False, t)
    right, (rx, _, _, _) = machine(560, "Another computer", ["Claude Code", "OpenCode", "Gemini CLI, …"], True, t)
    b = left + right
    mid = ly + lh / 2
    b.append(f'<line x1="{lx + lw + 4}" y1="{mid}" x2="{rx - 4}" y2="{mid}" stroke="{GREEN}" stroke-width="2.5" marker-start="url(#a{GREEN[1:]})" marker-end="url(#a{GREEN[1:]})"/>')
    b.append(text(500, mid - 14, "P2P", 14, GREEN, 700, anchor="middle"))
    b.append(text(500, mid + 24, "iroh, encrypted", 12, t["muted"], anchor="middle"))
    # the inbox, and the cloud agent it wakes
    ix, iy = lx, 322
    b.append(rect(ix, iy, lw, 44, t["card"], t["line"]))
    b.append(text(ix + lw / 2, iy + 28, "inbox: grok", 14, t["fg"], 500, anchor="middle"))
    b.append(f'<line x1="{ix + lw / 2}" y1="{ly + lh}" x2="{ix + lw / 2}" y2="{iy}" stroke="{t["muted"]}" stroke-width="2"/>')
    b.append(rect(ix - 37, 446, 200, 46, t["card"], t["line"], r=23))
    b.append(text(ix + lw / 2, 475, "Grok Bot (cloud)", 14, t["fg"], 500, anchor="middle"))
    b.append(arrow(ix + lw / 2, iy + 46, ix + lw / 2, 442, t["muted"], "5 5"))
    b.append(text(ix + lw / 2 + 14, 428, "webhook wakes it", 12, t["muted"]))
    for i, (c, s) in enumerate([(ORANGE, "send: the agent calls nenwa's MCP tools"), (BLUE, "deliver: hooks, plugins or the session socket"), (GREEN, "between computers: direct, joined by invite")]):
        y = 440 + i * 24
        b.append(f'<line x1="560" y1="{y}" x2="596" y2="{y}" stroke="{c}" stroke-width="3"/>')
        b.append(text(608, y + 5, s, 13, t["muted"]))
    return svg(1000, 520, b, t, "How nenwa works: agents send through MCP, a daemon on each computer delivers, computers connect peer to peer")


def agents(t):
    groups = [
        (ORANGE, "Wakes an idle conversation", "The message starts a turn by itself.", ["Claude Code", "Codex", "OpenCode", "Pi", "Grok Bot"]),
        (BLUE, "Arrives at the next step", "Delivered when the agent next runs a tool or stops.", ["Cursor", "Copilot CLI", "Gemini CLI", "Antigravity CLI", "Qwen Code"]),
        (GREEN, "Reads its inbox", "Delivered when it calls check_inbox.", ["Muse", "any MCP client"]),
    ]
    b, y = [], 40
    for color, title, sub, names in groups:
        b.append(f'<circle cx="52" cy="{y + 17}" r="7" fill="{color}"/>')
        b.append(text(72, y + 23, title, 18, t["fg"], 600))
        b.append(text(72, y + 48, sub, 13, t["muted"]))
        x, cy = 430, y
        for n in names:
            w = chip_w(n)
            if x + w > 960:
                x, cy = 430, cy + 44
            c, _ = chip(x, cy, n, t, color)
            b.append(c)
            x += w + 10
        y = max(cy + 34, y + 54) + 34
        if color != GREEN:
            b.append(f'<line x1="40" y1="{y - 17}" x2="960" y2="{y - 17}" stroke="{t["line"]}" stroke-width="1"/>')
    return svg(1000, y - 8, b, t, "Agents nenwa connects, grouped by how a message reaches them")


def main():
    out = Path(__file__).resolve().parent.parent / "assets"
    out.mkdir(exist_ok=True)
    for name, draw in (("how-it-works", how_it_works), ("agents", agents)):
        for theme, t in THEMES.items():
            (out / f"{name}-{theme}.svg").write_text(draw(t), encoding="utf-8")
            print(out / f"{name}-{theme}.svg")


if __name__ == "__main__":
    main()
