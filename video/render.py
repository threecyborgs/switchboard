#!/usr/bin/env python3
"""Render a terminal-demo explainer video from a JSON storyboard.

    python3 video/render.py storyboard.json out.mp4 [--preview-frames DIR] [--scenes 1,3-4]

Frames are composed with Pillow and streamed as raw RGB into ffmpeg (H.264, yuv420p,
crf 20, +faststart). See video/sample.json for every scene and step type.

Scene types: title, card, diagram, terminal (layouts single / split2 / split3).
Terminal steps: cmd, output, caption, wait, clear, highlight.
"""
import argparse
import json
import math
import os
import re
import subprocess
import sys
import time
import unicodedata
from functools import lru_cache

import numpy as np
from PIL import Image, ImageDraw, ImageFilter, ImageFont

try:
    from fontTools.ttLib import TTCollection, TTFont
except ImportError:  # coverage checks degrade to "Menlo has everything"
    TTCollection = TTFont = None

# --------------------------------------------------------------------------- fonts

FONT_DIR = "/System/Library/Fonts"
MENLO = os.path.join(FONT_DIR, "Menlo.ttc")
SFNS = os.path.join(FONT_DIR, "SFNS.ttf")
HELVETICA = os.path.join(FONT_DIR, "Helvetica.ttc")
FALLBACK_MONO = [os.path.join(FONT_DIR, "SFNSMono.ttf"), os.path.join(FONT_DIR, "Apple Symbols.ttf")]
EMOJI = os.path.join(FONT_DIR, "Apple Color Emoji.ttc")
FFMPEG = "/opt/local/bin/ffmpeg" if os.path.exists("/opt/local/bin/ffmpeg") else "ffmpeg"


@lru_cache(maxsize=None)
def ui_font(size, weight="Regular"):
    """SF Pro (variable) at a named weight; Helvetica as a fallback."""
    size = max(6, int(round(size)))
    if os.path.exists(SFNS):
        f = ImageFont.truetype(SFNS, size)
        try:
            f.set_variation_by_name(weight)
        except Exception:
            pass
        return f
    bold = weight in ("Semibold", "Bold", "Heavy", "Black")
    return ImageFont.truetype(HELVETICA, size, index=1 if bold else 0)


@lru_cache(maxsize=None)
def _cmap(path, index=0):
    if TTFont is None:
        return None
    try:
        if path.endswith(".ttc"):
            return frozenset(TTCollection(path).fonts[index].getBestCmap().keys())
        return frozenset(TTFont(path).getBestCmap().keys())
    except Exception:
        return frozenset()


class Mono:
    """Menlo at one size, monospaced cell grid, with per-glyph fallbacks."""

    def __init__(self, size):
        self.size = size
        self.reg = ImageFont.truetype(MENLO, size, index=0)
        self.bold = ImageFont.truetype(MENLO, size, index=1)
        self.cw = self.reg.getlength("M")
        self.lh = int(round(size * 1.34))
        asc, desc = self.reg.getmetrics()
        self.baseline = int(round((self.lh - (asc + desc)) / 2 + asc))
        self.menlo_cmap = _cmap(MENLO, 0)
        self.fallbacks = [(p, _cmap(p)) for p in FALLBACK_MONO if os.path.exists(p)]
        self.emoji_cmap = _cmap(EMOJI, 0) if os.path.exists(EMOJI) else frozenset()
        self._resolved = {}
        self._emoji = {}

    def resolve(self, ch):
        """-> 'menlo' | ImageFont | 'emoji' | None"""
        r = self._resolved.get(ch)
        if r is not None or ch in self._resolved:
            return r
        cp = ord(ch)
        if self.menlo_cmap is None or cp in self.menlo_cmap or cp < 128:
            r = "menlo"
        else:
            r = None
            for path, cm in self.fallbacks:
                if cm and cp in cm:
                    r = ImageFont.truetype(path, self.size)
                    break
            if r is None and cp in self.emoji_cmap:
                r = "emoji"
        self._resolved[ch] = r
        return r

    def emoji_sprite(self, ch, cells):
        key = (ch, cells)
        if key not in self._emoji:
            spr = None
            try:
                f = ImageFont.truetype(EMOJI, 64)
                im = Image.new("RGBA", (80, 80), (0, 0, 0, 0))
                ImageDraw.Draw(im).text((0, 0), ch, font=f, embedded_color=True)
                bb = im.getbbox()
                if bb:
                    im = im.crop(bb)
                    side = int(min(self.lh * 0.82, self.cw * cells))
                    spr = im.resize((side, side), Image.LANCZOS)
            except Exception:
                spr = None
            self._emoji[key] = spr
        return self._emoji[key]


def char_width(ch):
    if unicodedata.combining(ch):
        return 0
    return 2 if unicodedata.east_asian_width(ch) in ("W", "F") else 1


# --------------------------------------------------------------------------- colours

def hexrgb(h):
    h = h.lstrip("#")
    return tuple(int(h[i:i + 2], 16) for i in (0, 2, 4))


def mix(a, b, t):
    return tuple(int(round(a[i] + (b[i] - a[i]) * t)) for i in range(3))


TERM_BG = hexrgb("#0d1117")
TERM_FG = hexrgb("#e6edf3")
TITLEBAR = hexrgb("#161b22")
BORDER = hexrgb("#30363d")
ACCENT = hexrgb("#58a6ff")
ACCENT2 = hexrgb("#79c0ff")
PROMPT = hexrgb("#3fb950")
MUTED = hexrgb("#8b949e")
WHITE = (255, 255, 255)

ANSI = {
    30: "#6e7681", 31: "#ff7b72", 32: "#3fb950", 33: "#d29922", 34: "#58a6ff",
    35: "#bc8cff", 36: "#39c5cf", 37: "#b1bac4",
    90: "#8b949e", 91: "#ffa198", 92: "#56d364", 93: "#e3b341", 94: "#79c0ff",
    95: "#d2a8ff", 96: "#56d4dd", 97: "#f0f6fc",
}
ANSI = {k: hexrgb(v) for k, v in ANSI.items()}

# style = (fg rgb or None, bold, dim)
DEFAULT_STYLE = (None, False, False)


def style_color(style):
    fg, bold, dim = style
    c = fg or TERM_FG
    if dim:
        c = mix(c, TERM_BG, 0.45)
    return c


