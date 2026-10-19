"""The archive pick, once a day on a desktop with the disk and the S3 access for it (Railway only
trades: the bot reloads the day's rows when they change, `src/rollstats.rs`): every wallet of
the exchange, not only the ones the bot fetched, its copy backtested over the window, the day's
statistics written to `backtest_stats_daily`. The research walk-forward of 2026-10-18 (S3
archive, 2025-05 .. 2026-10) found the copy's Sharpe over 90 days the one score that held out of
sample; the bot's portfolios rank these rows as they ranked rollstats' (this replaces it: run
the bot with ROLLSTATS=0).

  1. Hyperliquid's hourly fill files (s3://hl-mainnet-node-data/node_fills_by_block, requester
     pays) of the last DAYS days, each hour once: extracted (main perp only) into ARCHIVE_DIR as
     per-wallet slim fills in 16 buckets, minute bars and the coins' volume; the raw file is not
     kept. A wallet-hour of more than HEAVY fills, mostly maker, is a market maker's: left out,
     and the wallet with it. Hours older than the window deleted.
  2. Funding from the archive's asset contexts (s3://hyperliquid-archive/asset_ctxs).
  3. Every wallet's copy over the window (rollstats' sim.py, our 20% stop on the minute bars)
     at the prices we would really get (LAG_MS, EXEC: the tape after the trader's fill, our
     spread; the live check of 2026-10-19: the paper's slippage model was ~10 bp a trip too
     kind, most of it on the traders who move a thin book themselves and are out in seconds);
     its statistics over the last 90 days (rollstats' metrics.py: over the whole window); HFT
     (by its decisions), market makers and the traders whose copy trips last under HOLD_MIN_MS
     on average left out.
  4. Rows for the day (upserted): the bot's universe (`trader_fills`) with copy trips, and the
     TOP of the exchange by Sharpe (ties: the LCB) among those with 10+ closed trips and a profit.

Env: DATABASE_URL; AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY (S3 read, requester pays);
ARCHIVE_DIR (default D:/hl-daily); ARCHIVE_DAYS (120: 90 of statistics after 30 of lead-in);
ARCHIVE_PROCS (8); ARCHIVE_TOP (500); ARCHIVE_RAW: a local copy of the raw archive read before S3
(D:/hl-archive); ARCHIVE_DRY=1: the rows only saved (ARCHIVE_DIR/rows_<day>.json), not written;
ARCHIVE_UPLOAD=1: the saved rows of today written, nothing computed.
Run each morning: archive/run_daily.ps1 (it reads the repo's .env)."""
import bisect, datetime as dt, glob, json, os, shutil, struct, sys, time, zlib
from array import array
from collections import Counter
from multiprocessing import Pool
HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, '..', 'rollstats')); sys.path.insert(0, HERE)
import boto3, lz4.frame, numpy as np, orjson
import sim
from metrics import metrics
from market import ArchMarket

D, H = 86_400_000, 3_600_000
DIR = os.environ.get('ARCHIVE_DIR', 'D:/hl-daily')
DAYS = int(os.environ.get('ARCHIVE_DAYS', 120)); WIN = 90
PROCS = int(os.environ.get('ARCHIVE_PROCS', 8)); TOP = int(os.environ.get('ARCHIVE_TOP', 500))
NB, SUB, HEAVY = 16, 32, 500
FILLS, CTX = 'hl-mainnet-node-data', 'hyperliquid-archive'
# Our real copy (the live check of 2026-10-19 against the archive's tape): our fill is the worst
# price for us (a buy: the highest) of the tape's last block at or before LAG_MS after the
# trader's fill (the bot's median lag from Singapore is 0.87 s) but after his own block (none in
# between: the next one), and our spread EXEC bp on it; not the paper's model of our slippage
# against the trader's own price (it misses how far he moves a thin book himself). A block's
# fills share a time and come in no price order, so no single "last trade" of it is ours.
LAG_MS, EXEC = 800, 2.0
# A trader whose copy trips last under this on average over the window is left out: we cannot
# follow him (his move is over before we are in).
HOLD_MIN_MS = 30_000
_s3 = {}


