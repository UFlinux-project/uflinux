#!/usr/bin/env python3
"""Exercise real labwc output test/apply/confirmation/rollback, not a display mock."""
import json
import os
import time
import subprocess
from gi.repository import Gio, GLib

bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
name = 'org.uflinux.Settings1'
path = '/org/uflinux/Settings'


def state():
    return json.loads(bus.call_sync(name,path,name,'GetState',None,None,0,5000,None).unpack()[0])


def call(action, connection=bus):
    return json.loads(connection.call_sync(name,path,name,'Execute',GLib.Variant('(s)',(json.dumps(action),)),None,0,20000,None).unpack()[0])


def wait(predicate, timeout=5):
    for _ in range(int(timeout/.05)):
        if predicate(state()):
            return
        time.sleep(.05)
    raise AssertionError('Display state did not converge')


def config(outputs):
    return [{**{k:o[k] for k in ['name','enabled','x','y','scale','transform','adaptive_sync']},'mode':o['current']} for o in outputs]


def layout():
    """The compositor's outputs plus the surfaces the shell drew for them.

    A hotplug removes a wl_output while clients are binding it, and a client
    that binds an id the compositor already dropped is killed by the resulting
    protocol error, so the shell's own view is part of the picture. A shell
    that is gone answers --status with the null fallback, which is reported as
    no surfaces at all."""
    surfaces=config(state()['displays']['outputs'])
    try:
        status=json.loads(subprocess.check_output(['uflinux-desktop','--status'],text=True,timeout=5))
    except (subprocess.SubprocessError,OSError,json.JSONDecodeError):
        return surfaces,None
    background=status.get('background') or {}
    drawn=[entry['output'] for entry in background.get('outputs',[])]
    # A monitor rebuilt by a hotplug announces itself before its name arrives,
    # so a surface without one is still being adopted. A shell that is gone
    # answers --status with the null fallback, which looks like no surfaces at
    # all. Neither counts as settled.
    return surfaces,(sorted(drawn) if drawn and all(drawn) else None)


def settle(timeout=60):
    """Hand the session over only once the output list has gone quiet.

    The last apply rebuilds the outputs, and the add/remove pairs it produces
    stay in flight after Execute returns. Waiting for a second of an unchanged
    layout lets the shell finish re-binding before the next test starts poking
    it; on riscv64 it used to die from a wl_output global that had already been
    removed by the time it bound it."""
    deadline=time.time()+timeout
    steady=None
    since=0.0
    while time.time()<deadline:
        current=layout()
        if current != steady:
            steady=current
            since=time.time()
        elif current[1] is not None and time.time()-since>=1:
            return
        time.sleep(.25)
    if steady is not None and steady[1] is None:
        raise AssertionError('The shell never redrew its outputs after the hotplug cycles')
    raise AssertionError('The output layout never settled after the hotplug cycles')


before=config(state()['displays']['outputs'])
assert before
invalid=[{**o,'enabled':False} for o in before]
try:
    call({'action':'apply-displays','outputs':invalid,'hdr':False})
    raise AssertionError('All outputs were disabled')
except GLib.Error:
    pass
changed=[dict(o) for o in before]
changed[-1]['x']+=80
identifier=call({'action':'apply-displays','outputs':changed,'hdr':False})['id']
wait(lambda s:s['displays']['outputs'][-1]['x']==changed[-1]['x'])
call({'action':'confirm-displays','id':identifier})
assert state()['displays']['pending'] is None
temporary=[dict(o) for o in changed]
temporary[-1]['x']+=80
call({'action':'apply-displays','outputs':temporary,'hdr':False})
wait(lambda s:s['displays']['pending'] is None,20)
wait(lambda s:s['displays']['outputs'][-1]['x']==changed[-1]['x'])
other=Gio.DBusConnection.new_for_address_sync(os.environ['DBUS_SESSION_BUS_ADDRESS'],
    Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT|Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION,None,None)
identifier=call({'action':'apply-displays','outputs':temporary,'hdr':False},other)['id']
try:
    call({'action':'confirm-displays','id':identifier})
    raise AssertionError('A different caller confirmed the configuration')
except GLib.Error:
    pass
other.close_sync(None)
wait(lambda s:s['displays']['pending'] is None)
wait(lambda s:s['displays']['outputs'][-1]['x']==changed[-1]['x'])
# Leave the reference compositor in its original layout.
identifier=call({'action':'apply-displays','outputs':before,'hdr':False})['id']
wait(lambda s:s['displays']['outputs'][-1]['x']==before[-1]['x'])
call({'action':'confirm-displays','id':identifier})
# Exercise fractional scale and rotation against the actual compositor.
scaled=[dict(o) for o in before]
scaled[0].update(scale=1.25,transform=1)
identifier=call({'action':'apply-displays','outputs':scaled,'hdr':False})['id']
wait(lambda s:abs(s['displays']['outputs'][0]['scale']-1.25)<.01 and s['displays']['outputs'][0]['transform']==1)
call({'action':'confirm-displays','id':identifier})
identifier=call({'action':'apply-displays','outputs':before,'hdr':False})['id']
call({'action':'confirm-displays','id':identifier})
# Private labwc test bindings create and remove a real headless output.
subprocess.run(['wtype','-M','ctrl','-M','alt','-k','F10','-m','alt','-m','ctrl'],check=True)
wait(lambda s:any(o['name']=='UF-TEST' for o in s['displays']['outputs']))
time.sleep(.5) # Let the startup restore transaction finish.
with_virtual=config(state()['displays']['outputs'])
virtual_only=[dict(o,enabled=o['name']=='UF-TEST') for o in with_virtual]
identifier=call({'action':'apply-displays','outputs':virtual_only,'hdr':False})['id']
call({'action':'confirm-displays','id':identifier})
subprocess.run(['wtype','-M','ctrl','-M','alt','-k','F11','-m','alt','-m','ctrl'],check=True)
wait(lambda s:len(s['displays']['outputs'])==len(before) and any(o['enabled'] for o in s['displays']['outputs']))
# A disappearing output during an unconfirmed change must also release the transaction.
subprocess.run(['wtype','-M','ctrl','-M','alt','-k','F10','-m','alt','-m','ctrl'],check=True)
wait(lambda s:any(o['name']=='UF-TEST' for o in s['displays']['outputs']))
time.sleep(.5)
temporary=config(state()['displays']['outputs'])
temporary[-1]['x']+=40
call({'action':'apply-displays','outputs':temporary,'hdr':False})
subprocess.run(['wtype','-M','ctrl','-M','alt','-k','F11','-m','alt','-m','ctrl'],check=True)
wait(lambda s:s['displays']['pending'] is None and len(s['displays']['outputs'])==len(before) and any(o['enabled'] for o in s['displays']['outputs']))
settle()
identifier=call({'action':'apply-displays','outputs':before,'hdr':False})['id']
call({'action':'confirm-displays','id':identifier})
settle()
print('Display test/apply, timeout, caller loss, scaling, rotation, hotplug and last-output recovery passed')