ESC_RE = re.compile(r"\x1b\[([0-9;?]*)([@-~])|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[@-_]?")


def parse_ansi(text, style=DEFAULT_STYLE):
    """Text with SGR codes -> list of lines, each a list of (str, style)."""
    text = text.replace("\r\n", "\n")
    lines, cur, col = [], [], 0
    fg, bold, dim = style

    def emit(s):
        nonlocal col
        if not s:
            return
        out = []
        for ch in s:
            if ch == "\t":
                n = 8 - (col % 8)
                out.append(" " * n)
                col += n
            elif ch == "\r":
                continue
            elif ord(ch) < 32 or ord(ch) == 127:
                continue
            else:
                out.append(ch)
                col += char_width(ch)
        s = "".join(out)
        if s:
            cur.append((s, (fg, bold, dim)))

    def text_part(s):
        nonlocal cur, col
        parts = s.split("\n")
        for i, p in enumerate(parts):
            if i:
                lines.append(cur)
                cur, col = [], 0
            emit(p)

    pos = 0
    for m in ESC_RE.finditer(text):
        text_part(text[pos:m.start()])
        pos = m.end()
        if m.group(2) == "m":
            params = [p for p in (m.group(1) or "").split(";")]
            codes = []
            for p in params:
                try:
                    codes.append(int(p) if p else 0)
                except ValueError:
                    pass
            if not codes:
                codes = [0]
            i = 0
            while i < len(codes):
                c = codes[i]
                if c == 0:
                    fg, bold, dim = None, False, False
                elif c == 1:
                    bold = True
                elif c == 2:
                    dim = True
                elif c == 22:
                    bold = dim = False
                elif c == 39:
                    fg = None
                elif c in ANSI:
                    fg = ANSI[c]
                elif c in (38, 48):  # skip extended colour params
                    if i + 1 < len(codes) and codes[i + 1] == 5:
                        i += 2
                    elif i + 1 < len(codes) and codes[i + 1] == 2:
                        i += 4
                i += 1
    text_part(text[pos:])
    lines.append(cur)
    # drop one trailing empty line produced by a final newline
    if len(lines) > 1 and not lines[-1]:
        lines.pop()
    return lines


def plain(line):
    return "".join(s for s, _ in line)


# --------------------------------------------------------------------------- helpers

def clamp01(x):
    return 0.0 if x < 0 else 1.0 if x > 1 else x


def smooth(x):
    x = clamp01(x)
    return x * x * (3 - 2 * x)


def alpha_mask(layer, a):
    A = layer.getchannel("A")
    if a >= 0.999:
        return A
    a = max(0.0, a)
    return A.point([int(v * a + 0.5) for v in range(256)])


def paste_layer(frame, layer, pos, a=1.0):
    if layer is None or a <= 0.003:
        return
    frame.paste(layer.convert("RGB") if layer.mode != "RGB" else layer, pos, alpha_mask(layer, a))


def rounded_layer(w, h, radius, fill, outline=None, width=0, ss=3):
    """Anti-aliased rounded rectangle as an RGBA image (supersampled)."""
    big = Image.new("RGBA", (w * ss, h * ss), (0, 0, 0, 0))
    d = ImageDraw.Draw(big)
    d.rounded_rectangle((0, 0, w * ss - 1, h * ss - 1), radius=radius * ss, fill=fill,
                        outline=outline, width=width * ss)
    return big.resize((w, h), Image.LANCZOS)


def wrap_words(text, font, max_w):
    out = []
    for para in text.split("\n"):
        words = para.split(" ")
        line = ""
        for w in words:
            cand = w if not line else line + " " + w
            if font.getlength(cand) <= max_w or not line:
                line = cand
            else:
                out.append(line)
                line = w
        out.append(line)
    return out


def text_layer(lines, font, fill, line_h, align="center", max_w=None):
    """RGBA layer with lines of text; returns (img, w, h)."""
    w = int(max(font.getlength(l) for l in lines)) + 4 if lines else 1
    h = int(line_h * len(lines)) + 8
    img = Image.new("RGBA", (max(1, w), max(1, h)), fill + (0,))
    d = ImageDraw.Draw(img)
    asc, desc = font.getmetrics()
    for i, l in enumerate(lines):
        lw = font.getlength(l)
        x = (w - lw) / 2 if align == "center" else 0
        y = i * line_h + (line_h - (asc + desc)) / 2 + asc
        d.text((x, y), l, font=font, fill=fill + (255,), anchor="ls")
    return img


# --------------------------------------------------------------------------- shared look

class Look:
    def __init__(self, W, H):
        self.W, self.H = W, H
        self.S = W / 1920.0
        self._bg = None
        self._layout_bg = {}

    def px(self, v):
        return int(round(v * self.S))

    def background(self):
        if self._bg is None:
            W, H = self.W, self.H
            yy, xx = np.mgrid[0:H, 0:W].astype(np.float32)
            g = (xx / W) * 0.4 + (yy / H) * 0.6
            top = np.array([8, 12, 28], np.float32)
            bot = np.array([16, 24, 52], np.float32)
            img = top + (bot - top) * g[..., None]
            d = np.sqrt(((xx - W * 0.78) / W) ** 2 + ((yy - H * 0.12) / H) ** 2)
            glow = np.clip(1 - d / 0.65, 0, 1) ** 2
            img += glow[..., None] * np.array([10, 20, 42], np.float32)
            d2 = np.sqrt(((xx - W * 0.1) / W) ** 2 + ((yy - H * 0.95) / H) ** 2)
            glow2 = np.clip(1 - d2 / 0.5, 0, 1) ** 2
            img += glow2[..., None] * np.array([14, 8, 30], np.float32)
            rng = np.random.default_rng(7)
            img += rng.uniform(-0.6, 0.6, img.shape).astype(np.float32)
            bg = Image.fromarray(np.clip(img, 0, 255).astype(np.uint8))
            # watermark
            f = ui_font(self.px(19), "Medium")
            txt = "switchboard · Three Cyborgs"
            lay = text_layer([txt], f, (170, 182, 205), self.px(26))
            paste_layer(bg, lay, (W - lay.width - self.px(30), H - lay.height - self.px(18)), 0.55)
            self._bg = bg
        return self._bg

    def bg_with_shadows(self, rects, radius):
        key = (tuple(rects), radius)
        if key not in self._layout_bg:
            bg = self.background().copy()
            sh = Image.new("L", bg.size, 0)
            d = ImageDraw.Draw(sh)
            off = self.px(10)
            for (x, y, w, h) in rects:
                d.rounded_rectangle((x, y + off, x + w, y + h + off), radius=radius, fill=150)
            sh = sh.filter(ImageFilter.GaussianBlur(self.px(22)))
            bg.paste((0, 0, 0), (0, 0), sh)
            self._layout_bg[key] = bg
        return self._layout_bg[key]


