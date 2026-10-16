"""Decisions from fills, trading style, and a backtest of our copy of a trader (the research's,
2026-10-10; its data in model.json: coin liquidity, our measured slippage).

Fill (slim): [coin, side 'B'/'A', px, sz, time_ms, startPosition, closedPnl, fee, crossed, oid, dir]
"""
import bisect, json, math, os, random, statistics as st
from collections import defaultdict

HERE = os.path.dirname(os.path.abspath(__file__))
MODEL = json.load(open(os.path.join(HERE, 'model.json')))
GAP_MS = 10 * 60 * 1000
H = 3600 * 1000
D = 86400 * 1000
TAKER = 0.00045
STOP = 0.20          # our stop, against our entry
ENTRY_PCT = 0.10     # entry notional = 10% of the copy's equity (2% risk / 20% stop)
START_USD = 1000.0
MIN_ORDER = 10.0

# ---------------------------------------------------------------- decisions

def decisions(fills, gap_ms=GAP_MS):
    """Fills of one direction in one coin with gaps under `gap_ms` = one decision."""
    out, cur = [], {}
    for f in fills:
        coin, side, px, sz, t, start = f[0], f[1], f[2], f[3], f[4], f[5]
        delta = sz if side == 'B' else -sz
        d = cur.get(coin)
        if d is None or d['side'] != side or t - d['t1'] > gap_ms:
            d = dict(coin=coin, side=side, t0=t, t1=t, n=0, sz=0.0, ntl=0.0, maker=0.0, pnl=0.0, fee=0.0,
                     before=start, after=start, px0=px)
            cur[coin] = d
            out.append(d)
        d['t1'] = t; d['n'] += 1; d['sz'] += sz; d['ntl'] += sz * px
        if not f[8]: d['maker'] += sz * px
        d['pnl'] += f[6]; d['fee'] += f[7]
        d['after'] = start + delta
    for d in out:
        b, a = d['before'], d['after']
        eps = 1e-9 * max(1.0, abs(b), abs(a))
        d['vwap'] = d['ntl'] / d['sz'] if d['sz'] else d['px0']
        if abs(b) <= eps and abs(a) > eps: d['type'] = 'open'
        elif abs(a) <= eps and abs(b) > eps: d['type'] = 'close'
        elif b * a < 0: d['type'] = 'flip'
        elif abs(a) > abs(b): d['type'] = 'add'
        else: d['type'] = 'reduce'
    out.sort(key=lambda d: d['t0'])
    return out


def trips(decs):
    """Position lifecycles per coin, from an open (or flip) to flat (or the next flip)."""
    open_, done = {}, []
    for d in decs:
        c = d['coin']
        t = open_.get(c)
        if t:
            t['pnl'] += d['pnl'] - d['fee']; t['decs'] += 1
            t['peak'] = max(t['peak'], abs(d['after']) * d['vwap'])
            if d['type'] in ('close', 'flip'):
                t['t1'] = d['t1']; done.append(t); del open_[c]
        if d['type'] in ('open', 'flip'):
            open_[c] = dict(coin=c, dir=1 if d['after'] > 0 else -1, t0=d['t0'], t1=None, pnl=0.0, decs=1, peak=abs(d['after']) * d['vwap'])
    return done, list(open_.values())

# ---------------------------------------------------------------- style

