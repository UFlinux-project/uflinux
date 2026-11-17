"""Exercise renderer startup and logout behavior without touching graphics hardware."""
import json
import os
import shlex
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
import tomllib

WRAPPER=Path(__file__).resolve().parents[1]/'aports/uflinux/uflinux-compositor'
class RendererTests(unittest.TestCase):
    def run_wrapper(self, **values):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);record=root/'attempts'
            probe=root/'uflinux-settingsctl'
            probe.write_text('#!/bin/sh\nprintf "%s\\n" "$TEST_RENDERER"\n');probe.chmod(0o755)
            compositor=root/'labwc'
            compositor.write_text('#!'+sys.executable+'\n'+'''import json,os,sys
from pathlib import Path
with Path(os.environ['TEST_RECORD']).open('a') as output:
    output.write(json.dumps([os.environ.get('WLR_RENDERER'),os.environ.get('GSK_RENDERER')])+'\\n')
if os.environ.get('WLR_LIBINPUT_NO_DEVICES') != os.environ.get('TEST_INPUT_POLICY', '1'):
    sys.exit(99)
if os.environ.get('TEST_READY'):
    (Path(os.environ['XDG_RUNTIME_DIR'])/'uflinux-compositor-ready').touch()
sys.exit(0 if os.environ.get('WLR_RENDERER')==os.environ.get('TEST_SUCCEED') else 1)
''');compositor.chmod(0o755)
            environment=dict(os.environ,PATH=str(root)+os.pathsep+os.environ['PATH'],XDG_RUNTIME_DIR=str(root),
                TEST_RECORD=str(record),TEST_RENDERER='vulkan',TEST_SUCCEED='pixman')
            for key in ('WLR_RENDERER','GSK_RENDERER','WLR_LIBINPUT_NO_DEVICES'):environment.pop(key,None)
            environment.update(values)
            result=subprocess.run(['sh',str(WRAPPER)],env=environment,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
            return result.returncode,[json.loads(l) for l in record.read_text().splitlines()]
    def test_startup_fallback_is_bounded_and_uses_cairo_for_software(self):
        code,attempts=self.run_wrapper()
        self.assertEqual(code,0);self.assertEqual(attempts,[['vulkan',None],['gles2',None],['pixman','cairo']])
    def test_opengl_selection_falls_back_to_software(self):
        code,attempts=self.run_wrapper(TEST_RENDERER='gles2')
        self.assertEqual(code,0);self.assertEqual(attempts,[['gles2',None],['pixman','cairo']])
    def test_normal_logout_does_not_retry(self):
        code,attempts=self.run_wrapper(TEST_SUCCEED='vulkan')
        self.assertEqual(code,0);self.assertEqual(len(attempts),1)
    def test_crash_after_desktop_start_does_not_restart_session(self):
        code,attempts=self.run_wrapper(TEST_READY='1')
        self.assertEqual(code,1);self.assertEqual(len(attempts),1)
    def test_explicit_renderer_does_not_fallback(self):
        code,attempts=self.run_wrapper(WLR_RENDERER='gles2')
        self.assertEqual(code,1);self.assertEqual(attempts,[['gles2',None]])
    def test_explicit_gtk_renderer_survives_software_fallback(self):
        code,attempts=self.run_wrapper(TEST_RENDERER='pixman',GSK_RENDERER='gl')
        self.assertEqual(code,0);self.assertEqual(attempts,[['pixman','gl']])

    def test_explicit_input_policy_is_preserved(self):
        code, attempts = self.run_wrapper(WLR_LIBINPUT_NO_DEVICES='0', TEST_INPUT_POLICY='0')
        self.assertEqual(code, 0)


class GreeterInputTests(unittest.TestCase):
    def test_live_and_installed_greeters_allow_late_input_devices(self):
        for name in ('greetd-installed.toml', 'greetd-live.toml'):
            with self.subTest(config=name), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                launcher = root / 'dbus-run-session'
                launcher.write_text('#!' + sys.executable + '\n' + '''import json, os, sys
print(json.dumps([os.environ.get('WLR_LIBINPUT_NO_DEVICES'), sys.argv[1:]]))
''')
                launcher.chmod(0o755)
                config = tomllib.loads((WRAPPER.parent / name).read_text())
                command = shlex.split(config['default_session']['command'])
                result = subprocess.check_output(command, text=True,
                    env=dict(os.environ, PATH=str(root) + os.pathsep + os.environ['PATH']))
                policy, args = json.loads(result)
                self.assertEqual(policy, '1')
                self.assertEqual(args[0], 'cage')
                self.assertEqual(args[-1], 'regreet')

class LegacyIntelTests(unittest.TestCase):
    def test_pci_classification(self):
        source = Path(__file__).resolve().parents[1] / 'settings/daemon/src'
        with tempfile.TemporaryDirectory() as directory:
            program = Path(directory) / 'test.c'
            program.write_text('''#include "renderer_ids.h"
int main(void) {
    return !(uflinux_legacy_intel_gpu(0x8086, 0x0166) &&
             uflinux_legacy_intel_gpu(0x8086, 0x0f31) &&
             uflinux_legacy_intel_gpu(0x8086, 0x0126) &&
             !uflinux_legacy_intel_gpu(0x8086, 0x0412) &&
             !uflinux_legacy_intel_gpu(0x1002, 0x0166));
}
''')
            binary = Path(directory) / 'test'
            subprocess.run(['cc', '-Wall', '-Werror', '-I', str(source), str(program), '-o', str(binary)], check=True)
            subprocess.run([str(binary)], check=True)
