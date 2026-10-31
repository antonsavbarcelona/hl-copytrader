#!/usr/bin/env python3
# Generates a research catalog, NOT production trading instructions.
import re, json, math, itertools, collections, hashlib, zipfile, os
from pathlib import Path
from datetime import date

ROOT=Path('/mnt/data')
SOURCE=ROOT/'Вставленная ​​уценка(3).md'
text=SOURCE.read_text(encoding='utf-8')
legacy=[]
section='Источник: не определено'
for line in text.splitlines():
    h=re.match(r'^\s*(\d{1,2})\.\s+([А-ЯЁA-Z][^\n]+?)\s*$',line)
    if h and len(h.group(2)) > 5 and not re.search(r'\[',line):
        section=h.group(2).strip()
    m=re.match(r'^\s*(\d{1,3})\.\s+\[(ЖИВАЯ|проверяли|НОВОЕ)\]\s+(.*?)\s*$',line)
    if m:
        legacy.append(dict(id=f'LEG-{int(m.group(1)):03d}',source_id=int(m.group(1)),name=m.group(3).strip(),source_status=m.group(2),source_section=section, status='source_reference_not_fully_specified',definition='Оригинальный каталог пользователя. Формулу, доступность и семантику требуется привязать к реализации; не считать автоматически executable.'))
assert len(legacy)==467,(len(legacy),[a['source_id'] for a in legacy][-10:])

# Source tags and prerequisites: no pretending we have all historic L2/context data.
S={
'BAR': ('HL_FILLS', 'При достаточной полноте timestamp и price; агрегировать только завершённые интервалы.'),
'FILL': ('HL_FILLS', 'Дедуплицировать торговые события по уникальному идентификатору; проверить сторону aggressor.'),
'BBO': ('HL_BBO_42D', 'Локальные top-of-book snapshots около 42 дней, ограниченный набор монет; снимки не равны order events.'),
'BOOK': ('FULL_L2_REQUIRED', 'Полная архивная глубина/обновления не гарантированы; для HIP-3 исторических L2 может не быть.'),
'CONTEXT': ('HL_ASSET_CONTEXT_HISTORY', 'Нужен point-in-time архив funding/OI/mark/oracle, а не сегодняшнее состояние.'),
'EXTERNAL': ('DIRECT_CEX_PUBLIC_FEEDS', 'Исторические публичные сделки/свечи доступны не для всех метрик; full historical BBO требуется отдельно.'),
'ADDRESS': ('HL_FILLS_AND_ADDRESS_RECON', 'Нужны правильные идентификаторы сторон и начальные позиции; позиция не равна одному fill.'),
'HIP3': ('HIP3_MARKET_AND_PRIMARY_TIMES', 'Требуются исторические торговые часы, отметки сессий и первичные mark/oracle данные.'),
'EVENT': ('PRIMARY_TIMESTAMPED_EVENTS', 'Нужны опубликованные на тот момент события и время доступности.'),
'LIVE': ('LIVE_ORDER_LIFECYCLE', 'Только реальные live ордера и их статусы позволяют проверять собственную очередь.'),
'PORT': ('PORTFOLIO_REPLAY', 'Нужен общий equity, cross margin, занятые слоты, комиссии, risk cap.'),
}

seeds=[]
def add(group,source,items,windows=('30s','2m','5m','15m','1h','4h'),market='main_perp+HIP3',notes=''):
    for item in items.strip().split('\n'):
        if not item.strip() or item.lstrip().startswith('#'): continue
        parts=item.strip().split(' | ')
        assert len(parts)==3,(group,item)
        name,formula,mechanism=parts
        seeds.append(dict(group=group,source=source,name=name,formula=formula,mechanism=mechanism,windows=list(windows),market=market,notes=notes))

