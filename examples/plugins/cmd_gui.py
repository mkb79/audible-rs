"""GUI plugin (AUD-161, slice 1): local web dashboard for the library.

Serves a read-only single-page dashboard on 127.0.0.1 showing the
library with per-item download status — which kinds (audio / cover /
chapter / pdf) are already recorded in which formats, and which are
still missing. Data comes exclusively from the ephemeral plugin broker
via ``invoke`` (re-entrancy, scope ``invoke``); the plugin never sees
auth material.

Install: copy this file into the plugin dir (default
``<data_dir>/plugins``) and make ``audible_plugin_sdk`` importable
(``pip install <repo>/sdk/python`` or PYTHONPATH). Then::

    audible -m all gui [--no-open] [--port N]

The session is pinned to the invoking ``-a``/``-m``/``-s`` selection
(one account per GUI session) — start with ``-m all`` to see every
marketplace of the account. Stop with Ctrl-C.
"""

import argparse
import hmac
import json
import secrets
import threading
import webbrowser
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

from audible_plugin_sdk import Broker, BrokerError, run

MANIFEST = {
    "name": "gui",
    "version": "0.1.0",
    "description": "Local web dashboard: library + download status",
    "scopes": ["invoke"],
    "help": "usage: audible [-m all] gui [--no-open] [--port N]",
}

#: Artifact kinds tracked in the downloads table (db/schema.rs).
KINDS = ("audio", "cover", "chapter", "pdf")


def slim_item(doc):
    """The few doc fields the table needs — keeps the browser payload
    small (full docs easily reach several MB per library)."""
    authors = [a.get("name", "") for a in doc.get("authors") or []]
    images = doc.get("product_images") or {}
    cover = images.get("500") or next(iter(images.values()), None)
    return {
        "title": doc.get("title"),
        "subtitle": doc.get("subtitle"),
        "authors": [a for a in authors if a],
        "runtime_min": doc.get("runtime_length_min"),
        "purchase_date": doc.get("purchase_date"),
        "is_archived": bool(doc.get("is_archived")),
        "cover": cover,
    }


class Api:
    """Broker-backed data endpoints. Each call runs one built-in via
    ``/v1/invoke`` with ``-o json`` and reshapes the result."""

    def __init__(self, broker):
        self.broker = broker

    def _invoke_json(self, argv, empty):
        """Invoke a built-in and parse its JSON stdout. Commands print
        human hints to stderr and nothing to stdout when a result set is
        empty — that case maps to `empty`, every other failure raises."""
        reply = self.broker.invoke(argv)
        stdout = reply.get("stdout", "")
        if reply.get("code") != 0:
            detail = (reply.get("stderr") or stdout or "").strip()
            raise BrokerError(500, f"`audible {' '.join(argv)}` failed: {detail[:300]}")
        if not stdout.strip():
            return empty
        return json.loads(stdout)

    def library(self):
        # `library list` is the (mp, asin) source of truth; the export's
        # raw docs contribute display fields (cover, authors, …), joined
        # by asin. A same-asin title in two marketplaces shares one doc —
        # fine for display.
        listing = self._invoke_json(["library", "list", "--limit", "0"], empty=[])
        export = self._invoke_json(["library", "export"], empty={})
        docs = {}
        for doc in export.get("items", []):
            docs.setdefault(doc.get("asin"), doc)
        items = []
        for row in listing:
            slim = slim_item(docs.get(row.get("asin"), {}))
            slim["asin"] = row.get("asin")
            slim["marketplace"] = row.get("mp")
            slim["title"] = slim["title"] or row.get("title")
            items.append(slim)
        return {"count": len(items), "items": items}

    def downloads(self):
        return self._invoke_json(["db", "downloads", "list"], empty=[])


