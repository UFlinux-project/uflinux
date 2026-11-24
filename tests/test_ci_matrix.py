"""Keep the full architecture gate and release-secret eligibility independent."""
import importlib.util
from pathlib import Path
import unittest
import sys
path=Path(__file__).resolve().parents[1]/'scripts/ci-matrix.py'
sys.path.insert(0, str(path.parent))
spec=importlib.util.spec_from_file_location('ci_matrix',path)
ci=importlib.util.module_from_spec(spec);spec.loader.exec_module(ci)
class MatrixTests(unittest.TestCase):
    def test_main_and_prs_smoke_without_release_secrets(self):
        for event,ref in [('push','refs/heads/main'),('pull_request','refs/tags/v0.1.0')]:
            result=ci.configuration(event,ref)
            self.assertEqual([m['arch'] for m in result['matrix']['include']],['x86_64'])
            self.assertEqual(result['release'],'false')
    def test_main_and_prs_boot_only_x86_64_images(self):
        for event,ref in [('push','refs/heads/main'),('pull_request','refs/tags/v0.1.0')]:
            modes={}
            for job in ci.configuration(event,ref)['vm']['include']:
                modes.setdefault(job['arch'],set()).add(job['mode'])
            self.assertEqual(modes,{'x86_64':{'bios','uefi'}})
    def test_manual_runs_cover_all_architectures_without_release_secrets(self):
        result=ci.configuration('workflow_dispatch','refs/heads/main',full=True)
        self.assertEqual({job['arch'] for job in result['matrix']['include']},
                         {'x86_64', 'aarch64', 'ppc64le', 'riscv64'})
        self.assertEqual(result['release'],'false')
    def test_default_manual_run_smokes_x86_64_only(self):
        result=ci.configuration('workflow_dispatch','refs/heads/main')
        self.assertEqual([m['arch'] for m in result['matrix']['include']],['x86_64'])
        self.assertEqual(result['release'],'false')
    def test_only_valid_pushed_version_tags_enable_release(self):
        result=ci.configuration('push','refs/tags/v0.1.0')
        self.assertEqual({job['arch'] for job in result['matrix']['include']},
                         {'x86_64', 'aarch64', 'ppc64le', 'riscv64'})
        self.assertEqual(result['release'],'true')
        for tag in ('vlatest','v0.1.0;id','v0.1.0\nmalformed'):
            with self.assertRaises(ValueError):ci.configuration('push','refs/tags/'+tag)

    def test_settings_pgo_is_limited_to_native_release_shards(self):
        for event, ref, full, expected in (
            ('push', 'refs/tags/v0.1.0', False, {'x86_64', 'aarch64'}),
            ('push', 'refs/heads/main', False, set()),
            ('workflow_dispatch', 'refs/tags/v0.1.0', True, set()),
        ):
            optimized = ci.configuration(event, ref, full)['packages']['include']
            self.assertEqual(
                {(job['arch'], job['package']) for job in optimized if job['settings_pgo']},
                {(arch, 'uflinux-settings') for arch in expected},
            )

    def test_rds_pgo_is_limited_to_native_release_shards(self):
        for event, ref, full, expected in (
            ('push', 'refs/tags/v0.1.0', False, {'x86_64', 'aarch64'}),
            ('push', 'refs/heads/main', False, set()),
            ('workflow_dispatch', 'refs/tags/v0.1.0', True, set()),
        ):
            optimized = ci.configuration(event, ref, full)['packages']['include']
            self.assertEqual(
                {(job['arch'], job['package']) for job in optimized if job['rds_pgo']},
                {(arch, 'uflinux-rds') for arch in expected},
            )
            # The two optimized crates never share a shard, so a job can only
            # ever replay one profile set.
            self.assertFalse([job for job in optimized if job['settings_pgo'] and job['rds_pgo']])

    def test_full_matrix_covers_each_boot_platform(self):
        expected = {("x86_64", "bios"), ("x86_64", "uefi"), ("aarch64", "uefi"),
                    ("ppc64le", "native"), ("riscv64", "uefi")}
        for event, ref, full in (("workflow_dispatch", "refs/heads/main", True),
                                 ("push", "refs/tags/v0.1.0", False)):
            jobs = ci.configuration(event, ref, full)["vm"]["include"]
            self.assertEqual({(job["arch"], job["mode"]) for job in jobs}, expected)

    def test_device_images_are_supplementary_outside_releases(self):
        for event,ref,full,devices in [('push','refs/heads/main',False,False),
                                      ('pull_request','refs/heads/main',False,True),
                                      ('workflow_dispatch','refs/heads/main',True,False)]:
            result=ci.configuration(event,ref,full,devices)
            self.assertEqual(result['devices']['include'],[])
            self.assertEqual(result['devices_enabled'],'false')
    def test_manual_full_run_can_build_device_images_without_releasing(self):
        result=ci.configuration('workflow_dispatch','refs/heads/main',True,True)
        self.assertEqual(result['release'],'false')
        self.assertEqual(result['devices_enabled'],'true')
        self.assertEqual(len(result['devices']['include']),7)
    def test_release_tags_build_every_device_and_label_experimental_targets(self):
        result=ci.configuration('push','refs/tags/v0.1.0')
        self.assertEqual(result['devices_enabled'],'true')
        jobs=result['devices']['include']
        self.assertEqual(sorted((job['device'],job['arch']) for job in jobs),
                         [('pc-f2fs','x86_64'),('pc-xfs','x86_64'),
                          ('qcs6490','aarch64'),('rpi','aarch64'),('spacemit-k1','riscv64'),
                          ('vm','aarch64'),('vm','x86_64')])
        self.assertEqual({job['device'] for job in jobs if job['experimental']=='true'},
                         {'qcs6490','spacemit-k1'})
        for job in jobs:
            self.assertEqual(job['runner'],ci.arches[job['arch']]['runner'])

    def test_native_architectures_compile_with_more_cargo_jobs_than_emulated_ones(self):
        # Native runners have spare cores; the QEMU-emulated ones do not, and an
        # over-subscribed emulator is slower than a serialized build.
        expected={'x86_64':4,'aarch64':4,'ppc64le':2,'riscv64':2}
        self.assertEqual({arch:config['jobs'] for arch,config in ci.arches.items()},expected)
        for full,selected in ((False,{'x86_64'}),(True,set(expected))):
            result=ci.configuration('workflow_dispatch','refs/heads/main',full)
            self.assertEqual({job['arch']:job['cargo_jobs'] for job in result['matrix']['include']},
                             {arch:expected[arch] for arch in selected})
            self.assertEqual({job['arch'] for job in result['packages']['include']},selected)
            for job in result['packages']['include']:
                self.assertEqual(job['cargo_jobs'],expected[job['arch']])

    def test_each_architecture_has_nine_independent_jobs(self):
        for event,ref,full,count in [('pull_request','refs/heads/main',False,9),
                                     ('workflow_dispatch','refs/heads/main',True,36)]:
            result=ci.configuration(event,ref,full)
            jobs=result['packages']['include']
            self.assertEqual(len(jobs),count)
            for architecture in result['matrix']['include']:
                selected=[job for job in jobs if job['arch']==architecture['arch']]
                self.assertEqual({job['package'] for job in selected},{'musl','connman','gpu-screen-recorder','simp1e-cursors','uflinux-regreet','uflinux-rds','uflinux-settings','uflinux','xdg-desktop-portal-gtk4'})
                self.assertTrue(all(job['runner']==architecture['runner'] for job in selected))
