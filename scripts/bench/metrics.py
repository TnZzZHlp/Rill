import asyncio
import csv
import os
import time
from pathlib import Path

import container


class Sampler:
    """Per-second resource sampling for containerised targets.

    ``roots`` maps a group name to either ``('container', <name>)`` or
    ``('pid', <pid>)``. Process trees are resolved through the host-visible
    container PID, so container runtime helpers are never part of the tree.
    ``cgroups`` maps a group name to a cgroup v2 directory for an independent
    accounting series.
    """

    def __init__(self, path, roots, cgroups=None):
        self.file = path.open('x', newline='')
        self.csv = csv.writer(self.file)
        self.csv.writerow(['monotonic', 'group', 'cpu_percent', 'rss_bytes', 'hwm_bytes', 'threads', 'fds', 'processes'])
        self.roots = roots
        self.cgroup_file = None
        self.cgroup_csv = None
        if cgroups:
            self.cgroup_file = path.with_name(path.name.replace('-resources.csv', '-cgroup.csv')).open('x', newline='')
            self.cgroup_csv = csv.writer(self.cgroup_file)
            self.cgroup_csv.writerow(['monotonic', 'group', 'cpu_percent', 'memory_current', 'memory_peak', 'nr_throttled', 'throttled_usec', 'pids_current'])
        self.cgroups = cgroups or {}
        self.previous = {}
        self.cgroup_previous = {}
        self.peaks = {}
        self.ticks = os.sysconf('SC_CLK_TCK')
        self.last = None
        self.dead = {name: {} for name in roots}
        self.resolved = {}

    def root_pid(self, name, processes=None):
        """Resolve the host PID for a group, re-resolving when a cached container
        PID disappears so a restarted container is not sampled as zero forever."""
        kind, value = self.roots[name]
        if kind == 'pid':
            self.resolved[name] = value
            return value
        cached = self.resolved.get(name)
        if cached is not None and (processes is None or cached in processes):
            return cached
        try:
            pid = container.host_pid(value)
        except container.ContainerError:
            pid = None
        self.resolved[name] = pid
        return pid

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

    @staticmethod
    def cgroup_values(path):
        """Read required counters. A missing or malformed file is a hard error:
        treating it as zero would hide throttling and memory limits."""
        def read(name):
            try:
                return (path / name).read_text()
            except OSError as exc:
                raise RuntimeError(f'cgroup metric {path.name}/{name} unavailable') from exc

        fields = {}
        for line in read('cpu.stat').splitlines():
            parts = line.split()
            if len(parts) == 2 and parts[1].lstrip('-').isdigit():
                fields[parts[0]] = int(parts[1])
        for required in ('usage_usec', 'nr_throttled', 'throttled_usec'):
            if required not in fields:
                raise RuntimeError(f'cgroup metric {path.name}/cpu.stat missing {required}')
        values = {key: fields[key] for key in ('usage_usec', 'nr_throttled', 'throttled_usec')}
        for name, key in (('memory.current', 'memory_current'), ('memory.peak', 'memory_peak'), ('pids.current', 'pids_current')):
            text = read(name).strip()
            if not text.isdigit():
                raise RuntimeError(f'cgroup metric {path.name}/{name} malformed')
            values[key] = int(text)
        return values

    def sample(self):
        now = time.monotonic()
        processes = self.snapshot()
        for name in self.roots:
            pid = self.root_pid(name, processes)
            members = {pid} if pid else set()
            if members:
                while True:
                    more = {p for p, item in processes.items() if item[0] in members} - members
                    if not more:
                        break
                    members.update(more)
            live = [processes[p] for p in members if p in processes]
            for p in live:
                self.dead[name][p[1]] = p[2]
            total = sum(self.dead[name].values())
            prev = self.previous.get(name, total)
            cpu = 0 if self.last is None else max(0, (total - prev) / self.ticks / (now - self.last) * 100)
            self.previous[name] = total
            rss = sum(p[3] for p in live)
            hwm = sum(p[4] for p in live)
            self.csv.writerow([now, name, cpu, rss, hwm, sum(p[5] for p in live), sum(p[6] for p in live), len(live)])
            peak = self.peaks.setdefault(name, {'cpu_percent': 0, 'rss_bytes': 0, 'hwm_sum_bytes': 0,
                                                'pss_bytes': None, 'missing_root_samples': 0})
            if not live:
                peak['missing_root_samples'] += 1
            peak['cpu_percent'] = max(peak['cpu_percent'], cpu)
            peak['rss_bytes'] = max(peak['rss_bytes'], rss)
            peak['hwm_sum_bytes'] = max(peak['hwm_sum_bytes'], hwm)
        for name, path in self.cgroups.items():
            if path is None:
                raise RuntimeError(f'no cgroup path resolved for {name}')
            values = self.cgroup_values(path)
            usage = values['usage_usec']
            previous = self.cgroup_previous.get(name)
            cpu = 0 if self.last is None or previous is None else max(0, (usage - previous) / (now - self.last) / 10000)
            self.cgroup_previous[name] = usage
            self.cgroup_csv.writerow([now, name, cpu, values['memory_current'], values['memory_peak'],
                                      values['nr_throttled'], values['throttled_usec'], values['pids_current']])
            peak = self.peaks.setdefault(name + '_cgroup', {'cpu_percent': 0, 'memory_peak_bytes': 0,
                                                            'nr_throttled': 0, 'throttled_usec': 0})
            peak['cpu_percent'] = max(peak['cpu_percent'], cpu)
            peak['memory_peak_bytes'] = max(peak['memory_peak_bytes'], values['memory_peak'])
            peak['nr_throttled'] = max(peak['nr_throttled'], values['nr_throttled'])
            peak['throttled_usec'] = max(peak['throttled_usec'], values['throttled_usec'])
        self.file.flush()
        if self.cgroup_file is not None:
            self.cgroup_file.flush()
        self.last = now

    async def run(self):
        while True:
            self.sample()
            await asyncio.sleep(1)

    def close(self):
        self.file.close()
        if self.cgroup_file is not None:
            self.cgroup_file.close()
