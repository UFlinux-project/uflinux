"""Exercise image credentials with real Alpine account tools in a user namespace.

Set UFLINUX_TEST_ALPINE_ROOT to an unpacked x86_64 Alpine build root containing
sudo. No host account files are writable inside the test sandbox.
"""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


@unittest.skipUnless(os.environ.get("UFLINUX_TEST_ALPINE_ROOT")
                     and shutil.which("bwrap") and shutil.which("unshare"),
                     "requires an Alpine root and user-namespace sandbox tools")
class ImageUserTests(unittest.TestCase):
    def test_login_credentials_groups_privileges_and_password_preservation(self):
        alpine = Path(os.environ["UFLINUX_TEST_ALPINE_ROOT"]).resolve()
        with tempfile.TemporaryDirectory() as directory:
            stage = Path(directory)
            etc, home = stage / "etc", stage / "home"
            etc.mkdir()
            home.mkdir()
            for name in ("passwd", "group", "shadow", "gshadow", "login.defs",
                         "sudo.conf", "sudoers", "nsswitch.conf"):
                if (alpine / "etc" / name).is_file():
                    shutil.copyfile(alpine / "etc" / name, etc / name)
            (etc / "shadow").chmod(0o600)
            (etc / "sudoers").chmod(0o440)
            script = r'''
set -eu
trap 'rm -rf /home/*' EXIT
sh /tmp/image-user
id user
test -d /home/user
test "$(awk -F: '$1 == "user" { print $7 }' /etc/passwd)" = /bin/ash
hash=$(awk -F: '$1 == "user" { print $2 }' /etc/shadow)
salt=$(printf "%s" "$hash" | cut -d '$' -f3)
test "$(busybox cryptpw -m sha512 -S "$salt" user)" = "$hash"
for group in wheel audio video input netdev; do
    id -nG user | tr ' ' '\n' | grep -qx "$group"
done
test "$(stat -c %a /etc/sudoers.d/uflinux-user)" = 440
test "$(stat -c %a /etc/doas.d/uflinux-user.conf)" = 600
grep -qxF 'user ALL=(ALL:ALL) ALL' /etc/sudoers.d/uflinux-user
grep -qxF 'permit persist user as root' /etc/doas.d/uflinux-user.conf
visudo -cf /etc/sudoers
printf 'user:changed-password\n' | chpasswd
before=$(awk -F: '$1 == "user" { print $2 }' /etc/shadow)
sh /tmp/image-user
after=$(awk -F: '$1 == "user" { print $2 }' /etc/shadow)
test "$before" = "$after"
'''
            subprocess.run([
                "unshare", "--user", "--map-auto", "--map-root-user",
                "bwrap", "--ro-bind", str(alpine), "/",
                "--bind", str(etc), "/etc", "--bind", str(home), "/home",
                "--dev", "/dev", "--proc", "/proc", "--tmpfs", "/tmp",
                "--ro-bind", str(ROOT / "aports/uflinux/uflinux-image-user"), "/tmp/image-user",
                "--setenv", "PATH", "/usr/sbin:/sbin:/usr/bin:/bin",
                "--chdir", "/", "/bin/sh", "-c", script,
            ], check=True, timeout=30)


if __name__ == "__main__":
    unittest.main()
