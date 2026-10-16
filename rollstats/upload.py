"""One-off: the research's fills of its 9 342 traders (cache/fills_roll, else cache/fillsA: the
original 90-day download, topped up) into `trader_fills`, the rolling backtest's start.
  python upload.py <research dir>     (DATABASE_URL, else RAILWAY_DATABASE_URL in the repo's .env)
Rows already there are kept (ON CONFLICT DO NOTHING)."""
import gzip, json, os, sys
import psycopg2
from psycopg2.extras import execute_values

R = sys.argv[1]
ENV = os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', '.env')
url = os.environ.get('DATABASE_URL') or [l.split('=', 1)[1].strip() for l in open(ENV) if l.startswith('RAILWAY_DATABASE_URL=')][0]
orig_now = int(open(os.path.join(R, 'cache', 'now.txt')).read())
universe = [json.loads(l)['u'] for l in open(os.path.join(R, 'backtest_all.jsonl'))]
c = psycopg2.connect(url, connect_timeout=30); c.autocommit = True; cur = c.cursor()
batch, n, size = [], 0, 0
for u in universe:
    p = os.path.join(R, 'cache', 'fills_roll', u + '.json.gz')
    j = json.load(gzip.open(p if os.path.exists(p) else os.path.join(R, 'cache', 'fillsA', u + '.json.gz'), 'rt'))
    blob = gzip.compress(json.dumps(j['fills']).encode())
    batch.append((u, psycopg2.Binary(blob), j.get('fetched_to', orig_now))); size += len(blob)
    if len(batch) == 200:
        execute_values(cur, "insert into trader_fills (address, fills, fetched_to) values %s on conflict (address) do nothing", batch); n += len(batch); batch = []
        print(n, f"{size / 1e6:.0f} MB", flush=True)
if batch:
    execute_values(cur, "insert into trader_fills (address, fills, fetched_to) values %s on conflict (address) do nothing", batch); n += len(batch)
print('uploaded', n, f"{size / 1e6:.0f} MB")
