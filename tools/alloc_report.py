"""The allocator portfolios of one generation against their benchmark (read-only on prod).

From generation 4 the portfolios are a grid (`src/alloc.rs`): who (12 scores, 10 random draws)
x traders (10 / 20 / 30 / 50 / 100) x sizing (risk an entry 0.5 / 1 / 2%, cap in one direction
20 / 50 / 100%, the trader's adds followed or not), named `EXPDD_20_R0.5_D20[_ADD]`,
`RANDOM7_20_R0.5_D20[_ADD]`. Each score's portfolio is ranked among the 10 random draws of the
same traders and sizing: its rank is how many of them it beat on return (0-10; picking at
random, 5 on average). A score is judged on all its 90 portfolios, never one cell (among 990
some beat chance by chance). The criterion, fixed before the paper (README, Allocator):

  after 21 days, a score works if its mean rank over its 90 portfolios is 7.5+ of 10 and its
  mean rank is above 5 at 4 of its 5 sizes; before 21 days: too early.

A portfolio added later (COMBO3, the breaker's `_BRK` family) is ranked on its own window: its
return since its first status against each draw's over the same hours. The `_BRK` family is
judged the same way against the draws with the breaker (3 sizes: above 5 at 2 of them).

Run: python tools/alloc_report.py [--generation N] [--score NAME] [--detail NAME] [--traders NAME]
(reads RAILWAY_DATABASE_URL from the repo's .env). By default: each score's mean rank at each
size and its verdict; each sizing's mean return and drawdown. --score: its 90 portfolios;
--detail: one portfolio; --traders: one portfolio's PnL by trader."""
import argparse, os, statistics as st
from collections import defaultdict
import psycopg2

DAYS, WORKS, SIZE_OK, SIZES_OK = 21, 7.5, 5.0, 4
ap = argparse.ArgumentParser()
ap.add_argument('--generation', type=int)
ap.add_argument('--score')
ap.add_argument('--detail')
ap.add_argument('--traders')
args = ap.parse_args()
ENV = os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', '.env')
url = os.environ.get('RAILWAY_DATABASE_URL') or [l.split('=', 1)[1].strip() for l in open(ENV) if l.startswith('RAILWAY_DATABASE_URL=')][0]
c = psycopg2.connect(url, connect_timeout=20); c.set_session(readonly=True); cur = c.cursor()

cur.execute("select to_regclass('allocator_status')")
if not cur.fetchone()[0]:
    raise SystemExit("no allocator_status yet (the grid is from generation 4)")
cur.execute("select max(generation) from allocator_status")
gen = args.generation or cur.fetchone()[0]
if gen is None:
    raise SystemExit("allocator_status is empty")
# Each portfolio's hourly status: the last, and the peak-to-trough of its equity.
cur.execute("""select portfolio, min(at), max(at), (array_agg(row_to_json(s) order by at desc))[1], array_agg(equity order by at)
  from allocator_status s where generation = %s group by 1""", (gen,))
S = {}; t0 = t1 = None
for name, a, b, last, eq in cur.fetchall():
    peak, dd = eq[0], 0.0
    for e in eq:
        peak = max(peak, e); dd = max(dd, 1 - e / peak)
    S[name] = dict(last=last, dd=dd * 100, ret=last['return_pct'], r=[], t0=a)
cur.execute("select portfolio, at, equity from allocator_status where generation = %s order by at", (gen,))
SERIES = defaultdict(list)
for name, at, e in cur.fetchall():
    SERIES[name].append((at, e))
    t0 = min(t0 or a, a); t1 = max(t1 or b, b)
days = (t1 - t0).total_seconds() / 86400
cur.execute("select portfolio, trader, coin, pnl, r, reason from allocator_trades where generation = %s", (gen,))
trades = defaultdict(list)
for row in cur.fetchall():
    trades[row[0]].append(row)
for n, s in S.items():
    s['r'] = [t[4] for t in trades.get(n, [])]

def parts(name):
    """('EXPDD', 20, '0.5', '20', adds, breaker) from EXPDD_20_R0.5_D20[_ADD][_BRK]."""
    x = name.split('_')
    return x[0], int(x[1]), x[2][1:], x[3][1:], 'ADD' in x[4:], 'BRK' in x[4:]

def suffix(add, brk):
    return ('_ADD' if add else '') + ('_BRK' if brk else '')

def draw_names(name):
    who, n, r, d, add, brk = parts(name)
    return [k for k in (f"RANDOM{i}_{n}_R{r}_D{d}{suffix(add, brk)}" for i in range(1, 11)) if k in S]

def draws(name):
    return [S[k] for k in draw_names(name)]

def since(name, t0):
    """A portfolio's return since `t0` (its first status at or after it)."""
    v = SERIES[name]
    e0 = next((e for at, e in v if at >= t0), None)
    return (v[-1][1] / e0 - 1) * 100 if e0 else 0.0

