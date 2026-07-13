"""library-gui plugin (AUD-161): local web dashboard for the library.

Serves a read-only single-page dashboard on 127.0.0.1 showing the
library with per-item download status — which kinds (audio / cover /
chapter / pdf) are already recorded in which formats, and which are
still missing — plus an expandable detail panel per title, sortable
columns and marketplace/ownership filters. Data comes exclusively from
the ephemeral plugin broker via ``invoke`` (re-entrancy, scope
``invoke``); the plugin never sees auth material.

Install: ``audible plugin add [--symlink] <path to this file>`` (or the
raw GitHub URL) and make ``audible_plugin_sdk`` importable
(``pip install <repo>/sdk/python`` or PYTHONPATH). Then::

    audible -m all library-gui [--no-open] [--port N]

The session is pinned to the invoking ``-a``/``-m``/``-s`` selection
(one account per GUI session) — start with ``-m all`` to see every
marketplace of the account. Tip for a sync-free dashboard: a settings
bundle with ``[settings.gui.db] auto_sync = "none"`` started as
``audible -m all -s gui library-gui``. Stop with Ctrl-C.
"""

import argparse
import hmac
import json
import secrets
import sys
import threading
import webbrowser
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

MANIFEST = {
    "name": "library-gui",
    "version": "0.1.0",
    "description": "Local web dashboard: library + download status",
    "scopes": ["invoke"],
    "help": "usage: audible [-m all] library-gui [--no-open] [--port N]",
}

# Answer the discovery probe before importing the SDK: the manifest
# needs nothing from the broker, so `plugin list` shows this plugin as
# intact even when audible_plugin_sdk is not importable (yet).
if __name__ == "__main__" and "--audible-describe" in sys.argv:
    print(json.dumps(MANIFEST))
    raise SystemExit(0)

try:
    from audible_plugin_sdk import Broker, BrokerError, run
except ImportError:
    print(
        "error: audible_plugin_sdk is not importable — install it with\n"
        "  pip install <audible-rs repo>/sdk/python\n"
        "or add <audible-rs repo>/sdk/python to PYTHONPATH.",
        file=sys.stderr,
    )
    raise SystemExit(2) from None

#: Artifact kinds tracked in the downloads table (db/schema.rs).
KINDS = ("audio", "cover", "chapter", "pdf")


