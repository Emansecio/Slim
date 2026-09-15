"""Two held-out native scenarios: repository scale and SQL fan-out correctness."""
import daily

MONEY = '''def round_half_up_ratio(value, numerator, denominator):
    return (value * numerator * 2 + denominator) // (denominator * 2)
'''
CALLER = '''from shop.orders.totals import quote

def checkout_total(lines):
    return quote(lines, discount_bps=1000, tax_bps=500)['total_cents']
'''
REPO_FILES = {
    'shop/__init__.py': '', 'shop/orders/__init__.py': '',
    'shop/orders/money.py': MONEY, 'shop/api.py': CALLER,
    'shop/orders/totals.py': '''from .money import round_half_up_ratio

def quote(lines, discount_bps=0, tax_bps=0):
    subtotal = sum(row['price_cents'] for row in lines)
    discount = round_half_up_ratio(subtotal, discount_bps, 10000)
    tax = round_half_up_ratio(subtotal, tax_bps, 10000)
    return dict(subtotal_cents=subtotal, discount_cents=discount,
                tax_cents=tax, total_cents=subtotal + tax - discount)
''',
}
for i in range(80):
    REPO_FILES[f'archives/component_{i:03}.py'] = f'# Archived component {i}; unrelated to checkout.\n' + ('# Historical implementation notes.\n' * 40)
REPO_CHECK = '''import copy
from pathlib import Path
from shop.orders.totals import quote
from shop.api import checkout_total

source = [{'price_cents': 101, 'quantity': 3}, {'price_cents': 49, 'quantity': 2}]
before = copy.deepcopy(source)
assert quote(source, 1250, 500) == dict(subtotal_cents=401, discount_cents=50, tax_cents=18, total_cents=369)
assert source == before
assert checkout_total(source) == 379
assert quote([], 10000, 10000) == dict(subtotal_cents=0, discount_cents=0, tax_cents=0, total_cents=0)
assert quote([{'price_cents': 1, 'quantity': 1}], 5000, 5000) == dict(subtotal_cents=1, discount_cents=0, tax_cents=1, total_cents=2)
assert quote([{'price_cents': 100, 'quantity': 2}], 10000, 10000)['total_cents'] == 0
bad_lines = [None, (), [None], [{}], [{'price_cents': True, 'quantity': 1}],
             [{'price_cents': -1, 'quantity': 1}], [{'price_cents': 1.5, 'quantity': 1}],
             [{'price_cents': 1, 'quantity': False}], [{'price_cents': 1, 'quantity': 0}],
             [{'price_cents': 1, 'quantity': -1}], [{'price_cents': 1, 'quantity': 1.5}]]
for invalid in bad_lines:
    try: quote(invalid)
    except ValueError: pass
    else: raise AssertionError(('accepted bad lines', invalid))
for invalid in [True, -1, 10001, 0.5, '5', None]:
    for position in ['discount_bps', 'tax_bps']:
        try: quote(source, **{position: invalid})
        except ValueError: pass
        else: raise AssertionError(('accepted bad rate', position, invalid))
assert source == before
for i in range(80):
    assert Path(f'archives/component_{i:03}.py').read_text(encoding='utf-8') == f'# Archived component {i}; unrelated to checkout.\\n' + '# Historical implementation notes.\\n' * 40
'''
REPO_CHECK += f"assert Path('shop/orders/money.py').read_text(encoding='utf-8') == {MONEY!r}\n"
REPO_CHECK += f"assert Path('shop/api.py').read_text(encoding='utf-8') == {CALLER!r}\n"
REPO_CHECK += "assert Path('shop/__init__.py').read_bytes() == Path('shop/orders/__init__.py').read_bytes() == b''\nprint('PASS: integer pricing, validation, caller, immutable input and 84 preserved files')\n"

