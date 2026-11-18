#!/usr/bin/env python3
"""Exercise the real Files services from the resident shell in a private session."""
import json
import os
from pathlib import Path
import select
import subprocess
from statistics import median
import time

import pyatspi
from gi.repository import Gio, GLib


root = Path(os.environ['HOME'])
runtime = Path(os.environ['XDG_RUNTIME_DIR'])
apps = Path(os.environ['XDG_DATA_HOME']) / 'applications'
desktop = root / 'Desktop'
bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
provider_name = 'org.uflinux.FileManager.Desktop1'
provider_path = '/org/uflinux/FileManager/Desktop'


def command(*args):
    return subprocess.check_output(args, text=True, timeout=8)


def shell(*args):
    return command(os.environ['UFLINUX_DESKTOP_BIN'], *args)


def find(name):
    queue = [app for app in pyatspi.Registry.getDesktop(0) if app.name == 'uflinux-desktop']
    while queue:
        node = queue.pop(0)
        if node.name == name and node.getState().contains(pyatspi.STATE_SHOWING):
            return node
        queue.extend(child for child in node if child)
    return None


def wait(check, message):
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        try:
            value = check()
            if value:
                return value
        except (GLib.Error, RuntimeError, LookupError):
            pass
        time.sleep(.08)
    raise AssertionError(message)


def provider(method, tag, **args):
    reply = bus.call_sync(
        provider_name, provider_path, provider_name, method,
        GLib.Variant('(s)', (json.dumps({'method': tag, **args}),)), None,
        Gio.DBusCallFlags.NO_AUTO_START, 2000, None,
    )
    return json.loads(reply.unpack()[0])


def slot():
    model = provider('GetModel', 'get-model')
    return next((item.get('at') for item in model['items'] if item['name'] == 'Open Desktop Fixture'), None)


def service(variable):
    binary = os.environ[variable]
    with (runtime / (variable + '.log')).open('ab') as log:
        return subprocess.Popen([binary], stdout=log, stderr=subprocess.STDOUT)


def stop(process):
    if process.poll() is None:
        process.terminate()
        process.wait(timeout=5)


def search(text):
    field = wait(lambda: find('Search applications and files'), 'Start search did not appear')
    assert field.queryEditableText().setTextContents(text)


def icon_memory():
    command('wlr-randr', '--output', 'HEADLESS-1', '--custom-mode', '1920x1200')
    wait(lambda: any(grid['rows'] * grid['columns'] >= 202
                     for grid in provider('GetModel', 'get-model')['monitors']),
         'The memory test needs room for 200 additional icons')
    pid = bus.call_sync(
        'org.freedesktop.DBus', '/org/freedesktop/DBus', 'org.freedesktop.DBus',
        'GetConnectionUnixProcessID', GLib.Variant('(s)', ('org.uflinux.Desktop',)),
        None, Gio.DBusCallFlags.NONE, 2000, None,
    ).unpack()[0]

    def sample():
        values = []
        for _ in range(5):
            lines = Path(f'/proc/{pid}/smaps_rollup').read_text().splitlines()
            values.append(next(int(line.split()[1]) for line in lines if line.startswith('Pss:')))
            time.sleep(.15)
        return median(values)

    # Warm the resized surface and renderer before measuring its icon storage.
    warmup = desktop / 'Memory baseline folder'
    warmup.mkdir()
    wait(lambda: find(warmup.name), 'The resized wallpaper did not repaint')
    before = sample()
    # Directory icons do not request thumbnails, even when the provider supports them.
    for index in range(200):
        (desktop / f'Icon memory fixture {index:03}').mkdir()
    wait(lambda: sum(item.get('at') is not None
                     for item in provider('GetModel', 'get-model')['items']) == 202,
         'The provider did not place all 200 additional icons')
    time.sleep(.6)
    increase = sample() - before
    wait(lambda: find('Icon memory fixture 199'), 'The 200 desktop icons were not displayed')
    assert increase <= 12 * 1024, f'200 icons added {increase} KiB PSS'
    print(f'200 directory icons added {increase} KiB PSS')
    command('wlr-randr', '--output', 'HEADLESS-1', '--custom-mode', '1280x720')


def keyboard_reply(keyboard):
    assert select.select([keyboard.stdout], [], [], 8)[0], 'Virtual keyboard timed out'
    return keyboard.stdout.readline().strip()


def keys(keyboard, *requests):
    for request in requests:
        keyboard.stdin.write(request + '\n')
        keyboard.stdin.flush()
        assert keyboard_reply(keyboard) == 'ok', f'Virtual keyboard failed for {request}'


