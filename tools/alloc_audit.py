"""An independent audit of the allocator portfolios of one generation, from the database only
(read-only): each check recomputes what the bot did by another route, and prints OK / FAIL.
Run: python tools/alloc_audit.py [generation]  (reads RAILWAY_DATABASE_URL from the repo's .env)

  1. every entry's trader passes the filters on its statistics (the backtest's; a paper copy's
     only after 90 days copied: PnL > 0, 20+ trips from paper or for a score over risk or their
     number); from generation 4 a trader with no backtest and no 90 days of paper never enters;
  2. the caps hold in the saved state (2 positions a coin; 20% a trader and 100% in all, of the
     risk now: a leg reduced holds less), and every entry granted under 2% says which cap cut it;
  3. each position's size is its risk over the 20% stop (notional = 5 x its risk in USD, to the
     re-averaging of its entry as it adds);
  4. stops: each lost ~1 R, no other close lost more (a stop missed);
  5. each position's PnL has the sign of its price move (its prices are averages: a path of
     adds and reduces can turn it, so a handful is no fault);
  6. the books: each portfolio's account and its positions, booked by separate code, give the
     same equity at any price (to the cent);
  7. our entries against the paper copies' at the same moment: the same book, so ~0 bp."""
import os, sys
from collections import defaultdict
import psycopg2

ENV = os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', '.env')
url = os.environ.get('RAILWAY_DATABASE_URL') or [l.split('=', 1)[1].strip() for l in open(ENV) if l.startswith('RAILWAY_DATABASE_URL=')][0]
c = psycopg2.connect(url, connect_timeout=20); c.set_session(readonly=True); cur = c.cursor()
RISK_SCORES = ('EXPDD', 'PF', 'TSTAT', 'PNLSQRTN')
ok = lambda b: 'OK  ' if b else 'FAIL'

cur.execute("select state, updated_at from docs where name = 'allocator'")
doc, doc_at = cur.fetchone()
GEN = int(sys.argv[1]) if len(sys.argv) > 1 else doc['generation']
cur.execute("""select portfolio, trader, coin, side, extract(epoch from opened_at), extract(epoch from closed_at), entry_px, exit_px,
  risk, risk_usd, notional, pnl, fees, r, reason, closed_at from allocator_trades where generation = %s order by closed_at""", (GEN,))
T = cur.fetchall()
cur.execute("""select portfolio, trader, coin, granted_risk, reject_reason from allocator_decisions where generation = %s""", (GEN,))
D = cur.fetchall()
print(f"generation {GEN}: {len(T)} positions closed, {len(D)} decisions; the state saved {doc_at:%Y-%m-%d %H:%M} UTC"
      + ("" if doc['generation'] == GEN else f" (of generation {doc['generation']}: checks 2 and 6 skipped)") + "\n")

# 1. The filters.
cur.execute("select address, trips, pnl from backtest_stats")
bt = {a: (t, p) for a, t, p in cur.fetchall()}
cur.execute("""select address, (state->'stats'->>'trips')::int, equity - (state->'acct'->>'start')::float8,
  extract(epoch from now()) - (state->>'enrolled_at')::float8 >= 90 * 86400 from copy_accounts""")
paper = {a: (t or 0, p) for a, t, p, aged in cur.fetchall() if aged or GEN < 4}
granted = [d for d in D if d[3] > 0]
bad = []
for p, u, coin, g, why in granted:
    paper_wins = u in paper and (u not in bt or GEN >= 4 or paper[u][0] >= 20)
    src, (trips, pnl) = ('paper', paper[u]) if paper_wins else ('backtest', bt.get(u, (0, 0)))
    if u not in bt and u not in paper:
        bad.append((p, u[:10], 'unknown'))
    elif trips < (20 if p.startswith(RISK_SCORES) or src == 'paper' else 1) or pnl <= 0:
        bad.append((p, u[:10], src, trips, round(pnl, 1)))
print(f"{ok(not bad)} 1. entries' traders pass the filters: {len(granted)} checked, {len(bad)} not"
      "  (a paper copy's statistics as now, not as at its ranking)")
for b in bad[:8]:
    print("       ", b)

