"""Static wiring checks for the printing/scanning stack and its toggle.

Everything here is a repository-level guarantee: the stack ships installed but
inert, one Settings page owns it, and the Document Scanner shortcut is hidden by
default. The runtime behaviour is covered by tests/settings-printing-protocol.py
and the installed-image harness.
"""
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]
UFLINUX = ROOT / 'aports/uflinux'
SETTINGS = ROOT / 'aports/uflinux-settings'


def read(path):
    return path.read_text(encoding='utf-8')


class PageTests(unittest.TestCase):
    """The Printing page has to be reachable and covered by the tests."""

    def test_page_is_registered(self):
        self.assertIn('("printing", "Printing")', read(ROOT / 'settings/ui/src/app.rs'))
        self.assertIn('"printing" => printing::page(app, parent)',
                      read(ROOT / 'settings/ui/src/pages/mod.rs'))

    def test_smoke_and_vm_tests_cover_the_page(self):
        self.assertIn('printing', read(ROOT / 'tests/settings-smoke.sh'))
        self.assertIn("'printing'", read(ROOT / 'tests/settings-installed.py'))

    def test_documentation_lists_the_page(self):
        self.assertIn('printing', read(ROOT / 'docs/settings.md'))
        self.assertIn('printing', read(ROOT / 'settings/README.md'))


class WireTests(unittest.TestCase):
    """The surface, its verbs and its authorization have to agree."""

    def setUp(self):
        self.model = read(ROOT / 'settings/core/src/model.rs')

    def test_the_snapshot_carries_a_printing_surface(self):
        self.assertIn('pub printing: Printing', self.model)
        # Older daemons publish no printing key, so it must default rather than
        # make the whole snapshot fail to parse.
        self.assertIn('#[serde(default)]\n    pub printing: Printing', self.model)

    def test_the_verbs_use_the_printing_names(self):
        # The Action enum is kebab-case renamed, so these variants travel under
        # exactly these wire names.
        self.assertIn('"action":"printing-status"', self.model)
        self.assertIn('"action":"printing-system"', self.model)
        self.assertIn('printing: PrintingSystemAction,', self.model)

    def test_the_daemon_probes_and_dispatches(self):
        daemon = read(ROOT / 'settings/daemon/src/main.rs')
        session = read(ROOT / 'settings/daemon/src/session.rs')
        self.assertIn('mod printing;', daemon)
        self.assertIn('refresh_printing', session)
        self.assertIn('Action::PrintingStatus', session)
        self.assertIn('Action::PrintingSystem { operation }', session)

    def test_only_the_read_only_verb_skips_authorization(self):
        operations = read(ROOT / 'settings/system/src/operations.rs')
        arm = operations.split('SystemAction::Printing { printing } =>')[1]
        body = arm.split('_ => "org.uflinux.settings.datetime"')[0]
        self.assertIn('PrintingSystemAction::Status => return Ok(())', body)
        self.assertIn('"org.uflinux.settings.printing"', body)
        self.assertIn('crate::printing::execute(printing)', operations)

    def test_the_polkit_action_is_shipped(self):
        policy = read(ROOT / 'settings/data/org.uflinux.settings.policy')
        self.assertIn('<action id="org.uflinux.settings.printing">', policy)
        action = policy.split('<action id="org.uflinux.settings.printing">')[1]
        action = action.split('</action>')[0]
        # An active local session keeps the credential for a while, so turning
        # the stack on and configuring a printer does not prompt twice.
        self.assertIn('<allow_active>auth_admin_keep</allow_active>', action)
        # Nothing may authorize a remote or non-local session.
        self.assertIn('<allow_any>no</allow_any>', action)
        self.assertIn('<allow_inactive>no</allow_inactive>', action)


class PackagingTests(unittest.TestCase):
    """The stack is installed by default but starts nothing."""

    def setUp(self):
        self.apkbuild = read(UFLINUX / 'APKBUILD')

    def test_desktop_meta_pulls_in_the_printing_stack(self):
        desktop = self.apkbuild.split('desktop() {')[1]
        for package in ('cups', 'cups-client', 'cups-filters', 'avahi', 'avahi-libs',
                        'sane-backends', 'sane-utils', 'sane-airscan', 'sane-udev',
                        'simple-scan'):
            with self.subTest(package=package):
                self.assertIn(package, desktop.split())

    def test_driver_packages_are_not_shipped(self):
        # Vendor drivers install on demand; IPP Everywhere covers the rest.
        desktop = self.apkbuild.split('desktop() {')[1]
        for package in ('hplip', 'gutenprint'):
            with self.subTest(package=package):
                self.assertNotIn(package, desktop.split())

    def test_the_visibility_helper_is_shipped(self):
        self.assertIn('uflinux-printing-sync', self.apkbuild.split('build()')[0])
        self.assertIn('install -Dm755 "$srcdir/uflinux-printing-sync" '
                      '"$pkgdir/usr/libexec/uflinux-printing-sync"', self.apkbuild)
        self.assertIn('usr/libexec/uflinux-printing-sync',
                      self.apkbuild.split('defaults() {')[1])

    def test_the_helper_runs_from_the_session(self):
        autostart = read(UFLINUX / 'uflinux-autostart')
        self.assertIn('/usr/libexec/uflinux-printing-sync &', autostart)

    def test_the_helper_hides_the_scanner_until_the_stack_is_on(self):
        script = read(UFLINUX / 'uflinux-printing-sync')
        # The same state the daemon reads: runlevel membership is a symlink.
        self.assertIn('/etc/runlevels/default/cupsd', script)
        self.assertIn('org.gnome.SimpleScan.desktop', script)
        self.assertIn('NoDisplay=true', script)

    def test_the_installer_starts_no_printing_service(self):
        # The runlevel table is the "disabled by default" guarantee.
        installer = read(UFLINUX / 'uflinux-install')
        table = installer.split('[("devfs", "sysinit")')[1].split(']:')[0]
        for service in ('cupsd', 'avahi-daemon', 'cups-browsed'):
            with self.subTest(service=service):
                self.assertNotIn(service, table)

    def test_the_installer_grants_device_groups_conditionally(self):
        installer = read(UFLINUX / 'uflinux-install')
        self.assertIn('for group in ("lp", "scanner"):', installer)
        # A missing group must not fail the install, so the probe cannot use the
        # checking helper.
        block = installer.split('for group in ("lp", "scanner"):')[1]
        self.assertIn('probe = subprocess.run(', block)
        self.assertIn('if probe.returncode == 0:', block)

    def test_the_live_image_joins_the_device_groups(self):
        script = read(UFLINUX / 'uflinux-image-user')
        self.assertIn('for group in wheel audio video input netdev render lp scanner; do',
                      script)


if __name__ == '__main__':
    unittest.main()
