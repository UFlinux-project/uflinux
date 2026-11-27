#!/usr/bin/env python3
"""Round-trip the printing surface and its wire format over D-Bus.

The stack ships disabled, so this harness never asks for a state change: it only
uses the read-only verbs, which the system helper exempts from polkit. The
mutating verbs are deliberately not sent, because an interactive authorization
prompt cannot be answered in a headless session and would stall the daemon;
their wire format is covered by the core crate's serde tests, their input rules
by the Printing page's own tests, and their authorization by
tests/test_printing.py.
"""
import json
import time
from gi.repository import Gio, GLib

bus = Gio.bus_get_sync(Gio.BusType.SESSION)


def call(method, action=None):
    value = GLib.Variant('(s)', (json.dumps(action),)) if action else None
    return json.loads(bus.call_sync('org.uflinux.Settings1', '/org/uflinux/Settings',
                                    'org.uflinux.Settings1', method, value, None,
                                    Gio.DBusCallFlags.NO_AUTO_START, 15000, None).unpack()[0])


def wait(predicate, description):
    for _ in range(80):
        try:
            if predicate():
                return
        except GLib.Error:
            pass
        time.sleep(.1)
    raise AssertionError(description)


# A snapshot published before the printing surface existed must still parse, so
# the key is always present once the daemon in this image is up.
state = call('GetState')
assert 'printing' in state, 'The snapshot carries no printing surface'
printing = state['printing']
for key in ('installed', 'enabled', 'running', 'shared', 'discovery', 'queues',
            'devices', 'scanners', 'default', 'error'):
    assert key in printing, 'The printing surface is missing %r: %r' % (key, printing)
assert isinstance(printing['installed'], bool) and isinstance(printing['enabled'], bool)
assert isinstance(printing['running'], bool) and isinstance(printing['shared'], bool)
assert isinstance(printing['discovery'], bool)
assert isinstance(printing['queues'], list) and isinstance(printing['devices'], list)
assert isinstance(printing['scanners'], list)
# The stack is installed but inert, so nothing may be running before a switch
# has been flipped. A queue list is only gathered while cupsd is up.
assert printing['installed'], 'The image must ship the printing stack'
assert not printing['enabled'] and not printing['running'], \
    'Printing must ship disabled: %r' % (printing,)
assert printing['queues'] == [] and printing['devices'] == [] and printing['scanners'] == []

refreshed = call('Execute', {'action': 'printing-status'})
assert refreshed['installed'] and not refreshed['enabled'] and not refreshed['running']
assert refreshed['queues'] == [] and refreshed['error'] is None
wait(lambda: call('GetState')['printing'] == refreshed,
     'The published printing state differs from the reported one')

# The status verb is exempt from authorization, so it round-trips here even
# though the helper answers it. Everything else would raise a prompt.
status = call('Execute', {'action': 'printing-system', 'operation': {'operation': 'status'}})
assert isinstance(status.get('queues'), list) and isinstance(status.get('devices'), list)
assert isinstance(status.get('scanners'), list) and isinstance(status.get('shared'), bool)
assert not status.get('enabled') and not status.get('running')

# A malformed payload must be refused before the helper is asked to do anything.
for rejected in ({'action': 'printing-system'},
                 {'action': 'printing-system', 'operation': {'operation': 'not-a-verb'}},
                 {'action': 'printing-system', 'operation': {'operation': 'set-shared'}},
                 {'action': 'printing-system', 'operation': {'operation': 'add-printer',
                                                             'name': 'Office'}},
                 {'action': 'printing-system', 'operation': {'operation': 'status',
                                                             'extra': True}}):
    try:
        call('Execute', rejected)
        raise AssertionError('Rejected payload %r was accepted' % (rejected,))
    except GLib.Error:
        pass
assert call('GetState')['printing'] == refreshed, 'A rejected request changed the surface'
print('Printing surface, read-only verbs and request validation passed')