add('Price path / return geometry','BAR', '''
Log return | ln(C(t)/C(t-W)) | Импульс движения
Drawdown from rolling peak | C(t)/max(C[t-W:t])-1 | Давление от пика
Runup from rolling low | C(t)/min(C[t-W:t])-1 | Восстановление от минимума
Rolling close-location | (C(t)-min(L[t-W:t]))/(max(H[t-W:t])-min(L[t-W:t])+eps) | Положение закрытия внутри диапазона
Directional efficiency | abs(C(t)-C(t-W))/(sum(abs(diff(C)),W)+eps) | Монотонность движения
Path excess length | sum(abs(diff(log(C))),W)-abs(log(C(t)/C(t-W))) | Шум относительно движения
Positive return share | count(ret_1m>0,W)/(count(ret_1m,W)+eps) | Доля положительных интервалов
Negative return share | count(ret_1m<0,W)/(count(ret_1m,W)+eps) | Доля отрицательных интервалов
Positive return mean | mean(max(ret_1m,0),W) | Сила положительных интервалов
Negative return mean | mean(min(ret_1m,0),W) | Сила отрицательных интервалов
Maximum up bar | max(ret_1m,W) | Максимальный положительный скачок
Maximum down bar | min(ret_1m,W) | Максимальный отрицательный скачок
Reversal fraction from low | (C(t)-min(L,W))/(max(H,W)-min(L,W)+eps) | Доля возврата после минимума
Reversal fraction from high | (max(H,W)-C(t))/(max(H,W)-min(L,W)+eps) | Доля отката после максимума
Longest negative bar streak | max_run(ret_1m<0,W) | Продолжительность непрерывного снижения
Longest positive bar streak | max_run(ret_1m>0,W) | Продолжительность непрерывного роста
Close-open volatility ratio | abs(C(t)-O(t-W))/(max(H,W)-min(L,W)+eps) | Доля направленного движения в диапазоне
Gap from previous swing | (C(t)-prior_confirmed_swing(W))/(prior_confirmed_swing(W)+eps) | Расстояние до уже подтверждённого экстремума
Time since low | t-argmin_time(L,W) | Сколько прошло после минимума
Time since high | t-argmax_time(H,W) | Сколько прошло после максимума
Price stall ratio | count(abs(ret_1m)<past_median_abs_ret_1m,W)/count(ret_1m,W) | Доля остановок
Overnight jump share | sum(abs(ret_1m)*session_boundary_indicator,W)/(sum(abs(ret_1m),W)+eps) | Доля скачков на смене сессии
''')
add('Realized volatility / jumps / tails','BAR', '''
Realized variance | sum(ret_1m^2,W) | Квадратичная вариация
Realized upside variance | sum(max(ret_1m,0)^2,W) | Верхняя полувариация
Realized downside variance | sum(min(ret_1m,0)^2,W) | Нижняя полувариация
Upside-downside variance ratio | sum(max(ret_1m,0)^2,W)/(sum(min(ret_1m,0)^2,W)+eps) | Асимметрия волатильности
Absolute variation | sum(abs(ret_1m),W) | Суммарный путь
Bipower variation | (pi/2)*sum(abs(ret_1m)*abs(lag(ret_1m,1)),W) | Непрерывная компонента вариации
Jump proxy | max(sum(ret_1m^2,W)-(pi/2)*sum(abs(ret_1m)*abs(lag(ret_1m,1)),W),0) | Скачковая компонента
Downside tail count | count(ret_1m<q_past(ret_1m,0.01,90d),W) | Частота нижних хвостов
Upside tail count | count(ret_1m>q_past(ret_1m,0.99,90d),W) | Частота верхних хвостов
Return skew | skew(ret_1m,W) | Направленная асимметрия
Return kurtosis | excess_kurtosis(ret_1m,W) | Тяжесть хвостов
Volatility acceleration | rv(W)/rv(4*W)-1 | Ускорение волатильности
Downside ulcer | sqrt(mean(min(C/rolling_max(C,W)-1,0)^2,W)) | Глубина текущих просадок
Volatility of volatility | stdev(rv(5m),W) | Изменчивость волатильности
High-low range variance | mean(ln(H/L)^2,W) | Размах свечей
Range-to-volume | (max(H,W)-min(L,W))/(sum(trade_usd,W)+eps) | Движение к объёму
Trend-adjusted residual variance | var(ret_1m-beta_past*ret_BTC_1m,W) | Остаточная вариация
Return entropy | shannon_hist(ret_1m,W,past_only_bins) | Неопределённость доходностей
Sign entropy | shannon_hist(sign(ret_1m),W) | Неопределённость направления
Extreme move concentration | sum_largest_k(abs(ret_1m),k=3,W)/(sum(abs(ret_1m),W)+eps) | Концентрация движения
''')
add('Taker trade flow / pressure','FILL', '''
Signed taker USD | sum(taker_sign*trade_usd,W) | Направленный поток
Taker buy USD | sum(trade_usd where taker_sign>0,W) | Покупательское давление
Taker sell USD | sum(trade_usd where taker_sign<0,W) | Давление продаж
Taker buy fraction | sum(buy_usd,W)/(sum(all_usd,W)+eps) | Доля агрессивных покупок
Taker imbalance | sum(taker_sign*trade_usd,W)/(sum(trade_usd,W)+eps) | Дисбаланс потока
Taker count imbalance | sum(taker_sign,W)/(count(trade,W)+eps) | Перекос по числу сделок
Trade count intensity | count(trade,W)/seconds(W) | Интенсивность исполнения
Trade notional velocity | sum(trade_usd,W)/seconds(W) | Долларовая скорость торгов
Mean trade notional | mean(trade_usd,W) | Типичный размер сделки
Trade size CV | std(trade_usd,W)/(mean(trade_usd,W)+eps) | Неоднородность размеров
Trade size top-decile share | sum(trade_usd if size>past_q(size,0.9),W)/(sum(trade_usd,W)+eps) | Доля крупных ордеров
Trade size concentration | sum(trade_usd^2,W)/(sum(trade_usd,W)^2+eps) | Концентрация исполнений
Taker sign autocorr lag1 | corr(taker_sign,lag(taker_sign,1),W) | Настойчивость направления
Taker sign autocorr lag5 | corr(taker_sign,lag(taker_sign,5),W) | Длинная память потока
Longest sell run | max_run(taker_sign<0,W) | Каскад агрессивных продаж
Longest buy run | max_run(taker_sign>0,W) | Каскад агрессивных покупок
Intertrade arrival median | median(diff(trade_time),W) | Интервалы между сделками
Intertrade arrival CV | std(diff(trade_time),W)/(mean(diff(trade_time),W)+eps) | Кластеризация времени
Trade sign entropy | shannon_hist(taker_sign,W) | Неопределённость потока
Trade burstiness | (std(intertrade_dt,W)-mean(intertrade_dt,W))/(std(intertrade_dt,W)+mean(intertrade_dt,W)+eps) | Неравномерность торгов
Signed-flow acceleration | signed_usd(W)/seconds(W)-lag(signed_usd(W)/seconds(W),W) | Ускорение давления
Trade-side flip count | count(sign_flip(taker_sign),W) | Переключения сторон
Sell-to-buy speed | time_to_first_buy_after_sell_streak(W) | Скорость прекращения продаж
Absorption price-flow ratio | abs(ret(W))/(abs(signed_usd(W))+eps) | Изменение цены на направленный доллар
Flow normalization by typical | signed_usd(W)/(past_median(abs(signed_usd(W)),90d)+eps) | Необычность потока
Flow-to-open-interest | signed_usd(W)/(latest_past_OI_usd+eps) | Давление к объёму позиций
Price-flow disagreement | sign(ret(W))!=sign(signed_usd(W)) | Дивергенция цены и потока
Trade price dispersion | std(trade_price,W)/(mean(trade_price,W)+eps) | Разброс цен исполнения
Dominant side dwell time | max_run_duration(taker_sign,W) | Устойчивость одной стороны
''')
add('BBO / book surface','BBO', '''
Quoted spread bps | 1e4*(best_ask-best_bid)/mid | Стоимость немедленного исполнения
Top bid USD depth | best_bid*best_bid_size | Доступный верхний bid
Top ask USD depth | best_ask*best_ask_size | Доступный верхний ask
Top-book imbalance | (best_bid_size-best_ask_size)/(best_bid_size+best_ask_size+eps) | Дисбаланс глубины верхнего уровня
Top-book microprice | (best_ask*best_bid_size+best_bid*best_ask_size)/(best_bid_size+best_ask_size+eps) | Взвешенная по ликвидности цена
Spread maximum | max(spread_bps,W) | Пик издержек исполнения
Spread median | median(spread_bps,W) | Типичный спред
Spread persistence | count(spread_bps>past_q(spread_bps,0.9),W)/count(bbo,W) | Длительность широкого спреда
Bid depth depletion | (bid_depth(t-W)-bid_depth(t))/(bid_depth(t-W)+eps) | Исчезновение покупателей
Ask depth depletion | (ask_depth(t-W)-ask_depth(t))/(ask_depth(t-W)+eps) | Исчезновение продавцов
Bid depth resilience | bid_depth(t)/(past_median(bid_depth,30d)+eps) | Восстановление глубины bid
Ask depth resilience | ask_depth(t)/(past_median(ask_depth,30d)+eps) | Восстановление глубины ask
Mid movement per top depth | abs(mid(t)-mid(t-W))/(bid_depth(t)+ask_depth(t)+eps) | Чувствительность к тонкому рынку
Quote age | t-last_change_time(best_bid,best_ask) | Устаревание котировок
Locked/crossed snapshot indicator | best_bid>=best_ask | Аномальная книга или ошибка данных
Spread-depth product | spread_bps*(1/(bid_depth+ask_depth+eps)) | Совместная тонкость и спред
Bid-to-ask reprice asymmetry | abs(change(best_bid,W))/(abs(change(best_ask,W))+eps) | Асимметрия изменения сторон
Mid-printed-trade gap | (last_trade_price-mid)/mid | Отклонение last от mid
Top-book pressure composite | imbalance*(1/(spread_bps+eps)) | Давление с поправкой на спред
BBO message arrival rate | count(bbo_updates,W)/seconds(W) | Скорость изменения котировок
''',windows=('5s','15s','1m','5m'),notes='Только top-of-book; не выдавать за full-depth L2 или очередь.')
add('Full depth / order book events','BOOK', '''
Depth to +10bp | sum(ask_size*ask_px where ask_px<=mid*1.001) | Доступный ask близко к mid
Depth to -10bp | sum(bid_size*bid_px where bid_px>=mid*0.999) | Доступный bid близко к mid
Depth to +50bp | sum(ask_size*ask_px where ask_px<=mid*1.005) | Глубина продаж 50bp
Depth to -50bp | sum(bid_size*bid_px where bid_px>=mid*0.995) | Глубина покупок 50bp
Book slope bid | robust_slope(cumulative_bid_usd,price_distance_bp) | Форма bid-книги
Book slope ask | robust_slope(cumulative_ask_usd,price_distance_bp) | Форма ask-книги
Multi-level OFI | sum(level_weight*delta_quoted_bid_minus_ask,W) | Изменения ликвидности по уровням
Cancellation pressure bid | sum(removed_bid_not_traded_usd,W) | Снятие покупателей
Cancellation pressure ask | sum(removed_ask_not_traded_usd,W) | Снятие продавцов
Best queue depletion time | remaining_best_bid_usd/(aggressive_sell_usd_per_sec+eps) | Время до истощения bid
Replenishment half-life | time_for_book_depth_to_recover_half(previous_shock,W) | Скорость восстановления
Sweepable sell size | notional_to_move_bid_mid_down_50bp(book_now) | Цена агрессивной продажи
Sweepable buy size | notional_to_move_ask_mid_up_50bp(book_now) | Цена агрессивной покупки
Book convexity | curvature(cumulative_depth_by_price(book_now)) | Нелинейность доступного объёма
Order-book cancellation entropy | entropy(cancelled_level_indices,W) | Где снимается ликвидность
''',windows=('5s','30s','2m','10m'),notes='Нужна точная история уровней и diff updates; нельзя восстановить по BBO.')
add('Impact / resilience','FILL', '''
Impact per sell dollar | abs(min(ret(W),0))/(abs(min(signed_usd(W),0))+eps) | Локальная цена потока продаж
Impact per buy dollar | max(ret(W),0)/(max(signed_usd(W),0)+eps) | Локальная цена потока покупок
Price-flow sign agreement | sign(ret(W))*sign(signed_usd(W)) | Согласие движения и давления
Impact acceleration | (abs(ret(W))/(sum(trade_usd,W)+eps))/(abs(ret(2*W))/(sum(trade_usd,2*W)+eps)+eps) | Изменение чувствительности
Marginal sell impact | abs(ret(W))/(sum(sell_usd,W)+eps) | Давление на единицу продаж
Marginal buy impact | abs(ret(W))/(sum(buy_usd,W)+eps) | Давление на единицу покупок
Flow concentration at low | sum(sell_usd where trade_price<=past_q(trade_price,0.1),W)/(sum(sell_usd,W)+eps) | Продавцы у минимума
Flow concentration at high | sum(buy_usd where trade_price>=past_q(trade_price,0.9),W)/(sum(buy_usd,W)+eps) | Покупатели у максимума
Shock fade observed | (ret(W)-ret(W/2))/(abs(ret(W))+eps) | Уже наблюдаемый возврат движения
Last-move persistence | ret(W/2)/(ret(W)+eps) | Что осталось от старого движения
''')
add('Derivatives / mark / oracle','CONTEXT', '''
Open interest delta | (OI_usd(t)-OI_usd(t-W))/(OI_usd(t-W)+eps) | Открытие или закрытие позиций
OI turnover ratio | trade_usd(W)/(OI_usd(t)+eps) | Ротация контрактов
Funding divergence | funding_t-past_median(funding,90d) | Экстремальность ставки
Funding slope | funding_t-funding(t-W) | Изменение финансирования
Mark-last basis | (mark-last_trade)/mark | Разрыв исполнения и маржи
Mark-oracle basis | (mark-oracle)/oracle | Отклонение маржинальной цены
OI-price quadrant | sign(delta_OI(W))*sign(ret(W)) | Согласие цены и позиций
Funding accumulated | sum(past_realized_funding_cashflow_events,W) | Стоимость удержания
Oracle update age | t-oracle_last_update_time | Устаревание ориентира
Mark update age | t-mark_last_update_time | Устаревание маржинальной цены
OI velocity | delta_OI_usd(W)/seconds(W) | Скорость изменения позиций
OI-to-volume pressure | OI_usd/(trade_usd(W)+eps) | Загруженность рынка позициями
Funding deviation from spot basis | funding_t-normalized_spot_perp_basis_past | Несоответствие финансирования и базиса
''',windows=('15m','1h','4h','24h'))
add('Direct cross-venue / price discovery','EXTERNAL', '''
HL-Binance mid residual | ln(mid_HL/mid_Binance)-past_median(ln(mid_HL/mid_Binance),W) | Локальная дислокация HL
HL-Bybit mid residual | ln(mid_HL/mid_Bybit)-past_median(ln(mid_HL/mid_Bybit),W) | Локальная дислокация HL
CEX median reference residual | ln(mid_HL/median(valid_fresh_CEX_mids))-past_basis(W) | Общая внешняя справочная цена
Binance taker imbalance | sum(taker_sign*trade_usd,W)/sum(trade_usd,W) on Binance | Направленный поток Binance
Bybit taker imbalance | sum(taker_sign*trade_usd,W)/sum(trade_usd,W) on Bybit | Направленный поток Bybit
CEX-HL flow agreement | sign(signed_usd_HL(W))*sign(signed_usd_CEX(W)) | Синхронность потоков
CEX-HL local return residual | ret_HL(W)-beta_past*ret_CEX(W) | Идиосинкратическое движение HL
Cross-venue spread widening | max(mid_venue)-min(mid_venue) across fresh venues | Фрагментация цены
CEX/HL traded volume ratio | trade_usd_CEX(W)/(trade_usd_HL(W)+eps) | Относительная активность
CEX trade leadership | lagged_corr_past(ret_CEX_1s,ret_HL_1s,W) | Историческое лидерство потока
Spot-perp basis | ln(mid_perp/mid_spot) | Базис между инструментами
Cross-venue price consensus | 1/(std(log(fresh_mid_venues))+eps) | Согласие площадок
Cross-venue quote staleness | max(current_time-last_quote_ts(venue)) | Возраст входных котировок
CEX pressure surprise | signed_usd_CEX(W)/(past_median(abs(signed_usd_CEX(W)),90d)+eps) | Необычность внешнего потока
''',windows=('30s','2m','5m','15m','1h'))
add('Cross-sectional / factors / sectors','BAR', '''
BTC residual return | ret_coin(W)-beta_BTC_past*ret_BTC(W) | Идиосинкратическая доходность
ETH residual return | ret_coin(W)-beta_ETH_past*ret_ETH(W) | Идиосинкратическая доходность
Factor R2 | past_rolling_R2(ret_coin,ret_BTC,ret_ETH,W) | Объяснимость движения факторами
Sector-relative return | ret_coin(W)-median(ret_same_sector(W)) | Относительная доходность к сектору
Sector breadth positive | share(coin_ret(W)>0,sector) | Ширина сектора
Sector return dispersion | std(ret_all_sector(W)) | Расхождение в секторе
Market breadth positive | share(ret_eligible_coins(W)>0) | Ширина рынка
Median alt return | median(ret_eligible_alts(W)) | Типичное движение альткоинов
Market crash synchrony | share(ret_coins(W)<q_past(ret_coins(W),0.05)) | Синхронность экстремумов
Top decile momentum spread | median(top_decile_ret(W))-median(bottom_decile_ret(W)) | Фактор относительной силы
Cross-sectional return skew | skew(ret_universe(W)) | Перекос по монетам
Sector beta drift | beta_coin_sector_recent(W)-beta_coin_sector_past(4*W) | Смена факторной нагрузки
Market leader turnover | count_changes(top_volume_ranked_coin,W) | Смена лидеров рынка
Dominance of BTC flow | BTC_trade_usd(W)/sum(trade_usd_all_coins,W) | Концентрация оборота в BTC
Coin idiosyncratic volatility | stdev(ret_coin_1m-beta_past*ret_BTC_1m,W) | Остаточный риск монеты
''',windows=('15m','1h','4h','24h','7d'))
add('Accounts / participants','ADDRESS', '''
Active taker wallets | count_distinct(taker_address,W) | Ширина активности участников
Active maker wallets | count_distinct(maker_address,W) | Ширина пассивного предложения
Taker notional HHI | sum_over_addr((addr_taker_usd(W)/total_taker_usd(W))^2) | Концентрация агрессивного потока
Maker notional HHI | sum_over_addr((addr_maker_usd(W)/total_maker_usd(W))^2) | Концентрация исполнений поставщиков
New wallet share | share(addr_first_seen_before_t <= W) | Приток новых участников (с ограничением наблюдаемой истории)
Large wallet sell share | sell_usd_from_past_topN_volume_addresses(W)/sell_usd(W) | Доля продаж крупных адресов
Cross-asset wallet overlap | jaccard(taker_addresses_coin,other_coin,W) | Общие активные участники
Wallet directional consensus | share_wallets(net_signed_flow_per_wallet>0,W) | Согласованность адресов
Wallet flow herding | corr_direction_of_wallets_within_group(W) | Синхронность действий
Maker-to-taker role flips | count(addr_switches_from_maker_to_taker,W) | Переключение ролей
Wallet stopout proxy | share_accounts_with_observed_large_loss_exits(W) | Потенциальные вынужденные выходы
Address holding age | median(time_since_first_observed_entry_per_address,W) | Возраст наблюдаемого позиционирования
Multi-coin forced selling | count_addresses_selling_3plus_coins_within(W) | Кросс-активные разгрузки
Net profitable close ratio | realized_profit_closes/(all_closes+eps) | Доля закрытий с прибылью при полной реконструкции позиции
''',windows=('5m','15m','1h','4h','24h'))
add('HIP-3 / session / oracle','HIP3', '''
Minutes to US open | minutes_to_next_primary_market_open(t) | Близость открытой базовой сессии
Minutes since US close | minutes_since_primary_market_close(t) | Возраст закрытия рынка базы
Closed-underlying flag | not primary_market_open(t) | База закрыта
Weekend elapsed hours | hours_since_saturday_00UTC(t) if weekend else null | Время выходных
Underlying quote freshness | t-last_verified_primary_underlying_quote_ts | Доступность справочной цены
HIP3 intraday volume ratio | trade_usd_HIP3(W)/past_typical_same_session_volume(W) | Аномальная активность HIP-3
HIP3 oracle-last gap | (last_trade-oracle)/oracle | Разрыв цены исполнения и оракула
HIP3 mark-last gap | (last_trade-mark)/mark | Разрыв риска и исполнения
HIP3 gap decay observed | abs(gap(t))/max(abs(gap(t-W)),eps) | Уже наблюдаемая нормализация
HIP3 open session volatility ratio | rv_during_primary_open(W)/(rv_during_primary_closed(W)+eps) | Отличие часов рынка
HIP3 session transition jump | abs(ret_around_primary_open_or_close(W)) | Скачок на переходе сессии
HIP3 pair stale dispersion | std(prices_of_comparable_contracts_adjusted_for_ref,W) | Разброс сопоставимых рынков
''',windows=('15m','1h','4h','24h'),market='HIP3_only')
add('Calendars / primary events','EVENT', '''
Time to scheduled event | event_publish_or_release_ts-t | Близость известного события
Time since observed event | t-known_event_publication_ts | Возраст публикации
Announcement return since known | ln(C(t)/C(known_event_publication_ts)) | Переоценка после объявления
Pre-event realized variance | rv_from_history_ending_at_t(W) | Риск перед событием
Event-frequency per issuer | count_past_confirmed_events(W) | Частота информации
Event type burstiness | count_events_same_type(W)/past_typical_event_count(W) | Скопление событий
Session-open volume ratio | trade_usd_since_session_open/(past_avg_same_elapsed_session_volume+eps) | Активность открытия
Holiday-session indicator | known_holiday_calendar(t) | Закрытие базового рынка
Funding-window proximity | time_to_next_scheduled_funding_boundary(t) | Время до выплаты
Expiry-window proximity | time_to_known_exchange_expiry(t) | Близость экспирации
''',windows=('1h','4h','24h','7d'))
add('Execution / order lifecycle','LIVE', '''
Own order ACK lag | exchange_ack_ts-client_submit_ts | Задержка постановки
Own cancel ACK lag | exchange_cancel_ack_ts-client_cancel_ts | Задержка отмены
Own fill ratio | filled_qty/requested_qty | Исполнимость собственного ордера
Own partial fill count | count(partial_fills_for_order,W) | Частичное исполнение
Maker adverse drift | signed_log_mid_change_after_completed_fill_observed_only_in_expost_eval | Убыток от отбора (только label)
Order live dwell | sum(intervals_order_confirmed_active,W) | Время в книге
Cancel-replace gap | new_order_ack_ts-old_cancel_ack_ts | Период без заявки
Own stop slippage | realized_exit_px-trigger_stop_px | Реальное исполнение защиты
Own take slippage | realized_exit_px-limit_take_px | Точность тейка
Order submit rejection rate | rejected_submissions(W)/(all_submissions(W)+eps) | Ошибки постановки
''',windows=('1h','4h','24h'))
add('Portfolio / risk state','PORT', '''
Portfolio gross exposure | sum(abs(position_notional(t))) | Общая экспозиция
Portfolio net exposure | sum(signed_position_notional(t)) | Направленная экспозиция
BTC factor beta exposure | sum(pos_notional_i*beta_i_BTC_past)/equity | Совокупная бета
Top coin exposure share | max(abs(position_notional_i))/sum(abs(position_notional_i)) | Концентрация в монете
Top sector exposure share | max(sector_exposure)/gross_exposure | Концентрация сектора
Stop-risk sum | sum(predicted_stop_loss_i)/equity | Портфельный плановый риск
Available margin fraction | available_margin/equity | Запас обеспечения
Per-DEX maintenance buffer | collateral_dex/maintenance_margin_dex | Запас до риска ликвидации
Rolling portfolio drawdown | equity(t)/running_peak_equity(t)-1 | Просадка капитала
Portfolio exposure correlation | weighted_average_past_pairwise_corr(positions,W) | Совместное направление риска
Simultaneous signal congestion | count(valid_signals_competing_for_slots,W) | Конкуренция сигналов
Free slot fraction | available_strategy_slots/total_strategy_slots | Доступность исполнений
Portfolio trade age median | median(t-position_entry_ts) | Возраст открытых сделок
Margin capital fragmentation | std(available_margin_per_dex/equity) | Неравномерность маржи
''',windows=('15m','1h','4h','24h'))

