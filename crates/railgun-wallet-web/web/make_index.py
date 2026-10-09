#!/usr/bin/env python3
"""Builds the static page from the wallet daemon's front: install the /api shim before any of the
front's scripts run, make the waku-bundle path relative, add a footer line saying where the engine
runs. Usage: make_index.py <wallet index.html> <out index.html>"""
import io
import sys

src, dst = sys.argv[1], sys.argv[2]
html = io.open(src, encoding="utf-8").read()


def must(old, new):
    global html
    if html.count(old) == 0:
        sys.exit(f"make_index: anchor {old!r} not found")
    html = html.replace(old, new)


# the shim must be installed before any script of the front runs
i = html.find("<head>")
if i < 0:
    sys.exit("make_index: no <head>")
i += len("<head>")
html = html[:i] + '\n<script src="./shim.js"></script>' + html[i:]

# served from any sub-path (GitHub Pages project site, a folder of a static host)
must('"/waku-bundle.js"', '"./waku-bundle.js"')

# where the engine runs, at the bottom of the page
note = ('<div style="margin:18px auto 12px;max-width:1280px;text-align:center;font-size:12px;opacity:.65">'
        'Runs entirely in this browser: keys stay in the page, the wallet database is kept in '
        'IndexedDB, only the RPC, the subsquid, the POI node, the bundler and the Waku network are '
        'contacted.</div>\n')
j = html.rfind("</body>")
if j < 0:
    sys.exit("make_index: no </body>")
html = html[:j] + note + html[j:]

io.open(dst, "w", encoding="utf-8", newline="").write(html)
print(f"make_index: {dst} ({len(html)} bytes)")
