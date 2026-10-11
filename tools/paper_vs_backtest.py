"""The paper copies of the backtest's picks against the backtest (read-only on prod):
latency, slippage, fees, funding, and PnL per day set against the backtest's copy ROI.
Run: python paper_vs_backtest.py  (reads RAILWAY_DATABASE_URL from the repo's .env)"""
import json, statistics as st, sys
import psycopg2

import os
ENV = os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', '.env')
url = os.environ.get('RAILWAY_DATABASE_URL') or [l.split('=', 1)[1].strip() for l in open(ENV) if l.startswith('RAILWAY_DATABASE_URL=')][0]
c = psycopg2.connect(url, connect_timeout=20); c.set_session(readonly=True); cur = c.cursor()
cur.execute("select min(picked_at) from backtest_picks"); t0 = cur.fetchone()[0]
cur.execute("select now()"); now = cur.fetchone()[0]
days = (now - t0).total_seconds() / 86400
print(f"since the picks went live: {t0:%Y-%m-%d %H:%M} UTC, {days:.2f} days\n")

# Our fills of the picks since then: lag, slippage vs the trader, fees, realized.
cur.execute("""select e.address, e.data from events e join backtest_picks p on p.address = e.address
  where e.kind = 'fill' and e.at >= %s""", (t0,))
F = cur.fetchall()
copy = [d for _, d in F if d.get('why') == 'copy' and d.get('lag_s') is not None]
def q(x, p):
    x = sorted(x); return x[min(len(x) - 1, int(p * len(x)))] if x else float('nan')
ntl = sum(d['notional'] for _, d in F) or 1
print(f"our fills: {len(F)} ({len(copy)} following a trade of theirs), ${ntl:,.0f} traded")
if copy:
    lag = [d['lag_s'] for d in copy]; slip = [d['slip_bps'] for d in copy if d.get('slip_bps') is not None]
    w = sum(d['notional'] for d in copy if d.get('slip_bps') is not None) or 1
    print(f"  latency (their fill -> ours): p50 {q(lag, .5):.2f} s, p90 {q(lag, .9):.2f} s, p99 {q(lag, .99):.2f} s")
    print(f"  slippage vs their price: mean {st.mean(slip):.1f} bp, notional-weighted {sum(d['slip_bps'] * d['notional'] for d in copy if d.get('slip_bps') is not None) / w:.1f} bp, p90 {q(slip, .9):.1f} bp")
    print(f"  slippage cost: ${sum(d['slip_bps'] / 1e4 * d['notional'] for d in copy if d.get('slip_bps') is not None):,.2f}")
fees = sum(d['fee'] for _, d in F); realized = sum(d['realized'] for _, d in F)
print(f"  fees ${fees:,.2f} ({fees / ntl * 1e4:.1f} bp of volume), realized ${realized:,.2f}\n")

# Per pick: copies enrolled after the picks went live start clean at $1000.
cur.execute("""select p.address, p.style, p.copy_roi_month, p.copy_trips, a.equity, a.copy_fills, (a.state->>'enrolled_at')::float, a.state->'acct'->'fees',
  a.state->'acct'->'funding', a.open_positions from backtest_picks p left join copy_accounts a on a.address = p.address""")
rows = cur.fetchall()
enrolled = [r for r in rows if r[4] is not None]
clean = [r for r in enrolled if r[6] and r[6] >= t0.timestamp()]
traded = [r for r in clean if r[5] > 0]
print(f"picks: {len(rows)}, enrolled (traded since we follow them): {len(enrolled)}, clean (enrolled after): {len(clean)}, with copy fills: {len(traded)}")
if traded:
    pnl = [r[4] - 1000 for r in traded]
    per_day = sum(pnl) / max(days, 1e-6)
    exp = sum(r[2] * 1000 / 30 for r in traded)
    print(f"  paper PnL of the clean copies: ${sum(pnl):,.2f} ({sum(p > 0 for p in pnl) / len(pnl) * 100:.0f}% at a profit), ${per_day:,.2f}/day")
    print(f"  the backtest expected for them: ${exp:,.2f}/day (out of sample, about half: ${exp / 2:,.2f}/day)")
    print(f"  fees ${sum(float(r[7] or 0) for r in traded):,.2f}, funding ${sum(float(r[8] or 0) for r in traded):,.2f}")
    def rank(x):
        o = sorted(range(len(x)), key=lambda i: x[i]); rk = [0] * len(x)
        for k, i in enumerate(o): rk[i] = k
        return rk
    if len(traded) >= 10:
        a, b = rank([r[2] for r in traded]), rank(pnl)
        ma, mb = st.mean(a), st.mean(b)
        rho = sum((x - ma) * (y - mb) for x, y in zip(a, b)) / (len(a) * st.pstdev(a) * st.pstdev(b))
        print(f"  rank correlation backtest copy ROI vs paper PnL: {rho:.2f}")
    by = {}
    for r in traded:
        b = '50+' if r[3] >= 50 else '20-49' if r[3] >= 20 else '5-19' if r[3] >= 5 else '1-4'
        by.setdefault(b, []).append(r[4] - 1000)
    print("  by the backtest's number of copy trades:")
    for b in ['50+', '20-49', '5-19', '1-4']:
        if b in by:
            print(f"    {b:6s} copies {len(by[b]):4d}  paper PnL ${sum(by[b]):8.2f}  at a profit {sum(x > 0 for x in by[b]) / len(by[b]) * 100:.0f}%")