# Produce genuinely specified indicator *instances*; derived transforms operate on strictly past values.
# Unknown primitives carry explicit availability caveats above. Do not infer that any field is already archived.
indicator_specs=[]
transforms=[('raw', '{expr}'), ('rank90d','rank_past({expr},lookback=90d)'), ('z90d','z_past({expr},lookback=90d)'), ('slope8','slope_past({expr},n_completed=8)')]
for i,base in enumerate(seeds,1):
    for win in base['windows']:
        expr=re.sub(r'\bW\b',win,base['formula']) # symbolic W token only; never mutate VWAP/CEX
        for transform,pattern in transforms:
            if base['source'] in {'EVENT','LIVE','PORT'} and transform=='slope8' and ('indicator' in base['name'].lower() or 'flag' in base['name'].lower()):
                continue
            output=pattern.replace('{expr}',f'({expr})')
            evaluation_only=base['name']=='Maker adverse drift'
            indicator_specs.append(dict(id=f'IND-{len(indicator_specs)+1:05d}',base_id=f'NEW-{i:03d}',name=f'{base["name"]} [{win}; {transform}]',family=base['group'],source=base['source'],data_tag=S[base['source']][0],window=win,transform=transform,expression=output,market=base['market'],economic_mechanism=base['mechanism'],availability_caveat=S[base['source']][1],status='EVALUATION_ONLY_NOT_A_SIGNAL' if evaluation_only else 'GENERATED_UNTESTED',feature_role='evaluation_label' if evaluation_only else 'signal_eligible_if_data_available',formula_status='DSL_PSEUDOCODE_NOT_A_RUNTIME_IMPLEMENTATION'))

