#!/usr/bin/env python3
"""Operate real GTK controls over AT-SPI and the private Wayland input protocol."""
import json
import os
from pathlib import Path
import subprocess
import time
import pyatspi
from gi.repository import Gio,GLib
try:
    # Gio.DesktopAppInfo moved to GioUnix and is deprecated in newer GI
    # introspection data; fall back on older glib where GioUnix is absent.
    from gi.repository import GioUnix
    DesktopAppInfo = GioUnix.DesktopAppInfo
except ImportError:
    DesktopAppInfo = Gio.DesktopAppInfo

def command(*args):
    subprocess.run(args,check=True,stdout=subprocess.DEVNULL,stderr=subprocess.PIPE,timeout=15)
def state(): return json.loads(subprocess.check_output(['uflinux-settingsctl','status'],text=True,timeout=15))
def wait(predicate,description):
    for _ in range(80):
        while GLib.MainContext.default().pending(): GLib.MainContext.default().iteration(False)
        try:
            value=predicate()
            if value:return value
        except (RuntimeError,LookupError,AttributeError,GLib.GError):pass
        time.sleep(.05)
    raise AssertionError(description)
def tree():
    root=pyatspi.Registry.getDesktop(0)
    queue=[a for a in root if a.name=='uflinux-settings']
    result=[]
    while queue:
        a=queue.pop()
        # A resident client can destroy an accessible between reading a child
        # list and querying it; treat atspi_error "Object does not exist" as a
        # node that is simply gone.
        try:
            a.getRoleName()
        except GLib.GError:
            continue
        result.append(a)
        try:queue.extend([c for c in a if c])
        except Exception:pass
        assert len(result)<2000,'Unbounded accessible tree'
    return result
def find(name,role=None):
    return next((a for a in tree() if a.name==name and (not role or a.getRoleName()==role)),None)
def click(a):
    action=a.queryAction()
    assert action.nActions, 'Control has no keyboard/accessibility action: '+a.name
    assert action.doAction(0)
current=['network']
def page(name):
    command('uflinux-settings','--page',name);current[0]=name;time.sleep(.2)
def network_tabs():
    # A network tab exists exactly when its technology or subsystem is
    # available (see tabs() in pages/network.rs), so derive the expected set
    # from the published daemon state instead of hardcoding a layout the
    # connman fixture may not provide (it exposes wifi only).
    s=state();n=s['network'];kinds={t['kind'] for t in n['technologies']}
    labels=[]
    if 'wifi' in kinds or n['wifi_hardware_blocked'] or n['wifi_software_blocked']:labels.append('Wi-Fi')
    if 'ethernet' in kinds:labels.append('Ethernet')
    if s['bluetooth']['available']:labels.append('Bluetooth')
    if s['vpn']['available']:labels.append('VPN')
    return labels
def diagnostics():
    try:
        network=state().get('network')
    except Exception as error:
        network='status failed: '+repr(error)
    try:
        nodes=sorted({(a.getRoleName(),a.name) for a in tree()})
    except Exception as error:
        nodes='tree failed: '+repr(error)
    return json.dumps({'network':network,'atspi':nodes})

command('wlrctl','toplevel','focus','app_id:org.uflinux.Settings')
command('wtype','-M','ctrl','-M','alt','-k','F9','-m','alt','-m','ctrl')
command('wlrctl','toplevel','maximize','app_id:org.uflinux.Settings')
time.sleep(.3)
# Every page tab must be a visible control, and switching tabs must not resize
# the window (the stack keeps one homogeneous size request).
def tab(label):
    # GtkStackSwitcher exposes its buttons as AT-SPI page tabs, not toggle
    # buttons. A transient session-bus failure can also leave the resident UI
    # on its empty startup snapshot with no Changed to recover it, so
    # re-activate the page and ask the daemon to re-publish between rounds,
    # and dump the daemon state plus the AT-SPI tree on failure.
    for _ in range(10):
        for _ in range(20):
            while GLib.MainContext.default().pending(): GLib.MainContext.default().iteration(False)
            try:
                found=next((a for a in tree() if a.name==label and a.getRoleName() in ('page tab','toggle button')),None)
                if found:return found
            except (RuntimeError,LookupError,AttributeError,GLib.GError):pass
            time.sleep(.05)
        page(current[0])
        subprocess.run(['uflinux-settingsctl','request'],input='{"action":"refresh"}',text=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,timeout=15)
    raise AssertionError('Missing '+label+' tab: '+diagnostics())
def window_size():
    frame=next(a for a in tree() if a.getRoleName()=='frame')
    return frame.queryComponent().getExtents(pyatspi.WINDOW_COORDS)