SCHEMA = '''CREATE TABLE customers(id INTEGER PRIMARY KEY, name TEXT NOT NULL);
CREATE TABLE orders(id INTEGER PRIMARY KEY, customer_id INTEGER NOT NULL, status TEXT NOT NULL);
CREATE TABLE items(order_id INTEGER NOT NULL, quantity INTEGER NOT NULL, unit_cents INTEGER NOT NULL);
CREATE TABLE payments(order_id INTEGER NOT NULL, cents INTEGER NOT NULL);
'''
SQL_CHECK = '''import sqlite3
from pathlib import Path

db = sqlite3.connect(':memory:')
db.executescript(Path('schema.sql').read_text(encoding='utf-8'))
db.executemany('INSERT INTO customers VALUES (?, ?)', [(1, 'ação'), (2, 'Main, East'), (3, 'Empty')])
db.executemany('INSERT INTO orders VALUES (?, ?, ?)', [(10, 1, 'completed'), (11, 1, 'completed'), (12, 1, 'cancelled'), (20, 2, 'completed')])
db.executemany('INSERT INTO items VALUES (?, ?, ?)', [(10, 2, 100), (10, 1, 50), (11, 1, 100), (12, 10, 999), (20, 1, 125), (20, 3, 25)])
db.executemany('INSERT INTO payments VALUES (?, ?)', [(10, 100), (10, 200), (11, 20), (12, 99999), (20, 50), (20, 50)])
query = Path('queries/customer_balances.sql').read_text(encoding='utf-8')
rows = db.execute(query).fetchall()
assert rows == [(1, 'ação', 2, 350, 320, 80), (2, 'Main, East', 1, 200, 100, 100), (3, 'Empty', 0, 0, 0, 0)], rows
# An empty completed order still counts; unrelated payment cannot create debt/credit.
db.execute("INSERT INTO orders VALUES (21, 2, 'completed')")
assert db.execute(query).fetchall()[1] == (2, 'Main, East', 2, 200, 100, 100)
db.execute('DELETE FROM payments')
assert db.execute(query).fetchall() == [(1, 'ação', 2, 350, 0, 350), (2, 'Main, East', 2, 200, 0, 200), (3, 'Empty', 0, 0, 0, 0)]
db.execute('DELETE FROM orders')
assert db.execute(query).fetchall() == [(1, 'ação', 0, 0, 0, 0), (2, 'Main, East', 0, 0, 0, 0), (3, 'Empty', 0, 0, 0, 0)]
'''
SQL_CHECK += f"assert Path('schema.sql').read_text(encoding='utf-8') == {SCHEMA!r}\n"
SQL_CHECK += "print('PASS: per-order fan-out, overpayment isolation, cancelled/empty orders and zero customers')\n"

SCENARIOS = {
    'repo_wide_repair': dict(
        prompt='Read SPEC.md, fix the implementation in this repository and run python check.py. Preserve unrelated files, SPEC.md and check.py. Do not install dependencies. Finish with a brief summary.',
        spec_text='''Fix quote(lines, discount_bps=0, tax_bps=0) in shop/orders/totals.py.
Return a dict with subtotal_cents, discount_cents, tax_cents and total_cents.
Subtotal is sum(price_cents * quantity). Discount is subtotal * discount_bps / 10000
rounded down; tax is computed on the discounted subtotal and rounded half up.
Total is discounted subtotal plus tax. Use integers only; do not mutate input.
lines must be a list of dicts with price_cents a nonnegative int and quantity a
positive int. Both rates must be ints from 0 through 10000. bool is not an int
for these contracts. Invalid input raises ValueError. Extra row keys are allowed.
Empty lines yield all zeros. Existing checkout_total caller must keep working.
Change only totals.py; preserve the money helper, caller, package markers and archives.
''', files=REPO_FILES, check=REPO_CHECK),
    'sqlite_balances': dict(
        prompt='Read SPEC.md, repair the SQL query and run python check.py. Do not change schema.sql, SPEC.md or check.py. Do not install dependencies. Finish with a brief summary.',
        spec_text='''Repair queries/customer_balances.sql as one SQLite SELECT statement.
Return one row per customer, ordered by customer id, with columns in this order:
customer id, name, completed_orders, gross_cents, paid_cents, outstanding_cents.
Only orders whose status is completed contribute. Gross per order is sum of
item quantity * unit_cents. Paid per order is sum of its payments, even if overpaid.
Outstanding per order is max(gross - paid, 0), then summed per customer: an
overpayment on one order must not cancel another order's debt. Avoid multiplying
items by payments when joining. Include customers without completed orders as
zero totals, and count completed orders even if they have no items or payments.
Use the provided schema without modification; no DDL/DML or helper functions.
''', files={'schema.sql': SCHEMA, 'queries/customer_balances.sql': '''SELECT c.id, c.name, COUNT(o.id), SUM(i.quantity * i.unit_cents),
       SUM(p.cents), MAX(0, SUM(i.quantity * i.unit_cents) - SUM(p.cents))
FROM customers c
JOIN orders o ON o.customer_id = c.id
JOIN items i ON i.order_id = o.id
JOIN payments p ON p.order_id = o.id
GROUP BY c.id, c.name
ORDER BY c.id;
'''}, check=SQL_CHECK),
}

if __name__ == '__main__':
    daily.SCENARIOS.clear()
    daily.SCENARIOS.update(SCENARIOS)
    daily.main()