def s3(region):
    if region not in _s3: _s3[region] = boto3.client('s3', region_name=region)
    return _s3[region]


def ymd(ms): return time.strftime('%Y%m%d', time.gmtime(ms / 1000))


# ---------------------------------------------------------------- 1. the hours

def extracted(day, h):
    """The hour is extracted, with our price LAG_MS after each fill (an older extraction is not)."""
    p = os.path.join(DIR, 'slim', day, f'{h}.meta.json')
    return os.path.exists(p) and orjson.loads(open(p, 'rb').read()).get('copy_px') == [LAG_MS, 2]


def hour(key):
    """One hourly file: downloaded, extracted, dropped. Returns the key, or None if it was there.
    Each slim fill gets a 12th and 13th field: the highest and the lowest price of the block we
    would fill in LAG_MS after it (see LAG_MS; at the hour's end, with no block after his in the
    hour: his own)."""
    day, h = key.split('/')[-2], key.split('/')[-1].split('.')[0]
    od = os.path.join(DIR, 'slim', day)
    if extracted(day, h): return None
    local = os.path.join(os.environ.get('ARCHIVE_RAW', 'D:/hl-archive'), key)
    raw = open(local, 'rb').read() if os.path.exists(local) else s3('ap-northeast-1').get_object(Bucket=FILLS, Key=key, RequestPayer='requester')['Body'].read()
    users, bars, vol, tape = {}, {}, {}, {}
    for line in lz4.frame.decompress(raw).split(b'\n'):
        if not line: continue
        o = orjson.loads(line)
        for u, f in (o['events'] if isinstance(o, dict) else [o]):
            c = f['coin']
            if ':' in c or c[0] in '#@' or '/' in c: continue
            px = float(f['px']); sz = float(f['sz']); t = f['time']
            b = bars.setdefault(c, {}); m = t // 60000; x = b.get(m)
            if x is None: b[m] = [px, px, px]
            else:
                if px < x[0]: x[0] = px
                if px > x[1]: x[1] = px
                x[2] = px
            vol[c] = vol.get(c, 0.0) + px * sz / 2
            tape.setdefault(c, []).append((t, px))
            users.setdefault(u, []).append([c, f['side'], px, sz, t, float(f['startPosition']), float(f['closedPnl']), float(f['fee']),
                                            bool(f['crossed']), f['oid'], f.get('dir', ''), px, px])
    # Each coin's blocks: their times, highest and lowest price.
    blocks = {}
    for c, v in tape.items():
        b = {}
        for t, px in v:
            x = b.get(t)
            if x is None: b[t] = [px, px]
            elif px > x[0]: x[0] = px
            elif px < x[1]: x[1] = px
        ts = sorted(b); blocks[c] = (ts, [b[t] for t in ts])
    later = {}
    for v in users.values():
        for f in v:
            k = (f[0], f[4]); p = later.get(k)
            if p is None:
                ts, hl = blocks[f[0]]
                i = bisect.bisect_right(ts, f[4] + LAG_MS) - 1
                if ts[i] <= f[4] and i + 1 < len(ts): i += 1
                p = later[k] = hl[i]
            f[11], f[12] = p
    heavy = [u for u, v in users.items() if len(v) > HEAVY and sum(1 for f in v if not f[8]) >= len(v) / 2]
    hs = set(heavy); parts = [{} for _ in range(NB)]
    for u, v in users.items():
        if u not in hs: parts[zlib.crc32(u.encode()) % NB][u] = v
    os.makedirs(od, exist_ok=True)
    for b, p in enumerate(parts):
        open(os.path.join(od, f'{h}.{b}.lz4'), 'wb').write(lz4.frame.compress(orjson.dumps(p)))
    open(os.path.join(od, f'{h}.bars.json'), 'wb').write(orjson.dumps({c: {str(m): x for m, x in b.items()} for c, b in bars.items()}))
    open(os.path.join(od, f'{h}.meta.json'), 'wb').write(orjson.dumps(dict(heavy=heavy, vol=vol, copy_px=[LAG_MS, 2])))
    return key