# A self-contained illustrative DSL for hypotheses: every trigger uses completed data at decision timestamp t.
# Mechanism catalogs are structured: distinct trigger/confirm lists combined intentionally, not blind mixing of every metric.
families=[]
def fam(name, mechanism, sources, markets, side, triggers, confirms, entries, exits, overlap='medium', universe='eligible_liquid', notes=''):
    families.append(dict(name=name,mechanism=mechanism,sources=sources,markets=markets,side=side,
        triggers=triggers,confirms=confirms,entries=entries,exits=exits,overlap=overlap,universe=universe,notes=notes))

# Conditions are detailed DSL predicates, threshold and time window instantiated below; all pX() fit only on preceding observations.
W='${W}'; Q='${Q}'; C='${C}'; S1='${S}'
# Candidate generation uses independent event families; do not claim novelty of every parameter variant.
fam('Local shock reversal','Локальный ценовой шок мог быть неинформативным','FILL+BAR+EXTERNAL','crypto', 'LONG',
    ['ret(coin,W)<qPast(ret(coin,W),Q)', 'impact_sell(W)>pPast(impact_sell(W),Q)', 'ret_residual_vs_cex(W)<qPast(ret_residual_vs_cex(W),Q)', 'sell_run_length(W)>pPast(sell_run_length(W),Q)', 'drawdown_from_peak(W)<qPast(drawdown_from_peak(W),Q)'],
    ['abs(ret_CEX(W))<pPast(abs(ret_CEX(W)),C)', 'buy_taker_fraction(W)>pPast(buy_taker_fraction(W),C)', 'flow_acceleration(W)>0', 'mid_reclaims_prior_low(W)', 'spread_bps<=pPast(spread_bps,C)'],
    ['TAKER_NEXT_BAR','PASSIVE_1ATR_BELOW_REF'],['TIME_OR_STOP','STRUCTURE_RETRACE'],overlap='high',notes='Сравнить с DIP/Wick; внешняя quote нужна если сигнал её использует.')
fam('Local shock continuation','Локальный продавец продолжает давление при ухудшении ликвидности','FILL+BAR+EXTERNAL','crypto','SHORT',
    ['signed_usd(W)<qPast(signed_usd(W),Q)', 'sell_run_length(W)>pPast(sell_run_length(W),Q)', 'impact_sell(W)>pPast(impact_sell(W),Q)', 'flow_acceleration(W)<qPast(flow_acceleration(W),Q)'],
    ['ret_CEX(W)<0', 'no_reclaim_of_low(W)', 'market_breadth_negative(W)>C', 'spread_bps>pPast(spread_bps,C)', 'OI_delta(W)>0'],
    ['TAKER_NEXT_BAR'],['TRAIL_OR_STOP','TIME_OR_STOP'],overlap='medium')
fam('Liquidation absorption','Вынужденная разгрузка поглощается и перестаёт двигать цену','FILL+BAR+CONTEXT','crypto+HIP3','LONG',
    ['sell_run_length(W)>pPast(sell_run_length(W),Q)', 'liquidation_usd(W)>pPast(liquidation_usd(W),Q)', 'OI_delta(W)<qPast(OI_delta(W),Q)', 'sell_share_near_low(W)>pPast(sell_share_near_low(W),Q)'],
    ['ret(W/2)>ret(W)', 'price_flow_disagreement(W)', 'bid_depth_resilience(W)>C', 'taker_buy_fraction(W)>C'],
    ['TAKER_NEXT_BAR','RESTING_BID_IF_VALID'],['TIME_OR_STOP','STRUCTURE_RETRACE'],overlap='high',notes='Liquidation event tagging must be authoritative; no proxy mislabeled as confirmed liquidation.')
fam('Orderflow trend','Направленный поток поддерживает устойчивое движение','FILL+BAR','crypto+HIP3','BOTH',
    ['signed_usd(W)>pPast(signed_usd(W),Q)', 'trade_count_intensity(W)>pPast(trade_count_intensity(W),Q)', 'taker_sign_autocorr(W)>pPast(taker_sign_autocorr(W),Q)', 'directional_efficiency(W)>pPast(directional_efficiency(W),Q)', 'close_location(W)>C'],
    ['OI_delta(W)>0','ret_sector(W)>0','mid_above_VWAP(W)','spread_bps<pPast(spread_bps,C)','BTC_regime_up(W)'],
    ['TAKER_NEXT_BAR','RETEST_LIMIT'],['TRAIL_OR_STOP','TIME_OR_STOP'],overlap='low')
fam('Orderflow fade','Односторонний поток перестаёт создавать движение','FILL+BAR','crypto+HIP3','CONTRA_FLOW',
    ['sell_usd(W)>pPast(sell_usd(W),Q)','buy_usd(W)>pPast(buy_usd(W),Q)','trade_count_intensity(W)>pPast(trade_count_intensity(W),Q)','flow_concentration_at_extreme(W)>pPast(flow_concentration_at_extreme(W),Q)'],
    ['abs(ret(W/2))<C*abs(ret(W))','price_flow_disagreement(W)','trade_sign_entropy(W)>C','opposing_taker_flow_rises(W)'],
    ['TAKER_NEXT_BAR'],['TIME_OR_STOP','STRUCTURE_RETRACE'],overlap='medium')
fam('Book resilience','Книга восстанавливается быстрее цены','BBO+BOOK','crypto','LONG',
    ['bid_depth_depletion(W)>pPast(bid_depth_depletion(W),Q)','spread_bps>pPast(spread_bps,Q)','multi_level_OFI(W)<qPast(multi_level_OFI(W),Q)','book_slope_bid(W)<qPast(book_slope_bid(W),Q)'],
    ['bid_depth_resilience(W)>C','spread_compresses(W)','microprice_above_mid(W)','cancellation_pressure_bid(W)<C'],
    ['RESTING_BID_IF_VALID','TAKER_NEXT_BAR'],['TIME_OR_STOP','MAKER_TAKE'],overlap='medium',notes='BOOK criteria untestable on top BBO alone.')
fam('Book fragility','Ликвидность исчезает до ускорения направления','BBO+BOOK','crypto','SHORT',
    ['bid_depth_depletion(W)>pPast(bid_depth_depletion(W),Q)','cancel_pressure_bid(W)>pPast(cancel_pressure_bid(W),Q)','spread_bps>pPast(spread_bps,Q)','top_book_imbalance(W)<qPast(top_book_imbalance(W),Q)'],
    ['ret(W)<0','sell_usd(W)>pPast(sell_usd(W),C)','best_bid_reprices_down(W)','CEX_sells(W)'],
    ['TAKER_NEXT_BAR'],['TRAIL_OR_STOP','TIME_OR_STOP'],overlap='low')
