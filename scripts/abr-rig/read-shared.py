#!/usr/bin/env python3
"""What two sessions on one path did to each other, on one clock.

    read-shared.py <tag> [link_kbps] [--lone KBPS] [--bucket S]

Reads <tag>-1.jsonl / <tag>-2.jsonl and their probe logs out of this script's
out/, or $PF_RIG_OUT (the `-p1` spelling an older driver wrote is read too).

CLOCK ALIGNMENT.  Each probe's `t_ms` counts from its own session open, so the
two files do not share a zero.  The client log's `trajectory session open` line
carries that instant as wall-clock, so a window's absolute time is
`open_wall + t_ms`, and the run's t=0 is the earlier of the two opens.  Nothing
is inferred from the order of the files or from a join constant.

A PINNED session's controller never arms, so its windows carry target 0; the
open line's `start_kbps` is the rate the host acked it, and that is its series.
A probe killed before it wrote its file leaves only its log, which carries
`wire ingress` lines; a run with neither falls back to the host's per-minute
`link health` lines.

`lost` counts frames the client saw a shard of; a frame lost as one GSO train
shows only in `unrec`, which the host's whole-minute `link health` lines carry.
"""
import json, re, sys, os, statistics, datetime

ANSI = re.compile(r'\x1b\[[0-9;]*m')
TS = re.compile(r'^(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d+)Z')
OUT = os.environ.get('PF_RIG_OUT') or os.path.join(
    os.path.dirname(os.path.abspath(__file__)), 'out')
# The host says so when it divides a path or hands one back, and the client says
# so when it is told a share. Counting the three is how a run answers "was the
# governor live at all", which no trajectory column can.
HOST_SHARE = 'share of a path it is not alone on'
HOST_ALONE = 'alone on this path again'
CLIENT_SHARE = 'the host divided this path'


def epoch_ms(s):
    t = datetime.datetime.strptime(s[:26].ljust(26, '0'), '%Y-%m-%dT%H:%M:%S.%f')
    return t.replace(tzinfo=datetime.timezone.utc).timestamp() * 1000


def log_lines(path):
    if not path or not os.path.exists(path):
        return
    for ln in open(path, errors='replace'):
        ln = ANSI.sub('', ln)
        m = TS.match(ln)
        if m:
            yield epoch_ms(m.group(1)), ln


def first_existing(*paths):
    for p in paths:
        if os.path.exists(p):
            return p
    return None


def probe_files(tag, n):
    """(jsonl, log) for probe n, whichever spelling the driver used."""
    return (first_existing(f'{OUT}/{tag}-{n}.jsonl', f'{OUT}/{tag}-p{n}.jsonl'),
            first_existing(f'{OUT}/{tag}-{n}.log', f'{OUT}/{tag}-p{n}-client.log'))


def count_lines(path, needle):
    return sum(1 for _, ln in log_lines(path) if needle in ln)


