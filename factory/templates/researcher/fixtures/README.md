# Rates

Rates converts an amount of money from one currency to another at today's exchange rate,
which it gets from an exchange-rate service, and prints the result.

## Starting it

Rates needs Python 3.9 or later, and nothing else. From this folder, run:

    python src/main.py 120 EUR USD

It prints the amount converted, for example `120.00 EUR = 130.44 USD`. Without the second
currency, it converts to the default one in `config.ini`.

## Settings

`config.ini` holds the currency to convert to by default, how many decimals to print, and
how long to wait for the exchange-rate service.
