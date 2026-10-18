"""sim.Market on the archive: stops on minute bars built from every fill of the window (the low /
high of each minute), marks at the minute's last trade, hourly funding from the archive's asset
contexts, a coin's liquidity bucket from its mean $ volume a day over the window."""
import bisect, os
import numpy as np
M = 60000


class ArchMarket:
    def __init__(self, bars_dir, funding, dayvol):
        self.dir, self.b = bars_dir, {}
        self.f = {c: ([x[0] for x in v], [x[1] for x in v]) for c, v in funding.items()}
        self.vol = {c: sum(v.values()) / len(v) for c, v in dayvol.items()}

    def bars(self, coin):
        if coin not in self.b:
            p = os.path.join(self.dir, f'{coin}.npy')
            # Memory-mapped: the processes share them through the page cache.
            self.b[coin] = np.load(p, mmap_mode='r') if os.path.exists(p) else np.zeros((4, 0))
        return self.b[coin]

    def bucket(self, coin):
        v = self.vol.get(coin, 0)
        return '>=100M' if v >= 1e8 else '20-100M' if v >= 2e7 else '5-20M' if v >= 5e6 else '<5M'

    def stop_hit(self, coin, direction, t0, t1, stop_px):
        """The first minute after t0's through the one before t1's whose range reaches the stop."""
        a = self.bars(coin)
        i = int(np.searchsorted(a[0], t0 // M + 1)); j = int(np.searchsorted(a[0], t1 // M))
        if j <= i: return None
        hit = a[1, i:j] <= stop_px if direction > 0 else a[2, i:j] >= stop_px
        k = int(np.argmax(hit))
        return int(a[0, i + k]) * M + M // 2 if hit[k] else None

    def mark(self, coin, t):
        a = self.bars(coin)
        i = int(np.searchsorted(a[0], t // M, side='right')) - 1
        return float(a[3, i]) if i >= 0 else None

    def funding_paid(self, coin, direction, notional, t0, t1):
        ts, rate = self.f.get(coin, ([], []))
        i, j = bisect.bisect_left(ts, t0), bisect.bisect_left(ts, t1)
        return direction * notional * sum(rate[i:j])