class Handler(BaseHTTPRequestHandler):
    server_version = "audible-gui/0.1"
    # Injected by serve(): the shared Api and the session token.
    api = None
    token = None

    def log_message(self, fmt, *args):  # keep the terminal quiet
        pass

    def _authorized(self):
        query = parse_qs(urlparse(self.path).query)
        sent = (query.get("token") or [self.headers.get("X-Token", "")])[0]
        return hmac.compare_digest(sent, self.token)

    def _reply(self, status, content_type, body):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def _reply_json(self, status, value):
        self._reply(status, "application/json", json.dumps(value).encode())

    def do_GET(self):
        if not self._authorized():
            self._reply_json(403, {"error": "missing or wrong token"})
            return
        route = urlparse(self.path).path
        try:
            if route == "/":
                page = PAGE.replace("__TOKEN__", self.token)
                self._reply(200, "text/html; charset=utf-8", page.encode())
            elif route == "/api/library":
                self._reply_json(200, self.api.library())
            elif route == "/api/downloads":
                self._reply_json(200, self.api.downloads())
            else:
                self._reply_json(404, {"error": f"no route {route}"})
        except BrokerError as error:
            self._reply_json(502, {"error": str(error)})
        except Exception as error:  # surface, never crash the server thread
            self._reply_json(500, {"error": f"{type(error).__name__}: {error}"})


def main(argv):
    parser = argparse.ArgumentParser(prog="audible gui")
    parser.add_argument(
        "--no-open",
        action="store_true",
        help="do not open the browser automatically (URL is printed either way)",
    )
    parser.add_argument(
        "--port",
        type=int,
        default=0,
        help="fixed port instead of a random free one",
    )
    args = parser.parse_args(argv)

    Handler.api = Api(Broker())
    Handler.token = secrets.token_urlsafe(16)

    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    url = f"http://127.0.0.1:{server.server_address[1]}/?token={Handler.token}"
    print(f"audible gui listening on {url}", flush=True)
    print("Ctrl-C stops the dashboard.", flush=True)
    if not args.no_open:
        threading.Timer(0.3, webbrowser.open, [url]).start()
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\nbye")
    finally:
        server.server_close()
    return 0


# --- embedded single-page app (no dependencies, no CDN) ---------------

