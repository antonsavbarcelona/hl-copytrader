"""The allocator portfolios of one generation against their benchmark (read-only on prod). The
portfolios are a grid (`src/alloc.rs`: score x traders x risk an entry x cap in one direction,
named `EXPDD_20_R0.5_D20`), each score's against the three random draws of the same traders,
risk and cap (`RANDOM1_20_R0.5_D20` ..), by the criterion fixed before the paper (README,
Allocator):

  after 21 days, a portfolio works if its return is above all three random draws' and its
  closed trades' mean R is above theirs (pooled) in 80%+ of bootstrap draws (30+ closed trades
  each); before 21 days or under 30 trades: too early.

Run: python tools/alloc_report.py [--generation N] [--risk 0.5] [--cap 20] [--detail NAME] [--traders NAME]
(reads RAILWAY_DATABASE_URL from the repo's .env). By default one block per risk and cap:
each score's return at each size (* works, . too early, - no better than random), and the
random draws' range. --detail: one portfolio's numbers; --traders: its PnL by trader."""
import argparse, os, random, statistics as st
from collections import defaultdict
import psycopg2

DAYS, MIN_TRADES, CONFIDENCE, DRAWS = 21, 30, 0.80, 1000
ap = argparse.ArgumentParser()
ap.add_argument('--generation', type=int)
ap.add_argument('--risk')
ap.add_argument('--cap')
ap.add_argument('--detail')
ap.add_argument('--traders')
args = ap.parse_args()
ENV = os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', '.env')
url = os.environ.get('RAILWAY_DATABASE_URL') or [l.split('=', 1)[1].strip() for l in open(ENV) if l.startswith('RAILWAY_DATABASE_URL=')][0]
c = psycopg2.connect(url, connect_timeout=20); c.set_session(readonly=True); cur = c.cursor()

cur.execute("""select max(coalesce((p->>'generation')::int, 1)) from bot_status s, jsonb_array_elements(s.data->'allocators') p
  where s.at > now() - interval '1 day'""")
gen = args.generation or cur.fetchone()[0]
if gen is None:
    raise SystemExit("no portfolios in bot_status in the last day")
# Each portfolio's first and last status, and its peak-to-trough on the way (every 10 min).
cur.execute("""select p->>'name', min(s.at), max(s.at), (array_agg(p order by s.at desc))[1],
  array_agg((p->>'equity')::float8 order by s.at)
  from bot_status s, jsonb_array_elements(s.data->'allocators') p
  where coalesce((p->>'generation')::int, 1) = %s group by 1""", (gen,))
S = {}
t0 = t1 = None
for name, a, b, last, eq in cur.fetchall():
    peak, dd = eq[0], 0.0
    for e in eq:
        peak = max(peak, e); dd = max(dd, 1 - e / peak)
    S[name] = dict(last=last, dd=dd * 100, r=[])
    t0 = min(t0 or a, a); t1 = max(t1 or b, b)
if not S:
    raise SystemExit(f"generation {gen}: no status rows")
days = (t1 - t0).total_seconds() / 86400
trades = defaultdict(list)
cur.execute("select to_regclass('allocator_trades')")
if cur.fetchone()[0]:
    cur.execute("select portfolio, trader, coin, pnl, r, reason from allocator_trades where generation = %s", (gen,))
    for row in cur.fetchall():
        trades[row[0]].append(row)
for n, s in S.items():
    s['r'] = [t[4] for t in trades.get(n, [])]

def parts(name):
    """('EXPDD', 20, '0.5', '20') from EXPDD_20_R0.5_D20; None for the older names."""
    x = name.split('_')
    if len(x) != 4 or not x[2].startswith('R') or not x[3].startswith('D'):
        return None
    return x[0], int(x[1]), x[2][1:], x[3][1:]

def beats(a, b):
    rnd = random.Random(7)
    return sum(st.mean(rnd.choices(a, k=len(a))) > st.mean(rnd.choices(b, k=len(b))) for _ in range(DRAWS)) / DRAWS