fam('Maker spread carry','Пассивное исполнение в нетоксичном широком спреде','BBO+LIVE','crypto+HIP3','MAKER_BOTH',
    ['spread_bps>pPast(spread_bps,Q)','top_bid_depth+top_ask_depth<pPast(book_depth,C)','mid_stability(W)>C','quote_age(W)>pPast(quote_age(W),Q)'],
    ['signed_flow_near_zero(W)','no_event_risk_within(W)','own_order_age>min_ack_buffer','estimated_capture_after_costs>0'],
    ['POST_ONLY_DYNAMIC'],['INVENTORY_CLOSE','TIME_OR_STOP'],overlap='low',notes='Backtest fills NOT confirmed without live queue; do not infer from mid touch.')
fam('CEX-HL dislocation','Две исполнимые площадки временно расходятся','EXTERNAL+BBO','crypto','BOTH',
    ['abs(hl_cex_basis_z(W))>qPast(abs(hl_cex_basis_z(W)),Q)','hl_cex_quote_gap_bps>pPast(hl_cex_quote_gap_bps,Q)','abs(CEX_HL_flow_disagreement(W))>C','cex_price_moves_without_hl(W)'],
    ['quotes_fresh_all_venues','two_side_executable_after_fees','hedge_capacity_available','basis_reverts_observed(W)'],
    ['HEDGED_PAIR_MARKET','TAKER_NEXT_BAR'],['PAIR_SPREAD_STOP','TIME_OR_STOP'],overlap='high',notes='Сравнить с закрытым V9; stale/mark цены не использовать как executable.')
fam('Basis term evolution','Funding/базис изменяет доходность пары, а не одиночной ставки','EXTERNAL+CONTEXT','crypto','HEDGED',
    ['abs(spot_perp_basis(W))>pPast(abs(spot_perp_basis(W)),Q)','funding_deviation(W)>pPast(funding_deviation(W),Q)','OI_turnover_ratio(W)>pPast(OI_turnover_ratio(W),Q)'],
    ['hedge_both_legs_executable','expected_basis_decay_bps>all_cost_bps','cross_margin_buffers_safe','funding_calendar_known'],
    ['HEDGED_PAIR_MARKET'],['PAIR_SPREAD_STOP','TIME_OR_STOP'],overlap='high',notes='Ранее исследовались basis/carry; лишь новая реализация/контекст.')
fam('Cross-asset factor residual','Актив временно расходится со своим фактором или сектором','BAR','crypto+HIP3','PAIR_CONTRA_RESIDUAL',
    ['abs(sector_residual(W))>pPast(abs(sector_residual(W)),Q)','abs(BTC_factor_residual(W))>pPast(abs(BTC_factor_residual(W)),Q)','sector_dispersion(W)>pPast(sector_dispersion(W),Q)'],
    ['factor_betas_stable_past(W)','both_legs_liquid','transaction_costs_pair<expected_edge','no_known_idiosyncratic_event'],
    ['BETA_NEUTRAL_PAIR'],['PAIR_SPREAD_STOP','TIME_OR_STOP'],overlap='high',notes='Нужно сравнить с V12 cross-section и проверять только новые механизмы.')
fam('Cross-market cascade','Общий отрицательный поток создаёт системный риск','BAR+FILL+EXTERNAL','crypto','SHORT_OR_HEDGE',
    ['market_breadth_negative(W)>Q','signed_flow_fraction_negative_coins(W)>Q','cross_section_corr(W)>pPast(cross_section_corr(W),Q)','BTC_downside_variance(W)>pPast(BTC_downside_variance(W),Q)'],
    ['hl_bid_depth_depletion_basket(W)>C','spread_widens_basket(W)','CEX_selling_confirmed(W)','portfolio_gross_long_exposure>0'],
    ['HEDGE_TO_BETA_TARGET','TAKER_NEXT_BAR'],['HEDGE_EXIT','TIME_OR_STOP'],overlap='low',notes='Сравнить с простым уменьшением long экспозиции и старым breadth filter.')
fam('HIP3 closed base mean reversion','Тонкая HIP-3 книга без свежей внешней цены может иметь временные проколы','HIP3+FILL','HIP3','LONG',
    ['closed_underlying_flag','hip3_oracle_last_gap(W)<qPast(hip3_oracle_last_gap(W),Q)','hip3_volume_ratio(W)>pPast(hip3_volume_ratio(W),Q)','hip3_downside_jump(W)<qPast(hip3_downside_jump(W),Q)'],
    ['last_quote_age_primary>W','sell_flow_intensity_falls(W)','hip3_reclaim_fraction(W)>C','spread_bps<C'],
    ['TAKER_NEXT_BAR','RESTING_BID_IF_VALID'],['TIME_OR_STOP','STRUCTURE_RETRACE'],overlap='high',notes='Сравнить с уже живой weekend HIP-3, не подменять внешний стакан устаревшей ценой.')
fam('HIP3 opening discovery','Цена HIP-3 меняется при открытии рынка базового актива','HIP3+EXTERNAL+EVENT','HIP3','BOTH',
    ['minutes_since_primary_open<W','hip3_price_diverges_from_fresh_primary(W)','hip3_open_session_volume_ratio(W)>Q','primary_open_gap_abs(W)>Q'],
    ['fresh_underlying_quote','executable_spread_below_budget','CEX_equity_direction_confirmed','no_halt_status'],
    ['TAKER_NEXT_BAR','HEDGED_PAIR_MARKET'],['TIME_OR_STOP','PAIR_SPREAD_STOP'],overlap='low',notes='Только если исторически архивированы синхронные equity quotes и календарь.')
fam('Event repricing','Публичное событие создаёт устойчивую новую цену','EVENT+BAR','crypto+HIP3','BOTH',
    ['event_newly_public_at_t','event_trading_intensity(W)>pPast(intensity(W),Q)','event_price_jump(W)>pPast(abs(ret(W)),Q)','event_type_matches_whitelist'],
    ['external_price_confirms(W)','no_oracle_stale','orderbook_recovery_observed(W)','abnormal_volume_persists(W)'],
    ['TAKER_NEXT_BAR','RETEST_LIMIT'],['TRAIL_OR_STOP','TIME_OR_STOP'],overlap='medium',notes='События нужны с достоверными первичными timestamp; не использовать задним числом размеченные новости.')
fam('Account crowded unwind','Концентрированный поток крупных кошельков способен вызвать разгрузку','ADDRESS+FILL','crypto','SHORT',
    ['addr_taker_HHI(W)>pPast(addr_taker_HHI(W),Q)','multi_coin_forced_sell_addresses(W)>pPast(multi_coin_forced_sell_addresses(W),Q)','large_wallet_sell_fraction(W)>pPast(large_wallet_sell_fraction(W),Q)','wallet_direction_consensus_negative(W)>Q'],
    ['OI_delta(W)<0','spread_widens(W)','BTC_return(W)<0','sell_impact_rising(W)'],
    ['TAKER_NEXT_BAR'],['TRAIL_OR_STOP','TIME_OR_STOP'],overlap='medium',notes='Нет уверенной реконструкции позиции => NO_TEST, не заменять proxy уверенностью.')
fam('Account flow exhaustion','Продажи крупной группы прекращаются и временный импакт исчезает','ADDRESS+FILL','crypto','LONG',
    ['large_wallet_sell_fraction(W)>pPast(large_wallet_sell_fraction(W),Q)','addr_taker_HHI(W)>pPast(addr_taker_HHI(W),Q)','wallet_loss_exit_burst(W)>pPast(wallet_loss_exit_burst(W),Q)'],
    ['no_further_large_wallet_sell(W/2)','price_stops_falling(W/2)','CEX_stable(W)','maker_bids_replenish(W)'],
    ['TAKER_NEXT_BAR'],['TIME_OR_STOP','STRUCTURE_RETRACE'],overlap='high')
fam('Low-volatility trend emergence','После длительного затухания начинается новый направленный режим','BAR','crypto+HIP3','BOTH',
    ['rv(W)<qPast(rv(W),Q)','range(W)<qPast(range(W),Q)','trade_count_intensity(W)<qPast(trade_count_intensity(W),Q)','market_dispersion(W)<qPast(market_dispersion(W),Q)'],
    ['breakout_of_past_completed_range(W)','taker_flow_confirms_breakout(W)','CEX_direction_confirms(W)','efficiency_ratio_rising(W)'],
    ['TAKER_NEXT_BAR','RETEST_LIMIT'],['TRAIL_OR_STOP','TIME_OR_STOP'],overlap='low')
fam('High-volatility decay','После шока уменьшается волатильность, возможны новые цены для ликвидности','BAR+BBO','crypto','MAKER_BOTH',
    ['rv(W)>pPast(rv(W),Q)','spread_bps>pPast(spread_bps,Q)','price_path_excess_length(W)>pPast(path_excess_length(W),Q)'],
    ['rv_short_down_vs_long','spread_compresses(W)','market_breadth_stabilizes','bid_depth_replenished'],
    ['POST_ONLY_DYNAMIC'],['INVENTORY_CLOSE','TIME_OR_STOP'],overlap='medium',notes='Бэк-тестовый fill из touch недопустим.')