def style(fills, decs, spot_fills=0):
    if not fills:
        return dict(style='inactive')
    span_d = max((fills[-1][4] - fills[0][4]) / D, 1 / 24)
    done, _ = trips(decs)
    holds = sorted((t['t1'] - t['t0']) / H for t in done)
    hold_med = holds[len(holds) // 2] if holds else None
    ntl = sum(f[2] * f[3] for f in fills) or 1.0
    maker = sum(f[2] * f[3] for f in fills if not f[8]) / ntl
    fpd, dpd = len(fills) / span_d, len(decs) / span_d
    spot_share = spot_fills / (spot_fills + len(fills))
    shorts = sum(1 for t in done if t['dir'] < 0) / len(done) if done else 0
    m = dict(span_d=span_d, fills_per_day=fpd, decisions_per_day=dpd, trips=len(done), hold_med_h=hold_med, maker=maker,
             spot_share=spot_share, short_share=shorts, decisions=len(decs))
    if fpd > 500 or (dpd > 100 and (hold_med or 0) < 0.1):
        s = 'hft'
    elif maker >= 0.7 and dpd >= 20:
        s = 'market_maker'
    elif spot_share >= 0.3 and shorts >= 0.7 and (hold_med or 0) >= 24:
        s = 'basis_funding'
    elif hold_med is None:
        s = 'no_trips'
    elif hold_med < 1:
        s = 'scalper'
    else:
        s = 'directional'
    m['style'] = s
    return m

# ---------------------------------------------------------------- prices, funding, slippage

class Market:
    """1h candles and hourly funding per coin (fetched lazily, cached), and day volumes."""
    def __init__(self, info, now_ms, days=95):
        self.info, self.now, self.days = info, now_ms, days
        self.c, self.f = {}, {}
        self.vol = MODEL['dayvol']

    def candles(self, coin):
        if coin not in self.c:
            try:
                r = self.info({'type': 'candleSnapshot', 'req': {'coin': coin, 'interval': '1h', 'startTime': self.now - self.days * D, 'endTime': self.now}})
            except Exception:
                r = []
            r = r or []
            self.c[coin] = ([x['t'] for x in r], [float(x['l']) for x in r], [float(x['h']) for x in r], [float(x['c']) for x in r])
        return self.c[coin]

    def funding(self, coin):
        if coin not in self.f:
            rows, start = [], self.now - self.days * D
            while True:
                try:
                    r = self.info({'type': 'fundingHistory', 'coin': coin, 'startTime': start, 'endTime': self.now})
                except Exception:
                    r = []
                if not r: break
                rows += r
                if len(r) < 500: break
                start = r[-1]['time'] + 1
            self.f[coin] = ([x['time'] for x in rows], [float(x['fundingRate']) for x in rows])
        return self.f[coin]

    def bucket(self, coin):
        v = self.vol.get(coin, 0)
        return '>=100M' if v >= 1e8 else '20-100M' if v >= 2e7 else '5-20M' if v >= 5e6 else '<5M'

    def stop_hit(self, coin, direction, t0, t1, stop_px):
        """First 1h candle after t0 (from the next hour) through t1 whose range reaches stop: its time."""
        ts, lo, hi, _ = self.candles(coin)
        i = bisect.bisect_right(ts, t0)
        j = bisect.bisect_right(ts, t1)
        for k in range(i, j):
            if (direction > 0 and lo[k] <= stop_px) or (direction < 0 and hi[k] >= stop_px):
                return ts[k] + H // 2
        return None

    def funding_paid(self, coin, direction, notional, t0, t1):
        ts, rate = self.funding(coin)
        i, j = bisect.bisect_left(ts, t0), bisect.bisect_left(ts, t1)
        return direction * notional * sum(rate[i:j])

    def mark(self, coin, t):
        ts, _, _, close = self.candles(coin)
        i = bisect.bisect_right(ts, t) - 1
        return close[i] if i >= 0 else None


class Slip:
    """Our slippage vs the trader's price (bp), drawn from our copier's measured fills per
    coin liquidity and kind (entry from flat / full exit / partial)."""
    def __init__(self, seed=1):
        self.m = MODEL['slip_mean']

    def draw(self, bucket, kind):
        raise NotImplementedError('only the means are kept (mode="mean")')

    def mean(self, bucket, kind):
        return self.m.get(f"{bucket}|{kind}", self.m[f"<5M|{kind}"])

# ---------------------------------------------------------------- backtest of our copy

def backtest(decs, mkt, slip, t_from, t_to, mode='draw', add_max=1.0):
    """Our copy of the trader over [t_from, t_to): copy trips. Positions it held at t_from are not
    entered. Each entry = 10% of the copy's equity at the trader's first price + our slippage;
    reduces follow its decisions in proportion to its peak in the position; the stop is 20%
    against our entry (1h candles); close with it; taker fees and funding. Trips still open at
    t_to are marked at the last 1h close.
    add_max > 1: the trader's adds are followed too, our size the trader's over its size at our
    entry, up to add_max x our entry's (2: up to 4% risk); the entry re-averaged, the stop 20%
    from it; reduces then cut our size at its peak in proportion to the trader's from its peak.
    (Funding on an add is counted from the position's start: a small approximation.)"""
    equity = START_USD
    legs, out = {}, []
    sl = (lambda b, k: slip.draw(b, k)) if mode == 'draw' else (lambda b, k: slip.mean(b, k))
    held_before = set()
    seen = set()
    for d in decs:
        if d['t0'] < t_from or d['t0'] >= t_to:
            continue
        # Only main-dex perps are copied (no HIP-3 'dex:COIN', no '#' outcome markets).
        if ':' in d['coin'] or d['coin'].startswith('#'):
            continue
        c = d['coin']
        if c not in seen:
            seen.add(c)
            if abs(d['before']) > 0 and d['type'] != 'open':
                held_before.add(c)
        b = mkt.bucket(c)
        leg = legs.get(c)
        # A stop inside the trader's position, before this decision.
        if leg:
            hit = mkt.stop_hit(c, leg['dir'], leg['chk'], d['t0'], leg['stop'])
            leg['chk'] = max(leg['chk'], d['t0'])
            if hit:
                close_leg(out, legs, c, leg, leg['stop'] * (1 - leg['dir'] * 0.001), hit, 'stop', mkt)
                equity += out[-1]['pnl']
                leg = None
                leg_out = True
            else:
                leg_out = False
        if d['type'] in ('open', 'flip'):
            held_before.discard(c)
            if leg:  # a flip: our leg closes with it
                px = d['vwap'] * (1 - leg['dir'] * sl(b, 'exit') / 1e4)
                close_leg(out, legs, c, leg, px, d['t0'], 'flip', mkt); equity += out[-1]['pnl']
            direction = 1 if d['after'] > 0 else -1
            px = d['px0'] * (1 + direction * sl(b, 'entry') / 1e4)
            notional = ENTRY_PCT * max(equity, 0.0)
            if notional < MIN_ORDER:
                continue
            legs[c] = dict(coin=c, dir=direction, t0=d['t0'] + 1000, entry=px, size=notional / px, full=notional / px,
                           peak=abs(d['after']), stop=px * (1 - direction * STOP), fee=notional * TAKER, slip_in=(px / d['px0'] - 1) * direction * 1e4,
                           funding=0.0, realized=0.0, ntl_in=notional, ntl0=notional, pos0=abs(d['after']), chk=d['t0'] + 1000, top=notional / px)
            continue
        if c in held_before or not leg:
            if d['type'] == 'close':
                held_before.discard(c)
            continue
        if d['type'] == 'add':
            if add_max > 1 and leg['pos0'] > 0:
                target = leg['full'] * min(abs(d['after']) / leg['pos0'], add_max)
                more = target - leg['size']
                px = d['px0'] * (1 + leg['dir'] * sl(b, 'entry') / 1e4)
                if more * px >= MIN_ORDER:
                    leg['entry'] = (leg['entry'] * leg['size'] + px * more) / target
                    leg['size'] = target
                    leg['stop'] = leg['entry'] * (1 - leg['dir'] * STOP)
                    leg['fee'] += more * px * TAKER
                    leg['ntl_in'] += more * px
            if abs(d['after']) >= leg['peak']:
                leg['peak'] = abs(d['after']); leg['top'] = max(leg['top'], leg['size'])
            continue
        if d['type'] == 'reduce':
            target = (leg['top'] if add_max > 1 else leg['full']) * abs(d['after']) / leg['peak']
            if target >= leg['size']:
                continue
            cut = leg['size'] - target
            px = d['vwap'] * (1 - leg['dir'] * sl(b, 'partial') / 1e4)
            if cut * px < MIN_ORDER:
                continue
            leg['funding'] += mkt.funding_paid(c, -leg['dir'], cut * leg['entry'], leg['t0'], d['t0'])
            leg['realized'] += cut * (px - leg['entry']) * leg['dir']
            leg['fee'] += cut * px * TAKER
            leg['size'] = target
            continue
        if d['type'] == 'close':
            px = d['vwap'] * (1 - leg['dir'] * sl(b, 'exit') / 1e4)
            close_leg(out, legs, c, leg, px, d['t0'], 'exit', mkt); equity += out[-1]['pnl']
    for c, leg in list(legs.items()):
        hit = mkt.stop_hit(c, leg['dir'], leg['chk'], t_to, leg['stop'])
        if hit:
            close_leg(out, legs, c, leg, leg['stop'] * (1 - leg['dir'] * 0.001), hit, 'stop', mkt)
        else:
            m = mkt.mark(c, t_to)
            if m:
                close_leg(out, legs, c, leg, m, t_to, 'open_at_end', mkt, fee=False)
    return out


def close_leg(out, legs, c, leg, px, t, why, mkt, fee=True):
    leg['funding'] += mkt.funding_paid(c, -leg['dir'], leg['size'] * leg['entry'], leg['t0'], t)
    leg['realized'] += leg['size'] * (px - leg['entry']) * leg['dir']
    if fee:
        leg['fee'] += leg['size'] * px * TAKER
    pnl = leg['realized'] - leg['fee'] + leg['funding']
    out.append(dict(coin=c, dir=leg['dir'], t0=leg['t0'], t1=t, entry=leg['entry'], exit=px, why=why, ntl=leg['ntl_in'], ntl0=leg.get('ntl0', leg['ntl_in']),
                    pnl=pnl, bp=pnl / leg['ntl_in'] * 1e4, slip_in=leg['slip_in'], funding=leg['funding']))
    legs.pop(c, None)
