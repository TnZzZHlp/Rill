import asyncio
import csv
import os
import time
from pathlib import Path


class Sampler:
    def __init__(self, path, roots):
        self.file = path.open('x', newline='')
        self.csv = csv.writer(self.file)
        self.csv.writerow(['monotonic', 'group', 'cpu_percent', 'rss_bytes', 'hwm_bytes', 'threads', 'fds', 'processes'])
        self.roots = roots
        self.previous = {}
        self.peaks = {}
        self.ticks = os.sysconf('SC_CLK_TCK')
        self.last = None
        self.dead = {name: {} for name in roots}

    def snapshot(self):
        result = {}
        for path in Path('/proc').iterdir():
            if not path.name.isdigit():
                continue
            try:
                raw = (path / 'stat').read_text()
                parts = raw[raw.rfind(')') + 2:].split()
                pid = int(path.name)
                ppid = int(parts[1])
                ticks = int(parts[11]) + int(parts[12])
                start = int(parts[19])
                status = (path / 'status').read_text().splitlines()
                fields = {line.split(':', 1)[0]: line.split(':', 1)[1].strip() for line in status if ':' in line}
                rss = int(fields.get('VmRSS', '0').split()[0]) * 1024
                hwm = int(fields.get('VmHWM', '0').split()[0]) * 1024
                threads = int(fields.get('Threads', '0'))
                fds = len(list((path / 'fd').iterdir()))
                result[pid] = (ppid, (pid, start), ticks, rss, hwm, threads, fds)
            except (OSError, ValueError, IndexError):
                continue
        return result

    def sample(self):
        now = time.monotonic()
        processes = self.snapshot()
        for name, root in self.roots.items():
            members = {root}
            if name == 'server':
                while True:
                    more = {pid for pid, p in processes.items() if p[0] in members} - members
                    if not more:
                        break
                    members.update(more)
            live = [processes[pid] for pid in members if pid in processes]
            for p in live:
                self.dead[name][p[1]] = p[2]
            total = sum(self.dead[name].values())
            prev = self.previous.get(name, total)
            cpu = 0 if self.last is None else max(0, (total - prev) / self.ticks / (now - self.last) * 100)
            self.previous[name] = total
            rss = sum(p[3] for p in live)
            hwm = sum(p[4] for p in live)
            self.csv.writerow([now, name, cpu, rss, hwm, sum(p[5] for p in live), sum(p[6] for p in live), len(live)])
            peak = self.peaks.setdefault(name, {'cpu_percent': 0, 'rss_bytes': 0, 'hwm_sum_bytes': 0, 'pss_bytes': None})
            peak['cpu_percent'] = max(peak['cpu_percent'], cpu)
            peak['rss_bytes'] = max(peak['rss_bytes'], rss)
            peak['hwm_sum_bytes'] = max(peak['hwm_sum_bytes'], hwm)
        self.file.flush()
        self.last = now

    async def run(self):
        while True:
            self.sample()
            await asyncio.sleep(1)

    def close(self):
        self.file.close()