fam('Session flow transition','На смене сессии меняется качество исполнения и поток','BAR+FILL+EVENT','crypto+HIP3','BOTH',
    ['session_transition_within(W)','volume_ratio_to_same_session(W)>Q','intertrade_arrival_CV(W)>pPast(intertrade_arrival_CV(W),Q)','spread_change_after_open(W)>Q'],
    ['flow_direction_agrees_with_price','CEX_quotes_fresh','no_known_halt','spread_below_budget'],
    ['TAKER_NEXT_BAR','RETEST_LIMIT'],['TIME_OR_STOP','TRAIL_OR_STOP'],overlap='medium')
fam('Portfolio congestion arbiter','Много коррелированных сигналов конкурируют за лимит риска','PORT+BAR','portfolio','ALLOCATION_ONLY',
    ['simultaneous_valid_signals>Q','portfolio_beta_exposure>Q','sector_concentration>Q','available_margin_fraction<C'],
    ['expected_R_net_of_costs_positive','source_signal_live_unchanged','no_cross_Dex_margin_false_nets','max_risk_cap_respected'],
    ['SELECT_RISK_BUDGET'],['PORTFOLIO_RISK_EXIT'],overlap='high',notes='Менять распределение в эксперименте, действующий live не трогать.')
fam('Drawdown regime hedge','Хедж против общей потери, с сопоставлением обычному Risk-Off','PORT+BAR+EXTERNAL','portfolio','HEDGE_OR_REDUCE',
    ['market_breadth_negative(W)>Q','portfolio_beta_exposure>Q','cross_section_corr(W)>pPast(corr(W),Q)','gross_long_exposure>Q'],
    ['CEX_sell_flow_persists(W)','BTC_vol_rising(W)','quote_fresh_and_liquid','maintenance_buffer_adequate'],
    ['HEDGE_TO_BETA_TARGET','SELECT_RISK_BUDGET'],['HEDGE_EXIT','PORTFOLIO_RISK_EXIT'],overlap='low',notes='Режим должен быть point-in-time, без отбора заранее известных дней просадки.')

# Hypothesis instances: varied trigger + confirm from relevant families, with concrete windows, thresholds and entry/exit rules.
# Preserve what kind of event and economic premise; numbers are proposed research grid, NOT tuned to data.
WINS=['2m','5m','15m','1h','4h']
QVALUES=[0.90,0.95,0.98,0.99]
CVALUES=[0.55,0.7,0.85]
VARIANTS=24

def instantiate(template,win,q,c,side):
    # standalone W/Q/C tokens ONLY; never damage CEX, BTC, VWAP, or other variable names.
    # qPast(..., Q) encodes a LOWER-tail extreme at q=(1-Q). pPast is the upper tail.
    template=re.sub(r'qPast\((.*?),Q\)',r'qPast(\1,LOW_Q)',template)
    template=re.sub(r'\bW/2\b',{'2m':'1m','5m':'2m','15m':'5m','1h':'30m','4h':'2h'}[win],template)
    for token,val in [('W',win),('LOW_Q',str(round(1-q,5))),('Q',str(q)),('C',str(c))]:
        template=re.sub(r'\b'+re.escape(token)+r'\b',val,template)
    return template

def risk_class(f):
    if 'HEDGE' in f['side'] or 'ALLOCATION' in f['side']: return 'portfolio_hedge_cap'
    if 'MAKER' in f['side']: return 'maker_inventory_cap'
    if 'PAIR' in f['side'] or f['side']=='HEDGED': return 'paired_two_leg_cap'
    if f['markets']=='HIP3': return 'hip3_mark_oracle_stop'
    return 'directional_hard_stop'

hypotheses=[]
blueprints=[]
for family_id,f in enumerate(families,1):
    # combinations are structured within each *economically meaningful family*
    for ti,trigger in enumerate(f['triggers']):
        for ci,confirm in enumerate(f['confirms']):
            blueprint_id=f'BP-{len(blueprints)+1:04d}'
            blueprint=dict(id=blueprint_id,family_id=family_id,family=f['name'],economic_mechanism=f['mechanism'],markets=f['markets'],side=f['side'],trigger_template=trigger,confirm_template=confirm,entry_types=f['entries'],exit_types=f['exits'],overlap_with_live=f['overlap'],data_sources=f['sources'],notes=f['notes'])
            blueprints.append(blueprint)
            # one frame / threshold set per instance. 24 combinations, not full Cartesian explosion.
            combos=list(itertools.product(WINS,QVALUES,CVALUES))
            # each blueprint deterministic 24 grid points with distinct parameter tuples
            rotate=(ti*11+ci*7+family_id*3) % len(combos)
            picked=[combos[(rotate+idx*7) % len(combos)] for idx in range(VARIANTS)]
            assert len(set(picked))==VARIANTS
            for vi,(win,q,c) in enumerate(picked):
                entry=f['entries'][(ci+vi)%len(f['entries'])]
                exit_=f['exits'][(ti+vi)%len(f['exits'])]
                # side BOTH is a hypothesis program with direction computed from trigger, not automatic LONG+SHORT optimization
                side=f['side']
                pred=instantiate(trigger,win,q,c,side)
                conf=instantiate(confirm,win,q,c,side)
                # prohibit listed post-fill diagnostics from being used as signal
                assert 'realized_exit' not in pred+conf
                h=dict(id=f'HYP-{len(hypotheses)+1:06d}',blueprint_id=blueprint_id,family=f['name'],market=f['markets'],side=side,
                   timeframe=win,quantile_param=q,confirmation_param=c,
                   condition_at_t=pred,confirmation_at_t=conf,logical_condition=f'({pred}) AND ({conf})',
                   action=entry,exit_model=exit_,risk_profile=risk_class(f),
                   direction_resolution=(
                       'LONG only' if side=='LONG' else 'SHORT only' if side=='SHORT' else
                       'LONG if signed return and signed flow > 0; SHORT if both < 0; else NO_SIGNAL' if side=='BOTH' else
                       'LONG if signed taker flow < 0; SHORT if signed taker flow > 0; else NO_SIGNAL' if side=='CONTRA_FLOW' else
                       'quote both sides with risk-capped inventory; no directional fill assumption' if side=='MAKER_BOTH' else
                       'LONG coin / SHORT beta hedge if residual < 0, reverse if residual > 0' if side=='PAIR_CONTRA_RESIDUAL' else
                       'two legs with executable hedge and capped basis exposure' if side=='HEDGED' else
                       'hedge existing portfolio beta or SHORT only under separate risk budget' if side=='SHORT_OR_HEDGE' else
                       'allocate risk budget only; do not manufacture entries' if side=='ALLOCATION_ONLY' else
                       'hedge or reduce risk, compare equal-capital no-hedge benchmark' if side=='HEDGE_OR_REDUCE' else
                       'pair side determined from direction of executable residual; otherwise NO_SIGNAL'),
                   economic_rationale=f['mechanism'],data_sources=f['sources'],status='HYPOTHESIS_UNTESTED',
                   overlap_with_live=f['overlap'],comparison='existing_portfolio, same_regime_placebo, simple_risk_off',
                   signal_time='end_of_closed_window_t',entry_earliest='first_executable_quote_after_t_plus_latency',
                   failure_condition='No incremental net-of-costs performance vs matching placebo and current portfolio',
                   missing_data_policy='NO_SIGNAL',notes=f['notes'])
                hypotheses.append(h)

# Explicit risk-policy and allocator choices are indexed separately; not pretend they automatically create alpha.
stop_families=[
('hard_pct', 'stop = entry_px ± p% (direction-aware); exchange mark trigger separate from trade fill', '0.10,0.15,0.20,0.25'),
('atr', 'stop = entry_px ± k * ATR_completed_1h; cap max stop distance', '2,3,4,5'),
('swing', 'stop beyond last CONFIRMED pivot ± buffer; fallback hard stop', '0.5,1,1.5,2'),
('vol', 'stop distance = k * past_sigma_1h * sqrt(hold_h); cap + floor', '2,2.5,3,3.5'),
('time', 'close at fixed time, with hard emergency exchange stop', '15m,30m,60m,4h'),
('spread', 'paired trade: stop on executable spread and individual leg liquidations', '1,1.5,2,3'),
('trail', 'trail after 1R favorable move; hard original stop stays', '1,1.5,2,3'),
('impact', 'exit when impact ceases or opposite flow confirmed; emergency hard stop', '1,2,3,4'),
]
exit_families=[
('take_fixed_R','limit take at 0.5R'),('take_1R','limit take at 1R'),('take_2R','limit take at 2R'),('time_only','time stop and emergency hard stop'),('partial','take 50% at 1R and trail rest'),('signal','exit on opposite confirmed signal + hard stop')]
size_modes=[0.25,0.5,1.0,1.5,2.0]
slots_modes=[1,2,3]
risk_models=[]
for stop_name,stop_desc,values in stop_families:
    for param in values.split(','):
        for size in size_modes:
            for exn,exdesc in exit_families:
                risk_models.append(dict(id=f'RISK-{len(risk_models)+1:05d}',family=stop_name,stop_param=param,
                    risk_per_trade_pct_equity=size,stop_rule=stop_desc,exit_family=exn,exit_rule=exdesc,
                    position_sizing='risk_budget_USD / estimated_worst_case_loss_fraction_including_fees_slippage',
                    rule='portfolio_concurrent_stop_risk_cap_must_hold',margins='check_per_dex_collateral_and_liquidation_buffer',
                    status='RESEARCH_ONLY_UNTESTED',warning='Stop price not guaranteed; partial fills and mark trigger require executable simulation.'))

