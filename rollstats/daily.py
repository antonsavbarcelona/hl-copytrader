"""The rolling backtest, once a day (run by the bot, `src/rollstats.rs`): the allocator's
statistics on the 90 days to today 00:00 UTC, so its ranking moves a day every day.

  1. each trader's fills (`trader_fills`: gzip JSON, the slim fills of sim.py) topped up with
     the new ones since its last (incremental; saved as it goes, so a run cut short resumes);
  2. the copy backtest of each on the window, the research's model (sim.py, model.json), its
     statistics as backtest_stats has them and over time (metrics.py);
  3. a row each in `backtest_stats_daily` for the day (the bot reloads the latest and ranks
     again); days older than KEEP_DAYS dropped.

Env: DATABASE_URL; ROLL_API_PER_MIN (default 250); ROLL_LIMIT (traders, for a test).
The universe is the traders in trader_fills (the research's 9 342; refreshing it from the
leaderboard is a later step)."""
import gzip, json, os, sys, time
from collections import Counter
import psycopg2
from psycopg2.extras import execute_values
from hl import info
import sim
from metrics import metrics

D = 86_400_000
KEEP_DAYS = 14


def slim(f):
    return [f['coin'], f['side'], float(f['px']), float(f['sz']), f['time'], float(f['startPosition']), float(f['closedPnl']), float(f['fee']),
            bool(f['crossed']), f['oid'], f.get('dir', '')]


def new_fills(u, since, now):
    out, start = [], since
    while True:
        r = info({'type': 'userFillsByTime', 'user': u, 'startTime': start, 'endTime': now, 'aggregateByTime': True})
        out += r
        if len(r) < 2000:
            return out
        start = max(x['time'] for x in r) + 1


def main():
    t0 = time.time()
    c = psycopg2.connect(os.environ['DATABASE_URL'], connect_timeout=30)
    c.autocommit = True
    cur = c.cursor()
    now = int(time.time() * 1000)
    end = now // D * D
    day = time.strftime('%Y-%m-%d', time.gmtime(end / 1000))
    t_from = end - 90 * D
    cur.execute("select address from trader_fills order by address")
    universe = [r[0] for r in cur.fetchall()]
    if os.environ.get('ROLL_LIMIT'):
        universe = universe[:int(os.environ['ROLL_LIMIT'])]
    print(f"rollstats: {day}, {len(universe)} traders, window {time.strftime('%Y-%m-%d', time.gmtime(t_from / 1000))} .. {day}", flush=True)
    mkt = sim.Market(info, end, days=95)
    slip = sim.Slip()
    rows, added, failed = [], Counter(), 0
    for i, u in enumerate(universe):
        cur.execute("select fills, fetched_to from trader_fills where address = %s", (u,))
        blob, fetched_to = cur.fetchone()
        fills = json.loads(gzip.decompress(bytes(blob)))
        last = max([f[4] for f in fills] + [fetched_to or 0])
        if now - last > 60_000:
            try:
                fresh = new_fills(u, last + 1, now)
            except Exception as e:
                fresh = None; failed += 1
                print(f"rollstats: {u} not fetched: {e}", flush=True)
            if fresh is not None:
                seen = {(f[9], f[4], f[2], f[3]) for f in fills if f[4] >= last - 60_000}
                new = [s for s in map(slim, fresh) if (s[9], s[4], s[2], s[3]) not in seen]
                fills = sorted(fills + new, key=lambda f: f[4])
                fills = [f for f in fills if f[4] >= now - 100 * D]
                cur.execute("update trader_fills set fills = %s, fetched_to = %s, updated_at = now() where address = %s",
                            (gzip.compress(json.dumps(fills).encode()), now, u))
                added['fills'] += len(new); added['traders'] += bool(new)
        F = [f for f in fills if t_from <= f[4] < end]
        if F:
            start = max(t_from, F[0][4])
            try:
                tr = sim.backtest(sim.decisions(F), mkt, slip, start, end, mode='mean')
            except Exception as e:
                tr = []
                print(f"rollstats: {u} not backtested: {e}", flush=True)
            if tr:
                # Over the whole window (days before the copy's first trip count as flat), not
                # from its first trip: from its first trip a wallet of a few lucky days ranked first
                # by Sharpe, Sortino and Calmar (the archive walk-forward of 2026-10-18).
                m = metrics(tr, t_from, end)
                rows.append((day, u, m['n'], m['pnl'], m['win'], m['loss'], m['dd'], m['sq'], sum(t['slip_in'] for t in tr) / len(tr),
                             json.dumps(dict(Counter(t['coin'] for t in tr))), m['sharpe'], m['sortino'], m['calmar'], m['pnl30'], m['weeks_pos'], m['weeks']))
        if (i + 1) % 500 == 0:
            print(f"rollstats: {i + 1} of {len(universe)} in {time.time() - t0:.0f} s, {dict(added)}, {len(rows)} with copy trips", flush=True)
    if not rows:
        print(f"rollstats: no traders with copy trips on {day} (none in trader_fills yet?): nothing written", flush=True)
        return
    # Its own traders' rows only: the archive pick (archive/daily.py) adds others for the day.
    cur.execute("delete from backtest_stats_daily where day = %s and address = any(%s)", (day, universe))
    execute_values(cur, """insert into backtest_stats_daily (day, address, trips, pnl, win_usd, loss_usd, max_dd_pct, pnl_sq, slip_bps, coins,
        sharpe_d, sortino_d, calmar, pnl_30d, weeks_pos, weeks) values %s""", rows, template="(%s, %s, %s, %s, %s, %s, %s, %s, %s, %s::jsonb, %s, %s, %s, %s, %s, %s)",
        page_size=1000)
    cur.execute("delete from backtest_stats_daily where day < %s::date - %s", (day, KEEP_DAYS))
    pos = sum(1 for r in rows if r[3] > 0)
    print(f"rollstats: done {day}: {len(rows)} traders with copy trips, {pos} in profit, {sum(1 for r in rows if r[3] > 0 and r[2] >= 20)} of them 20+ trips; "
          f"{added['fills']} new fills of {added['traders']} traders, {failed} not fetched; {time.time() - t0:.0f} s", flush=True)


if __name__ == '__main__':
    try:
        main()
    except Exception as e:
        print(f"rollstats: failed: {e!r}", flush=True)
        sys.exit(1)
