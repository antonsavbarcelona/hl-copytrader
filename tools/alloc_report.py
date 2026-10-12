"""The allocator portfolios of one generation against their benchmark (read-only on prod): each
portfolio's return, drawdown (from its equity every 10 min), trades closed and their R (PnL / the
risk granted), and the same against RANDOM_<N> of its size, with the verdict by the criterion
fixed before the paper (README, Allocator):

  after 21 days, a portfolio works if its return is above its RANDOM_<N>'s and its closed
  trades' mean R is above the random's in 80%+ of bootstrap draws (30+ closed trades each);
  before 21 days or under 30 trades: too early.

Run: python tools/alloc_report.py [--generation 2] [--traders PORTFOLIO]
(reads RAILWAY_DATABASE_URL from the repo's .env). --traders: the portfolio's PnL by trader."""
import argparse, os, random, statistics as st
from collections import defaultdict
import psycopg2

DAYS, MIN_TRADES, CONFIDENCE, DRAWS = 21, 30, 0.80, 2000
ap = argparse.ArgumentParser()
ap.add_argument('--generation', type=int)
ap.add_argument('--traders')
args = ap.parse_args()
ENV = os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', '.env')
url = os.environ.get('RAILWAY_DATABASE_URL') or [l.split('=', 1)[1].strip() for l in open(ENV) if l.startswith('RAILWAY_DATABASE_URL=')][0]
c = psycopg2.connect(url, connect_timeout=20); c.set_session(readonly=True); cur = c.cursor()

# The portfolios' status every 10 min (bot_status.data.allocators), this generation's.
cur.execute("""select max(coalesce((p->>'generation')::int, 1)) from bot_status s, jsonb_array_elements(s.data->'allocators') p
  where s.at > now() - interval '1 day'""")
gen = args.generation or cur.fetchone()[0]
if gen is None:
    raise SystemExit("no portfolios in bot_status in the last day")
cur.execute("""select s.at, p from bot_status s, jsonb_array_elements(s.data->'allocators') p
  where coalesce((p->>'generation')::int, 1) = %s order by s.at""", (gen,))
series = defaultdict(list)
for at, p in cur.fetchall():
    series[p['name']].append((at, p))
if not series:
    raise SystemExit(f"generation {gen}: no status rows")
t0 = min(v[0][0] for v in series.values()); t1 = max(v[-1][0] for v in series.values())
days = (t1 - t0).total_seconds() / 86400
trades = defaultdict(list)
cur.execute("select to_regclass('allocator_trades')")
if cur.fetchone()[0]:  # (from generation 2 on)
    cur.execute("select portfolio, trader, coin, pnl, r, reason, held_h from allocator_trades where generation = %s", (gen,))
    for row in cur.fetchall():
        trades[row[0]].append(row)
print(f"generation {gen}: {t0:%Y-%m-%d %H:%M} .. {t1:%Y-%m-%d %H:%M} UTC, {days:.1f} days")
print(f"criterion: after {DAYS} days, return above RANDOM_<N>'s and mean R above it in {CONFIDENCE:.0%}+ of draws ({MIN_TRADES}+ closed trades)\n")

def stats(name):
    s = series.get(name, [])
    if not s:
        return None
    eq = [p['equity'] for _, p in s]; peak = eq[0]; dd = 0.0
    for e in eq:
        peak = max(peak, e); dd = max(dd, 1 - e / peak)
    last = s[-1][1]; tr = trades.get(name, [])
    r = [t[4] for t in tr]
    return dict(ret=last['return_pct'], dd=dd * 100, entries=last['entries'], refused=last['refused'], stops=last['stops'],
                open_risk=last['open_risk_pct'], legs=last['legs'], closed=len(tr), r=r,
                win=sum(1 for x in r if x > 0) / len(r) * 100 if r else float('nan'), mean_r=st.mean(r) if r else float('nan'))

def beats(a, b):
    """Share of bootstrap draws with a's mean R above b's."""
    rnd = random.Random(7); n = 0
    for _ in range(DRAWS):
        ma = st.mean(rnd.choices(a, k=len(a))); mb = st.mean(rnd.choices(b, k=len(b)))
        n += ma > mb
    return n / DRAWS

names = sorted(series, key=lambda n: (n.startswith('RANDOM'), n))
S = {n: stats(n) for n in names}
print(f"{'portfolio':13s} {'return':>8s} {'maxDD':>6s} {'entries':>7s} {'refused':>7s} {'stops':>5s} {'open':>5s} {'closed':>6s} {'win':>5s} {'mean R':>7s} | {'vs RANDOM':>9s} {'P(R>)':>6s}  verdict")
for n in names:
    s = S[n]
    bench = 'RANDOM_' + n.rsplit('_', 1)[-1]
    b = S.get(bench) if not n.startswith('RANDOM') else None
    excess = f"{s['ret'] - b['ret']:+8.2f}%" if b else ''
    p = beats(s['r'], b['r']) if b and len(s['r']) >= 2 and len(b['r']) >= 2 else None
    if n.startswith('RANDOM'):
        verdict = 'benchmark'
    elif days < DAYS or s['closed'] < MIN_TRADES or not b or len(b['r']) < MIN_TRADES:
        verdict = 'too early'
    else:
        verdict = 'WORKS' if s['ret'] > b['ret'] and p is not None and p >= CONFIDENCE else 'no better than random'
    print(f"{n:13s} {s['ret']:+7.2f}% {s['dd']:5.1f}% {s['entries']:7d} {s['refused']:7d} {s['stops']:5d} {s['open_risk']:4.0f}% {s['closed']:6d} "
          f"{('%4.0f%%' % s['win']) if s['r'] else '-':>5s} {('%+7.3f' % s['mean_r']) if s['r'] else '-':>7s} | {excess:>9s} {('%5.0f%%' % (p * 100)) if p is not None else '':>6s}  {verdict}")

# Why positions closed, all portfolios.
by = defaultdict(lambda: [0, 0.0])
for tr in trades.values():
    for t in tr:
        by[t[5]][0] += 1; by[t[5]][1] += t[3]
if by:
    print("\nclosed by: " + ", ".join(f"{k} {v[0]} (${v[1]:+,.0f})" for k, v in sorted(by.items())))

if args.traders:
    tr = trades.get(args.traders, [])
    per = defaultdict(lambda: [0, 0.0, 0.0])
    for t in tr:
        per[t[1]][0] += 1; per[t[1]][1] += t[3]; per[t[1]][2] += t[4]
    total = sum(v[1] for v in per.values())
    print(f"\n{args.traders}: {len(tr)} closed trades, ${total:+,.2f}, by trader")
    for u, (k, pnl, r) in sorted(per.items(), key=lambda x: -x[1][1]):
        print(f"  {u}  {k:4d} trades  ${pnl:+9.2f}  mean R {r / k:+.3f}")
