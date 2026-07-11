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
`sdk/python` on `PYTHONPATH`). A complete example lives in
`examples/plugins/cmd_listening-stats.py`. PyPI publishing happens with
the release (M8).