def read_client(tag, n):
    """(open_ms, windows, kind).  windows: list of dicts with abs_ms."""
    jsonl, log = probe_files(tag, n)
    if log is None:
        return None, [], 'absent'      # that probe never ran
    open_ms, pin_kbps, ingress = None, 0, []
    for ms, ln in log_lines(log):
        if 'trajectory session open' in ln and open_ms is None:
            open_ms = ms
            d = dict(re.findall(r'(\w+)=(\S+)', ln))
            if d.get('pinned') == 'true':
                pin_kbps = int(d.get('start_kbps', 0))
        if 'wire ingress' in ln:
            d = dict(re.findall(r'(\w+)=(-?\d+)', ln))
            ingress.append({'abs_ms': ms, 'target_kbps': int(d.get('video_kbps', 0)),
                            'delivered_kbps': int(d.get('video_kbps', 0)),
                            'lost_frames': int(d.get('frames_dropped', 0)),
                            'request_kbps': None, 'delay_mean_us': None, 'discarded': False})
    if jsonl and open_ms is not None:
        ws = []
        for ln in open(jsonl):
            try:
                o = json.loads(ln)
            except Exception:
                continue
            if 'ramp' in o or 'ramp_step' in o or 'windows' in o:
                continue
            if o.get('discarded'):
                continue
            o['abs_ms'] = open_ms + o['t_ms']
            # A disarmed controller has no target to write down; the rate this
            # session ran at is the pin the host acked at open.
            if pin_kbps:
                o['target_kbps'] = pin_kbps
            ws.append(o)
        return open_ms, ws, 'pinned' if pin_kbps else 'automatic'
    if ingress:
        return (ingress[0]['abs_ms'], ingress, 'ingress')
    # No file at all: the host logs `link health` per session per minute. The ids
    # are distinct but name no probe, so the pinned session is recognised the way
    # it always could be — its abr_kbps band does not move, minute after minute.
    # That band IS the invariant this case tests.
    host = f'{OUT}/{tag}-host.log'
    rows = []
    for ms, ln in log_lines(host):
        if 'link health' not in ln:
            continue
        eg = re.search(r'egress_mbps=([\d.]+)', ln)
        abr = re.search(r'abr_kbps=(\d+)\.\.(\d+)', ln)
        rt = re.search(r'retargets=(\d+)', ln)
        unrec = re.search(r'unrecovered=(\d+)', ln)
        secs = re.search(r'secs=(\d+)', ln)
        wins = re.search(r'windows=(\d+)', ln)
        if not (eg and abr and secs and wins):
            continue
        if int(secs.group(1)) == 0 or int(wins.group(1)) == 0:
            continue          # a start/teardown line, not a minute of stream
        rows.append(
            {'abs_ms': ms, 'target_kbps': int(abr.group(2)),
             'delivered_kbps': int(float(eg.group(1)) * 1000),
             'lost_frames': int(unrec.group(1)) if unrec else 0,
             'request_kbps': None, 'delay_mean_us': None, 'discarded': False,
             'retargets': int(rt.group(1)) if rt else 0,
             'abr_lo': int(abr.group(1)), 'abr_hi': int(abr.group(2))})
    # An Automatic session can also sit still for one whole minute, so take the
    # value that is flat in most minutes, not the only one.
    flat = [w for w in rows if w['abr_lo'] == w['abr_hi']]
    counts = {}
    for w in flat:
        counts[w['abr_lo']] = counts.get(w['abr_lo'], 0) + 1
    if counts and max(counts.values()) >= 3:
        val = max(counts, key=lambda k: counts[k])
        v = [w for w in flat if w['abr_lo'] == val]
        # Back-date to the first sample minus one minute: link health covers the
        # minute behind it.
        return v[0]['abs_ms'] - 60_000, v, f'pinned(link {val})'
    return None, [], 'absent'


# The host's field, and the column it lands in.
HOST_DEAD = (('unrecovered', 'unrec_10min'), ('rfi', 'rfi_10min'), ('idr', 'idr_10min'),
             ('keyframe_req', 'kf_req_10min'))


def host_dead(tag):
    """The host's dead-frame counters per session, in session-id order, which is open order."""
    by_sid = {}
    for _, ln in log_lines(f'{OUT}/{tag}-host.log'):
        if 'link health' not in ln:
            continue
        sid = re.search(r'session=(\d+)', ln)
        secs = re.search(r'secs=(\d+)', ln)
        if not (sid and secs) or int(secs.group(1)) != 60:
            continue
        row = by_sid.setdefault(int(sid.group(1)), {k: 0 for k, _ in HOST_DEAD})
        for k, _ in HOST_DEAD:
            m = re.search(rf'\b{k}=(\d+)', ln)
            row[k] += int(m.group(1)) if m else 0
    return [by_sid[k] for k in sorted(by_sid)]


def per_client(ws, kind, divided, host):
    if not ws:
        return None
    rates = [w['target_kbps'] for w in ws]
    delivered = [w['delivered_kbps'] for w in ws]
    delays = [w['delay_mean_us'] for w in ws if w.get('delay_mean_us') is not None]
    cuts, casc, last = 0, 0, None
    for w in ws:
        r = w.get('request_kbps')
        if r is not None and r < w['target_kbps']:
            cuts += 1
            if last is not None and w['abs_ms'] - last < 3000:
                casc += 1
            last = w['abs_ms']
    span_s = (ws[-1]['abs_ms'] - ws[0]['abs_ms']) / 1000 or 1
    return {
        'kind': kind, 'n': len(ws),
        'mean': sum(rates) / len(rates),
        'got': sum(delivered) / len(delivered),
        'under5_pct': 100.0 * sum(1 for r in rates if r < 5000) / len(rates),
        'cuts': cuts, 'cascades': casc, 'divided': divided,
        'lost': sum(w['lost_frames'] for w in ws),
        'lost_10min': sum(w['lost_frames'] for w in ws) * 600 / span_s,
        'delay_p95_us': (sorted(delays)[int(len(delays) * 0.95)] if delays else None),
        'span_s': span_s,
        **{col: (host[k] * 600 / span_s if host else None) for k, col in HOST_DEAD},
    }