def sync(first, end):
    days = sorted({ymd(t) for t in range(first, end, D)})
    keys = []
    for day in days:
        kw = dict(Bucket=FILLS, Prefix=f'node_fills_by_block/hourly/{day}/', RequestPayer='requester')
        r = s3('ap-northeast-1').list_objects_v2(**kw)
        keys += [o['Key'] for o in r.get('Contents', [])]
    keys = [k for k in keys if not extracted(k.split('/')[-2], k.split('/')[-1].split('.')[0])]
    print(f'archive: {len(keys)} hours to fetch of {len(days)} days', flush=True)
    t0 = time.time()
    with Pool(PROCS) as p:
        for i, k in enumerate(p.imap_unordered(hour, keys)):
            if (i + 1) % 100 == 0: print(f'archive: {i + 1}/{len(keys)} hours, {time.time() - t0:.0f} s', flush=True)
    for d in os.listdir(os.path.join(DIR, 'slim')):
        if d < days[0]: shutil.rmtree(os.path.join(DIR, 'slim', d), ignore_errors=True)
    return days


# ---------------------------------------------------------------- 2. funding, bars

def funding(days):
    out = {}
    os.makedirs(os.path.join(DIR, 'ctx'), exist_ok=True)
    for day in days:
        p = os.path.join(DIR, 'ctx', f'{day}.csv.lz4')
        if not os.path.exists(p):
            try:
                s3('us-east-1').download_file(CTX, f'asset_ctxs/{day}.csv.lz4', p + '.part', ExtraArgs={'RequestPayer': 'requester'}); os.replace(p + '.part', p)
            except Exception as e:
                print(f'archive: no asset contexts of {day}: {e}', flush=True); continue
        for line in lz4.frame.decompress(open(p, 'rb').read()).decode().split('\n')[1:]:
            x = line.split(',')
            if len(x) < 3 or not x[0].endswith(':00:00Z'): continue
            t = int(dt.datetime.strptime(x[0], '%Y-%m-%dT%H:%M:%SZ').replace(tzinfo=dt.timezone.utc).timestamp() * 1000)
            out.setdefault(x[1], []).append([t, float(x[2])])
    for f in os.listdir(os.path.join(DIR, 'ctx')):
        if f[:8] < days[0]: os.remove(os.path.join(DIR, 'ctx', f))
    return {c: sorted(v) for c, v in out.items()}


def bars(days):
    cols, vol = {}, {}
    for day in days:
        for h in range(24):
            p = os.path.join(DIR, 'slim', day, f'{h}.bars.json')
            if not os.path.exists(p): continue
            for c, b in orjson.loads(open(p, 'rb').read()).items():
                a = cols.get(c)
                if a is None: a = cols[c] = [array('d'), array('d'), array('d'), array('d')]
                for m in sorted(b, key=int):
                    x = b[m]; a[0].append(int(m)); a[1].append(x[0]); a[2].append(x[1]); a[3].append(x[2])
            for c, v in orjson.loads(open(os.path.join(DIR, 'slim', day, f'{h}.meta.json'), 'rb').read())['vol'].items():
                vol.setdefault(c, {})[day] = vol.get(c, {}).get(day, 0.0) + v
    bd = os.path.join(DIR, 'bars'); shutil.rmtree(bd, ignore_errors=True); os.makedirs(bd)
    for c, a in cols.items():
        np.save(os.path.join(bd, f'{c}.npy'), np.array([np.frombuffer(x, dtype=np.float64) for x in a]))
    return vol


# ---------------------------------------------------------------- 3. the copies

G = {}
def init(fund, vol, heavy, days, t_from, end, universe):
    G.update(mkt=ArchMarket(os.path.join(DIR, 'bars'), fund, vol), slip=sim.Slip(), heavy=heavy, days=days, t_from=t_from, end=end, universe=universe)


