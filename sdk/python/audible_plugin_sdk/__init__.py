"""SDK for audible-rs plugins (AUD-70) — stdlib only.

Speaks the broker protocol of AUD-69: HTTP/1.1 over the unix socket the
CLI passes in ``AUDIBLE_SOCKET``, authorized per request with the bearer
token from ``AUDIBLE_BROKER_TOKEN``. The CLI only starts the broker for
plugins whose manifest declares at least one scope; every call the scope
does not cover fails with :class:`BrokerError` (HTTP 403).

Typical plugin::

    from audible_plugin_sdk import Broker, run

    MANIFEST = {
        "name": "listening-stats",
        "version": "1.0",
        "description": "Monthly listening minutes",
        "scopes": ["api"],
    }

    def main(argv):
        broker = Broker()
        reply = broker.api_request("/1.0/stats/aggregates", query={...})
        ...
        return 0

    if __name__ == "__main__":
        raise SystemExit(run(MANIFEST, main))
"""

import http.client
import json
import os
import socket
import sys

__all__ = ["Broker", "BrokerError", "NotUnderAudible", "PluginError", "run"]


class PluginError(Exception):
    """Base class of every SDK error."""


class NotUnderAudible(PluginError):
    """The broker env pair is missing — not started via the audible CLI."""


class BrokerError(PluginError):
    """The broker answered with an error status (401/403/4xx/5xx)."""

    def __init__(self, status, message):
        super().__init__(message)
        self.status = status


class _UnixHTTPConnection(http.client.HTTPConnection):
    """`http.client` over an ``AF_UNIX`` socket (host is decorative)."""

    def __init__(self, path):
        super().__init__("broker")
        self._path = path

    def connect(self):
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.connect(self._path)
        self.sock = sock


class Broker:
    """Client for the CLI's per-invocation broker socket."""

    def __init__(self):
        self.socket_path = os.environ.get("AUDIBLE_SOCKET")
        self._token = os.environ.get("AUDIBLE_BROKER_TOKEN")
        if not self.socket_path or not self._token:
            raise NotUnderAudible(
                "AUDIBLE_SOCKET / AUDIBLE_BROKER_TOKEN are not set - run this "
                "plugin through the audible CLI (and declare a scope in the "
                "manifest)"
            )

    def _request(self, method, path, payload=None, selectors=None):
        connection = _UnixHTTPConnection(self.socket_path)
        headers = {
            "Authorization": f"Bearer {self._token}",
            "Content-Type": "application/json",
        }
        # Selector headers (AUD-125). Under a plugin's ephemeral broker
        # they are ignored (the invoking -a/-m/-s wins, AUD-123); against
        # the agent they select fail-closed (403/400, never substitution).
        for name, value in (selectors or {}).items():
            if value is not None:
                headers[f"X-Audible-{name.capitalize()}"] = value
        try:
            connection.request(
                method,
                path,
                body=json.dumps(payload) if payload is not None else None,
                headers=headers,
            )
            response = connection.getresponse()
            data = json.loads(response.read() or b"{}")
        finally:
            connection.close()
        if response.status >= 400:
            raise BrokerError(response.status, data.get("error", f"HTTP {response.status}"))
        return data

    @staticmethod
    def _selectors(account, marketplace, settings):
        return {"account": account, "marketplace": marketplace, "settings": settings}

    def api_request(
        self,
        path,
        method="GET",
        query=None,
        body=None,
        account=None,
        marketplace=None,
        settings=None,
    ):
        """One Audible API call (scope ``api``).

        Returns ``{"status": <upstream http status>, "body": <json>}``.
        ``path`` is an API path like ``/1.0/library``, or — with the
        ``hosts`` scope and the host on the user's allowlist — an
        ``https://`` URL. ``account``/``marketplace``/``settings`` are
        selector headers: honored by the agent, ignored under a plugin's
        ephemeral broker.
        """
        payload = {"path": path, "method": method}
        if query is not None:
            payload["query"] = query
        if body is not None:
            payload["body"] = body
        return self._request(
            "POST",
            "/v1/api/request",
            payload,
            selectors=self._selectors(account, marketplace, settings),
        )

    def invoke(self, argv, output="json", account=None, marketplace=None, settings=None):
        """Run a **built-in** audible command and capture its output
        (scope ``invoke``; AUD-114 re-entrancy).

        ``argv`` is the command line after ``audible`` (e.g.
        ``["library", "list", "--limit", "5"]``); the broker injects the
        selected account/settings/marketplace and ``-o json`` (override
        with ``output=``). Returns
        ``{"code": int, "stdout": str, "stderr": str}`` — with the
        default JSON output, ``json.loads(reply["stdout"])`` is the data.
        """
        return self._request(
            "POST",
            "/v1/invoke",
            {"argv": list(argv), "output": output},
            selectors=self._selectors(account, marketplace, settings),
        )

    def config_resolved(self, account=None, settings=None):
        """The effective settings view, secret-free (scope ``config``)."""
        return self._request(
            "GET",
            "/v1/config/resolved",
            selectors=self._selectors(account, None, settings),
        )

    def accounts(self):
        """Configured accounts: names, marketplace axes and each
        account's effective default settings bundle (scope ``config``)."""
        return self._request("GET", "/v1/accounts")["accounts"]

    def settings_bundles(self):
        """All selectable settings-bundle names — valid values for the
        ``settings`` selector (scope ``config``)."""
        return self._request("GET", "/v1/accounts")["settings"]


def run(manifest, main):
    """Plugin entry point: answers ``--audible-describe``, else runs
    ``main(argv)`` and maps SDK errors to exit codes (2 = not under
    audible, 1 = broker error)."""
    if len(sys.argv) > 1 and sys.argv[1] == "--audible-describe":
        json.dump(manifest, sys.stdout)
        return 0
    try:
        return main(sys.argv[1:]) or 0
    except NotUnderAudible as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    except BrokerError as error:
        print(f"error: broker returned {error.status}: {error}", file=sys.stderr)
        return 1
