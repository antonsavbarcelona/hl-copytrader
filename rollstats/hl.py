"""Hyperliquid's info API, paced by request weight: ROLL_API_PER_MIN of the IP's 1200 a minute
(default 350: the bot beside it takes ~330 on average, up to its own 800)."""
import json, os, threading, time, urllib.error, urllib.request
API = 'https://api.hyperliquid.xyz/info'
PER_MIN = float(os.environ.get('ROLL_API_PER_MIN', '350'))
_lock = threading.Lock(); _next = [0.0]


def _pace(w):
    with _lock:
        t = max(time.time(), _next[0]); _next[0] = t + 60.0 / PER_MIN * w
    d = t - time.time()
    if d > 0:
        time.sleep(d)


def info(body, weight=20, cache=None):
    """One request (`cache` is accepted for the research code's calls and ignored)."""
    body = {k: v for k, v in body.items() if not k.startswith('_')}
    for i in range(8):
        _pace(weight)
        try:
            return json.load(urllib.request.urlopen(urllib.request.Request(API, json.dumps(body).encode(), {'Content-Type': 'application/json'}), timeout=60))
        except urllib.error.HTTPError as e:
            if e.code == 429:
                time.sleep(10 * (i + 1)); continue
            if e.code >= 500:
                time.sleep(5); continue
            raise
        except Exception:
            time.sleep(5)
    raise RuntimeError('api failed %s' % body)
