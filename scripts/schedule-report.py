#!/usr/bin/env python3
"""Отчёт для SCHEDULE_LOG.md: что было у ботов перед банами и сколько приносит каждый час суток.

  python3 scripts/schedule-report.py            # баны + кривая за последние 3 суток
  python3 scripts/schedule-report.py --days 7   # кривая за 7 суток

Читает и дописывает: bots/ban-history.json (из bots/banned-memory.json); читает: bot-slots.jsonl (проба ботов раз в ~30 с), ../4narek-new/ml_data/pricing.db.
Ничего не пишет: результат копируется в SCHEDULE_LOG.md руками (раздел «Баны» и «Кривая»).
Время везде МСК. Онлайн считается по интервалам между пробами (разрыв >2 мин не засчитывается).
"""
import argparse, collections, datetime as dt, json, re, sqlite3
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MSK = dt.timezone(dt.timedelta(hours=3))


def msk(ts):
    return dt.datetime.fromtimestamp(ts, MSK)


def load_slots():
    by = collections.defaultdict(list)
    for line in open(ROOT / "bot-slots.jsonl"):
        x = json.loads(line)
        by[x["bot"]].append((x["ts"], bool(x["live"]), x.get("anarchy")))
    for v in by.values():
        v.sort()
    return by


def online_hours(samples, a, b):
    t = 0.0
    for (p, live, _), (q, _, _) in zip(samples, samples[1:]):
        if live and a <= p <= b:
            t += min(q - p, 120)
    return t / 3600


def stretches(samples, end):
    """Серии непрерывного онлайна (разрыв >10 мин = перерыв), закончившиеся до `end`."""
    out, st, last = [], None, None
    for ts, live, _ in samples:
        if ts > end:
            break
        if live:
            st = st if st is not None else ts
            last = ts
        elif st is not None and ts - last > 600:
            out.append((st, last)); st = None
    if st is not None:
        out.append((st, last))
    return out


def ban_ts(entry):
    m = re.search(r"Бан выдан: (\d\d)\.(\d\d)\.(\d{4}) (\d\d):(\d\d):(\d\d)", entry.get("reason", ""))
    if m:
        d, mo, y, h, mi, s = map(int, m.groups())
        return dt.datetime(y, mo, d, h, mi, s, tzinfo=MSK).timestamp()
    return float(entry.get("banned_at", 0))


def bans(slots):
    """Оркестратор чистит banned-memory.json (убирает ники, которых нет в конфиге), поэтому историю
    держим отдельно в bots/ban-history.json: новые баны из banned-memory.json дописываем сюда."""
    hist_path = ROOT / "bots" / "ban-history.json"
    mem = json.load(open(hist_path)) if hist_path.exists() else {}
    try:
        cur = json.load(open(ROOT / "bots" / "banned-memory.json"))
    except Exception:
        cur = {}
    new = [n for n in cur if n not in mem]
    for n in new:
        mem[n] = {**cur[n], "schedule": "?  (допиши версию расписания)"}
    if new:
        json.dump(mem, open(hist_path, "w"), ensure_ascii=False, indent=2)
        print(f"(в ban-history.json добавлено: {', '.join(new)})\n")
    print("## Баны (bots/ban-history.json)\n")
    print("| Бот | Анархия | Версия расписания | Бан (МСК) | Онлайн за 24 ч до бана | Последняя серия | Серия до бана, ч |")
    print("|---|---|---|---|---|---|---|")
    rows = sorted(mem.items(), key=lambda kv: ban_ts(kv[1]))
    for nick, e in rows:
        ts = ban_ts(e)
        s = slots.get(nick, [])
        on24 = online_hours(s, ts - 86400, ts)
        st = stretches(s, ts)
        last = st[-1] if st else None
        tail = f"{msk(last[0]):%d.%m %H:%M}→{msk(last[1]):%H:%M}" if last else "—"
        ln = f"{(last[1] - last[0]) / 3600:.1f}" if last else "—"
        print(f"| {nick} | {e.get('anarchy')} | {e.get('schedule', '?')} | {msk(ts):%d.%m %H:%M:%S} | {on24:.1f} ч | {tail} | {ln} |")
    print()


def curve(slots, days):
    db = sqlite3.connect(f"file:{ROOT.parent / '4narek-new/ml_data/pricing.db'}?mode=ro", uri=True)
    since = (dt.datetime.now(dt.timezone.utc) - dt.timedelta(days=days)).strftime("%Y-%m-%dT%H:%M:%SZ")
    prof = collections.defaultdict(float)
    for ts, p in db.execute("select ts,profit_now from capital_cycles where ts>=? and profit_now is not null", (since,)):
        t = dt.datetime.fromisoformat(ts.replace("Z", "+00:00")).astimezone(MSK)
        prof[t.hour] += p
    a = dt.datetime.now(dt.timezone.utc).timestamp() - days * 86400
    on = collections.defaultdict(float)
    for s in slots.values():
        for (p, live, _), (q, _, _) in zip(s, s[1:]):
            if live and p >= a:
                on[msk(p).hour] += min(q - p, 120) / 3600
    print(f"## Кривая дохода по часам МСК за {days} сут.\n")
    print("| Час | кк/сутки (все боты) | бот-часов онлайн/сутки | кк на бота в час |")
    print("|---|---|---|---|")
    for h in range(24):
        bh = on[h] / days
        k = prof[h] / 1e6 / days
        print(f"| {h:02d} | {k:.1f} | {bh:.1f} | {(k / bh if bh > 0.3 else 0):.1f} |")
    print(f"\nИтого {sum(prof.values()) / 1e6 / days:.0f} кк/сутки. Профит приписан часу цикла цены, а не продажи; ночью с малым онлайном оценка шумная.")


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--days", type=int, default=3)
    a = ap.parse_args()
    sl = load_slots()
    bans(sl)
    curve(sl, a.days)