def bucket(b):
    """Bucket b's wallets: its hours split into sub-shards on disk, then wallet by wallet."""
    tmp = os.path.join(DIR, 'tmp'); os.makedirs(tmp, exist_ok=True)
    fs = [open(os.path.join(tmp, f'{b}_{k}.bin'), 'wb') for k in range(SUB)]
    for day in G['days']:
        for h in range(24):
            p = os.path.join(DIR, 'slim', day, f'{h}.{b}.lz4')
            if not os.path.exists(p): continue
            parts = [{} for _ in range(SUB)]
            for u, v in orjson.loads(lz4.frame.decompress(open(p, 'rb').read())).items():
                if u not in G['heavy']: parts[(zlib.crc32(u.encode()) // NB) % SUB][u] = v
            for k in range(SUB):
                if parts[k]:
                    z = lz4.frame.compress(orjson.dumps(parts[k])); fs[k].write(struct.pack('<I', len(z)) + z)
    for f in fs: f.close()
    rows, n = [], 0
    lo = G['end'] - WIN * D
    for k in range(SUB):
        sub = {}; path = os.path.join(tmp, f'{b}_{k}.bin')
        with open(path, 'rb') as f:
            while True:
                x = f.read(4)
                if not x: break
                for u, v in orjson.loads(lz4.frame.decompress(f.read(struct.unpack('<I', x)[0]))).items(): sub.setdefault(u, []).extend(v)
        os.remove(path)
        for u in list(sub):
            F = sub.pop(u); F.sort(key=lambda f: (f[4], f[9])); n += 1
            decs = sim.decisions(F)
            sm = sim.style(F, decs); st = sm['style']
            # HFT by its decisions, not its fills: a taker splitting big orders into hundreds of
            # fills is one decision.
            if st == 'hft' and not (sm['decisions_per_day'] > 100 and (sm['hold_med_h'] or 0) < 0.1):
                st = 'no_trips' if sm['hold_med_h'] is None else 'scalper' if sm['hold_med_h'] < 1 else 'directional'
            if st in ('hft', 'market_maker', 'inactive', 'no_trips'): continue
            # Our fill at a decision: the worst for us of the block LAG_MS after its first fill, and our spread.
            later = {(f[0], f[4]): (f[11], f[12]) for f in F}
            def price(c, t, side, kind):
                p = later.get((c, t))
                return p[0 if side > 0 else 1] * (1 + side * EXEC / 1e4) if p else None
            try:
                tr = sim.backtest(decs, G['mkt'], G['slip'], G['t_from'], G['end'], mode='mean', price=price)
            except Exception:
                continue
            tr = [t for t in tr if t['why'] != 'open_at_end' and t['t1'] >= lo]
            known = u in G['universe']
            if not tr or (len(tr) < 10 and not known): continue
            if sum(t['t1'] - t['t0'] for t in tr) / len(tr) < HOLD_MIN_MS: continue
            m = metrics(tr, lo, G['end'])
            if m['pnl'] <= 0 and not known: continue
            rows.append((u, m, sum(t['slip_in'] for t in tr) / len(tr), dict(Counter(t['coin'] for t in tr)), known))
    return b, n, rows


# ---------------------------------------------------------------- 4. the day's rows

def main():
    t0 = time.time()
    end = int(time.time() * 1000) // D * D
    day = time.strftime('%Y-%m-%d', time.gmtime(end / 1000))
    first = end - DAYS * D
    os.makedirs(os.path.join(DIR, 'slim'), exist_ok=True)
    import psycopg2
    from psycopg2.extras import execute_values
    c = psycopg2.connect(os.environ['DATABASE_URL'], connect_timeout=30); c.autocommit = True; cur = c.cursor()
    saved = os.path.join(DIR, f'rows_{day}.json')
    if os.environ.get('ARCHIVE_UPLOAD') == '1':
        write(cur, execute_values, day, json.load(open(saved)), t0); return
    print(f'archive: {day}, the window {ymd(first)} .. {ymd(end - D)} in {DIR}', flush=True)
    days = sync(first, end)
    fund = funding(days)
    vol = bars(days)
    heavy = set()
    for p in glob.glob(os.path.join(DIR, 'slim', '*', '*.meta.json')):
        heavy.update(orjson.loads(open(p, 'rb').read())['heavy'])
    print(f'archive: {len(days)} days, {len(fund)} coins of funding, {len(vol)} of bars, {len(heavy)} market makers left out; '
          f'{time.time() - t0:.0f} s', flush=True)
    cur.execute('select address from trader_fills')
    universe = {r[0] for r in cur.fetchall()}
    rows, n = [], 0
    with Pool(PROCS, initializer=init, initargs=(fund, vol, heavy, days, first, end, universe)) as p:
        for b, nb, r in p.imap_unordered(bucket, range(NB)):
            rows += r; n += nb
            print(f'archive: bucket {b}: {nb} wallets, {len(r)} kept; {time.time() - t0:.0f} s', flush=True)
    def lcb(m):
        e = m['pnl'] / m['n']; sd = max(m['sq'] / m['n'] - e * e, 0.0) ** 0.5
        return e - 2 * sd / m['n'] ** 0.5
    best = sorted([r for r in rows if r[1]['n'] >= 10 and r[1]['pnl'] > 0], key=lambda r: (-r[1]['sharpe'], -lcb(r[1])))[:TOP]
    have = {r[0] for r in best}
    out = dict(best=best, known=[r for r in rows if r[4] and r[0] not in have], wallets=n, universe=len(universe))
    json.dump(out, open(saved, 'w'))
    for u, m, s, co, _ in best[:15]:
        print(f"archive: {u} sharpe {m['sharpe']:.1f} trips {m['n']} pnl {m['pnl']:.1f} dd {m['dd']:.1f}%", flush=True)
    if os.environ.get('ARCHIVE_DRY') == '1':
        print(f'archive: dry run, the rows saved to {saved}, nothing written; {time.time() - t0:.0f} s', flush=True)
        return
    write(cur, execute_values, day, out, t0)


def write(cur, execute_values, day, out, t0):
    best, top = out['best'], out['best'] + out['known']
    execute_values(cur, """insert into backtest_stats_daily (day, address, trips, pnl, win_usd, loss_usd, max_dd_pct, pnl_sq, slip_bps, coins,
        sharpe_d, sortino_d, calmar, pnl_30d, weeks_pos, weeks) values %s on conflict (day, address) do update set trips = excluded.trips, pnl = excluded.pnl,
        win_usd = excluded.win_usd, loss_usd = excluded.loss_usd, max_dd_pct = excluded.max_dd_pct, pnl_sq = excluded.pnl_sq, slip_bps = excluded.slip_bps,
        coins = excluded.coins, sharpe_d = excluded.sharpe_d, sortino_d = excluded.sortino_d, calmar = excluded.calmar, pnl_30d = excluded.pnl_30d,
        weeks_pos = excluded.weeks_pos, weeks = excluded.weeks""",
        [(day, u, m['n'], m['pnl'], m['win'], m['loss'], m['dd'], m['sq'], s, json.dumps(co), m['sharpe'], m['sortino'], m['calmar'], m['pnl30'], m['weeks_pos'], m['weeks'])
         for u, m, s, co, _ in top], template="(%s, %s, %s, %s, %s, %s, %s, %s, %s, %s::jsonb, %s, %s, %s, %s, %s, %s)", page_size=500)
    cur.execute("delete from backtest_stats_daily where day < %s::date - 14", (day,))
    print(f"archive: done {day}: {out['wallets']} wallets copied; {len(top)} rows written: the top {len(best)} by Sharpe of the exchange "
          f"(the first {best[0][1]['sharpe'] if best else 0:.1f}, the last {best[-1][1]['sharpe'] if best else 0:.1f}) and {len(out['known'])} more of the bot's "
          f"{out['universe']}; {time.time() - t0:.0f} s", flush=True)

if __name__ == '__main__':
    try:
        main()
    except Exception as e:
        print(f'archive: failed: {e!r}', flush=True)
        sys.exit(1)