# --------------------------------------------------------------------------- captions

class CaptionTrack:
    FADE = 0.35

    def __init__(self, look):
        self.look = look
        self.cur = ("", -10.0)
        self.prev = ("", -10.0)
        self._cache = {}

    def set(self, text, t):
        if text == self.cur[0]:
            return
        self.prev = self.cur
        self.cur = (text, t)

    def state(self, t):
        a = smooth((t - self.cur[1]) / self.FADE)
        items = []
        if self.prev[0] and a < 1:
            items.append((self.prev[0], 1 - a))
        if self.cur[0]:
            items.append((self.cur[0], a))
        return tuple((txt, round(al, 2)) for txt, al in items if al > 0.005)

    def layer(self, text):
        if text not in self._cache:
            L = self.look
            f = ui_font(L.px(34), "Medium")
            max_w = L.px(1480)
            lines = wrap_words(text, f, max_w)
            if len(lines) > 2:
                lines = lines[:2]
                while f.getlength(lines[1] + "…") > max_w and " " in lines[1]:
                    lines[1] = lines[1].rsplit(" ", 1)[0]
                lines[1] += "…"
            lh = L.px(46)
            padx, pady = L.px(40), L.px(16)
            tw = int(max(f.getlength(l) for l in lines))
            w, h = tw + 2 * padx, lh * len(lines) + 2 * pady
            band = rounded_layer(w, h, L.px(18), (12, 16, 30, 205), outline=(88, 166, 255, 70), width=1)
            d = ImageDraw.Draw(band)
            bar = L.px(5)
            d.rounded_rectangle((L.px(14), pady + L.px(6), L.px(14) + bar, h - pady - L.px(6)),
                                radius=bar // 2, fill=ACCENT + (255,))
            asc, desc = f.getmetrics()
            for i, l in enumerate(lines):
                lw = f.getlength(l)
                y = pady + i * lh + (lh - (asc + desc)) / 2 + asc
                d.text(((w - lw) / 2 + bar, y), l, font=f, fill=(255, 255, 255, 255), anchor="ls")
            self._cache[text] = band
        return self._cache[text]

    def draw(self, frame, state, center_y):
        for text, a in state:
            lay = self.layer(text)
            x = (self.look.W - lay.width) // 2
            y = int(center_y - lay.height / 2)
            paste_layer(frame, lay, (x, y), a)


# --------------------------------------------------------------------------- scenes

class Scene:
    duration = 1.0
    caption_y = None

    def frame(self, t):
        raise NotImplementedError


class TitleScene(Scene):
    def __init__(self, spec, look):
        self.look = look
        self.duration = float(spec.get("duration", 3.5))
        L = look
        tf = ui_font(L.px(96), "Bold")
        sf = ui_font(L.px(40), "Regular")
        tlines = wrap_words(spec.get("title", ""), tf, L.px(1600))
        self.tl = text_layer(tlines, tf, WHITE, L.px(112))
        sub = spec.get("subtitle", "")
        self.sl = text_layer(wrap_words(sub, sf, L.px(1500)), sf, (159, 179, 217), L.px(54)) if sub else None
        bar = rounded_layer(L.px(120), L.px(6), L.px(3), ACCENT + (255,))
        self.bar = bar
        gap = L.px(34)
        total = self.bar.height + gap + self.tl.height + (gap + self.sl.height if self.sl else 0)
        self.y0 = (L.H - total) // 2 - L.px(20)
        self.gap = gap
        self._last = None

    def frame(self, t):
        a_in = smooth(t / 0.8)
        a_out = smooth((self.duration - t) / 0.6)
        a = min(a_in, a_out)
        key = round(a, 3)
        if self._last and self._last[0] == key:
            return self._last[1]
        L = self.look
        fr = L.background().copy()
        rise = int((1 - a_in) * L.px(16))
        y = self.y0 + rise
        paste_layer(fr, self.bar, ((L.W - self.bar.width) // 2, y), a)
        y += self.bar.height + self.gap
        paste_layer(fr, self.tl, ((L.W - self.tl.width) // 2, y), a)
        y += self.tl.height + self.gap
        if self.sl:
            a2 = min(smooth((t - 0.35) / 0.8), a_out)
            paste_layer(fr, self.sl, ((L.W - self.sl.width) // 2, y + int((1 - a2) * L.px(10))), a2)
        self._last = (key, fr)
        return fr


def parse_inline_code(text):
    """'run `switchboard setup` now' -> [(word, is_code), ...] tokens keeping spaces."""
    parts = text.split("`")
    toks = []
    for i, p in enumerate(parts):
        code = i % 2 == 1
        for j, w in enumerate(p.split(" ")):
            if j:
                toks.append((" ", code))
            if w:
                toks.append((w, code))
    return toks


class CardScene(Scene):
    def __init__(self, spec, look):
        self.look = look
        L = look
        self.duration = float(spec.get("duration", 6))
        bullets = spec.get("bullets", [])
        hf = ui_font(L.px(66), "Bold")
        self.head = text_layer(wrap_words(spec.get("title", ""), hf, L.px(1600)), hf, WHITE, L.px(80), align="left")
        self.bar = rounded_layer(L.px(90), L.px(6), L.px(3), ACCENT + (255,))
        size = 42
        while True:
            layers = [self._bullet(b, size) for b in bullets]
            total = sum(l.height for l in layers) + L.px(26) * max(0, len(layers) - 1)
            if total + self.head.height + L.px(120) < L.H - L.px(260) or size <= 26:
                break
            size -= 2
        self.bullets = layers
        self.x0 = L.px(200)
        content_h = self.head.height + L.px(70) + total
        self.y0 = max(L.px(80), (L.H - content_h) // 2 - L.px(30))
        n = len(layers)
        span = max(0.0, self.duration - 2.2)
        step = span / (n - 1) if n > 1 else 0
        step = max(0.3, step) if n > 1 else 0
        self.times = [0.6 + i * step for i in range(n)]
        self._last = None

    def _bullet(self, text, size):
        L = self.look
        f = ui_font(L.px(size), "Regular")
        cf = ImageFont.truetype(MENLO, L.px(size * 0.86))
        max_w = L.px(1440)
        indent = L.px(size * 1.15)
        lh = L.px(size * 1.4)
        toks = parse_inline_code(text)
        rows, row, wcur = [], [], 0.0
        for w, code in toks:
            fw = (cf if code else f).getlength(w)
            if w == " " and not row:
                continue
            if wcur + fw > max_w and row and w != " ":
                while row and row[-1][0] == " ":
                    wcur -= (cf if row[-1][1] else f).getlength(" ")
                    row.pop()
                rows.append(row)
                row, wcur = [], 0.0
            row.append((w, code))
            wcur += fw
        if row:
            rows.append(row)
        h = lh * len(rows) + L.px(6)
        img = Image.new("RGBA", (indent + max_w + L.px(40), h), (0, 0, 0, 0))
        d = ImageDraw.Draw(img)
        r = L.px(size * 0.17)
        cy = lh / 2
        d.ellipse((L.px(4), cy - r, L.px(4) + 2 * r, cy + r), fill=ACCENT + (255,))
        asc, desc = f.getmetrics()
        for i, rw in enumerate(rows):
            x = indent
            base = i * lh + (lh - (asc + desc)) / 2 + asc
            # merge consecutive code tokens into one chip
            j = 0
            while j < len(rw):
                w, code = rw[j]
                if code:
                    k = j
                    s = ""
                    while k < len(rw) and rw[k][1]:
                        s += rw[k][0]
                        k += 1
                    s = s.strip()
                    cw = cf.getlength(s)
                    pad = L.px(8)
                    d.rounded_rectangle((x - 2, i * lh + L.px(size * 0.12), x + cw + 2 * pad, (i + 1) * lh - L.px(size * 0.12)),
                                        radius=L.px(8), fill=(22, 30, 52, 255), outline=(48, 54, 61, 255))
                    d.text((x + pad, base), s, font=cf, fill=ACCENT2 + (255,), anchor="ls")
                    x += cw + 2 * pad + 2
                    j = k
                else:
                    d.text((x, base), w, font=f, fill=(226, 232, 245, 255), anchor="ls")
                    x += f.getlength(w)
                    j += 1
        return img

    def frame(self, t):
        L = self.look
        alphas = tuple(round(smooth((t - ti) / 0.4), 2) for ti in self.times)
        ha = smooth(t / 0.5)
        key = (round(ha, 2), alphas)
        if self._last and self._last[0] == key:
            return self._last[1]
        fr = L.background().copy()
        y = self.y0
        paste_layer(fr, self.bar, (self.x0, y), ha)
        y += self.bar.height + L.px(26)
        paste_layer(fr, self.head, (self.x0, y), ha)
        y += self.head.height + L.px(44)
        for lay, a in zip(self.bullets, alphas):
            paste_layer(fr, lay, (self.x0 + int((1 - a) * L.px(-24)) + L.px(8), y), a)
            y += lay.height + L.px(26)
        self._last = (key, fr)
        return fr


class DiagramScene(Scene):
    def __init__(self, spec, look):
        self.look = look
        L = look
        W, H = L.W, L.H
        self.duration = float(spec.get("duration", 6))
        tf = ui_font(L.px(56), "Bold")
        self.title = text_layer(wrap_words(spec.get("title", ""), tf, L.px(1700)), tf, WHITE, L.px(70)) if spec.get("title") else None
        self.caps = CaptionTrack(look)
        self.caption = spec.get("caption", "")
        nodes = spec.get("nodes", [])
        edges = spec.get("edges", [])
        ss = 2
        f1 = ui_font(L.px(32) * ss, "Semibold")
        f2 = ui_font(L.px(24) * ss, "Regular")
        fl = ui_font(L.px(21) * ss, "Medium")

        self.boxes = {}
        node_layers = []
        for n in nodes:
            lines = str(n.get("label", n["id"])).split("\n")
            fonts = [f1] + [f2] * (len(lines) - 1)
            lhs = [L.px(42) * ss] + [L.px(32) * ss] * (len(lines) - 1)
            tw = max(fo.getlength(li) for fo, li in zip(fonts, lines))
            padx, pady = L.px(30) * ss, L.px(18) * ss
            bw = int(max(tw + 2 * padx, L.px(240) * ss))
            bh = int(sum(lhs) + 2 * pady)
            col = hexrgb(n["color"]) if n.get("color") else ACCENT
            img = Image.new("RGBA", (bw + 8, bh + 8), (0, 0, 0, 0))
            d = ImageDraw.Draw(img)
            d.rounded_rectangle((4, 4, bw + 3, bh + 3), radius=L.px(18) * ss, fill=(22, 27, 34, 255),
                                outline=mix(col, (22, 27, 34), 0.35) + (255,), width=3 * ss // 2 + 1)
            d.rounded_rectangle((4 + L.px(18) * ss, 4, bw + 3 - L.px(18) * ss, 4 + 3 * ss), radius=2 * ss, fill=col + (255,))
            y = 4 + pady
            for i, (fo, li, lh) in enumerate(zip(fonts, lines, lhs)):
                asc, desc = fo.getmetrics()
                fill = WHITE if i == 0 else mix(col, WHITE, 0.25)
                d.text((4 + bw / 2, y + (lh - (asc + desc)) / 2 + asc), li, font=fo, fill=fill + (255,), anchor="ms")
                y += lh
            img = img.resize((img.width // ss, img.height // ss), Image.LANCZOS)
            cx, cy = n["x"] * W, n["y"] * H
            pos = (int(cx - img.width / 2), int(cy - img.height / 2))
            self.boxes[n["id"]] = (cx, cy, (bw / ss) / 2, (bh / ss) / 2)
            node_layers.append((img, pos))

        pairs = {(e["from"], e["to"]) for e in edges}
        self.edge_paths = []
        line_layers, label_layers = [], []
        for e in edges:
            a, b = self.boxes[e["from"]], self.boxes[e["to"]]
            ax, ay, bx, by = a[0], a[1], b[0], b[1]
            dx, dy = bx - ax, by - ay
            ln = math.hypot(dx, dy) or 1
            ux, uy = dx / ln, dy / ln
            nx, ny = -uy, ux
            off = L.px(16) if (e["to"], e["from"]) in pairs and not e.get("both") else 0
            ax, ay, bx, by = ax + nx * off, ay + ny * off, bx + nx * off, by + ny * off

            def clip(box, sx, sy, sign):
                hw, hh = box[2] + L.px(12), box[3] + L.px(12)
                tt = min(hw / abs(ux) if ux else 1e9, hh / abs(uy) if uy else 1e9)
                return sx + sign * ux * tt, sy + sign * uy * tt

            p0 = clip(a, ax, ay, 1)
            p1 = clip(b, bx, by, -1)
            self.edge_paths.append((p0, p1))
            col = hexrgb(e["color"]) if e.get("color") else ACCENT
            # line layer in local bbox, supersampled
            pad = L.px(24)
            x0, y0 = int(min(p0[0], p1[0]) - pad), int(min(p0[1], p1[1]) - pad)
            x1, y1 = int(max(p0[0], p1[0]) + pad), int(max(p0[1], p1[1]) + pad)
            big = Image.new("RGBA", ((x1 - x0) * ss, (y1 - y0) * ss), (0, 0, 0, 0))
            d = ImageDraw.Draw(big)
            q0 = ((p0[0] - x0) * ss, (p0[1] - y0) * ss)
            q1 = ((p1[0] - x0) * ss, (p1[1] - y0) * ss)
            ah = L.px(16) * ss
            lw = max(2, L.px(3)) * ss

            def head(tip, ux_, uy_):
                bxp, byp = tip[0] - ux_ * ah, tip[1] - uy_ * ah
                d.polygon([tip, (bxp + nx * ah * 0.55, byp + ny * ah * 0.55), (bxp - nx * ah * 0.55, byp - ny * ah * 0.55)],
                          fill=col + (255,))

            s0 = (q0[0] + ux * ah * 0.8, q0[1] + uy * ah * 0.8) if e.get("both") else q0
            s1 = (q1[0] - ux * ah * 0.8, q1[1] - uy * ah * 0.8)
            if e.get("dashed"):
                seg = L.px(14) * ss
                total = math.hypot(s1[0] - s0[0], s1[1] - s0[1])
                k = 0.0
                while k < total:
                    k2 = min(total, k + seg)
                    d.line([(s0[0] + ux * k, s0[1] + uy * k), (s0[0] + ux * k2, s0[1] + uy * k2)], fill=col + (255,), width=lw)
                    k += seg * 1.8
            else:
                d.line([s0, s1], fill=col + (255,), width=lw)
            head(q1, ux, uy)
            if e.get("both"):
                head(q0, -ux, -uy)
            line_layers.append((big.resize((x1 - x0, y1 - y0), Image.LANCZOS), (x0, y0)))

            lab = e.get("label")
            if lab:
                lines = lab.split("\n")
                lh = L.px(28) * ss
                tw = max(fl.getlength(li) for li in lines)
                pw, ph = int(tw + L.px(28) * ss), int(lh * len(lines) + L.px(12) * ss)
                img = Image.new("RGBA", (pw + 4, ph + 4), (0, 0, 0, 0))
                d2 = ImageDraw.Draw(img)
                d2.rounded_rectangle((2, 2, pw + 1, ph + 1), radius=L.px(12) * ss, fill=(13, 17, 23, 255),
                                     outline=mix(col, (13, 17, 23), 0.55) + (255,), width=ss)
                asc, desc = fl.getmetrics()
                for i, li in enumerate(lines):
                    yb = 2 + L.px(6) * ss + i * lh + (lh - (asc + desc)) / 2 + asc
                    d2.text((2 + pw / 2, yb), li, font=fl, fill=(201, 209, 217, 255), anchor="ms")
                img = img.resize((img.width // ss, img.height // ss), Image.LANCZOS)
                mx, my = (p0[0] + p1[0]) / 2, (p0[1] + p1[1]) / 2
                lo = e.get("label_offset", 0)
                mx += nx * L.px(lo)
                my += ny * L.px(lo)
                label_layers.append((img, (int(mx - img.width / 2), int(my - img.height / 2))))
            else:
                label_layers.append(None)

        # reveal schedule: nodes in order, then edges in order
        n_items = len(node_layers) + len(line_layers)
        span = min(self.duration * 0.55, 0.55 * n_items)
        step = span / max(1, n_items)
        t = 0.5
        self.node_items = []
        for lay in node_layers:
            self.node_items.append((lay, t))
            t += step
        self.edge_items = []
        for i, lay in enumerate(line_layers):
            self.edge_items.append((lay, label_layers[i], t))
            t += step
        self.reveal_end = t + 0.5
        self.caps.set(self.caption, 0.6 if self.caption else 0)
        self.caption_y = L.H - L.px(118)
        self.dot = self._dot_sprite()
        self._static = None
        self._last = None

    def _dot_sprite(self):
        L = self.look
        r = L.px(7)
        s = 4
        big = Image.new("RGBA", (r * 6 * s, r * 6 * s), (0, 0, 0, 0))
        d = ImageDraw.Draw(big)
        c = r * 3 * s
        d.ellipse((c - r * 2.4 * s, c - r * 2.4 * s, c + r * 2.4 * s, c + r * 2.4 * s), fill=ACCENT2 + (60,))
        d.ellipse((c - r * s, c - r * s, c + r * s, c + r * s), fill=(220, 238, 255, 255))
        big = big.filter(ImageFilter.GaussianBlur(s))
        return big.resize((r * 6, r * 6), Image.LANCZOS)

    def _under(self, t, alpha_override=None):
        L = self.look
        fr = L.background().copy()
        if self.title:
            paste_layer(fr, self.title, ((L.W - self.title.width) // 2, L.px(70)), smooth(t / 0.5) if alpha_override is None else 1)
        for (img, pos), _, ti in self.edge_items:
            a = 1 if alpha_override else smooth((t - ti) / 0.45)
            paste_layer(fr, img, pos, a)
        for (img, pos), ti in self.node_items:
            a = 1 if alpha_override else smooth((t - ti) / 0.45)
            rise = 0 if alpha_override else int((1 - a) * L.px(14))
            paste_layer(fr, img, (pos[0], pos[1] + rise), a)
        return fr

    def frame(self, t):
        L = self.look
        if t >= self.reveal_end:
            if self._static is None:
                self._static = self._under(t, alpha_override=True)
            fr = self._static.copy()
        else:
            fr = self._under(t)
        # travelling dots
        for i, ((p0, p1), (_, _, ti)) in enumerate(zip(self.edge_paths, self.edge_items)):
            tt = t - ti - 0.6
            if tt < 0:
                continue
            ph = (tt / 1.8 + i * 0.37) % 1.0
            a = smooth(tt / 0.4) * smooth(ph / 0.12) * smooth((1 - ph) / 0.12)
            x = p0[0] + (p1[0] - p0[0]) * ph
            y = p0[1] + (p1[1] - p0[1]) * ph
            paste_layer(fr, self.dot, (int(x - self.dot.width / 2), int(y - self.dot.height / 2)), a)
        for _, lab, ti in self.edge_items:
            if lab:
                paste_layer(fr, lab[0], lab[1], smooth((t - ti - 0.15) / 0.45))
        self.caps.draw(fr, self.caps.state(t), self.caption_y)
        return fr


# --------------------------------------------------------------------------- terminal

class Pane:
    TITLE_H = 40

    def __init__(self, spec, rect, mono, look):
        self.id = spec["id"]
        self.title = spec.get("title", spec["id"])
        self.prompt = spec.get("prompt", "$ ")
        self.rect = rect
        self.mono = mono
        self.look = look
        x, y, w, h = rect
        self.th = look.px(self.TITLE_H)
        self.padx = look.px(18)
        self.pady = look.px(12)
        self.cols = max(10, int((w - 2 * self.padx) // mono.cw))
        self.rows = max(3, int((h - self.th - 2 * self.pady) // mono.lh))
        self.lines = []          # list of Line objects
        self.input = None        # None = no prompt shown; str = prompt + typed text
        self.version = 0
        self.last_type = -10.0
        self.hl = None           # (line, t0, t1)
        self._chrome = {}
        self._cache = (None, None)

    # ----- state mutation (called by events)
    def add_line(self, segs):
        self.lines.append(Line(segs))
        if len(self.lines) > 2000:
            self.lines = self.lines[-1500:]
        self.version += 1

    def set_input(self, s, t=None):
        self.input = s
        if t is not None:
            self.last_type = t
        self.version += 1

    def commit_input(self):
        if self.input is not None:
            self.lines.append(Line([(self.prompt, (PROMPT, True, False)), (self.input, DEFAULT_STYLE)]))
        self.input = None
        self.version += 1

    def clear(self):
        self.lines = []
        self.hl = None
        self.version += 1

    # ----- drawing
    def chrome(self, active):
        if active not in self._chrome:
            L = self.look
            x, y, w, h = self.rect
            ss = 3
            big = Image.new("RGBA", (w * ss, h * ss), (0, 0, 0, 0))
            d = ImageDraw.Draw(big)
            r = L.px(12) * ss
            bcol = mix(ACCENT, TERM_BG, 0.15) if active else BORDER
            bw = (2 if active else 1) * ss
            d.rounded_rectangle((0, 0, w * ss - 1, h * ss - 1), radius=r, fill=bcol + (255,))
            d.rounded_rectangle((bw, bw, w * ss - 1 - bw, h * ss - 1 - bw), radius=r - bw, fill=TERM_BG + (255,))
            # title bar: top rounded, bottom square
            th = self.th * ss
            d.rounded_rectangle((bw, bw, w * ss - 1 - bw, th), radius=r - bw, fill=TITLEBAR + (255,))
            d.rectangle((bw, th - r, w * ss - 1 - bw, th), fill=TITLEBAR + (255,))
            d.rectangle((bw, th, w * ss - 1 - bw, th + ss - 1), fill=BORDER + (255,))
            cy = th / 2
            for i, c in enumerate(("#ff5f57", "#febc2e", "#28c840")):
                cx = L.px(22) * ss + i * L.px(20) * ss
                rr = L.px(6) * ss
                d.ellipse((cx - rr, cy - rr, cx + rr, cy + rr), fill=hexrgb(c) + (255,))
            img = big.resize((w, h), Image.LANCZOS)
            dd = ImageDraw.Draw(img)
            f = ui_font(L.px(16), "Semibold" if active else "Medium")
            title = self.title
            maxw = w - L.px(180)
            while f.getlength(title) > maxw and len(title) > 4:
                title = title[:-2]
            if title != self.title:
                title = title.rstrip() + "…"
            asc, desc = f.getmetrics()
            dd.text((w / 2, self.th / 2 + (asc - desc) / 2), title, font=f,
                    fill=(WHITE if active else MUTED) + (255,), anchor="ms")
            if active:
                # small accent "live" dot to the right of the title
                tx = w / 2 + f.getlength(title) / 2 + L.px(12)
                rr = L.px(4)
                dd.ellipse((tx - rr, self.th / 2 - rr, tx + rr, self.th / 2 + rr), fill=ACCENT + (255,))
            self._chrome[active] = img
        return self._chrome[active]

    def visual_rows(self):
        """All rows as (segments, line_or_None, is_input)."""
        rows = []
        for ln in self.lines:
            for r in ln.wrapped(self.cols):
                rows.append((r, ln))
        cursor = None
        if self.input is not None:
            segs = [(self.prompt, (PROMPT, True, False)), (self.input, DEFAULT_STYLE)]
            wrapped = wrap_segments(segs, self.cols)
            used = sum(char_width(c) for c in self.prompt + self.input)
            crow, ccol = divmod(used, self.cols)
            while len(wrapped) <= crow:
                wrapped.append([])
            base = len(rows)
            for r in wrapped:
                rows.append((r, None))
            cursor = (base + crow, ccol)
        return rows, cursor

    def render(self, t, active):
        cursor_on = active and self.input is not None and ((t - self.last_type) < 0.5 or (t % 1.0) < 0.55)
        hl_a = 0.0
        if self.hl:
            ln, t0, t1 = self.hl
            hl_a = min(smooth((t - t0) / 0.2), smooth((t1 - t) / 0.35))
            if t > t1:
                self.hl = None
        key = (self.version, active, cursor_on, round(hl_a, 1))
        if self._cache[0] == key:
            return key, self._cache[1]
        img = self.chrome(active).copy()
        d = ImageDraw.Draw(img)
        m = self.mono
        rows, cursor = self.visual_rows()
        start = max(0, len(rows) - self.rows)
        ox = self.padx
        oy = self.th + self.pady
        hl_line = self.hl[0] if (self.hl and hl_a > 0.01) else None
        for i, (segs, ln) in enumerate(rows[start:]):
            y = oy + i * m.lh
            if hl_line is not None and ln is hl_line:
                c = mix(TERM_BG, hexrgb("#1f6feb"), 0.38 * hl_a)
                d.rectangle((ox - self.look.px(8), y, img.width - self.padx + self.look.px(8), y + m.lh - 1), fill=c + (255,))
                d.rectangle((ox - self.look.px(8), y, ox - self.look.px(5), y + m.lh - 1),
                            fill=mix(TERM_BG, ACCENT, hl_a) + (255,))
            draw_segments(img, d, segs, ox, y, m)
        if cursor_on and cursor and cursor[0] >= start:
            r, c = cursor
            y = oy + (r - start) * m.lh
            x = ox + c * m.cw
            d.rectangle((x, y + 2, x + m.cw - 1, y + m.lh - 3), fill=mix(TERM_BG, TERM_FG, 0.85) + (255,))
        self._cache = (key, img)
        return key, img


class Line:
    __slots__ = ("segs", "text", "_wrap")

    def __init__(self, segs):
        self.segs = segs
        self.text = plain(segs)
        self._wrap = None

    def wrapped(self, cols):
        if self._wrap is None or self._wrap[0] != cols:
            self._wrap = (cols, wrap_segments(self.segs, cols))
        return self._wrap[1]


def wrap_segments(segs, cols):
    rows, row, col = [], [], 0
    for text, st in segs:
        buf = []
        for ch in text:
            w = char_width(ch)
            if w == 0:
                continue
            if col + w > cols:
                if buf:
                    row.append(("".join(buf), st))
                    buf = []
                rows.append(row)
                row, col = [], 0
            buf.append(ch)
            col += w
        if buf:
            row.append(("".join(buf), st))
    rows.append(row)
    return rows


def draw_segments(img, d, segs, x0, y, m):
    col = 0
    base = y + m.baseline
    for text, st in segs:
        fill = style_color(st) + (255,)
        font = m.bold if st[1] else m.reg
        run, run_col = [], col
        for ch in text:
            src = m.resolve(ch)
            w = char_width(ch)
            if src == "menlo" and w == 1:
                if not run:
                    run_col = col
                run.append(ch)
                col += 1
                continue
            if run:
                d.text((x0 + run_col * m.cw, base), "".join(run), font=font, fill=fill, anchor="ls")
                run = []
            cx = x0 + col * m.cw
            if src == "emoji":
                spr = m.emoji_sprite(ch, w)
                if spr is not None:
                    img.alpha_composite(spr, (int(cx + (w * m.cw - spr.width) / 2), int(y + (m.lh - spr.height) / 2)))
            elif src == "menlo":
                d.text((cx + (w - 1) * m.cw / 2, base), ch, font=font, fill=fill, anchor="ls")
            elif src is not None:
                d.text((cx + w * m.cw / 2, base), ch, font=src, fill=fill, anchor="ms")
            else:
                d.text((cx, base), "?", font=font, fill=fill, anchor="ls")
            col += w
        if run:
            d.text((x0 + run_col * m.cw, base), "".join(run), font=font, fill=fill, anchor="ls")


class TerminalScene(Scene):
    def __init__(self, spec, look):
        self.look = look
        L = look
        layout = spec.get("layout", "single")
        panes = spec.get("panes", [])
        rects = self._layout(layout, len(panes))
        size = L.px(spec.get("font_size", 22 if layout == "single" else 18))
        mono = Mono(size)
        self.panes = [Pane(p, r, mono, look) for p, r in zip(panes, rects)]
        self.by_id = {p.id: p for p in self.panes}
        self.radius = L.px(12)
        self.bg = L.bg_with_shadows([p.rect for p in self.panes], self.radius)
        self.caps = CaptionTrack(look)
        self.caption_y = L.px(946)
        self.active = None
        self.events = []
        self._compile(spec)
        self.ev_i = 0
        self._last = None

    def _layout(self, layout, n):
        L = self.look
        top, bottom, left, right, gap = L.px(44), L.px(860), L.px(80), L.W - L.px(80), L.px(24)
        h = bottom - top
        if layout == "split2":
            w = (right - left - gap) // 2
            return [(left, top, w, h), (left + w + gap, top, w, h)]
        if layout == "split3":
            lw = int((right - left - gap) * 0.58)
            rw = right - left - gap - lw
            rh = (h - gap) // 2
            return [(left, top, lw, h), (left + lw + gap, top, rw, rh), (left + lw + gap, top + rh + gap, rw, h - rh - gap)]
        return [(L.px(140), top, L.W - L.px(280), h)]

    def _pane(self, step):
        pid = step.get("pane")
        if pid not in self.by_id:
            if pid is None and self.panes:
                return self.panes[0]
            raise SystemExit(f"terminal step refers to unknown pane {pid!r}; panes are {list(self.by_id)}")
        return self.by_id[pid]

    def _compile(self, spec):
        ev = self.events
        t = 0.0
        if spec.get("caption"):
            ev.append((0.0, ("caption", spec["caption"])))
        t = 0.4
        for step in spec.get("steps", []):
            kind = step.get("type")
            if kind == "cmd":
                p = self._pane(step)
                text = step.get("text", "")
                cps = float(step.get("cps", 28))
                ev.append((t, ("active", p)))
                ev.append((t, ("input", p, "")))
                t += 0.3
                for i in range(len(text)):
                    # slight humanised jitter, deterministic
                    jit = ((i * 7919) % 13 - 6) / 6.0 * 0.25 / cps
                    ev.append((t + i / cps + jit, ("input", p, text[:i + 1])))
                t += len(text) / cps + 0.35
                ev.append((t, ("commit", p)))
                t = self._output(p, step.get("output", ""), t, step.get("line_delay", 0.03), beat=0.12)
                if not step.get("running"):
                    ev.append((t + 0.05, ("input", p, "")))
                t += float(step.get("pause_after", 0.8))
            elif kind == "output":
                p = self._pane(step)
                t = self._output(p, step.get("text", ""), t, step.get("line_delay", 0.03), beat=0.0)
                t += float(step.get("pause_after", 0.0))
            elif kind == "caption":
                ev.append((t, ("caption", step.get("text", ""))))
            elif kind == "wait":
                t += float(step.get("seconds", 1.0))
            elif kind == "clear":
                p = self._pane(step)
                ev.append((t, ("clear", p)))
                t += float(step.get("pause_after", 0.1))
            elif kind == "highlight":
                p = self._pane(step)
                secs = float(step.get("seconds", 1.5))
                ev.append((t, ("highlight", p, step.get("line_contains", ""), secs)))
                t += secs
            elif kind == "active":
                ev.append((t, ("active", self._pane(step))))
            else:
                print(f"warning: unknown terminal step type {kind!r}", file=sys.stderr)
        self.duration = t + float(spec.get("tail", 0.8))
        ev.sort(key=lambda e: e[0])  # stable: equal times keep insertion order

    def _output(self, p, text, t, line_delay, beat):
        if not text:
            return t + beat
        lines = parse_ansi(text)
        d = float(line_delay)
        if len(lines) > 40:
            d = min(d, 2.5 / len(lines))
        t += beat
        for ln in lines:
            t += d
            self.events.append((t, ("line", p, ln)))
        return t

    def _apply(self, t):
        while self.ev_i < len(self.events) and self.events[self.ev_i][0] <= t + 1e-9:
            et, e = self.events[self.ev_i]
            self.ev_i += 1
            k = e[0]
            if k == "caption":
                self.caps.set(e[1], et)
            elif k == "active":
                self.active = e[1]
            elif k == "input":
                e[1].set_input(e[2], et)
            elif k == "commit":
                e[1].commit_input()
            elif k == "line":
                # output lands above an idle prompt, which stays at the bottom
                e[1].add_line(e[2])
            elif k == "clear":
                e[1].clear()
            elif k == "highlight":
                p, needle, secs = e[1], e[2], e[3]
                target = None
                for ln in reversed(p.lines):
                    if needle in ln.text:
                        target = ln
                        break
                if target is None:
                    print(f"warning: highlight {needle!r} found no line in pane {p.id}", file=sys.stderr)
                else:
                    p.hl = (target, et, et + secs)

    def frame(self, t):
        self._apply(t)
        rendered = [p.render(t, p is self.active) for p in self.panes]
        cap = self.caps.state(t)
        key = (tuple(k for k, _ in rendered), cap)
        if self._last and self._last[0] == key:
            return self._last[1]
        fr = self.bg.copy()
        for p, (_, img) in zip(self.panes, rendered):
            fr.paste(img, p.rect[:2], img)
        self.caps.draw(fr, cap, self.caption_y)
        self._last = (key, fr)
        return fr


SCENES = {"title": TitleScene, "card": CardScene, "diagram": DiagramScene, "terminal": TerminalScene}


# --------------------------------------------------------------------------- main

def parse_selection(sel, n):
    if not sel:
        return list(range(n))
    out = []
    for part in sel.split(","):
        if "-" in part:
            a, b = part.split("-")
            out.extend(range(int(a), int(b) + 1))
        else:
            out.append(int(part))
    return [i for i in out if 0 <= i < n]


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("storyboard")
    ap.add_argument("out")
    ap.add_argument("--preview-frames", metavar="DIR", help="write the last frame of each scene as PNG")
    ap.add_argument("--scenes", help="render only these scene indices (0-based), e.g. 0,3-5")
    ap.add_argument("--crf", type=int, default=20)
    ap.add_argument("--preset", default="medium")
    args = ap.parse_args()

    with open(args.storyboard) as f:
        sb = json.load(f)
    W, H, fps = int(sb.get("width", 1920)), int(sb.get("height", 1080)), int(sb.get("fps", 30))
    look = Look(W, H)
    specs = sb["scenes"]
    idx = parse_selection(args.scenes, len(specs))

    t_start = time.time()
    scenes = []
    for i in idx:
        s = specs[i]
        cls = SCENES.get(s.get("type"))
        if cls is None:
            raise SystemExit(f"scene {i}: unknown type {s.get('type')!r}")
        scenes.append((i, s.get("type"), cls(s, look)))

    if args.preview_frames:
        os.makedirs(args.preview_frames, exist_ok=True)

    cmd = [FFMPEG, "-y", "-loglevel", "error", "-f", "rawvideo", "-pix_fmt", "rgb24", "-s", f"{W}x{H}",
           "-r", str(fps), "-i", "-", "-c:v", "libx264", "-preset", args.preset, "-crf", str(args.crf),
           "-pix_fmt", "yuv420p", "-movflags", "+faststart", args.out]
    proc = subprocess.Popen(cmd, stdin=subprocess.PIPE)
    out = proc.stdin
    n_frames = 0
    last_img, last_bytes = None, None

    def emit(img):
        nonlocal n_frames, last_img, last_bytes
        if img is not last_img:
            last_img, last_bytes = img, img.tobytes()
        out.write(last_bytes)
        n_frames += 1

    trans = max(1, int(round(0.4 * fps)))
    black = Image.new("RGB", (W, H), (0, 0, 0))
    prev = black
    try:
        for si, (i, kind, sc) in enumerate(scenes):
            t0 = time.time()
            nf = max(1, int(round(sc.duration * fps)))
            first = sc.frame(0.0).copy()
            for k in range(1, trans + 1):
                emit(Image.blend(prev, first, smooth(k / (trans + 1))))
            img = first
            for fi in range(nf):
                img = sc.frame(fi / fps)
                emit(img)
            prev = img.copy()
            if args.preview_frames:
                img.save(os.path.join(args.preview_frames, f"scene_{i:02d}_{kind}.png"))
            print(f"scene {i:2d} {kind:8s} {sc.duration:6.2f}s  rendered in {time.time() - t0:5.1f}s",
                  file=sys.stderr)
        for k in range(1, trans + 1):
            emit(Image.blend(prev, black, smooth(k / (trans + 1))))
        out.close()
    except BrokenPipeError:
        pass
    rc = proc.wait()
    if rc != 0:
        raise SystemExit(f"ffmpeg failed with exit code {rc}")
    print(f"wrote {args.out}: {n_frames} frames, {n_frames / fps:.1f}s of video in {time.time() - t_start:.1f}s",
          file=sys.stderr)


if __name__ == "__main__":
    main()
