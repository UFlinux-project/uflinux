"""Fail closed when independent package jobs are missing, mixed or modified."""
import importlib.util
import io
import json
import os
import re
import subprocess
from pathlib import Path
import shutil
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import Mock, patch
import urllib.error

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
import builder
from test_build_inputs import stream

spec = importlib.util.spec_from_file_location('merge_packages', builder.PROJECT / 'scripts/merge-packages.py')
merge = importlib.util.module_from_spec(spec)
spec.loader.exec_module(merge)
spec = importlib.util.spec_from_file_location('reuse_packages', builder.PROJECT / 'scripts/reuse-packages.py')
reuse = importlib.util.module_from_spec(spec)
spec.loader.exec_module(reuse)


class FixtureMixin:
    def fixture(self, out):
        for origin, names in builder.PACKAGE_NAMES.items():
            root = out / 'package-shards/x86_64' / origin
            packages = root / 'packages/x86_64'
            packages.mkdir(parents=True)
            upstream = builder.package_version(origin, '0.1.0')
            revision = builder.package_revision(origin)
            version = upstream + '-r' + revision
            records, payloads = {}, {}
            for name in names:
                path = packages / (name + '-' + version + '.apk')
                info = f'pkgname = {name}\npkgver = {version}\norigin = {origin}\narch = x86_64\n'.encode()
                path.write_bytes(stream('.SIGN.' + builder.SIGN_STREAM + '.' + builder.SIGNING_KEY + '.pub', b'sig') + stream('.PKGINFO', info) + stream('usr/share/example', b'data'))
                records[str(path.relative_to(root))] = {'sha256': builder.sha(path)}
                payloads[path.name] = builder.unsigned_digest(path)
            (packages / builder.BUILD_KEY).write_text('test key for ' + origin)
            metadata = root / 'metadata/x86_64'
            metadata.mkdir(parents=True)
            record = {'arch': 'x86_64', 'version': '0.1.0', 'package': origin, 'sources': builder.LOCK,
                      'source_commit': 'same-commit', 'source_date_epoch': 123, 'repository_url': 'https://example.org/apk', 'files': records}
            (metadata / 'compile.json').write_text(json.dumps(record))
            (metadata / 'reproducibility.json').write_text(json.dumps({'fresh_compiler_target': True, 'matched': True, 'baseline': payloads, 'rebuilt': payloads}))
            inputs = root / 'inputs/x86_64'
            inputs.mkdir(parents=True)
            archives = ['build-apks.tar.gz', 'sources.tar.gz', 'cargo-sources.tar.gz']
            if origin == 'musl':
                archives.append('musl-bolt-profiles.tar.gz')
            if origin in builder.CLANG_APORTS:
                archives.append(builder.CLANG_APORT_PROFILES)
            for name in archives:
                (inputs / name).write_bytes(b'fixture retained archive')


