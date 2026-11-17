"""The bot's health from outside: every loop's pulse (the doc `health` the bot saves each minute, see src/health.rs),
every position with its stop and take on the exchange, the resting orders, each strategy's events and errors (24 h).

    py tools/health.py [hours]

Reads RAILWAY_DATABASE_URL from .env (never printed) and the exchange's public info for LIVE_ACCOUNT."""
import collections, datetime as dt, json, pathlib, re, sys, time, urllib.request

import psycopg2

sys.stdout.reconfigure(encoding='utf-8')
HOURS = float(sys.argv[1]) if len(sys.argv) > 1 else 24.0
ENV = dict(re.findall(r'^(\w+)=(.*)$', (pathlib.Path(__file__).resolve().parent.parent / '.env').read_text(encoding='utf-8-sig'), re.M))
ACCOUNT = ENV.get('LIVE_ACCOUNT', '0xfe196b6f17d6898906c6d6a3cda30f2369682719').strip().strip('"').lower()
NOW = time.time()
ago = lambda t: f"{(NOW - t) / 60:5.1f} мин назад" if t else "никогда"
hm = lambda t: dt.datetime.fromtimestamp(t, dt.UTC).strftime('%m-%d %H:%M')
bad = []


def info(body):
    for i in range(5):
        try:
            r = urllib.request.Request('https://api.hyperliquid.xyz/info', json.dumps(body).encode(), {'Content-Type': 'application/json'})
            return json.load(urllib.request.urlopen(r, timeout=20))
        except Exception:
            time.sleep(2 * (i + 1))
    raise RuntimeError(f'info {body["type"]} not read')


c = psycopg2.connect(ENV['RAILWAY_DATABASE_URL'].strip().strip('"')).cursor()

# --- the start
c.execute("select at, data->>'version' from events where kind='bot' and data->>'what'='start' order by at desc limit 1")
st = c.fetchone()
print(f"бот запущен {st[0]:%m-%d %H:%M} UTC, версия {(st[1] or '?')[:7]}")

# --- the loops
c.execute("select state, updated_at from docs where name='health' order by updated_at desc limit 1")
row = c.fetchone()
print('\n== циклы стратегий (пульс)')
if not row:
    print('  доки health нет: бот ещё без хелсчека (до деплоя)'); bad.append('нет пульса')
else:
    h, upd = row
    age = NOW - upd.timestamp()
    print(f"  снимок {age:.0f} с назад" + ('  <-- СТАРЫЙ: сам сторож не работает' if age > 300 else ''))
    if age > 300: bad.append('сторож молчит')
    for l in h['loops']:
        s = 'МОЛЧИТ' if l['silent_s'] else 'ok'
        if l['silent_s']: bad.append(f"{l['name']} молчит")
        print(f"  {l['name']:14} {s:7} шаг раз в {l['tick_s']:>4.0f} с, последний {ago(l['done'])}, шагов {l['ticks']}, самый долгий {l['slowest_s']:.1f} с")
    if h.get('account_error'):
        print('  счёт не прочитан:', h['account_error'][:200]); bad.append('счёт не читается')

# --- the positions and orders
dexes = [''] + [d['name'] for d in info({'type': 'perpDexs'})[1:] if d]
orders, pos = [], []
for d in dexes:
    q = {'type': 'frontendOpenOrders', 'user': ACCOUNT}
    s = {'type': 'clearinghouseState', 'user': ACCOUNT}
    if d: q['dex'] = s['dex'] = d
    orders += info(q)
    pos += [p['position'] for p in info(s)['assetPositions'] if float(p['position']['szi']) != 0]
print('\n== позиции')
for p in pos:
    co = p['coin']; ex = [o for o in orders if o['coin'] == co and o['reduceOnly']]
    stop = [o['triggerPx'] for o in ex if o['isTrigger']]
    take = [o['limitPx'] for o in ex if not o['isTrigger']]
    flag = '' if stop else '  <-- БЕЗ СТОПА'
    if not stop: bad.append(f'{co} без стопа')
    print(f"  {co:12} {float(p['szi']):>+12g} вход {p['entryPx']:<9} pnl {float(p['unrealizedPnl']):+8.2f}  стоп {stop or '-'} тейк {take or '-'}{flag}")
if not pos: print('  нет')
cnt = collections.Counter(('стоп' if o['isTrigger'] else 'reduce' if o['reduceOnly'] else 'покупка' if o['side'] == 'B' else 'продажа') for o in orders)
print(f"\n== ордера: {len(orders)}  " + ', '.join(f'{k} {v}' for k, v in cnt.items()))
spot = info({'type': 'spotClearinghouseState', 'user': ACCOUNT})
u = next((b for b in spot['balances'] if b['coin'] == 'USDC'), None)
if u: print(f"  USDC {float(u['total']):.0f}, под ордерами {float(u['hold']):.0f}, свободно {float(u['total']) - float(u['hold']):.0f}")

# --- each strategy's events
since = dt.datetime.fromtimestamp(NOW - HOURS * 3600, dt.UTC)
print(f'\n== события стратегий за {HOURS:.0f} ч (последнее, входы/выходы, ошибки)')
c.execute("""select kind, max(at), count(*) filter (where data->>'what'='open'), count(*) filter (where data->>'what'='close'),
  count(*) filter (where data->>'what'='error' or (data ? 'error' and data->>'error' is not null)) from events
  where at >= %s and kind not in ('walls','micro','bot') group by 1 order by 1""", (since,))
for k, last, o, cl, e in c.fetchall():
    print(f"  {k:16} последнее {ago(last.timestamp())}  входов {o:3} выходов {cl:3}  ошибок {e}")
c.execute("""select kind, left(coalesce(data->>'error', data::text), 140), count(*), max(at) from events where at >= %s and kind not in ('walls','micro')
  and (data->>'what'='error' or (data ? 'error' and data->>'error' is not null)) group by 1,2 order by 3 desc limit 10""", (since,))
errs = c.fetchall()
if errs:
    print('\n== ошибки (чаще всего)')
    for k, e, n, last in errs:
        print(f"  {k:12} x{n:<4} последняя {last:%H:%M}  {e}")
print('\nИТОГ:', 'всё в порядке' if not bad else 'ВНИМАНИЕ: ' + '; '.join(bad))