def grid(ws, t0, span_s):
    """Rate per whole second, held from the last window (windows are 750 ms)."""
    g = [None] * (span_s + 1)
    if not ws:
        return g
    for w in ws:
        s = int((w['abs_ms'] - t0) / 1000)
        if 0 <= s <= span_s:
            g[s] = w['target_kbps']
    # Hold the last rate forward — windows are 750 ms — but only while the
    # client is live. Past its last window the grid is empty, which is how a
    # leaver's departure is seen at all.
    end = int((ws[-1]['abs_ms'] - t0) / 1000) + 1
    last = None
    for i, v in enumerate(g):
        if i > end:
            g[i] = None
        elif v is None:
            g[i] = last
        else:
            last = v
    return g


def jain(vals):
    vals = [v for v in vals if v is not None]
    if not vals or sum(vals) == 0:
        return None
    return (sum(vals) ** 2) / (len(vals) * sum(v * v for v in vals))


def main():
    a = sys.argv[1:]
    if not a:
        sys.exit(__doc__)
    tag = a[0]
    link = int(a[1]) if len(a) > 1 and a[1].isdigit() else 0
    lone = int(a[a.index('--lone') + 1]) if '--lone' in a else 0
    bucket = int(a[a.index('--bucket') + 1]) if '--bucket' in a else 30

    o1, w1, k1 = read_client(tag, 1)
    o2, w2, k2 = read_client(tag, 2)
    if o1 is None:
        sys.exit(f'{tag}: no client 1')
    t0 = min(x for x in (o1, o2) if x is not None)
    span_s = int(max((w[-1]['abs_ms'] for w in (w1, w2) if w), default=t0) - t0) // 1000

    print(f'== {tag} ==  link={link or "?"} kbps  span={span_s}s')
    def at(o):
        return f'{(o - t0) / 1000:+.2f}s' if o is not None else 'absent'
    print(f'   clock: p1 opened at {at(o1)}, p2 opened at {at(o2)} '
          f'(t=0 is the earlier open); p1={k1} p2={k2}')
    host = f'{OUT}/{tag}-host.log'
    print(f'   governor: host divided the path {count_lines(host, HOST_SHARE)}x, '
          f'handed it back {count_lines(host, HOST_ALONE)}x '
          f'(0 and 0 = the governor never ran)')

    dead = host_dead(tag)
    opened = sorted((o, n) for n, o in ((1, o1), (2, o2)) if o is not None)
    hd = {n: (dead[i] if i < len(dead) else None) for i, (_, n) in enumerate(opened)}
    s1 = per_client(w1, k1, count_lines(probe_files(tag, 1)[1], CLIENT_SHARE), hd.get(1))
    s2 = per_client(w2, k2, count_lines(probe_files(tag, 2)[1], CLIENT_SHARE), hd.get(2))
    print(f'   {"client":<8}{"kind":<18}{"win":>5}{"rate":>8}{"got":>8}{"under5%":>9}'
          f'{"cuts":>6}{"casc":>6}{"shares":>8}{"lost/10m":>10}{"delay_p95":>11}'
          + ''.join(f'{col:>{len(col) + 2}}' for _, col in HOST_DEAD))
    for name, s in (('p1', s1), ('p2', s2)):
        if not s:
            print(f'   {name:<8}absent'); continue
        d = f'{s["delay_p95_us"]/1000:.1f} ms' if s['delay_p95_us'] else '-'
        print(f'   {name:<8}{s["kind"]:<18}{s["n"]:>5}{s["mean"]:>8.0f}{s["got"]:>8.0f}'
              f'{s["under5_pct"]:>9.1f}{s["cuts"]:>6}{s["cascades"]:>6}{s["divided"]:>8}'
              f'{s["lost_10min"]:>10.1f}{d:>11}'
              + ''.join(f'{(f"{s[c]:.1f}" if s[c] is not None else "-"):>{len(c) + 2}}'
                        for _, c in HOST_DEAD))

    g1 = grid(w1, t0, span_s)
    g2 = grid(w2, t0, span_s)

    print(f'   {"t..t+%ds" % bucket:<12}{"p1":>8}{"p2":>8}{"sum":>8}{"%link":>7}{"jain":>7}')
    for b in range(0, span_s + 1, bucket):
        sl1 = [v for v in g1[b:b + bucket] if v is not None]
        sl2 = [v for v in g2[b:b + bucket] if v is not None]
        m1 = sum(sl1) / len(sl1) if sl1 else None
        m2 = sum(sl2) / len(sl2) if sl2 else None
        both = [x for x in (m1, m2) if x is not None]
        j = jain(both) if len(both) == 2 else None
        tot = sum(both)
        print(f'   {b:<12}{(f"{m1:.0f}" if m1 else "-"):>8}{(f"{m2:.0f}" if m2 else "-"):>8}'
              f'{tot:>8.0f}{(100*tot/link if link else 0):>7.0f}'
              f'{(f"{1000*j:.0f}" if j else "-"):>7}')

    # pair figures over every second where both are live
    pairs = [(x, y) for x, y in zip(g1, g2) if x is not None and y is not None]
    if pairs:
        js = [jain([x, y]) for x, y in pairs]
        print(f'   pair: both live {len(pairs)}s | sum_mean={statistics.mean(x+y for x,y in pairs):.0f} kbps'
              f'{f" ({100*statistics.mean(x+y for x,y in pairs)/link:.0f}% of link)" if link else ""}'
              f' | jain_x1000 over the run = {1000*statistics.mean(js):.0f}'
              f' (min over a {bucket}s bucket = {1000*min(js):.0f})')

    # the pinned session: what it ran at, and whether anything moved it
    if s2 and k2.startswith('pinned'):
        band = f'{min(w["target_kbps"] for w in w2)}..{max(w["target_kbps"] for w in w2)}'
        moved = sum(w.get('retargets', 0) for w in w2) + s2['divided']
        print(f'   p2 pinned: rate band {band}, retargets+shares={moved} '
              f'-> {"UNTOUCHED" if moved == 0 else "TOUCHED"}')

    # convergence after the later join
    join_s = int((max(x for x in (o1, o2) if x is not None) - t0) / 1000) if o2 else None
    if join_s is not None and pairs:
        hold, first = 0, None
        for s in range(join_s, span_s + 1):
            x, y = g1[s], g2[s]
            if x is None or y is None or max(x, y) == 0:
                hold, first = 0, None; continue
            if min(x, y) / max(x, y) >= 0.80:
                if first is None:
                    first = s
                hold += 1
                if hold >= 10:
                    break
            else:
                hold, first = 0, None
        print(f'   converge: second session live at t={join_s}s; within 20 % of each other '
              + (f'from t={first}s (+{first-join_s}s), held 10 s' if first is not None and hold >= 10
                 else 'NEVER (no 10 s stretch inside 20 %)'))

    # the leaver
    live2 = [s for s, v in enumerate(g2) if v is not None]
    if live2 and live2[-1] < span_s - 20:
        t_leave = live2[-1] + 1
        ref = lone or (0.7 * link if link else 0)
        bar = 0.8 * ref
        hold, first = 0, None
        for s in range(t_leave, span_s + 1):
            if g1[s] is not None and g1[s] >= bar:
                if first is None:
                    first = s
                hold += 1
                if hold >= 10:
                    break
            else:
                hold, first = 0, None
        cuts_after = sum(1 for w in w1 if w.get('request_kbps') is not None
                         and w['request_kbps'] < w['target_kbps']
                         and t_leave * 1000 <= w['abs_ms'] - t0 <= (first or span_s) * 1000)
        print(f'   leaver: p2 left at t={t_leave}s; lone reference {ref:.0f} kbps, bar {bar:.0f}; '
              + (f'survivor reached it at t={first}s (+{first-t_leave}s), '
                 if first is not None and hold >= 10 else 'survivor NEVER reached it, ')
              + f'cuts by the survivor in that interval = {cuts_after}')


main()