class ShardTests(FixtureMixin, unittest.TestCase):
    def test_uflinux_recipe_packages_match_shard_allowlist(self):
        recipe = (builder.PROJECT / 'aports/uflinux/APKBUILD').read_text()
        subpackages = re.search(r'^subpackages="([^"]+)"', recipe, re.M).group(1)
        declared = {'uflinux'} | {entry.split(':', 1)[0] for entry in subpackages.split()}
        self.assertEqual(builder.PACKAGE_NAMES['uflinux'], declared)

    def test_complete_set_retains_independent_keys_inputs_and_proofs(self):
        with tempfile.TemporaryDirectory() as temporary:
            out = Path(temporary)
            self.fixture(out)
            merge.collect(out, 'x86_64', '0.1.0')
            packages = out / 'packages/x86_64'
            self.assertEqual(len(list(packages.glob('*.apk'))), sum(map(len, builder.PACKAGE_NAMES.values())))
            keys = json.loads((packages / 'build-keys.json').read_text())
            self.assertEqual(len(set(keys.values())), len(builder.PACKAGE_NAMES))
            proof = json.loads((out / 'metadata/x86_64/reproducibility.json').read_text())
            self.assertEqual(len(proof['baseline']), sum(map(len, builder.PACKAGE_NAMES.values())))
            self.assertEqual(proof['baseline'], proof['rebuilt'])
            self.assertTrue((out / 'inputs/x86_64/uflinux-settings/cargo-sources.tar.gz').exists())

    def test_invalid_inputs_never_replace_existing_signing_inputs(self):
        for mode in ('missing-shard', 'missing-apk', 'tampered-apk', 'mixed-commit', 'wrong-proof', 'missing-cache'):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as temporary:
                out = Path(temporary)
                self.fixture(out)
                shard = out / 'package-shards/x86_64/xdg-desktop-portal-gtk4'
                if mode == 'missing-shard':
                    shard.rename(shard.with_name('unexpected-origin'))
                elif mode == 'missing-apk':
                    next((shard / 'packages/x86_64').glob('*.apk')).unlink()
                elif mode == 'tampered-apk':
                    path = next((shard / 'packages/x86_64').glob('*.apk'))
                    path.write_bytes(path.read_bytes() + b'changed')
                elif mode == 'missing-cache':
                    (shard / 'inputs/x86_64/cargo-sources.tar.gz').unlink()
                else:
                    path = shard / 'metadata/x86_64' / ('compile.json' if mode == 'mixed-commit' else 'reproducibility.json')
                    record = json.loads(path.read_text())
                    record['source_commit' if mode == 'mixed-commit' else 'rebuilt'] = 'different'
                    path.write_text(json.dumps(record))
                destination = out / 'packages/x86_64'
                destination.mkdir(parents=True)
                (destination / 'sentinel').write_text('keep')
                with self.assertRaises((ValueError, EOFError)):
                    merge.collect(out, 'x86_64', '0.1.0')
                self.assertEqual((destination / 'sentinel').read_text(), 'keep')

    def test_package_outputs_are_isolated_and_serial_layout_is_unchanged(self):
        with tempfile.TemporaryDirectory() as temporary:
            self.assertEqual(builder.package_output(temporary, 'x86_64'), Path(temporary))
            self.assertNotEqual(builder.package_output(temporary, 'x86_64', 'connman'),
                                builder.package_output(temporary, 'x86_64', 'uflinux-settings'))


class FakeResponse:
    def __init__(self, payload):
        self.body = io.BytesIO(payload)
        self.status = 200

    def __enter__(self):
        return self

    def __exit__(self, *_args):
        return False

    def read(self, size=-1):
        return self.body.read(size)


