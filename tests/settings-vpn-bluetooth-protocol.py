#!/usr/bin/env python3
"""Exercise settings service Bluetooth and VPN flows against the D-Bus fixtures."""
import json
import time
from gi.repository import Gio, GLib

bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
path = "/org/uflinux/Settings"
name = "org.uflinux.Settings1"
device = "/org/bluez/hci0/dev_11_22_33_44_55_66"


def call(action):
    result = bus.call_sync(name, path, name, "Execute", GLib.Variant("(s)", (json.dumps(action),)), None, 0, 120000, None)
    return json.loads(result.unpack()[0])


def state():
    return json.loads(bus.call_sync(name, path, name, "GetState", None, None, 0, 5000, None).unpack()[0])


def wait(predicate, message, timeout=10):
    for _ in range(int(timeout / .05)):
        value = state()
        if predicate(value):
            return value
        while GLib.MainContext.default().iteration(False):
            pass
        time.sleep(.05)
    raise AssertionError(message)


def device_view(snapshot):
    return next(d for d in snapshot['bluetooth']['devices'] if d['path'] == device)


def tunnel(snapshot, number):
    return next(t for t in snapshot['vpn']['tunnels'] if t['path'].endswith(number))


# Bluetooth: the adapter fixture must appear, then power, scan and pairing all
# round-trip through the real service and the mock adapter.
wait(lambda s: s['bluetooth']['available'] and s['bluetooth']['devices'],
     'The Bluetooth fixture did not reach the settings service')
call({'action': 'bluetooth-power', 'enabled': False})
wait(lambda s: not s['bluetooth']['powered'], 'Bluetooth did not power off')
call({'action': 'bluetooth-power', 'enabled': True})
wait(lambda s: s['bluetooth']['powered'], 'Bluetooth did not power on')
call({'action': 'bluetooth-scan'})
wait(lambda s: s['bluetooth']['discovering'], 'Bluetooth scan did not start')
call({'action': 'bluetooth-pair', 'device': device})
wait(lambda s: device_view(s)['paired'], 'The fixture device did not pair')
call({'action': 'bluetooth-trust', 'device': device, 'enabled': True})
wait(lambda s: device_view(s)['trusted'], 'The fixture device did not trust')
call({'action': 'bluetooth-connect', 'device': device})
wait(lambda s: device_view(s)['connected'], 'The fixture device did not connect')
call({'action': 'bluetooth-disconnect', 'device': device})
wait(lambda s: not device_view(s)['connected'], 'The fixture device did not disconnect')

# VPN: an import becomes a ConnMan VPN provider, connects, and is removed again.
wait(lambda s: s['vpn']['available'], 'The ConnMan VPN fixture did not reach the settings service')
call({'action': 'vpn-import', 'name': 'UFLinux Fixture', 'host': 'vpn.example.test', 'keys': {
    'WireGuard.PrivateKey': 'aHl3Nzl4dThja2dwbG9rMnF6c3h2dTFoM25hbHl6cGQ=',
    'WireGuard.PublicKey': 'Q2dZSUtvWkl6ajBFQXdJRFNRQXdSZ0loQU9kU3lFOFFPUzA0VThYSk5',
    'WireGuard.AllowedIPs': '0.0.0.0/0',
    'WireGuard.Address': '10.192.122.2/32',
    'WireGuard.EndpointPort': '51820'}})
wait(lambda s: any(t['name'] == 'UFLinux Fixture' for t in s['vpn']['tunnels']),
     'The imported VPN tunnel never appeared')
tunnel_path = tunnel(state(), 'fixture1')['path']
assert tunnel(state(), 'fixture1')['state'] == 'idle'
assert tunnel(state(), 'fixture1')['host'] == 'vpn.example.test'
call({'action': 'vpn-connect', 'tunnel': tunnel_path})
wait(lambda s: tunnel(s, 'fixture1')['state'] == 'ready', 'The VPN tunnel never became ready')
call({'action': 'vpn-disconnect', 'tunnel': tunnel_path})
wait(lambda s: tunnel(s, 'fixture1')['state'] == 'idle', 'The VPN tunnel never disconnected')
call({'action': 'vpn-remove', 'tunnel': tunnel_path})
wait(lambda s: not s['vpn']['tunnels'], 'The VPN tunnel was not removed')

# Unpairing removes the fixture device from the adapter view.
call({'action': 'bluetooth-unpair', 'device': device})
wait(lambda s: all(d['path'] != device for d in s['bluetooth']['devices']),
     'The fixture device was not removed')
assert 'WireGuard.PrivateKey' not in json.dumps(state())
print('Bluetooth pairing/connections and WireGuard VPN import flows passed')