apps.mkdir(parents=True, exist_ok=True)
desktop.mkdir(exist_ok=True)
marker = runtime / 'desktop-launch'
file_marker = runtime / 'file-launch'
helper = root / 'record-uri'
helper.write_text('#!/bin/sh\nprintf "%s" "$1" > "$XDG_RUNTIME_DIR/file-launch"\n')
helper.chmod(0o755)
app_marker = runtime / 'app-launch'
(desktop / 'fixture.desktop').write_text(
    f'[Desktop Entry]\nType=Application\nName=Open Desktop Fixture\nExec=/bin/touch {marker}\n'
)
(apps / 'indexed-app.desktop').write_text(
    '[Desktop Entry]\nType=Application\nName=Indexed Fixture App\n'
    f'Comment=RemoteSearchNeedle\nExec=/bin/touch {app_marker}\n'
)
(apps / 'file-handler.desktop').write_text(
    '[Desktop Entry]\nType=Application\nName=Fixture handler\nNoDisplay=true\n'
    f'Exec={helper} %u\nMimeType=text/plain;inode/directory;\n'
)
Path(os.environ['XDG_CONFIG_HOME'], 'mimeapps.list').write_text(
    '[Default Applications]\ntext/plain=file-handler.desktop;\ninode/directory=file-handler.desktop;\n'
)
file = root / 'RemoteSearchNeedle DeskFileUnique.txt'
file.write_text('File visible through the index.\n')
folder = root / 'RemoteSearchNeedle DeskFolderUnique'
folder.mkdir()
stopword = root / 'SecondFileOnly.txt'
stopword.touch()
keyboard = subprocess.Popen(
    [str(Path(os.environ['UFLINUX_POINTER_DRIVER']).with_name('keyboard-driver'))],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
)
processes = [keyboard]
try:
    assert keyboard_reply(keyboard) == 'ready'
    processes.append(service('UFLINUX_FILES_PROVIDER_BIN'))
    assert wait(lambda: provider('Status', 'status')['renderer'], 'Shell did not claim the renderer')
    initial = wait(slot, 'Shell did not register desktop geometry')
    icon = wait(lambda: find('Open Desktop Fixture'), 'Desktop launcher was not rendered')
    rect = icon.queryComponent().getExtents(pyatspi.WINDOW_COORDS)
    width, height = map(int, os.environ.get('UFLINUX_TEST_OUTPUT_SIZE', '1280x720').split('x'))
    x, y = rect.x + rect.width // 2, rect.y + rect.height // 2
    pointer = os.environ['UFLINUX_POINTER_DRIVER']
    command(pointer, str(x), str(y), str(x), str(y), str(width), str(height))
    keys(keyboard, 'key Return')
    wait(marker.exists, f"Enter at desktop icon position ({x}, {y}) did not launch it")
    command(pointer, str(x), str(y), str(x + 96), str(y), str(width), str(height))
    moved = wait(lambda: slot() if slot() != initial else None, 'Desktop drag did not place the icon')
    assert moved == {**initial, 'col': initial['col'] + 1}
    new_icon = desktop / 'Live Desktop Folder'
    new_icon.mkdir()
    wait(lambda: find(new_icon.name), 'Desktop did not refresh after directory creation')
    new_icon.rmdir()
    wait(lambda: not find(new_icon.name), 'Deleted desktop icon was retained')
    stop(processes[-1])
    processes.append(service('UFLINUX_FILES_PROVIDER_BIN'))
    assert wait(lambda: provider('Status', 'status')['renderer'], 'Restarted provider missed the renderer')
    wait(lambda: find('Open Desktop Fixture'), 'Desktop icons did not recover after provider restart')
    assert slot() == moved, 'Provider restart lost the placement'
    if os.environ.get('UFLINUX_TEST_OUTPUT_HOTPLUG') == '1':
        keys(keyboard, 'mod ctrl 1', 'mod alt 1', 'key F10', 'mod alt 0', 'mod ctrl 0')
        def two_outputs():
            monitors = provider('GetModel', 'get-model')['monitors']
            return monitors if len(monitors) == 2 else None

        outputs = wait(two_outputs, 'Shell did not register the added output')
        added = next(grid['monitor'] for grid in outputs if grid['monitor'] != moved['monitor'])
        provider('Place', 'place', id='fixture.desktop', monitor=added, col=0, row=0)
        wait(lambda: slot()['monitor'] == added, 'Icon was not placed on the added output')
        keys(keyboard, 'mod ctrl 1', 'mod alt 1', 'key F11', 'mod alt 0', 'mod ctrl 0')
        wait(lambda: slot()['monitor'] == moved['monitor'], 'Icon was not repacked after removal')

    shell('--start-menu', '--output', 'HEADLESS-1')
    search('SecondFileOnly')
    wait(lambda: find('File search unavailable') or find('Indexed search unavailable'),
         'Start did not report that the search service was absent')
    processes.append(service('UFLINUX_SEARCHD_BIN'))
    search('RemoteSearchNeedle unmatchedword')
    wait(lambda: find('Indexed Fixture App'), 'Indexed application result was missing')
    wait(lambda: find(file.name) and find(folder.name), 'Indexed files and folders were missing')
    wait(lambda: find('Files and folders'), 'File results did not have a group')
    keys(keyboard, 'key Return')
    wait(app_marker.exists, 'The indexed application did not launch first')
    wait(lambda: json.loads(shell('--status'))['popup'] is None, 'Launch did not close Start')
    for path, query in ((file, 'DeskFileUnique'), (folder, 'DeskFolderUnique')):
        shell('--start-menu', '--output', 'HEADLESS-1')
        search(query)
        wait(lambda: find(path.name), 'Indexed path was missing')
        keys(keyboard, 'key Return')
        wait(file_marker.exists, f'Indexed path did not open: {path.name}')
        assert file_marker.read_text() in (str(path), path.as_uri())
        file_marker.unlink()
        wait(lambda: json.loads(shell('--status'))['popup'] is None, 'File activation did not close Start')
    shell('--start-menu', '--output', 'HEADLESS-1')
    search('RemoteSearchNeedle')
    search('SecondFileOnly')
    wait(lambda: find(stopword.name), 'Latest query did not show its file')
    assert not find(file.name) and not find('Indexed Fixture App')
    live_file = root / 'SecondFileOnly extra.txt'
    live_file.touch()
    wait(lambda: find(live_file.name), 'The open search did not refresh after indexing a new file')
    live_file.unlink()
    wait(lambda: not find(live_file.name), 'The open search retained a deleted file')
    shell('--hide')
    if os.environ.get('UFLINUX_TEST_ICON_MEMORY') == '1':
        icon_memory()
    print('Desktop icon launch, drag, refresh and recovery, and indexed Start search passed')
finally:
    for process in processes:
        stop(process)