page('network')
# The header bar owns the search field and the sidebar has no hide control, so the
# navigation list stays reachable without clicking anything.
search=wait(lambda:find('Search settings'),'The header search field is missing')
assert search.getRoleName() in ('entry','text'),'Header search has an unexpected role: '+search.getRoleName()
assert search.getState().contains(pyatspi.STATE_SHOWING),'Header search is not visible'
assert not [a for a in tree() if 'Show or hide settings navigation' in (a.name, a.description)], \
    'The removed sidebar toggle is still present'
sizes=[]
for label in network_tabs():
    click(tab(label));sizes.append(window_size())
assert len(set((s.width,s.height) for s in sizes))==1,'Network tabs changed the window size'
page('audio')
for label in ('Output','Input'):
    tab(label)
page('displays')
canvas=wait(lambda:find('Display layout'),'The layout preview must have an accessible name')
outputs=state()['displays']['outputs']
first=next(o for o in outputs if o['enabled'])
field=wait(lambda:find('Horizontal position · '+first['name'],'spin button'),'Horizontal position is unavailable')
before=field.queryValue().currentValue
# The frame contains the fitted layout; choose a point within its first monitor.
rect=canvas.queryComponent().getExtents(pyatspi.WINDOW_COORDS)
width=max(o['x']+o['current']['width']/o['scale'] for o in outputs if o['enabled'])
height=max(o['y']+o['current']['height']/o['scale'] for o in outputs if o['enabled'])
factor=min((rect.width-24)/width,(rect.height-24)/height)
x=int(rect.x+12+(first['x']+first['current']['width']/first['scale']*.3)*factor)
y=int(rect.y+12+(first['y']+first['current']['height']/first['scale']*.4)*factor)
driver=str(Path('/tmp/uflinux-ui-driver/pointer-driver'))
command(driver,str(x),str(y),str(x+30),str(y+8),str(int(width)),str(int(height)))
wait(lambda:field.queryValue().currentValue!=before,'Dragging did not update the numeric position control')
new_x=int(field.queryValue().currentValue)
click(find('Apply display settings','button'))
wait(lambda:state()['displays']['pending'],'Dragging was not applied')
wait(lambda:next(o['x'] for o in state()['displays']['outputs'] if o['name']==first['name'])==new_x,'Numeric field and applied layout differ')
click(wait(lambda:find('Revert','button'),'Missing display confirmation dialog'))
wait(lambda:state()['displays']['pending'] is None,'Display revert did not finish')
wait(lambda:find('Keep these display settings?','frame') is None,'Display confirmation dialog did not close')
time.sleep(.3)

# Install two harmless fixture applications, preserving a third association.
applications=Path(os.environ['XDG_DATA_HOME'])/'applications';applications.mkdir(parents=True,exist_ok=True)
for name in ('A','B'):
    (applications/('uflinux-test-'+name+'.desktop')).write_text('[Desktop Entry]\nType=Application\nName=UFLinux Test '+name+'\nExec=/bin/true %U\nMimeType=x-scheme-handler/http;x-scheme-handler/https;application/x-uflinux-unrelated;\n')
command('update-desktop-database',str(applications))
a=DesktopAppInfo.new('uflinux-test-A.desktop')
assert a and a.set_as_default_for_type('x-scheme-handler/http')
assert a.set_as_default_for_type('application/x-uflinux-unrelated')
page('default-apps')
label=wait(lambda:find('Web browser','label'),'Default browser row is missing')
row=label.parent
combo=next(child for child in row if child.getRoleName()=='combo box')
click(next(child for child in combo if child.getRoleName()=='toggle button'))
wait(lambda:find('UFLinux Test B','label'),'The browser dropdown did not open')
target=find('UFLinux Test B','label')
while target.getRoleName()!='list item':
    target=target.parent
    assert target is not None, 'The browser item is outside the dropdown list'
wait(lambda:target.getState().contains(pyatspi.STATE_SHOWING),'Browser choice is not visible')
# Keep one virtual keyboard alive across popup creation and activation. Creating
# a new keyboard after opening the popup can reset focus on wlroots compositors.
command('wtype','-k','Escape','-s','100','-k','space','-s','100','-k','Down','-s','100','-k','Return','-s','200')
wait(lambda:(Gio.AppInfo.get_default_for_type('x-scheme-handler/http',False).get_id()=='uflinux-test-B.desktop'),'The browser selection was not saved through GIO')
assert Gio.AppInfo.get_default_for_type('x-scheme-handler/https',False).get_id()=='uflinux-test-B.desktop'
assert Gio.AppInfo.get_default_for_type('application/x-uflinux-unrelated',False).get_id()=='uflinux-test-A.desktop'
print('GTK display drag/numeric synchronization, apply/revert and default-app association preservation passed')
