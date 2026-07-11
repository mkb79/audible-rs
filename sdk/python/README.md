# audible-plugin-sdk

Stdlib-only Python client for the audible-rs plugin broker (AUD-69/70).

A plugin is a `cmd_<name>.py` script or an `audible-<name>` executable
in the plugin dir (default `<data_dir>/plugins`, config `[plugins] dir`)
— the plugin dir is the **only** place audible looks; `PATH` is not
scanned. It must answer `--audible-describe` with its manifest JSON;
`audible_plugin_sdk.run(manifest, main)` does that for you. Declared
manifest scopes (`api`, `download`, `config`, `invoke`) decide what the
broker lets the plugin do — the plugin never sees auth material.

Install a plugin with `audible plugin add <file-or-https-url>` — the
manifest is verified before anything lands; https installs additionally
show url, size, sha256 and the requested scopes and ask for
confirmation (`--yes` skips). The official example plugins install
straight from the repo, e.g.:

    audible plugin add https://raw.githubusercontent.com/mkb79/audible-rs/main/examples/plugins/cmd_listening-stats.py

(a `main` URL may be newer than your installed binary — pin a release
via its tag URL, `…/vX.Y.Z/examples/…`). During development
`audible plugin add --symlink <file>` links instead of copying — edits
to the original apply immediately, but moving or deleting the original
breaks the plugin (`plugin list` shows it as `broken: symlink target
missing`). `audible plugin remove <name>` deletes only the plugin-dir
entry, never a symlink's original.

```python
from audible_plugin_sdk import Broker, run

MANIFEST = {"name": "hello", "version": "1.0", "scopes": ["api"]}

def main(argv):
    reply = Broker().api_request("/1.0/library", query={"num_results": "3"})
    print(reply["body"])
    return 0

if __name__ == "__main__":
    raise SystemExit(run(MANIFEST, main))
```

Install for development: `pip install <repo>/sdk/python` (or put
`sdk/python` on `PYTHONPATH`). Complete examples live in
`examples/plugins/` (`cmd_listening-stats.py` — scope `api`;
`cmd_library-gui.py` — a local web dashboard on scope `invoke`). PyPI
publishing happens with the release (M8).

## Plugin authoring patterns

Lessons the example plugins encode — copy them.

**Answer `--audible-describe` before heavy imports.** Discovery,
`plugin list` and the `plugin add` verification all probe your file
with `--audible-describe`; the manifest needs nothing from the broker.
If your module imports the SDK (or any dependency) at the top, the
probe dies before it can print the manifest and your plugin shows up as
broken. Instead:

```python
if __name__ == "__main__" and "--audible-describe" in sys.argv:
    print(json.dumps(MANIFEST))
    raise SystemExit(0)

try:
    from audible_plugin_sdk import Broker, run
except ImportError:
    print("error: audible_plugin_sdk is not importable — …", file=sys.stderr)
    raise SystemExit(2) from None
```

**Selectors go before the plugin name.** Everything after the plugin
name on the command line belongs to your plugin, and the broker pins
the invoking `-a`/`-m`/`-s` selection before your process starts — a
trailing `-m all` cannot work. If you do not define your own selector
flags, catch them and guide the user instead of letting argparse fail:

```python
for flags in (("-a", "--account"), ("-m", "--marketplace"), ("-s", "--settings")):
    parser.add_argument(*flags, dest="selector", help=argparse.SUPPRESS)
...
if args.selector is not None:
    parser.error("selectors are fixed when the plugin starts — put them "
                 "before the plugin name: audible -m all <plugin>")
```

**Recommend a settings bundle instead of changing global config.**
Invoked built-ins honour the user's `[db]` settings (`auto_sync`, …)
exactly like terminal runs. If your plugin wants different behaviour —
a dashboard that must never sync implicitly, say — document a bundle:

```toml
[settings.gui]
[settings.gui.db]
auto_sync = "none"
```

and start it as `audible -m all -s gui <plugin>`: every invoke of that
session inherits the bundle, the user's normal CLI behaviour stays
untouched.
