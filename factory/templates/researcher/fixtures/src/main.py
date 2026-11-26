"""Rates: converts an amount of money between currencies at today's exchange rate."""

import configparser
import json
import sys
import urllib.parse
import urllib.request
from pathlib import Path

RATES_URL = "https://api.example.test/rates"
SETTINGS = Path(__file__).resolve().parent.parent / "config.ini"


def load_settings(path=SETTINGS):
    """The [rates] section of config.ini."""
    parser = configparser.ConfigParser()
    parser.read(path, encoding="utf-8")
    return parser["rates"]


def fetch_rate(source, target, timeout):
    """Today's rate from currency `source` to `target`, from the exchange-rate service."""
    query = urllib.parse.urlencode({"from": source, "to": target})
    with urllib.request.urlopen(f"{RATES_URL}?{query}", timeout=timeout) as response:
        return float(json.load(response)["rate"])


def main(argv):
    """Converts the amount the command line gives: AMOUNT FROM [TO]."""
    if len(argv) not in (3, 4):
        print("usage: python src/main.py AMOUNT FROM [TO]", file=sys.stderr)
        return 2
    settings = load_settings()
    amount, source = float(argv[1]), argv[2].upper()
    target = argv[3].upper() if len(argv) == 4 else settings["default_target"]
    rate = fetch_rate(source, target, settings.getfloat("timeout"))
    decimals = settings.getint("decimals")
    print(f"{amount:.{decimals}f} {source} = {amount * rate:.{decimals}f} {target}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
