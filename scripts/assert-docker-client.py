#!/usr/bin/env python3
"""The production image must serve the fight canvas and WASM, not only /health."""

import re
import sys
import urllib.request

if len(sys.argv) != 2:
    raise SystemExit("usage: assert-docker-client.py <base-url>")

base = sys.argv[1].rstrip("/")
html = urllib.request.urlopen(base + "/").read().decode()
if "<title>git fight</title>" not in html or 'data-testid="stage"' not in html:
    raise SystemExit("index.html is not the fight client")
spa = urllib.request.urlopen(base + "/match/deadbeefdeadbeefdeadbeefdeadbeef").read().decode()
if "<title>git fight</title>" not in spa:
    raise SystemExit("/match/:id is not the fight SPA")
src = re.search(r'src="(/assets/index-[^"]+\.js)"', html)
if not src:
    raise SystemExit("index.html has no bundled script")
js = urllib.request.urlopen(base + src.group(1)).read().decode()
wasm = re.search(r'(/assets/git_fight_wasm_bg-[^"]+\.wasm)', js)
if not wasm:
    raise SystemExit("bundle does not reference git-fight wasm")
data = urllib.request.urlopen(base + wasm.group(1)).read()
if data[:4] != b"\0asm":
    raise SystemExit("wasm asset is not a WebAssembly module")
print("canvas+wasm ok", wasm.group(1), len(data))