class ReuseTests(ShardTests):
    """Published APKs may replace a compile job only as a complete origin set."""

    ARCH = 'x86_64'
    VERSION = '0.1.0'
    REPOSITORY = 'https://example.org/apk'

    def published_apk(self, filename, origin):
        pkgver = builder.package_version(origin, self.VERSION) + '-r' + builder.package_revision(origin)
        pkgname = filename[:-(len(pkgver) + len('.apk') + 1)]
        info = f'pkgname = {pkgname}\npkgver = {pkgver}\norigin = {origin}\narch = {self.ARCH}\n'.encode()
        return (stream('.SIGN.' + builder.SIGN_STREAM + '.' + builder.APPS_KEY, b'published signature')
                + stream('.PKGINFO', info) + stream('usr/share/doc/README', b'published payload'))

    def published_repository(self, origin, filenames, *, published=None, payload=None):
        listing = json.dumps([{'version': self.VERSION,
                               'packages': {self.ARCH: sorted(filenames if published is None else published)}}]).encode()

        def open_url(url, timeout):
            if url.endswith('/releases.json'):
                return FakeResponse(listing)
            name = url.rsplit('/', 1)[-1]
            if name in filenames:
                return FakeResponse(payload if payload is not None else self.published_apk(name, origin))
            raise urllib.error.HTTPError(url, 404, 'missing', None, None)

        return open_url

    def reuse_context(self, open_url):
        return patch.object(reuse.urllib.request, 'urlopen', side_effect=open_url)

    def run_reuse(self, out, origin, open_url, outputs=None):
        argv = ['reuse-packages.py', '--arch', self.ARCH, '--version', self.VERSION, '--outdir', str(out),
                '--repository-url', self.REPOSITORY, '--package', origin]
        env = {'GITHUB_SHA': 'same-commit', 'SOURCE_DATE_EPOCH': '123'}
        if outputs is not None:
            env['GITHUB_OUTPUT'] = str(outputs)
        with self.reuse_context(open_url), patch('sys.argv', argv), patch.dict(os.environ, env):
            reuse.main()
        return None if outputs is None else outputs.read_text()

    def test_the_compile_gate_skips_only_a_reused_origin(self):
        origin = 'uflinux-regreet'
        filenames = sorted(reuse.expected_files(origin, self.VERSION))
        with tempfile.TemporaryDirectory() as temporary:
            out, outputs = Path(temporary), Path(temporary) / 'output'
            self.fixture(out)
            shutil.rmtree(out / 'package-shards/x86_64' / origin)
            record = self.run_reuse(out, origin, self.published_repository(origin, filenames), outputs)
            self.assertIn('reused=true', record)
            self.assertIn('origins=' + origin, record)
            outputs.write_text('')
            # A profile origin is reusable too, so the gate skips it as well.
            musl = sorted(reuse.expected_files('musl', self.VERSION))
            self.assertIn('reused=true', self.run_reuse(out, 'musl', self.published_repository('musl', musl), outputs))
            outputs.write_text('')
            # An origin whose published set is incomplete must never report itself
            # as reused, otherwise its shard would be missing from the merge.
            self.assertIn('reused=false', self.run_reuse(out, 'musl', self.published_repository(origin, filenames), outputs))

    def test_a_reused_profile_origin_retains_empty_profile_inputs(self):
        # A job that did not compile generates no BOLT, PGO or Clang profile, but
        # the merge and the release asset set still expect the archive names.
        for origin, extra in (('musl', ('musl-bolt-profiles.tar.gz',)),
                              ('connman', ('clang-aport-profiles.tar.gz',)),
                              ('uflinux-settings', ('cargo-sources.tar.gz',))):
            with self.subTest(origin=origin), tempfile.TemporaryDirectory() as temporary:
                out = Path(temporary)
                filenames = sorted(reuse.expected_files(origin, self.VERSION))
                self.fixture(out)
                shutil.rmtree(out / 'package-shards/x86_64' / origin)
                self.run_reuse(out, origin, self.published_repository(origin, filenames))
                inputs = out / 'package-shards/x86_64' / origin / 'inputs/x86_64'
                self.assertEqual(sorted(path.name for path in inputs.iterdir()),
                                 sorted(('build-apks.tar.gz', 'sources.tar.gz') + extra))
                for path in inputs.iterdir():
                    with tarfile.open(path, 'r:gz') as archive:
                        self.assertEqual(archive.getnames(), ['.'])
                merge.collect(out, self.ARCH, self.VERSION)
                merged = json.loads((out / 'metadata/x86_64/compile.json').read_text())
                self.assertEqual(merged['reused'], [origin])

    def test_reuse_writes_a_shard_the_merge_accepts_without_a_rebuild_proof(self):
        origin = 'uflinux-regreet'
        filenames = sorted(reuse.expected_files(origin, self.VERSION))
        with tempfile.TemporaryDirectory() as temporary:
            out = Path(temporary)
            self.fixture(out)
            shutil.rmtree(out / 'package-shards/x86_64' / origin)
            self.run_reuse(out, origin, self.published_repository(origin, filenames))
            shard = out / 'package-shards/x86_64' / origin
            record = json.loads((shard / 'metadata/x86_64/compile.json').read_text())
            self.assertTrue(record['reused'])
            self.assertEqual(record['reused_from'], self.REPOSITORY)
            self.assertEqual(record['sources'], builder.LOCK)
            self.assertEqual(sorted(record['files']), sorted(
                ['packages/x86_64/' + name for name in filenames] +
                ['packages/x86_64/' + builder.BUILD_KEY] +
                ['inputs/x86_64/' + name for name in
                 ('build-apks.tar.gz', 'sources.tar.gz', 'cargo-sources.tar.gz')]))
            digests = set()
            for name in ('build-apks.tar.gz', 'sources.tar.gz', 'cargo-sources.tar.gz'):
                with tarfile.open(shard / 'inputs/x86_64' / name, 'r:gz') as archive:
                    # A reused shard cannot supply rebuild inputs, so it retains a
                    # deterministic empty archive that keeps the release asset set intact.
                    self.assertEqual(archive.getnames(), ['.'])
                digests.add(builder.sha(shard / 'inputs/x86_64' / name))
            self.assertEqual(len(digests), 1)
            merge.collect(out, self.ARCH, self.VERSION)
            merged = json.loads((out / 'metadata/x86_64/compile.json').read_text())
            self.assertEqual(merged['reused'], [origin])
            self.assertEqual(len(merged['files']), sum(map(len, builder.PACKAGE_NAMES.values())))
            self.assertIn('packages/x86_64/' + filenames[0], merged['files'])
            proof = json.loads((out / 'metadata/x86_64/reproducibility.json').read_text())
            self.assertEqual(proof['reused'], [origin])
            self.assertEqual(len(proof['baseline']), sum(map(len, builder.PACKAGE_NAMES.values())))

    def test_reuse_skips_an_origin_without_a_complete_published_set(self):
        origin = 'uflinux-regreet'
        filenames = sorted(reuse.expected_files(origin, self.VERSION))
        with tempfile.TemporaryDirectory() as temporary:
            out = Path(temporary)
            open_url = self.published_repository(origin, filenames, published=filenames[1:])
            with self.reuse_context(open_url):
                self.assertEqual(reuse.plan(self.REPOSITORY, self.ARCH, self.VERSION, (origin,)), {})
            self.run_reuse(out, origin, open_url)
            self.assertFalse((out / 'package-shards/x86_64' / origin).exists())

    def test_reuse_rejects_a_published_apk_that_is_not_the_expected_build(self):
        origin = 'uflinux-regreet'
        filenames = sorted(reuse.expected_files(origin, self.VERSION))
        wrong = stream('.SIGN.' + builder.SIGN_STREAM + '.' + builder.APPS_KEY, b'sig') + stream(
            '.PKGINFO', f'pkgname = {origin}\npkgver = 9.9.9-r9\norigin = {origin}\narch = {self.ARCH}\n'.encode()
        ) + stream('usr/share/doc/README', b'stale payload')
        with tempfile.TemporaryDirectory() as temporary:
            out = Path(temporary)
            self.run_reuse(out, origin, self.published_repository(origin, filenames, payload=wrong))
            self.assertFalse((out / 'package-shards/x86_64' / origin).exists())

    def test_merge_rejects_a_modified_reused_apk(self):
        origin = 'uflinux-regreet'
        with tempfile.TemporaryDirectory() as temporary:
            out = Path(temporary)
            self.fixture(out)
            shard = out / 'package-shards/x86_64' / origin
            record_path = shard / 'metadata/x86_64/compile.json'
            record = json.loads(record_path.read_text())
            record['reused'] = True
            record['reused_from'] = self.REPOSITORY
            record_path.write_text(json.dumps(record))
            (shard / 'metadata/x86_64' / 'reproducibility.json').unlink()
            version = builder.package_version(origin, self.VERSION) + '-r' + builder.package_revision(origin)
            (shard / 'packages/x86_64' / (origin + '-' + version + '.apk')).write_bytes(
                stream('.SIGN.' + builder.SIGN_STREAM + '.' + builder.APPS_KEY, b'sig')
                + stream('.PKGINFO', f'pkgname = {origin}\npkgver = {version}\norigin = {origin}\narch = {self.ARCH}\n'.encode())
                + stream('usr/share/example', b'tampered payload'))
            with self.assertRaisesRegex(ValueError, 'differs from compilation record'):
                merge.collect(out, self.ARCH, self.VERSION)