portfolio_policies=[]
selection=['first_arrival','score_expected_incremental_R','lowest_factor_exposure','lowest_sector_concentration','highest_liquidity','risk_parity_by_strategy']
caps=[5,10,15,20]
for sel in selection:
    for cap in caps:
        portfolio_policies.append(dict(id=f'PORT-{len(portfolio_policies)+1:03d}',policy=sel,concurrent_stop_risk_cap_pct_equity=cap,
            max_gross_exposure_multiple=2,unique_coin_enforcement=True,margin='per_dex_separately',
            oracle_mark_stops='exchange_mark_trigger_with_worst_executable_slippage',
            selection_decision='only observed info and available margin at signal time',
            status='UNTESTED_PORTFOLIO_POLICY'))

# Sanity checks and human-readable summary.
assert len(indicator_specs)>3000, len(indicator_specs)
assert len(hypotheses)>5000, len(hypotheses)
assert len({i['id'] for i in indicator_specs})==len(indicator_specs)
assert len({h['id'] for h in hypotheses})==len(hypotheses)
assert len({(h['blueprint_id'],h['timeframe'],h['quantile_param'],h['confirmation_param']) for h in hypotheses})==len(hypotheses)

prefix='hyperliquid_research_universe_v1'
paths={key: ROOT/f'{prefix}_{key}.jsonl' for key in ['legacy','indicators','blueprints','hypotheses','risks','portfolios']}
for key,data in [('legacy',legacy),('indicators',indicator_specs),('blueprints',blueprints),('hypotheses',hypotheses),('risks',risk_models),('portfolios',portfolio_policies)]:
    with paths[key].open('w',encoding='utf-8') as fo:
        for row in data:
            fo.write(json.dumps(row,ensure_ascii=False,separators=(',',':'))+'\n')

ncounts=collections.Counter(i['family'] for i in indicator_specs)
nblue=collections.Counter(b['family'] for b in blueprints)
nsource=collections.Counter(i['data_tag'] for i in indicator_specs)

md=[]
a=md.append
A='2026-10-29'
a('# Hyperliquid: исследовательская вселенная индикаторов, гипотез, рисков и портфелей')
a('')
a(f'**Версия:** 1.0 · **Дата:** {A} · **Статус всех сгенерированных вариантов:** НЕ ПРОВЕРЕНЫ.')
a('')
a('## Что внутри и как честно считать объём')
a('')
a(f'- **{len(legacy):,}** элементов исходного каталога пользователя (дословные названия и статусы, не все являются вычислимыми индикаторами).'.replace(',',' '))
a(f'- **{len(seeds):,}** новых базовых конструкций метрик: у каждой задана примерная формула, смысл, источник данных и ограничения.'.replace(',',' '))
a(f'- **{len(indicator_specs):,}** конкретизированных индикаторных рецептов по окнам и преобразованиям; это *параметризованные экземпляры*, а не столько независимых информационных факторов.'.replace(',',' '))
a(f'- **{len(families)}** экономических семейства; **{len(blueprints):,}** структур входа из конкретных пар условий; **{len(hypotheses):,}** экземпляров гипотез с явными условиями, рынком, стороной, исполнением, выходом, риском и проверкой добавочной пользы.'.replace(',',' '))
a(f'- **{len(risk_models):,}** исследовательских вариантов стопа/размера/выхода; **{len(portfolio_policies)}** политик портфельного аллокатора.'.replace(',',' '))
a('')
a('**Важно:** Слово «гипотеза» означает проверяемый кандидат. Здесь нет результатов бэктеста, t-stat, реального PnL, гарантии доходности и доказательства пригодности для live. Каждую конфигурацию нельзя считать отдельным независимым тестом. Каталог — пространство исследований, а не рекомендация запускать всё. Для вариантов, требующих неизвестных данных, ставить `NO_TEST` вместо симуляции из несуществующего источника.')
a('')
a('### Зачем здесь несколько файлов')
a('')
a('Этот Markdown — читаемый сводный каталог со **всеми** экземплярами. JSONL — те же записи в машинном формате: их проще стримить агенту, валидировать, дедуплицировать, сортировать и конвертировать в DSL бэктестера. Один JSON-объект на строку. Читатель может использовать Markdown без JSONL.')
a('')
a('## Архитектура: рынок → метрики → причинно-корректная гипотеза → исполнение → риск → портфель')
a('')
a('````text')
a('Primary sources (timestamped, point-in-time)')
a('   -> normalized trades / completed bars / available BBO / verified L2 / contexts / direct CEX feeds')
a('   -> indicator graph with dependency + lookback + availability tracking')
a('   -> hypothesis AST: trigger + confirmation + context + direction')
a('   -> executable execution emulator: mark/oracle, queues, latency, partial fills, fees')
a('   -> risk model: hard stop + max loss + sizing + per-DEX margin')
a('   -> allocator: overlap, capital, factor beta, total stop risk, concurrent exposures')
a('   -> realistic portfolio replay -> embargoed validation -> forward')
a('````')
a('')
a('### Семантика DSL и защита от утечки')
a('')
a('- `t` — время принятия решения после завершения входных окон; `W` — уже закончившийся интервал (`t-W`, `t`].')
a('- `qPast`, `pPast`, `z_past`, `rank_past`, `beta_past` обучаются ТОЛЬКО по наблюдениям, которые закончились **до** `t`; не использовать quantiles всего тестового периода. `qPast(x, q)` означает историческую нижнюю квантиль q: для отрицательного экстремума q=1-Q. `pPast(x,Q)` — верхняя квантиль.')
a('- `C` — коэффициент подтверждения, `Q` — уровень исторической экстремальности; численные Q/C в каталоге **исследовательская сетка**, они не выбраны по доходности.')
a('- `NEXT_BAR` означает исполнение не раньше первой доступной котировки **после** сигнала, плюс фактическая задержка. Сигнал и fill одной свечи не допускаются.')
a('- `completed bar` важен особенно для `high`, `low`, VWAP, диапазонов и pivot. Промежуточная минутная свеча не знает собственную будущую максимальную цену.')
a('- `maker`: топ-бук или last trade не подтверждает позицию в очереди. Возможны частичные fills, ACK задержка и cancel/fill гонка.')
a('- `stops`: условные TP/SL могут триггериться по mark, а исполняются по реальному доступному потоку; исполнение по цене стопа не гарантировано.')
a('- `market scope`: торговые часы базовых активов HIP-3 и свежесть их котировок — самостоятельная проверка. Нельзя переносить last close акции на выходные как исполнимый рынок.')
a('- `NaN`: отсутствие полного L2, исторической маржи, позиции или события => `NO_TEST` и отдельный coverage-report; никогда не заменять факт гипотезой.')
a('- `REVERSAL`: если триггер определён как positive-shock и `CONTRA_FLOW`, это SHORT, если negative-shock — LONG; для двустороннего шаблона отдельно отследить точную ветку в компиляторе.')
a('- Формулы ниже — **DSL-псевдоформулы**, не готовые executable выражения Rust. Некоторые строковые условия требуют explicit AST lowering и проверок data lineage.')
a('')
a('### Доступность исходных данных (не преувеличивать)')
a('')
for key,(tag,caveat) in S.items():a(f'- `{tag}` — {caveat}')
a('')
a('**Прямые публичные источники:** Hyperliquid historical-data (node fills, S3 market data/asset ctx; архив может иметь пропуски и задержки), Binance `binance-public-data` (исторические klines/trades/aggTrades), публичные прямые WS других CEX по необходимости. Никаких Coinglass/Hyblock/платных аналитических агрегаторов. Исторические API не всегда предоставляют order-level очередь и полноту L2.')
a('')
a('**Официальные ссылки для аудитора источников:** https://hyperliquid.gitbook.io/hyperliquid-docs/historical-data ; https://hyperliquid.gitbook.io/Hyperliquid-docs/for-developers/api/info-endpoint ; https://github.com/binance/binance-public-data .')
a('')
a('## 1. Исходные 467 записей проекта (не выдавать за новые)')
a('')
a('| ID | Оригинальный статус | Название | Раздел исходника |')
a('|---|---|---|---|')
for item in legacy:
    a(f'| {item["id"]} | {item["source_status"]} | {item["name"].replace("|","/")} | {item["source_section"].replace("|","/")} |')
a('')
a('## 2. Новые индикаторные основы и конкретные экземпляры')
a('')
a(f'Сначала {len(seeds)} новые формулы. Ниже идут все {len(indicator_specs)} конкретных варианта (разные окна + преобразования).')
a('')
a('| Основа | Семейство | Показатель | Сырьё | Исследуемая связь |')
a('|---|---|---|---|---|')
for i,base in enumerate(seeds,1):
    a(f'| NEW-{i:03d} | {base["group"]} | {base["name"].replace("|","/")} | {base["source"]} | {base["mechanism"]} |')
