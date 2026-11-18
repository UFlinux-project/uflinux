# Start menu

Click the leaf on the bottom taskbar or release either Super key to toggle the
launcher. Super combinations still work; Super+R and Super+Space also toggle the Start menu.
The Start menu runs in the resident GTK4 `uflinux-desktop` process, with
Papirus-Dark icons and the selected GTK theme. Login starts it hidden with its default
widgets, layout and application catalog prepared. It retains these between
openings; GTK and the compositor draw buffers when the window is mapped.

The left column contains Files, Terminal, Settings and the default web browser
when installed. All Applications opens the alphabetical catalog. Typing in
Search shows indexed applications first, then matching file and folder names
from the file manager's configured search roots. Local application matches
supplement the index when localized names or keywords differ, and remain
available if the search service is absent. Applications launch through GIO
desktop-entry handling. Files and folders open in their default applications.

The right column contains the account name, Home and configured XDG folders,
Settings and Terminal. Unavailable folders have an explanation. `xdg-user-dirs`
initializes standard folders during login and respects existing configuration.
Favorites are fixed; there is no tracking of recent files or applications.

Log out, Restart and Shut down require confirmation with Cancel selected by default.
Suspend is immediate. Unavailable elogind actions are disabled. Authorization
errors appear inside the menu. Power-button policy remains in Settings.

Search receives focus on opening. Arrow keys navigate results and Enter
opens the selection. Escape first cancels confirmation, then clears search,
returns from All Applications and finally closes the menu. Outside clicks close
it immediately. Start and the Network, Sound and Power menus replace each other while a full Settings
window can remain open.

`uflinux-desktop --start-menu [--output NAME]` opens or toggles the menu on the
specified monitor. With no monitor, labwc selects the keyboard activation output.
Panel buttons supply their originating monitor. The transparent
layer captures outside clicks and the Start button while leaving other taskbar
controls usable. Output removal closes the menu. The card adapts to smaller
outputs and compositor scaling.

The desktop connects to the Settings state service and provides the session
polkit agent. Settings shortcuts open `uflinux-settings` in a separate process.
The native desktop harness exercises GTK controls, state and tray fixtures in a
private labwc session. See the [validation report](desktop-prototype-validation.md)
for measurements and remaining release checks.

`uflinux-desktop --daemon` starts or ensures the hidden resident process.
`--hide` dismisses it, `--status` reports readiness and visibility, and `--quit`
ends it explicitly. Closing the menu resets search, selection and confirmations;
it does not destroy the catalog or widgets. Desktop-file changes refresh the
catalog while hidden, and monitor removal dismisses the menu. Resident memory
is part of the desktop's background cost; the old nonresident measurements in
the validation record do not establish this version's memory or latency.

The taskbar has separate Network, Sound and Power buttons with Papirus-Dark symbolic
icons. They track connection, output volume and battery state. Sound includes
input/output and application controls; Power includes battery information,
brightness and power profiles. Network and Sound are tabbed — Wi-Fi, Ethernet,
Bluetooth and VPN, or Output and Input — inside one fixed frame, so switching
tabs does not resize the flyout. The corresponding Settings page provides advanced
controls. Open a panel directly with `uflinux-desktop --panel network|sound|power
--output NAME`. The settings service reads system batteries and updates the panel
after device changes. Desktop systems use a bolt icon.

Beside the Start button, application buttons come from the pinned favourites in
`$XDG_CONFIG_HOME/uflinux/taskbar.json` plus the running-window groups. Right
click opens a menu listing the group's windows with a close button each, and,
for applications, entries to launch another window, pin or unpin the button and
close all windows. Resting the pointer on a running app opens thumbnail cards
with activation and close controls. Settings → Appearance → Window previews
controls both these images and labwc's native Alt+Tab thumbnail grid; it defaults
to enabled. With previews disabled, hover cards retain window titles and Alt+Tab
uses the classic list. Pinned buttons can be rearranged by dragging one onto another; the
new order is written back to the same file, and an unreadable file falls back
to the defaults rather than blocking the panel. The clock opens a popover with a
month-navigable calendar and a notification centre that reads Mako's list while
the popover is open and dismisses notifications individually or together. Mako
itself is styled as dark rounded cards grouped by summary, anchored bottom-right
above the panel.

Taskbar capture runs only while cards are visible, at up to two frames per second
per window and four frames per second in total. Images are at most 320 × 180;
the cache holds at most 64 images (under 16 MiB), expiring after 30 seconds.
One source buffer at a time is limited to 16 megapixels / 64 MiB. Images stay in
memory. Unsupported capture, oversized sources, and ambiguous app-ID/title pairs
fall back to icons and titles. Minimized windows may show a cached image or the
fallback. Alt+Tab rendering is handled directly by labwc.

StatusNotifier icons sit behind a single chevron at the right end of the taskbar,
like the hidden-icons flyout on Windows, so background apps never push the clock
off a narrow panel. The chevron is hidden while no item is registered and its
tooltip and accessible name report how many icons are behind it. The flyout is a
grid of square icons above the panel, four to eight columns wide depending on the
output, and scrolls once it grows taller than the flyout allows. Left click activates
an item, or opens its menu when it declares `ItemIsMenu`; right click always opens
its menu; scrolling over an icon sends its `Scroll` event. Only one tray menu is
open at a time, and the flyout stays open while one of its menus is up.