class InputArchiveTests(unittest.TestCase):
    def test_origin_archives_have_unique_release_asset_names(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary)
            directory=root/'out/inputs/x86_64'
            for origin in builder.PACKAGE_NAMES:
                target=directory/origin
                target.mkdir(parents=True)
                for name in ('build-apks.tar.gz','sources.tar.gz'):
                    (target/name).write_bytes(b'input')
                if origin == 'musl':
                    (target/'musl-bolt-profiles.tar.gz').write_bytes(b'profile')
                if origin in builder.CLANG_APORTS:
                    (target/builder.CLANG_APORT_PROFILES).write_bytes(b'profile')
                if origin in builder.CARGO_ORIGINS:
                    (target/'cargo-sources.tar.gz').write_bytes(b'cargo')
            (directory/'image-apks.tar.gz').write_bytes(b'image')
            command=[sys.executable,str(builder.PROJECT/'scripts/release-inputs.py'),'--arch','x86_64','--version','0.1.0']
            subprocess.run(command,cwd=root,check=True,capture_output=True)
            entries=(root/'out/SHA256SUMS-x86_64-inputs').read_text().splitlines()
            self.assertEqual(len(entries),26)
            self.assertEqual(len(list((root/'out').glob('uflinux-*.tar.gz'))),26)
            (directory/'uflinux-settings/cargo-sources.tar.gz').unlink()
            rejected=subprocess.run(command,cwd=root,capture_output=True)
            self.assertNotEqual(rejected.returncode,0)

    def test_asset_size_check_names_the_offending_file(self):
        with tempfile.TemporaryDirectory() as temporary:
            asset = Path(temporary) / 'uflinux-0.1.0-x86_64.iso'
            for size, expected in ((builder.RELEASE_ASSET_WARNING - 1, None),
                                   (builder.RELEASE_ASSET_WARNING, 'warning'),
                                   (builder.RELEASE_ASSET_LIMIT, 'error')):
                asset.write_bytes(b'')
                os.truncate(asset, size)
                with self.subTest(size=size):
                    if expected is None:
                        self.assertEqual(builder.check_asset_size(asset, 'Release ISO'), size)
                    elif expected == 'warning':
                        with patch('builtins.print') as reported:
                            self.assertEqual(builder.check_asset_size(asset, 'Release ISO'), size)
                        self.assertIn('::warning', reported.call_args.args[0])
                        self.assertIn(asset.name, reported.call_args.args[0])
                    else:
                        with self.assertRaises(ValueError) as raised:
                            builder.check_asset_size(asset, 'Release ISO')
                        self.assertIn(asset.name, str(raised.exception))
                        self.assertIn(str(size), str(raised.exception))

    def test_release_records_rejects_an_oversized_iso(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            iso = root / 'out/images/x86_64/uflinux-0.1.0-x86_64.iso'
            (root / 'out/metadata').mkdir(parents=True)
            (root / 'out/checks').mkdir()
            (root / 'out/vm').mkdir()
            iso.parent.mkdir(parents=True)
            iso.write_bytes(b'')
            os.truncate(iso, builder.RELEASE_ASSET_LIMIT)
            result = subprocess.run([sys.executable, str(builder.PROJECT / 'scripts/release-records.py'),
                                     '--version', '0.1.0'], cwd=root, capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('uflinux-0.1.0-x86_64.iso', result.stderr)
            self.assertFalse((root / 'out/SHA256SUMS').exists())


class EmulatedDependencyTests(unittest.TestCase):
    def test_dependencies_use_root_and_compilation_remains_unprivileged(self):
        instance = object.__new__(builder.Builder)
        instance.chroot = Mock()
        instance.build_origin('connman', {'CARGO_BUILD_JOBS': '1'})
        calls = instance.chroot.call_args_list
        self.assertEqual(calls[0].args, ('sh', '-ec', 'cd /project/aports/connman && abuild -F deps'))
        self.assertEqual(calls[0].kwargs['extra_env']['SUDO_APK'], 'apk')
        self.assertEqual(calls[1].args, ('su', '-s', '/bin/sh', 'builder', '-c', 'cd /project/aports/connman && abuild'))
        self.assertNotIn('SUDO_APK', calls[1].kwargs['extra_env'])
        self.assertTrue(calls[2].args[-1].endswith('abuild -F undeps'))

    def test_failed_compilation_still_cleans_dependencies_and_fails(self):
        instance = object.__new__(builder.Builder)
        instance.chroot = Mock(side_effect=[None, RuntimeError('compiler failed'), None])
        with self.assertRaisesRegex(RuntimeError, 'compiler failed'):
            instance.build_origin('uflinux-settings', {})
        self.assertTrue(instance.chroot.call_args_list[-1].args[-1].endswith('abuild -F undeps'))

    def test_shared_caches_stay_readable_to_the_host_runner(self):
        # Actions saves the restored cache as the host user, but abuild leaves the
        # files owned by uid 1000 with mode 640, which makes that save fail.
        for failed in (False, True):
            with self.subTest(failed=failed), patch.object(builder.Builder, 'build_origin') as build:
                instance = object.__new__(builder.Builder)
                instance.chroot = Mock()
                if failed:
                    build.side_effect = RuntimeError('compiler failed')
                    with self.assertRaisesRegex(RuntimeError, 'compiler failed'):
                        instance.build_origins(('uflinux-regreet',), {})
                else:
                    instance.build_origins(('uflinux-regreet', 'connman'), {})
                    self.assertEqual([call.args[0] for call in build.call_args_list],
                                     ['uflinux-regreet', 'connman'])
                self.assertEqual(instance.chroot.call_args.args,
                                 ('sh', '-ec', 'chmod -R a+rX /cache/cargo /cache/target'))
