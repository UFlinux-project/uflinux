#!/usr/bin/env python3
"""BlueZ D-Bus fixture. ObjectManager view of one adapter and one device."""
import signal
from gi.repository import Gio, GLib

NAME = "org.bluez"
ADAPTER = "/org/bluez/hci0"
DEVICE = "/org/bluez/hci0/dev_11_22_33_44_55_66"
ADAPTER_XML = """<node>
<interface name="org.bluez.Adapter1">
 <method name="SetProperty"><arg type="s" direction="in"/><arg type="v" direction="in"/></method>
 <method name="StartDiscovery"/>
 <method name="StopDiscovery"/>
 <method name="RemoveDevice"><arg type="o" direction="in"/></method>
 <property name="Powered" type="b" access="readwrite"/>
 <property name="Discovering" type="b" access="read"/>
</interface></node>"""
DEVICE_XML = """<node>
<interface name="org.bluez.Device1">
 <method name="SetProperty"><arg type="s" direction="in"/><arg type="v" direction="in"/></method>
 <method name="Pair"/>
 <method name="CancelPairing"/>
 <method name="Connect"/>
 <method name="Disconnect"/>
 <property name="Paired" type="b" access="read"/>
 <property name="Trusted" type="b" access="readwrite"/>
 <property name="Connected" type="b" access="read"/>
</interface></node>"""
bus = Gio.bus_get_sync(Gio.BusType.SYSTEM, None)
bus.call_sync("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "RequestName",
              GLib.Variant("(su)", (NAME, 4)), None, 0, 5000, None)
powered = True
discovering = False
paired = False
trusted = False
connected = False
removed = False


def properties():
    return {
        "org.bluez.Adapter1": {"Address": GLib.Variant("s", "00:11:22:33:44:55"),
                               "Name": GLib.Variant("s", "UFLinux Test Adapter"),
                               "Powered": GLib.Variant("b", powered),
                               "Discovering": GLib.Variant("b", discovering)},
        "org.bluez.Device1": {"Address": GLib.Variant("s", "11:22:33:44:55:66"),
                              "Name": GLib.Variant("s", "UFLinux Test Headset"),
                              "Alias": GLib.Variant("s", "UFLinux Test Headset"),
                              "Paired": GLib.Variant("b", paired),
                              "Trusted": GLib.Variant("b", trusted),
                              "Connected": GLib.Variant("b", connected),
                              "RSSI": GLib.Variant("n", -60)},
    }


def managed_objects():
    objects = [(ADAPTER, [("org.bluez.Adapter1", properties()["org.bluez.Adapter1"])])]
    if not removed:
        objects.append((DEVICE, [("org.bluez.Device1", properties()["org.bluez.Device1"])]))
    return objects


def changed(interface):
    bus.emit_signal(None, ADAPTER, "org.freedesktop.DBus.ObjectManager", "InterfacesAdded",
                    GLib.Variant("(oa{sa{sv}})", (DEVICE, [("org.bluez.Device1",
                                                            properties()["org.bluez.Device1"])])))
    bus.emit_signal(None, ADAPTER if interface == "org.bluez.Adapter1" else DEVICE,
                    "org.freedesktop.DBus.Properties", "PropertiesChanged",
                    GLib.Variant("(sa{sv}as)", (interface, {}, [])))


def removed_signal():
    bus.emit_signal(None, ADAPTER, "org.freedesktop.DBus.ObjectManager", "InterfacesRemoved",
                    GLib.Variant("(oas)", (DEVICE, ["org.bluez.Device1"])))


def method(connection, sender, path, interface, name, args, invocation):
    global powered, discovering, paired, trusted, connected, removed
    if name == "GetManagedObjects":
        invocation.return_value(GLib.Variant("(a{oa{sa{sv}}})", (managed_objects(),)))
    elif name == "SetProperty":
        key, value = args.unpack()
        if path == ADAPTER and key == "Powered":
            powered = value
            discovering = discovering and powered
        elif path == DEVICE and key == "Trusted":
            if not paired:
                invocation.return_dbus_error("org.bluez.Error.NotAvailable", "Pair first")
                return
            trusted = value
        else:
            invocation.return_dbus_error("org.bluez.Error.InvalidArguments", "Unsupported fixture property")
            return
        invocation.return_value(None)
        changed("org.bluez.Adapter1" if path == ADAPTER else "org.bluez.Device1")
    elif name == "StartDiscovery":
        if not powered:
            invocation.return_dbus_error("org.bluez.Error.NotReady", "Adapter is powered off")
            return
        discovering = True
        invocation.return_value(None)
        changed("org.bluez.Adapter1")
    elif name == "StopDiscovery":
        discovering = False
        invocation.return_value(None)
        changed("org.bluez.Adapter1")
    elif name == "RemoveDevice":
        if args.unpack()[0] != DEVICE:
            invocation.return_dbus_error("org.bluez.Error.DoesNotExist", "Unknown fixture device")
            return
        removed = True
        paired = trusted = connected = False
        invocation.return_value(None)
        removed_signal()
    elif name == "Pair":
        if not powered:
            invocation.return_dbus_error("org.bluez.Error.NotReady", "Adapter is powered off")
            return
        paired = True
        invocation.return_value(None)
        changed("org.bluez.Device1")
    elif name == "Connect":
        if not paired:
            invocation.return_dbus_error("org.bluez.Error.NotPaired", "Pair first")
            return
        connected = True
        invocation.return_value(None)
        changed("org.bluez.Device1")
    elif name == "Disconnect":
        connected = False
        invocation.return_value(None)
        changed("org.bluez.Device1")
    else:
        invocation.return_dbus_error("org.freedesktop.DBus.Error.UnknownMethod", "Unknown fixture operation")


OM_XML = """<node>
<interface name="org.freedesktop.DBus.ObjectManager">
 <method name="GetManagedObjects"><arg type="a{oa{sa{sv}}}" direction="out" name="object_tree"/></method>
 <signal name="InterfacesAdded"><arg type="o" name="object_path"/><arg type="a{sa{sv}}" name="interfaces"/></signal>
 <signal name="InterfacesRemoved"><arg type="o" name="object_path"/><arg type="as" name="interfaces"/></signal>
</interface></node>"""

# GDBus keeps interface infos as [transfer none]; the node infos must outlive registration.
NODES = [Gio.DBusNodeInfo.new_for_xml(ADAPTER_XML),
         Gio.DBusNodeInfo.new_for_xml(DEVICE_XML),
         Gio.DBusNodeInfo.new_for_xml(OM_XML)]
bus.register_object(ADAPTER, NODES[0].interfaces[0], method, None, None)
bus.register_object(DEVICE, NODES[1].interfaces[0], method, None, None)
bus.register_object("/", NODES[2].interfaces[0], method, None, None)
loop = GLib.MainLoop()
GLib.unix_signal_add(GLib.PRIORITY_DEFAULT, signal.SIGTERM, lambda: (loop.quit(), False)[1])
loop.run()