def rank(name):
    t0 = S[name]['t0']
    mine = since(name, t0)
    return sum(1 for k in draw_names(name) if mine > since(k, t0))

print(f"generation {gen}: {t0:%Y-%m-%d %H:%M} .. {t1:%Y-%m-%d %H:%M} UTC, {days:.1f} days, {len(S)} portfolios, "
      f"{sum(len(v) for v in trades.values())} positions closed")
print(f"criterion: after {DAYS} days a score works if its mean rank among the 10 random draws of the same traders and sizing is "
      f"{WORKS}+ over its portfolios, and above {SIZE_OK:g} at {SIZES_OK} of its 5 sizes (picking at random: 5)\n")

if args.detail or args.traders:
    name = args.detail or args.traders
    s = S[name]; l = s['last']
    print(f"{name}: return {s['ret']:+.2f}%, max drawdown {s['dd']:.1f}%, entries {l['entries']}, refused {l['refused']}, stops {l['stops']}, "
          f"open risk {l['open_risk_pct']}% (long {l['long_risk_pct']}%), legs {l['legs']}, closed {len(s['r'])}"
          + (f", mean R {st.mean(s['r']):+.3f}" if s['r'] else ""))
    if not name.startswith('RANDOM'):
        print(f"  the 10 draws: {', '.join(f'{x['ret']:+.2f}%' for x in sorted(draws(name), key=lambda x: x['ret']))}; rank {rank(name)}")
    if args.traders:
        per = defaultdict(lambda: [0, 0.0, 0.0])
        for t in trades.get(name, []):
            per[t[1]][0] += 1; per[t[1]][1] += t[3]; per[t[1]][2] += t[4]
        for u, (k, pnl, r) in sorted(per.items(), key=lambda x: -x[1][1]):
            print(f"  {u}  {k:4d} trades  ${pnl:+9.2f}  mean R {r / k:+.3f}")
    raise SystemExit

scored = [n for n in S if not n.startswith('RANDOM')]
if args.score:
    print(f"{args.score}: each portfolio's return, max drawdown, closed trades, rank among its 10 draws (their range)")
    for n in sorted((n for n in scored if parts(n)[0] == args.score), key=lambda n: parts(n)[1:]):
        s = S[n]; dr = [since(k, s['t0']) for k in draw_names(n)]
        print(f"  {n:28s} {s['ret']:+7.2f}%  dd {s['dd']:5.1f}%  closed {len(s['r']):5d}  rank {rank(n):2d}  ({min(dr):+.1f}..{max(dr):+.1f}%)")
    raise SystemExit

sizes = sorted({parts(n)[1] for n in scored})
print(f"{'score':12s}" + "".join(f"{'top ' + str(n):>9s}" for n in sizes) + f"{'mean rank':>11s}{'median ret':>12s}{'median dd':>11s}  verdict")
for who, brk in sorted({(parts(n)[0], parts(n)[5]) for n in scored}, key=lambda x: (x[1], x[0])):
    mine = [n for n in scored if parts(n)[0] == who and parts(n)[5] == brk]
    by = {sz: st.mean(rank(n) for n in mine if parts(n)[1] == sz) for sz in sizes if any(parts(n)[1] == sz for n in mine)}
    mean = st.mean(rank(n) for n in mine)
    need = SIZES_OK if len(by) == 5 else max(1, len(by) - 1)
    ok = mean >= WORKS and sum(1 for v in by.values() if v > SIZE_OK) >= need
    own_days = (t1 - min(S[n]['t0'] for n in mine)).total_seconds() / 86400
    verdict = 'too early' if own_days < DAYS else ('WORKS' if ok else 'no better than random')
    print(f"{who + ('+BRK' if brk else ''):12s}" + "".join(f"{by[sz]:9.1f}" if sz in by else f"{'-':>9s}" for sz in sizes)
          + f"{mean:11.1f}{st.median(since(n, S[n]['t0']) for n in mine):+11.2f}%{st.median(S[n]['dd'] for n in mine):10.1f}%  {verdict}")

print(f"\n{'sizing':22s}{'scored: mean ret':>17s}{'mean dd':>9s}{'worst dd':>10s}{'random: mean ret':>18s}{'mean dd':>9s}")
for (r, d, add, brk) in sorted({parts(n)[2:] for n in S}, key=lambda x: (x[3], float(x[0]), float(x[1]), x[2])):
    a = [S[n] for n in scored if parts(n)[2:] == (r, d, add, brk)]
    b = [S[n] for n in S if n.startswith('RANDOM') and parts(n)[2:] == (r, d, add, brk)]
    if not a or not b:
        continue
    print(f"{f'risk {r}% cap {d}%' + (' adds' if add else '') + (' breaker' if brk else ''):22s}{st.mean(x['ret'] for x in a):+16.2f}%{st.mean(x['dd'] for x in a):8.1f}%"
          f"{max(x['dd'] for x in a):9.1f}%{st.mean(x['ret'] for x in b):+17.2f}%{st.mean(x['dd'] for x in b):8.1f}%")
