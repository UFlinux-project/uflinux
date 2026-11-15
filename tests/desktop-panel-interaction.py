#!/usr/bin/env python3
"""AT-SPI regression for the taskbar clock popover and quick-launch buttons.

Runs inside desktop-prototype-smoke.sh (UFLINUX_TEST_PANEL_INTERACTION=1).
Every step checks that the desktop process is alive and still answers
--status, so a crash in the popover lifecycle fails right at the step
that caused it instead of at the end of the smoke run.
"""
import json
import os
import subprocess
import sys
import time

import pyatspi


def pump():
    from gi.repository import GLib
    while GLib.MainContext.default().pending():
        GLib.MainContext.default().iteration(False)


def nodes():
    queue = [a for a in pyatspi.Registry.getDesktop(0) if a.name == 'uflinux-desktop']
    while queue:
        node = queue.pop(0)
        yield node
        try:
            queue.extend(child for child in node if child)
        except Exception:
            pass


def find(match, roles=('button', 'push button', 'menu item', 'toggle button')):
    pump()
    fallback = None
    for node in nodes():
        if not match(node) or node.getRoleName() not in roles:
            continue
        try:
            if not node.getState().contains(pyatspi.STATE_SHOWING):
                continue
            if node.queryAction().nActions > 0:
                return node
            if fallback is None:
                fallback = node
        except Exception:
            continue
    return fallback


def click(node):
    node.queryAction().doAction(0)


def status():
    binary = os.environ['UFLINUX_DESKTOP_BIN']
    out = subprocess.run([binary, '--status'], capture_output=True, text=True, timeout=10)
    return json.loads(out.stdout)


def alive():
    pid = int(os.environ['UFLINUX_DESKTOP_PID'])
    try:
        os.kill(pid, 0)
        return True
    except OSError:
        return False


def dump_tree():
    for node in nodes():
        try:
            print('  %-14s %r showing=%s' % (
                node.getRoleName(), node.name,
                node.getState().contains(pyatspi.STATE_SHOWING)), flush=True)
        except Exception:
            pass


def wait_node(match, timeout=10):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        node = find(match)
        if node is not None:
            return node
        if not alive():
            return None
        time.sleep(.1)
    return None


def step(label):
    print(f'--- {label}: alive={alive()} status={status()}', flush=True)


def main():
    failures = []

    def is_clock(node):
        first = node.name.split('\n')[0]
        return len(first) == 5 and first[2] == ':' and first[:2].isdigit() and first[3:].isdigit()

    clock = wait_node(is_clock)
    if clock is None:
        print('FAIL: clock button not found; tree:')
        dump_tree()
        return 1
    step('clock button found')
    click(clock)
    time.sleep(.7)
    step('clock popover opened')
    clear = find(lambda n: n.name == 'Clear all')
    if clear is not None:
        click(clear)
        time.sleep(.4)
        step('clear-all clicked')
    clock = wait_node(is_clock)
    if clock is None:
        failures.append('clock button vanished after first click')
        return finish(failures)
    click(clock)
    time.sleep(.5)
    step('clock popover closed')
    click(clock)
    time.sleep(.5)
    step('clock popover reopened')
    click(clock)
    time.sleep(.3)
    if not alive():
        failures.append('clock click killed the desktop')

    foot = wait_node(lambda n: n.name.lower() == 'foot')
    if foot is None:
        print('FAIL: foot favorite not found; tree:')
        dump_tree()
    else:
        step('favorite found')
        click(foot)  # launch
        time.sleep(1.5)
        step('favorite launched')
        foot = wait_node(lambda n: n.name.lower() == 'foot')
        if foot is None:
            failures.append('favorite button vanished after launch')
            return finish(failures)
        click(foot)  # activate
        time.sleep(.7)
        step('favorite activated')
        click(foot)  # minimize
        time.sleep(.7)
        step('favorite minimized')
        foot = wait_node(lambda n: n.name.lower() == 'foot')
        if foot is None:
            failures.append('favorite button vanished after minimize')
            return finish(failures)
        click(foot)  # activate again
        time.sleep(.7)
        step('favorite reactivated')
        if not alive():
            failures.append('favorite interaction killed the desktop')

    return finish(failures)


def finish(failures):
    if failures:
        print('FAILURES:', failures)
        return 1
    print('panel interaction survived')
    return 0


if __name__ == '__main__':
    sys.exit(main())