a('')
a('### Весь список индикаторных экземпляров')
a('')
a('Формат: `ID | семейство | конкретная метрика | формула/псевдокод | источник` . При сложных формулах строка может быть длинной; для автоматического разбора предпочтителен JSONL.')
a('')
for group in ncounts:
    a(f'#### {group} ({ncounts[group]} экземпляров)')
    a('')
    a('| ID | Имя | Формула (каузально-прошлые данные) | Данные |')
    a('|---|---|---|---|')
    for obj in indicator_specs:
        if obj['family']!=group:continue
        a(f'| {obj["id"]} | {obj["name"].replace("|","/")} | `{obj["expression"].replace("|","/")}` | {obj["data_tag"]} |')
    a('')
a('## 3. Библиотека экономических механизмов')
a('')
a('| Семейство | Механизм | Рынки | Сторона | Ограничение/дубль |')
a('|---|---|---|---|---|')
for f in families: a(f'| {f["name"]} | {f["mechanism"]} | {f["markets"]} | {f["side"]} | {f["overlap"]} |')
a('')
a('## 4. Структурные шаблоны гипотез (без параметров)')
a('')
a('Это разные пары *условие + подтверждение* из ограниченной комбинации внутри одного экономического семейства. Не все связки экономически независимы.')
a('')
a('| Blueprint | Семейство | Первичный признак | Подтверждение | Данные |')
a('|---|---|---|---|---|')
for bp in blueprints:a(f'| {bp["id"]} | {bp["family"]} | `{bp["trigger_template"]}` | `{bp["confirm_template"]}` | {bp["data_sources"]} |')
a('')
a('## 5. Все конкретные гипотезы (для агента)')
a('')
a('Колонки: `HYP ID`, `blueprint`, `рынок/направление`, `входное окно`, `условие на t`, `подтверждение на t`, `исполнение`, `выход`, `риск-профиль`. Для деталей сопоставления с текущим портфелем и отказа смотреть JSONL. Ни одна строка не является подтверждённой альфой.')
a('')
for f in families:
    grp=[h for h in hypotheses if h['family']==f['name']]
    a(f'### {f["name"]} — {len(grp)} экземпляров')
    a(f'**Экономическая идея:** {f["mechanism"]}. **Данные:** `{f["sources"]}`. **Пересечение с live:** `{f["overlap"]}`.')
    if f['notes']:a(f'**Ограничение:** {f["notes"]}')
    a('')
    a('| ID | Шаблон | Рынок/сторона | W | Trigger | Confirm | Вход | Выход | Риск |')
    a('|---|---|---|---|---|---|---|---|---|')
    for h in grp:
        a(f'| {h["id"]} | {h["blueprint_id"]} | {h["market"]}/{h["side"]} | {h["timeframe"]} | `{h["condition_at_t"]}` | `{h["confirmation_at_t"]}` | {h["action"]} | {h["exit_model"]} | {h["risk_profile"]} |')
    a('')
a('## 6. Риск-менеджмент: независимый этаж')
a('')
a('**НЕЛЬЗЯ** оценивать риск-настройки так же, как сырые сигналы: они влияют на распределение потерь, маржу и capacity, а оптимизация их на той же истории дополнительно увеличивает множественное тестирование.')
a('')
a('Общие правила: размер = риск на сделку / **худший** ожидаемый loss_fraction с учётом fees/slippage. На каждом DEX отдельно проверить collateral, margin, net liquidation. Совокупный запланированный риск не является worst-case при гэпах и разрывах ликвидности.')
a('')
a('| Риск-режим | Концепция |')
a('|---|---|')
for row in stop_families:a(f'| {row[0]} | {row[1]} |')
a('')
a('### Все экземпляры риск-моделей')
a('')
a('| ID | Stop | Параметр | Риск % equity | Выход | Rule |')
a('|---|---|---:|---:|---|---|')
for r in risk_models:a(f'| {r["id"]} | {r["family"]} | {r["stop_param"]} | {r["risk_per_trade_pct_equity"]} | {r["exit_family"]} | {r["position_sizing"]} |')
a('')
a('## 7. Портфельные политики')
a('')
a('| ID | Приоритет сигналов | Совокупный плановый риск % equity | На монету | Ограничение |')
a('|---|---|---:|---|---|')
for p in portfolio_policies:a(f'| {p["id"]} | {p["policy"]} | {p["concurrent_stop_risk_cap_pct_equity"]} | одна позиция | маржа по каждому DEX отдельно |')
a('')
a('## 8. Рекомендуемый pipeline для агента')
a('')
a('1. Импортировать JSONL и валидировать целостность, источники, семантику каждого predicate и типы. Вывести отдельный `NO_TEST` для невозможных данных.')
a('2. Канонизировать AST, хешировать эквивалентные выражения, удалять дубли параметров, зависимые друг от друга индикаторы маркировать одним семейством.')
a('3. Проверять только уместные hypothesis families; не сканировать алгоритмически всё подряд на одной утёкшей истории. Версии V96 с lookahead исключить.')
a('4. Для каждого кандидата фиксировать входное решение point-in-time, учитывать информативность относительно существующих семейств DIP, RSI, Wick, HIP-3, short listings/pumps.')
a('5. Использовать корректную очередь/объём/latency на maker, executable bid/ask на taker, реальные параметры mark-trigger стопа и mar­gin; отбор low-capacity кандидатам не засчитывать прибыль от недоступных цен.')
a('6. Сравнить с ровно тем же капиталом, риском, периодом, комиссиями и allocator constraints, включая `risk-off` и placebo-трейды в том же режиме. Не использовать PnL отдельного сигнала как единственный критерий.')
a('7. Эмбарго пересекающихся событий, walk-forward, t по независимым дням/эпизодам, поправка на множественный поиск; уже виденная история не является чистым OOS.')
a('8. Новая победившая стратегия только в shadow/forward, live-параметры не менять. Первая цель — добавочный риск-скорректированный портфельный результат, а не количество комбинаций.')
a('')
a('## 9. Машинная схема пример')
a('')
example=hypotheses[0]
a('```json')
a(json.dumps(example,ensure_ascii=False,indent=2))
a('```')
a('')
a('## 10. QA качества большого генеративного каталога')
a('')
a(f'- Проверено: legacy ID уникальны, {len(legacy)} исходных элементов, {len(seeds)} новых основ, {len(indicator_specs)} отдельных индикаторных рецептов, {len(blueprints)} структур, {len(hypotheses)} конкретных гипотез.')
a('- Ни одна гипотеза не маркируется прибыльной. Производные от одной первичной гипотезы НЕ являются независимыми статистическими испытаниями.')
a('- Чтобы избежать бессмысленной универсальной сетки, подтверждения комбинируются с триггерами внутри явно указанных экономических семейств; конфликтные источники и недостаточные данные остаются в статусе NO_TEST.')
a('- Не все конструкции являются полноценным Rust кодом: AST lowering, формальное доказательство доступности данных, типизация денег/цен/долей, контроль причинности, IPO/листинга и timestamp synchronization — отдельный этап реализации.')
a('- Наблюдаемая часть рынка ≠ весь рынок: без исторической книги, внешних котировок или H3 oracle невозможно проверять некоторые гипотезы без притворства.')
a('')

md_path=ROOT/f'{prefix}.md'
md_path.write_text('\n'.join(md),encoding='utf-8')

readme=ROOT/f'{prefix}_README.txt'
readme.write_text(f'''Hyperliquid Research Universe v1 — 2026-10-29

Содержимое:
{len(legacy)} оригинальных элементов каталога
{len(seeds)} новых базовых метрик с DSL формулами
{len(indicator_specs)} индикаторных экземпляров
{len(families)} различных экономических семейств
{len(blueprints)} blueprint пар (триггер + подтверждение)
{len(hypotheses)} конкретных гипотез
{len(risk_models)} вариантов риска
{len(portfolio_policies)} вариантов портфеля

Главный файл: {md_path.name}
JSONL: {prefix}_legacy.jsonl, _indicators.jsonl, _blueprints.jsonl, _hypotheses.jsonl, _risks.jsonl, _portfolios.jsonl
Генератор: build_universe.py

Это НЕ протестированные стратегии. Разные окна/пороговые варианты одной формулы не независимые alpha.
Отсутствующие данные = NO_TEST. Действующий live не менять.
''',encoding='utf-8')

zip_path=ROOT/f'{prefix}_complete.zip'
with zipfile.ZipFile(zip_path,'w',compression=zipfile.ZIP_DEFLATED,compresslevel=7) as z:
    for file in [md_path,readme,*paths.values(),ROOT/'build_universe.py']:
        z.write(file,arcname=file.name)

print(json.dumps({'legacy':len(legacy),'new_base_metrics':len(seeds),'indicators':len(indicator_specs),'families':len(families),'blueprints':len(blueprints),'hypotheses':len(hypotheses),'risk_models':len(risk_models),'portfolio_policies':len(portfolio_policies),'md_bytes':md_path.stat().st_size,'zip_bytes':zip_path.stat().st_size,'per_family':dict(nblue)},ensure_ascii=False,indent=2))