def slim_item(doc):
    """The doc fields the table and the per-item detail panel need —
    keeps the browser payload small (full docs easily reach several MB
    per library)."""

    def names(field):
        return [n.get("name", "") for n in (doc.get(field) or []) if n.get("name")]

    images = doc.get("product_images") or {}
    cover = images.get("500") or next(iter(images.values()), None)
    series = (doc.get("series") or [{}])[0]
    rating = (doc.get("rating") or {}).get("overall_distribution") or {}
    ladders = [
        " › ".join(c.get("name", "") for c in (ladder.get("ladder") or []))
        for ladder in doc.get("category_ladders") or []
    ]
    return {
        "title": doc.get("title"),
        "subtitle": doc.get("subtitle"),
        "authors": names("authors"),
        "narrators": names("narrators"),
        "publisher": doc.get("publisher_name"),
        "language": doc.get("language"),
        "release_date": doc.get("release_date") or doc.get("issue_date"),
        "purchase_date": doc.get("purchase_date"),
        "runtime_min": doc.get("runtime_length_min"),
        "series": series.get("title"),
        "series_sequence": series.get("sequence"),
        "rating": rating.get("display_average_rating"),
        "num_ratings": rating.get("num_ratings"),
        "summary": doc.get("publisher_summary"),
        "categories": [ladder for ladder in ladders if ladder],
        "is_archived": bool(doc.get("is_archived")),
        # Same ownership marker as `library list --borrowed` (AUD-153):
        # origin_type == "Purchase" is owned; absent/other = borrowed.
        "owned": doc.get("origin_type") == "Purchase",
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
        # fine for display. `--kind book,podcast`: the dashboard shows
        # books and podcast shows; standalone episodes are deliberately
        # left out to keep the view uncluttered (AUD-173).
        listing = self._invoke_json(
            ["library", "list", "--limit", "0", "--kind", "book,podcast"], empty=[]
        )
        export = self._invoke_json(
            ["library", "export", "--kind", "book,podcast"], empty={}
        )
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
    server_version = "audible-library-gui/0.1"
    # Injected by main(): the shared Api and the session token.
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
                self._reply(200, "text/html; charset=utf-8", PAGE.encode())
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
    parser = argparse.ArgumentParser(
        prog="audible library-gui",
        epilog="Account/marketplace/settings are fixed when the plugin starts: "
        "put the selectors BEFORE the plugin name, e.g. `audible -m all library-gui`.",
    )
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
    # Catch selector flags placed after the plugin name — the broker has
    # already pinned the invoking -a/-m/-s selection (AUD-123), so guide
    # the user instead of failing with "unrecognized arguments".
    for flags in (("-a", "--account"), ("-m", "--marketplace"), ("-s", "--settings")):
        parser.add_argument(*flags, dest="selector", help=argparse.SUPPRESS)
    args = parser.parse_args(argv)
    if args.selector is not None:
        parser.error(
            "selectors are fixed when the plugin starts — put them before "
            "the plugin name: audible -m all library-gui"
        )

    Handler.api = Api(Broker())
    Handler.token = secrets.token_urlsafe(16)

    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    url = f"http://127.0.0.1:{server.server_address[1]}/?token={Handler.token}"
    print(f"audible library-gui listening on {url}", flush=True)
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


# --- embedded single-page app (no dependencies, no CDN; the JS reads
# --- the session token from the page URL) -----------------------------

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
  header select { padding: .3rem .5rem; border-radius: .5rem; color: inherit;
       background: transparent;
       border: 1px solid color-mix(in srgb, currentColor 25%, transparent); }
  header select option { color: CanvasText; background: Canvas; }
  #mps { display: inline-flex; gap: .35rem; }
  .mp-chip { padding: .15rem .5rem; border-radius: 1rem; font-size: .82rem;
       border: 1px solid color-mix(in srgb, currentColor 25%, transparent); }
  .mp-chip:has(input:checked) { background: color-mix(in srgb, var(--accent) 18%, transparent);
       border-color: var(--accent); }
  .mp-chip input { display: none; }
  button { cursor: pointer; border: 1px solid color-mix(in srgb, currentColor 25%, transparent);
           background: transparent; color: inherit; border-radius: .5rem; padding: .35rem .7rem; }
  button:hover { border-color: var(--accent); }
  table { width: 100%; border-collapse: collapse; }
  th, td { padding: .45rem .6rem; text-align: left; vertical-align: middle;
           border-bottom: 1px solid color-mix(in srgb, currentColor 10%, transparent); }
  th { font-size: .78rem; text-transform: uppercase; letter-spacing: .04em; opacity: .65;
       position: sticky; top: 3.3rem; background: Canvas; }
  th.sortable { cursor: pointer; user-select: none; }
  th.sortable:hover { opacity: 1; }
  th.sortable::after { content: "↕"; margin-left: .3rem; opacity: .4; }
  th.sortable.asc::after { content: "▲"; opacity: 1; }
  th.sortable.desc::after { content: "▼"; opacity: 1; }
  th.sortable.asc, th.sortable.desc { opacity: 1; color: var(--accent); }
  td.cover { width: 3.2rem; } td.cover img { width: 2.8rem; height: 2.8rem;
       object-fit: cover; border-radius: .35rem; display: block; }
  td.title b { display: block; } td.title small { opacity: .65; }
  td.authors { max-width: 16rem; }
  td.authors span { display: -webkit-box; -webkit-line-clamp: 2;
       -webkit-box-orient: vertical; overflow: hidden; }
  td.mono { font-family: ui-monospace, monospace; font-size: .8rem; white-space: nowrap; }
  tr.item { cursor: pointer; }
  tr.item:hover td { background: color-mix(in srgb, currentColor 5%, transparent); }
  tr.item.open td { border-bottom-color: transparent; }
  tr.detail > td { padding: 0 .6rem .9rem; }
  .detail-panel { display: flex; gap: 1rem; padding: .9rem;
       border: 1px solid color-mix(in srgb, currentColor 14%, transparent);
       border-radius: .6rem;
       background: color-mix(in srgb, currentColor 4%, transparent); }
  .detail-panel > img { width: 10rem; height: 10rem; object-fit: cover;
       border-radius: .5rem; flex-shrink: 0; }
  .detail-panel .body { flex: 1; min-width: 0; }
  .detail-panel .fields { margin: 0; display: grid;
       grid-template-columns: max-content 1fr; gap: .15rem 1rem;
       align-content: start; font-size: .85rem; }
  .detail-panel .fields dt { opacity: .6; }
  .detail-panel .fields dd { margin: 0; }
  .detail-panel .summary { margin: .6rem 0 0; font-size: .85rem;
       line-height: 1.5; max-height: 9rem; overflow-y: auto; opacity: .9; }
  .detail-files { margin-top: .6rem; font-family: ui-monospace, monospace;
       font-size: .76rem; opacity: .85; overflow-x: auto; white-space: nowrap; }
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
  <select id="ownership" title="ownership filter">
    <option value="all">all</option>
    <option value="owned">owned</option>
    <option value="borrowed">borrowed</option>
  </select>
  <span id="mps" title="show/hide marketplaces"></span>
  <label><input id="only-missing" type="checkbox"> only incomplete</label>
  <label><input id="show-archived" type="checkbox"> show archived</label>
  <button id="reload">Reload</button>
  <span class="count" id="count"></span>
</header>
<div id="status">loading library…</div>
<table id="grid" hidden>
  <thead><tr>
    <th></th>
    <th class="sortable" data-key="title">Title</th>
    <th class="sortable" data-key="authors">Authors</th>
    <th class="sortable" data-key="mp">MP</th>
    <th class="sortable" data-key="asin">ASIN</th>
    <th class="sortable" data-key="runtime">Runtime</th>
    <th>Downloads</th>
  </tr></thead>
  <tbody></tbody>
</table>
<script>
const TOKEN = new URLSearchParams(location.search).get("token") || "";
const KINDS = ["audio", "cover", "chapter", "pdf"];
let items = [], downloads = new Map(), downloadRows = new Map(), openAsin = null;
let sort = { key: null, dir: 1 };
let hiddenMps = new Set();

// Sort keys per column; null/absent values sort last in either direction.
const SORT_VALUE = {
  title: item => (item.title || "").toLowerCase(),
  authors: item => item.authors.join(", ").toLowerCase(),
  mp: item => item.marketplace || "",
  asin: item => item.asin || "",
  runtime: item => item.runtime_min,
};

function sortedItems() {
  if (!sort.key) return items;
  const value = SORT_VALUE[sort.key];
  return [...items].sort((a, b) => {
    const va = value(a), vb = value(b);
    if (va == null || va === "") return 1;
    if (vb == null || vb === "") return -1;
    const cmp = typeof va === "number"
      ? va - vb
      : va.localeCompare(vb, undefined, { sensitivity: "base", numeric: true });
    return sort.dir * cmp;
  });
}

function setSort(key) {
  sort = { key, dir: sort.key === key ? -sort.dir : 1 };
  for (const th of document.querySelectorAll("th.sortable")) {
    th.classList.toggle("asc", th.dataset.key === key && sort.dir === 1);
    th.classList.toggle("desc", th.dataset.key === key && sort.dir === -1);
  }
  render();
}

// One show/hide chip per marketplace present in the library.
function renderMpFilter() {
  const box = document.getElementById("mps");
  box.replaceChildren();
  for (const mp of [...new Set(items.map(i => i.marketplace))].sort()) {
    if (!mp) continue;
    const label = document.createElement("label");
    label.className = "mp-chip";
    const check = document.createElement("input");
    check.type = "checkbox";
    check.checked = !hiddenMps.has(mp);
    check.addEventListener("input", () => {
      check.checked ? hiddenMps.delete(mp) : hiddenMps.add(mp);
      render();
    });
    label.append(check, mp);
    box.appendChild(label);
  }
}

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

function audibleDomain(mp) {
  const map = { us: "audible.com", uk: "audible.co.uk", de: "audible.de",
                fr: "audible.fr", it: "audible.it", es: "audible.es",
                ca: "audible.ca", au: "audible.com.au", in: "audible.in",
                jp: "audible.co.jp", br: "audible.com.br" };
  return map[mp] || "audible.com";
}

// publisher_summary is publisher-supplied HTML — reduce it to plain text.
function plainText(html) {
  return new DOMParser().parseFromString(html || "", "text/html").body.textContent || "";
}

function field(dl, label, value) {
  if (!value) return;
  const dt = document.createElement("dt");
  dt.textContent = label;
  const dd = document.createElement("dd");
  if (value instanceof Node) dd.appendChild(value); else dd.textContent = value;
  dl.append(dt, dd);
}

// The expandable per-item panel: one <tr class="detail"> spanning the
// table, inserted right below the clicked row.
function detailRow(item) {
  const tr = document.createElement("tr");
  tr.className = "detail";
  const td = document.createElement("td");
  td.colSpan = 7;
  const panel = document.createElement("div");
  panel.className = "detail-panel";

  if (item.cover) {
    const img = document.createElement("img");
    img.src = item.cover;
    img.alt = "";
    panel.appendChild(img);
  }

  const body = document.createElement("div");
  body.className = "body";
  const dl = document.createElement("dl");
  dl.className = "fields";
  field(dl, "authors", item.authors.join(", "));
  field(dl, "narrators", (item.narrators || []).join(", "));
  field(dl, "series", item.series
    ? item.series + (item.series_sequence ? ` #${item.series_sequence}` : "") : "");
  field(dl, "publisher", item.publisher);
  field(dl, "language", item.language);
  field(dl, "released", (item.release_date || "").slice(0, 10));
  field(dl, "purchased", (item.purchase_date || "").slice(0, 10));
  field(dl, "runtime", runtime(item.runtime_min));
  field(dl, "rating", item.rating
    ? `${item.rating} ★${item.num_ratings ? ` (${item.num_ratings} ratings)` : ""}` : "");
  field(dl, "categories", (item.categories || []).join("  ·  "));
  if (item.is_archived) field(dl, "archived", "yes");
  const link = document.createElement("a");
  link.href = `https://www.${audibleDomain(item.marketplace)}/pd/${item.asin}`;
  link.target = "_blank";
  link.rel = "noopener";
  link.textContent = "open on audible ↗";
  field(dl, "page", link);
  body.appendChild(dl);

  const summary = plainText(item.summary).trim();
  if (summary) {
    const p = document.createElement("p");
    p.className = "summary";
    p.textContent = summary;
    body.appendChild(p);
  }

  const rows = downloadRows.get(item.asin) || [];
  if (rows.length) {
    const files = document.createElement("div");
    files.className = "detail-files";
    for (const r of rows) {
      const line = document.createElement("div");
      line.textContent =
        `${r.kind}  ${r.format || "-"}  ${r.variant}${r.size ? `  ${r.size}` : ""}  ${r.path}`;
      files.appendChild(line);
    }
    body.appendChild(files);
  }

  panel.appendChild(body);
  td.appendChild(panel);
  tr.appendChild(td);
  return tr;
}

function toggleDetail(row, item) {
  const wasOpen = openAsin === item.asin;
  document.querySelectorAll("tr.detail").forEach(r => r.remove());
  document.querySelectorAll("tr.item.open").forEach(r => r.classList.remove("open"));
  openAsin = null;
  if (!wasOpen) {
    row.classList.add("open");
    row.after(detailRow(item));
    openAsin = item.asin;
  }
}

function render() {
  const q = document.getElementById("q").value.trim().toLowerCase();
  const ownership = document.getElementById("ownership").value;
  const onlyMissing = document.getElementById("only-missing").checked;
  const showArchived = document.getElementById("show-archived").checked;
  const body = document.querySelector("#grid tbody");
  body.replaceChildren();
  openAsin = null;
  let shown = 0;
  for (const item of sortedItems()) {
    if (item.is_archived && !showArchived) continue;
    if (hiddenMps.has(item.marketplace)) continue;
    if (ownership === "owned" && !item.owned) continue;
    if (ownership === "borrowed" && item.owned) continue;
    const hay = (item.title + " " + (item.subtitle || "") + " "
                 + item.authors.join(" ") + " " + item.asin).toLowerCase();
    if (q && !hay.includes(q)) continue;
    const have = downloads.get(item.asin) || new Map();
    if (onlyMissing && KINDS.every(k => have.has(k))) continue;
    shown++;
    const row = document.createElement("tr");
    row.className = "item" + (item.is_archived ? " archived" : "");
    const badges = KINDS.map(kind => {
      const formats = have.get(kind);
      return formats
        ? `<span class="badge ok" title="${[...formats].join(", ")}">${kind}</span>`
        : `<span class="badge miss">${kind}</span>`;
    }).join("");
    row.innerHTML = `
      <td class="cover">${item.cover ? `<img loading="lazy" src="${item.cover}" alt="">` : ""}</td>
      <td class="title"><b></b><small></small></td>
      <td class="authors"><span></span></td>
      <td class="mono">${item.marketplace || ""}</td>
      <td class="mono">${item.asin}</td>
      <td>${runtime(item.runtime_min)}</td>
      <td>${badges}</td>`;
    row.querySelector("b").textContent = item.title || "";
    row.querySelector("small").textContent = item.subtitle || "";
    row.querySelector("td.authors span").textContent = item.authors.join(", ");
    row.addEventListener("click", () => toggleDetail(row, item));
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
    downloadRows = new Map();
    for (const row of dls) {
      // `db downloads list` rows carry no marketplace column — the
      // per-account database keys artifacts by asin, good enough here.
      if (!downloads.has(row.asin)) downloads.set(row.asin, new Map());
      const perKind = downloads.get(row.asin);
      if (!perKind.has(row.kind)) perKind.set(row.kind, new Set());
      perKind.get(row.kind).add((row.format || "?")
                                + (row.variant && row.variant !== "original" ? ` (${row.variant})` : ""));
      if (!downloadRows.has(row.asin)) downloadRows.set(row.asin, []);
      downloadRows.get(row.asin).push(row);
    }
    status.hidden = true; grid.hidden = false;
    renderMpFilter();
    render();
  } catch (error) {
    status.textContent = "error: " + error.message;
  }
}

for (const id of ["q", "ownership", "only-missing", "show-archived"])
  document.getElementById(id).addEventListener("input", render);
for (const th of document.querySelectorAll("th.sortable"))
  th.addEventListener("click", () => setSort(th.dataset.key));
document.getElementById("reload").addEventListener("click", load);
load();
</script>
</body>
</html>
"""


if __name__ == "__main__":
    raise SystemExit(run(MANIFEST, main))
