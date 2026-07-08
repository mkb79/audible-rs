"""Reference plugin (AUD-70): monthly listening minutes.

Port of audible-cli's ``cmd_listening-stats.py`` onto the audible-rs
plugin broker. Install: copy this file into the plugin dir (default
``<data_dir>/plugins``) and make ``audible_plugin_sdk`` importable
(``pip install <repo>/sdk/python`` or PYTHONPATH). Then::

    audible listening-stats [--year 2026] [-m de]

Scope ``api`` only — the plugin never sees auth material; the broker
signs the request with the account of the invoking CLI run.
"""

import argparse
import datetime

from audible_plugin_sdk import Broker, run

MANIFEST = {
    "name": "listening-stats",
    "version": "1.0.0",
    "description": "Monthly listening minutes from /1.0/stats/aggregates",
    "scopes": ["api"],
    "help": "usage: audible listening-stats [--year YYYY] [--marketplace CC]",
}


def main(argv):
    parser = argparse.ArgumentParser(prog="audible listening-stats")
    parser.add_argument("--year", type=int, default=datetime.date.today().year)
    parser.add_argument("--marketplace", "-m", default=None)
    args = parser.parse_args(argv)

    broker = Broker()
    reply = broker.api_request(
        "/1.0/stats/aggregates",
        marketplace=args.marketplace,
        query={
            "monthly_listening_interval_duration": "12",
            "monthly_listening_interval_start_date": f"{args.year}-01",
            "store": "Audible",
        },
    )
    if reply["status"] != 200:
        print(f"error: stats endpoint returned {reply['status']}: {reply['body']}")
        return 1

    months = reply["body"].get("aggregated_monthly_listening_stats", [])
    total = 0
    for stat in months:
        minutes = int(stat.get("aggregated_sum") or 0) // 60000  # ms → min
        total += minutes
        print(f"{stat.get('interval_identifier')}  {minutes:6d} min")
    print(f"{args.year} total  {total:6d} min ({total // 60}h {total % 60}m)")
    return 0


if __name__ == "__main__":
    raise SystemExit(run(MANIFEST, main))
