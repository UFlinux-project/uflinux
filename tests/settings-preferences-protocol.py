#!/usr/bin/env python3
"""Persist the wallpaper the shell draws and restore it across a service restart."""
import json
import os
from pathlib import Path
import signal
import subprocess
import time
from gi.repository import Gio,GLib
bus=Gio.bus_get_sync(Gio.BusType.SESSION)
results=Path(os.environ.get('UFLINUX_SMOKE_RESULTS','/tmp/uflinux-settings-smoke-results'))
def call(method,action=None):
    value=GLib.Variant('(s)',(json.dumps(action),)) if action else None
    return json.loads(bus.call_sync('org.uflinux.Settings1','/org/uflinux/Settings','org.uflinux.Settings1',method,value,None,Gio.DBusCallFlags.NO_AUTO_START,15000,None).unpack()[0])
def drawn():
    return json.loads(subprocess.check_output(['uflinux-desktop','--status'],text=True,timeout=5)).get('background')
def on_output(name='HEADLESS-1'):
    background=drawn()
    if not background:return None
    return next((entry for entry in background['outputs'] if entry['output']==name),None)
def showing(expected):
    entry=on_output() or {}
    return entry.get('file')==expected['wallpapers'][0]['file'] and entry.get('fit')==expected['wallpapers'][0]['fit']
desktop=os.environ.get('UFLINUX_DESKTOP_PID','')
desktop_log=os.environ.get('UFLINUX_DESKTOP_LOG','')
def died():
    """Why the wait can never succeed, if the shell that draws it is gone.
    A dead desktop answers --status with the null fallback forever, so without
    this the harness burns its whole budget and reports an opaque null instead
    of the desktop's last lines."""
    if desktop:
        try:
            os.kill(int(desktop),0)
        except PermissionError:
            return None
        except (OSError,ValueError):
            tail=''
            try:
                tail=' | '.join(Path(desktop_log).read_text(errors='replace').splitlines()[-3:])
            except OSError:pass
            return 'uflinux-desktop exited while waiting for the wallpaper'+(': '+tail if tail else '')
    return None
def wait(predicate):
    last=None
    for _ in range(80):
        try:
            if predicate():return
            last=drawn()
        except (GLib.Error,subprocess.SubprocessError,OSError):pass
        dead=died()
        if dead:raise AssertionError(dead)
        time.sleep(.1)
    raise AssertionError('Preferences or drawn wallpaper did not converge: '+json.dumps(last))
previous=call('GetState')['appearance']
wallpaper={'wallpapers':[{'output':'*','file':'/usr/share/backgrounds/uflinux/Dark_Ivy.jpg','fit':'fit'}],'color':'#153658'}
call('Execute',{'action':'set-appearance','appearance':wallpaper})
wait(lambda:call('GetState')['appearance']==wallpaper)
stored=Path(os.environ['XDG_CONFIG_HOME'])/'uflinux/appearance.json'
assert json.loads(stored.read_text())==wallpaper and stored.stat().st_mode&0o777==0o600
# The shell owns the pixels now, so a change is visible without a new client.
wait(lambda:showing(wallpaper))
assert not drawn()['failed'],drawn()['failed']
# An image the service cannot read never reaches the screen, and the appearance
# that was already drawn survives the rejected request.
try:
    call('Execute',{'action':'set-appearance','appearance':{'wallpapers':[{'output':'*','file':'/nonexistent/wallpaper.png','fit':'fill'}],'color':'#102c46'}})
    raise AssertionError('An unavailable wallpaper file was accepted')
except GLib.Error:pass
assert call('GetState')['appearance']==wallpaper
assert showing(wallpaper)
# Restarting the service must not disturb the wallpaper, and the service that
# comes back has to republish what it read from disk.
daemon=int((results/'service.pid').read_text())
os.kill(daemon,signal.SIGTERM)
wait(lambda:not Path('/proc/'+str(daemon)).exists())
with (results/'service.log').open('a') as log:
    restarted=subprocess.Popen(['/usr/libexec/uflinux-settingsd'],stdout=log,stderr=log)
(results/'service.pid').write_text(str(restarted.pid))
try:
    wait(lambda:restarted.poll() is None and call('GetState')['appearance']==wallpaper)
    # The service briefly looks absent while it is gone, which resets the shell
    # to the default appearance; the state it republishes has to win again.
    wait(lambda:showing(wallpaper))
    call('Execute',{'action':'set-appearance','appearance':previous})
    print('Wallpaper persistence, drawn state, service restart and restoration passed')
finally:
    call('Execute',{'action':'set-appearance','appearance':previous})
