"""Keep the polkit agent compiled once, inside the settings UI library.

`ui/src/polkit-agent.c` used to be compiled a third time by a duplicate
`start-menu` crate and a second time by `desktop`'s own build script. Both
extra copies are gone: `desktop` links the static library that `ui`'s build
script produces, and the launcher sources live in `start-menu/src` only as
`#[path]` modules of `desktop`. These assertions fail closed if either copy
comes back, because the duplication costs a C compile and a link per extra
crate on every architecture.
"""
from pathlib import Path
import tomllib
import unittest

ROOT = Path(__file__).resolve().parents[1]
SETTINGS = ROOT / 'settings'
UI_BUILD = SETTINGS / 'ui/build.rs'
DESKTOP_MANIFEST = SETTINGS / 'desktop/Cargo.toml'
DESKTOP_MAIN = SETTINGS / 'desktop/src/main.rs'
UI_MAIN = SETTINGS / 'ui/src/main.rs'
LAUNCHER = SETTINGS / 'start-menu/src'


class PolkitAgentBuildTests(unittest.TestCase):
    def test_only_the_ui_library_compiles_the_agent(self):
        builders = sorted(p.relative_to(SETTINGS).as_posix()
                          for p in SETTINGS.glob('*/build.rs')
                          if 'polkit-agent.c' in p.read_text())
        self.assertEqual(builders, ['ui/build.rs'],
                         'the polkit agent must be compiled by exactly one build script')

    def test_the_desktop_links_the_agent_without_its_own_compiler(self):
        manifest = tomllib.loads(DESKTOP_MANIFEST.read_text())
        self.assertNotIn('build-dependencies', manifest,
                         'desktop no longer compiles C, so it must not carry cc or pkg-config')
        self.assertTrue((SETTINGS / 'ui/src/polkit-agent.c').is_file())

    def test_the_launcher_stays_a_module_of_the_desktop(self):
        self.assertFalse((SETTINGS / 'start-menu/Cargo.toml').exists(),
                         'start-menu is not a crate of its own anymore')
        self.assertFalse((SETTINGS / 'install-core').exists(),
                         'the unused install-core library was removed')
        for name in ('catalog.rs', 'menu.rs', 'widgets.rs'):
            self.assertTrue((LAUNCHER / name).is_file())
        main = DESKTOP_MAIN.read_text()
        for name in ('catalog.rs', 'menu.rs', 'widgets.rs'):
            self.assertIn(f'../../start-menu/src/{name}', main,
                          f'desktop must still build {name} into the panel binary')

    def test_the_legacy_start_menu_command_reaches_the_desktop(self):
        body = UI_MAIN.read_text()
        self.assertIn('"--start-menu"', body)
        self.assertIn('Command::new("uflinux-desktop")', body,
                      'key bindings for uflinux-start-menu must be handed to the desktop binary')
        self.assertNotIn('uflinux-start-menu', body,
                         'no packaged binary provides uflinux-start-menu anymore')


if __name__ == '__main__':
    unittest.main()