PAGE = r"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>audible — library</title>
<style>
  :root { color-scheme: light dark;
          --ok: #2e9e5b; --miss: #b3b3b3; --accent: #f29d38; }
  * { box-sizing: border-box; }
  body { margin: 0; font: 14px/1.45 system-ui, sans-serif; }
  header { display: flex; gap: .8rem; align-items: center; flex-wrap: wrap;
           padding: .7rem 1rem; border-bottom: 1px solid color-mix(in srgb, currentColor 18%, transparent);
           position: sticky; top: 0; backdrop-filter: blur(6px);
           background: color-mix(in srgb, Canvas 82%, transparent); }
  header h1 { font-size: 1.05rem; margin: 0 .4rem 0 0; }
  header input[type=search] { flex: 1 1 14rem; max-width: 26rem; padding: .35rem .6rem;
           border: 1px solid color-mix(in srgb, currentColor 25%, transparent);
           border-radius: .5rem; background: transparent; color: inherit; }
  header label { display: inline-flex; gap: .3rem; align-items: center; cursor: pointer; }
  header .count { opacity: .7; margin-left: auto; }
  button { cursor: pointer; border: 1px solid color-mix(in srgb, currentColor 25%, transparent);
           background: transparent; color: inherit; border-radius: .5rem; padding: .35rem .7rem; }
  button:hover { border-color: var(--accent); }
  table { width: 100%; border-collapse: collapse; }
  th, td { padding: .45rem .6rem; text-align: left; vertical-align: middle;
           border-bottom: 1px solid color-mix(in srgb, currentColor 10%, transparent); }
  th { font-size: .78rem; text-transform: uppercase; letter-spacing: .04em; opacity: .65;
       position: sticky; top: 3.3rem; background: Canvas; }
  td.cover { width: 3.2rem; } td.cover img { width: 2.8rem; height: 2.8rem;
       object-fit: cover; border-radius: .35rem; display: block; }
  td.title b { display: block; } td.title small { opacity: .65; }
  td.mono { font-family: ui-monospace, monospace; font-size: .8rem; white-space: nowrap; }
  .badge { display: inline-block; margin: 0 .18rem .18rem 0; padding: .1rem .5rem;
           border-radius: 1rem; font-size: .74rem; border: 1px solid transparent; }
  .badge.ok { background: color-mix(in srgb, var(--ok) 18%, transparent);
              border-color: var(--ok); }
  .badge.miss { opacity: .55; border-color: var(--miss); border-style: dashed; }
  .archived td { opacity: .5; }
  #status { padding: 2rem; text-align: center; opacity: .7; }
</style>
</head>
<body>
<header>
  <h1>audible library</h1>
  <input id="q" type="search" placeholder="filter title / author / asin…">
  <label><input id="only-missing" type="checkbox"> only incomplete</label>
  <label><input id="show-archived" type="checkbox"> show archived</label>
  <button id="reload">Reload</button>
  <span class="count" id="count"></span>
</header>
<div id="status">loading library…</div>
<table id="grid" hidden>
  <thead><tr>
    <th></th><th>Title</th><th>Authors</th><th>MP</th><th>ASIN</th>
    <th>Runtime</th><th>Downloads</th>
  </tr></thead>
  <tbody></tbody>
</table>
<script>
const TOKEN = "__TOKEN__";
const KINDS = ["audio", "cover", "chapter", "pdf"];
let items = [], downloads = new Map();

async function fetchJson(path) {
  const reply = await fetch(path + "?token=" + TOKEN);
  const body = await reply.json();
  if (!reply.ok) throw new Error(body.error || reply.status);
  return body;
}

function runtime(min) {
  if (min == null) return "";
  return Math.floor(min / 60) + "h " + String(min % 60).padStart(2, "0") + "m";
}

function render() {
  const q = document.getElementById("q").value.trim().toLowerCase();
  const onlyMissing = document.getElementById("only-missing").checked;
  const showArchived = document.getElementById("show-archived").checked;
  const body = document.querySelector("#grid tbody");
  body.replaceChildren();
  let shown = 0;
  for (const item of items) {
    if (item.is_archived && !showArchived) continue;
    const hay = (item.title + " " + (item.subtitle || "") + " "
                 + item.authors.join(" ") + " " + item.asin).toLowerCase();
    if (q && !hay.includes(q)) continue;
    const have = downloads.get(item.asin) || new Map();
    if (onlyMissing && KINDS.every(k => have.has(k))) continue;
    shown++;
    const row = document.createElement("tr");
    if (item.is_archived) row.className = "archived";
    const badges = KINDS.map(kind => {
      const formats = have.get(kind);
      return formats
        ? `<span class="badge ok" title="${[...formats].join(", ")}">${kind}</span>`
        : `<span class="badge miss">${kind}</span>`;
    }).join("");
    row.innerHTML = `
      <td class="cover">${item.cover ? `<img loading="lazy" src="${item.cover}" alt="">` : ""}</td>
      <td class="title"><b></b><small></small></td>
      <td></td>
      <td class="mono">${item.marketplace || ""}</td>
      <td class="mono">${item.asin}</td>
      <td>${runtime(item.runtime_min)}</td>
      <td>${badges}</td>`;
    row.querySelector("b").textContent = item.title || "";
    row.querySelector("small").textContent = item.subtitle || "";
    row.children[2].textContent = item.authors.join(", ");
    body.appendChild(row);
  }
  document.getElementById("count").textContent = shown + " / " + items.length + " titles";
}

async function load() {
  const status = document.getElementById("status"), grid = document.getElementById("grid");
  status.hidden = false; grid.hidden = true;
  status.textContent = "loading library… (first load may run a delta sync)";
  try {
    const [library, dls] = await Promise.all([
      fetchJson("/api/library"), fetchJson("/api/downloads"),
    ]);
    items = library.items;
    downloads = new Map();
    for (const row of dls) {
      // `db downloads list` rows carry no marketplace column — the
      // per-account database keys artifacts by asin, good enough here.
      if (!downloads.has(row.asin)) downloads.set(row.asin, new Map());
      const perKind = downloads.get(row.asin);
      if (!perKind.has(row.kind)) perKind.set(row.kind, new Set());
      perKind.get(row.kind).add((row.format || "?")
                                + (row.variant && row.variant !== "original" ? ` (${row.variant})` : ""));
    }
    status.hidden = true; grid.hidden = false;
    render();
  } catch (error) {
    status.textContent = "error: " + error.message;
  }
}

for (const id of ["q", "only-missing", "show-archived"])
  document.getElementById(id).addEventListener("input", render);
document.getElementById("reload").addEventListener("click", load);
load();
</script>
</body>
</html>
"""


if __name__ == "__main__":
    raise SystemExit(run(MANIFEST, main))
