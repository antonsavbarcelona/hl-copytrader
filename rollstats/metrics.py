"""A copy's statistics from its closed trips, as the allocator ranks by them: the ones in
backtest_stats already (trips, PnL, wins / losses, drawdown, PnL squared) and the ones over time
(daily Sharpe / Sortino, Calmar, PnL of the last 30 days, the share of weeks in profit). Shared
by the walk-forward and the migration, so both define them the same way."""
import math
D = 86400000
START_USD = 1000.0


def metrics(trips, start, cut):
    """trips: closed before `cut` (ms), each {t1, pnl}; start: the copy's first day (ms)."""
    trips = sorted(trips, key=lambda t: t['t1'])
    n = len(trips)
    eq = peak = START_USD; dd = 0.0; win = loss = sq = s = 0.0
    days = max(int((cut - start) // D), 1)
    daily = [0.0] * days
    for t in trips:
        p = t['pnl']; s += p; sq += p * p
        if p > 0: win += p
        else: loss -= p
        eq += p; peak = max(peak, eq); dd = max(dd, (peak - eq) / peak * 100)
        k = min(max(int((t['t1'] - start) // D), 0), days - 1)
        daily[k] += p
    # Daily returns on the equity before each day.
    rets = []; e = START_USD
    for p in daily:
        rets.append(p / e if e > 0 else 0.0); e += p
    mean = sum(rets) / days
    sd = math.sqrt(max(sum(r * r for r in rets) / days - mean * mean, 0.0))
    down = math.sqrt(sum(min(r, 0.0) ** 2 for r in rets) / days)
    weeks = [sum(daily[w:w + 7]) for w in range(0, days - days % 7, 7)]
    traded = [w for w in weeks if w != 0.0]
    return dict(
        n=n, pnl=s, win=win, loss=loss, sq=sq, dd=dd,
        sharpe=mean / sd * math.sqrt(365) if sd > 0 else 0.0,
        sortino=mean / down * math.sqrt(365) if down > 0 else (99.0 if mean > 0 else 0.0),
        # Return a month over the drawdown (a floor of 1%: a few trips with no drawdown yet).
        calmar=(s / START_USD * 100) / (days / 30) / max(dd, 1.0),
        pnl30=sum(t['pnl'] for t in trips if t['t1'] >= cut - 30 * D),
        weeks_pos=sum(1 for w in traded if w > 0) / len(traded) if traded else 0.0,
        weeks=len(traded),
    )


def score(m, kind):
    n = m['n']
    if n == 0:
        return 0.0
    exp = m['s'] / n if 's' in m else m['pnl'] / n
    sd = math.sqrt(max(m['sq'] / n - exp * exp, 0.0))
    if kind == 'EXPDD': return exp / max(m['dd'], 1.0)
    if kind == 'PNL': return m['pnl']
    if kind == 'PNLSQRTN': return exp * math.sqrt(n)
    if kind == 'PF': return min(m['win'] / m['loss'], 50) if m['loss'] else 50
    if kind == 'TSTAT': return exp / sd * math.sqrt(n) if sd else 0.0
    if kind == 'LCB': return exp - 2 * sd / math.sqrt(n)
    if kind == 'SHARPE': return m['sharpe']
    if kind == 'SORTINO': return min(m['sortino'], 99.0)
    if kind == 'CALMAR': return m['calmar']
    if kind == 'PNL30': return m['pnl30']
    if kind == 'WEEKS': return m['weeks_pos'] + m['weeks'] * 1e-6   # ties: more weeks first
    raise KeyError(kind)


# The closed trips a score needs (the PnL sums: one; the rest measure a spread, losses, a
# drawdown, a number of trips or weeks: 20).
MIN_TRIPS = dict(EXPDD=20, PNL=1, PNLSQRTN=20, PF=20, TSTAT=20, LCB=20, SHARPE=20, SORTINO=20, CALMAR=20, PNL30=1, WEEKS=20, COMBO=20)
SCORES = ['EXPDD', 'PNL', 'PNLSQRTN', 'PF', 'TSTAT', 'LCB', 'SHARPE', 'SORTINO', 'CALMAR', 'PNL30', 'WEEKS']