# 2. The caps, in the saved state; and every cut grant names its cap.
if doc['generation'] == GEN:
    viol = []
    for po in doc['portfolios']:
        coins, traders, total = defaultdict(int), defaultdict(float), 0.0
        for l in po['legs'].values():
            now = l['risk'] * abs(l['size']) / l['full'] if l['full'] else 0.0
            coins[l['coin']] += 1; traders[l['user']] += now; total += now
        viol += [(po['name'], c_, n) for c_, n in coins.items() if n > 2]
        viol += [(po['name'], u[:10], round(r, 4)) for u, r in traders.items() if r > 0.20 + 1e-9]
        viol += [(po['name'], 'total', round(total, 4))] if total > 1.0 + 1e-9 else []
    unexplained = [d for d in granted if d[3] < 0.02 - 1e-9 and not d[4]]
    print(f"{ok(not viol and not unexplained)} 2. caps hold in the state ({sum(len(p['legs']) for p in doc['portfolios'])} open): "
          f"{viol or 'none broken'}; grants under 2%: {sum(1 for d in granted if d[3] < 0.02 - 1e-9)}, "
          f"{len(unexplained)} with no cap named")

# 3. Sizing.
ratio = [x[10] / x[9] for x in T if x[9] > 0]
off = [q for q in ratio if abs(q / 5 - 1) > 0.03]
if ratio:
    print(f"{ok(not off)} 3. size = risk / 20% stop: notional / risk in USD {min(ratio):.3f}..{max(ratio):.3f}, {len(off)} off by 3%+")

# 4. Stops.
stops = [x[13] for x in T if x[14] == 'STOP']
worst = min([x[13] for x in T if x[14] != 'STOP'] or [0])
print(f"{ok(all(-1.3 < r < -0.95 for r in stops) and worst > -1.05)} 4. stops: {len(stops)}"
      f"{' at R ' + ', '.join(f'{r:+.2f}' for r in stops[:10]) if stops else ''}; the worst other close R {worst:+.2f}")

# 5. PnL against the price move.
turned = []
for x in T:
    move = (x[7] / x[6] - 1) * (1 if x[3] == 'long' else -1)
    gross = x[11] + x[12]
    if gross * move < 0 and abs(gross) > 1e-4 * x[10] + 0.05:
        turned.append((x[0], x[2], round(move * 1e4, 1), round(gross, 2)))
print(f"{ok(len(turned) <= max(3, len(T) // 50))} 5. PnL has the sign of its move: {len(T) - len(turned)}/{len(T)}")
for t in turned[:5]:
    print("       ", t)

# 6. The books: account (cash, positions at their average entry) against the positions (closed
#    PnL, open legs at their own entries): their equity is linear in each coin's price, so both
#    the size in each coin and the rest must agree.
if doc['generation'] == GEN:
    closed = defaultdict(float)
    for x in T:
        if x[15] <= doc_at:
            closed[x[0]] += x[11]
    worst, sizes = 0.0, []
    for po in doc['portfolios']:
        a = po['acct']; ls = defaultdict(float); rest = 0.0
        for l in po['legs'].values():
            ls[l['coin']] += l['size']; rest += l['pnl'] - l['size'] * l['entry_px']
        acct_rest = a['cash'] - sum(p['size'] * p['entry'] for p in a['positions'].values())
        worst = max(worst, abs(acct_rest - (a['start'] + closed[po['name']] + rest)))
        sizes += [(po['name'], c_) for c_ in set(ls) | set(a['positions']) if abs(ls[c_] - a['positions'].get(c_, {'size': 0})['size']) > 1e-6]
    print(f"{ok(worst < 0.01 and not sizes)} 6. books: account and positions agree, off by ${worst:.4f} at most; sizes differing: {sizes or 'none'}")

# 7. Our entries against the paper copies' fills at the same moment.
cur.execute("""select address, coin, extract(epoch from at), (data->>'px')::float8 from events
  where kind = 'fill' and data->>'why' = 'copy' and at >= (select min(opened_at) from allocator_trades where generation = %s)""", (GEN,))
F = defaultdict(list)
for a, coin, at, px in cur.fetchall():
    F[(a, coin)].append((at, px))
s = []
for x in T:
    near = [f for f in F.get((x[1], x[2]), []) if abs(f[0] - x[4]) <= 5]
    if near:
        s.append((x[6] / min(near, key=lambda f: abs(f[0] - x[4]))[1] - 1) * (1 if x[3] == 'long' else -1) * 1e4)
s.sort()
if s:
    med = s[len(s) // 2]
    print(f"{ok(abs(med) < 10)} 7. our entry vs the paper copy's: {len(s)} matched, median {med:+.1f} bp, "
          f"p10 {s[len(s) // 10]:+.1f}, p90 {s[len(s) * 9 // 10]:+.1f}")