def verdict(name):
    who, n, r, d = parts(name)
    rands = [S.get(f"RANDOM{k}_{n}_R{r}_D{d}") for k in (1, 2, 3)]
    rands = [x for x in rands if x]
    s = S[name]
    pooled = [x for rr in rands for x in rr['r']]
    if days < DAYS or len(s['r']) < MIN_TRADES or len(rands) < 3 or min(len(x['r']) for x in rands) < MIN_TRADES:
        return '.', None, rands
    p = beats(s['r'], pooled)
    ok = s['last']['return_pct'] > max(x['last']['return_pct'] for x in rands) and p >= CONFIDENCE
    return ('*' if ok else '-'), p, rands

print(f"generation {gen}: {t0:%Y-%m-%d %H:%M} .. {t1:%Y-%m-%d %H:%M} UTC, {days:.1f} days, {len(S)} portfolios")
print(f"criterion: after {DAYS} days, return above all 3 random draws of its size, risk and cap, and mean R above theirs in "
      f"{CONFIDENCE:.0%}+ of bootstrap draws ({MIN_TRADES}+ closed trades)\n")

if args.detail or args.traders:
    name = args.detail or args.traders
    s = S[name]; l = s['last']
    print(f"{name}: return {l['return_pct']:+.2f}%, max drawdown {s['dd']:.1f}%, entries {l['entries']}, refused {l['refused']}, stops {l['stops']}, "
          f"open risk {l['open_risk_pct']}% (long {l.get('long_risk_pct')}%), legs {l['legs']}, closed {len(s['r'])}, "
          f"mean R {st.mean(s['r']):+.3f}" if s['r'] else "")
    if parts(name) and not name.startswith('RANDOM'):
        v, p, rands = verdict(name)
        print(f"  random draws: {', '.join(f'{x['last']['return_pct']:+.2f}%' for x in rands)}; P(mean R above) "
              f"{'-' if p is None else f'{p:.0%}'}; verdict {dict(zip('*.-', ['WORKS', 'too early', 'no better than random']))[v]}")
    if args.traders:
        per = defaultdict(lambda: [0, 0.0, 0.0])
        for t in trades.get(name, []):
            per[t[1]][0] += 1; per[t[1]][1] += t[3]; per[t[1]][2] += t[4]
        for u, (k, pnl, r) in sorted(per.items(), key=lambda x: -x[1][1]):
            print(f"  {u}  {k:4d} trades  ${pnl:+9.2f}  mean R {r / k:+.3f}")
    raise SystemExit

grid = defaultdict(dict)
for name in S:
    p = parts(name)
    if p:
        grid[(p[2], p[3])][(p[0], p[1])] = name
if not grid:   # a generation of the older names: one line each
    for name, s in sorted(S.items()):
        print(f"{name:13s} {s['last']['return_pct']:+7.2f}%  max DD {s['dd']:5.1f}%  closed {len(s['r'])}")
    raise SystemExit
sizes = sorted({k[1] for g in grid.values() for k in g})
for (r, d), g in sorted(grid.items(), key=lambda x: (float(x[0][0]), float(x[0][1]))):
    if (args.risk and r != args.risk) or (args.cap and d != args.cap):
        continue
    print(f"risk {r}% an entry, cap {d}% in one direction:   " + "".join(f"{n:>16d}" for n in sizes))
    for who in sorted({k[0] for k in g if not k[0].startswith('RANDOM')}):
        cells = []
        for n in sizes:
            name = g.get((who, n))
            if not name:
                cells.append(f"{'':>16s}"); continue
            s = S[name]; v, _, _ = verdict(name)
            cells.append(f"{s['last']['return_pct']:+7.2f}% dd{s['dd']:4.1f}{v}")
        print(f"  {who:41s}" + "".join(f"{c:>16s}" for c in cells))
    cells = []
    for n in sizes:
        rs = [S[g[(f'RANDOM{k}', n)]]['last']['return_pct'] for k in (1, 2, 3) if (f'RANDOM{k}', n) in g]
        cells.append(f"{min(rs):+.1f}..{max(rs):+.1f}%" if rs else '')
    print(f"  {'random (3 draws)':41s}" + "".join(f"{c:>16s}" for c in cells) + "\n")
print("* works, . too early, - no better than random; dd: max drawdown on the 10-min equity")
