"""Release identity, promotion and privacy policy; no network or publication."""
import copy
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import tomllib
import unittest

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / 'scripts/release_policy.py'
SHA = 'a' * 40


class ReleasePolicyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        spec = importlib.util.spec_from_file_location('release_policy', SCRIPT)
        cls.policy = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.policy)

    def successful_run(self, **changes):
        run = {'id': 10, 'run_attempt': 1, 'head_sha': SHA, 'head_branch': 'main',
               'event': 'push', 'status': 'completed', 'conclusion': 'success'}
        run.update(changes)
        return run

    def test_stable_and_prerelease_promotion_are_distinct(self):
        for version, prerelease, latest in [('0.0.5', False, True),
                                            ('0.1.0-rc.1', True, False),
                                            ('1.0.0-beta.2', True, False)]:
            result = self.policy.release_info('v' + version, version)
            self.assertEqual(result, {'version': version, 'prerelease': prerelease, 'latest': latest})

    def test_mismatched_or_unsafe_versions_cannot_be_published(self):
        for tag, version in [('v0.0.4', '0.0.5'), ('0.0.5', '0.0.5'),
                             ('v00.0.5', '00.0.5'), ('v1.0.0-rc.01', '1.0.0-rc.01'),
                             ('v1.0.0+build', '1.0.0+build'),
                             ('v1.0.0\nlatest=true', '1.0.0\nlatest=true')]:
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                self.policy.release_info(tag, version)

    def test_ci_requires_exact_successful_main_push(self):
        self.assertEqual(self.policy.require_ci({'workflow_runs': [self.successful_run()]}, SHA), 10)
        for change in [{'head_sha': 'b' * 40}, {'head_branch': 'feature'},
                       {'event': 'pull_request'}, {'event': 'workflow_dispatch'},
                       {'status': 'in_progress', 'conclusion': None},
                       {'conclusion': 'failure'}, {'conclusion': 'cancelled'}]:
            with self.subTest(change=change), self.assertRaises(ValueError):
                self.policy.require_ci({'workflow_runs': [self.successful_run(**change)]}, SHA)

    def test_newer_failed_ci_cannot_be_hidden_by_older_success(self):
        old = self.successful_run()
        newer = self.successful_run(id=11, conclusion='failure')
        with self.assertRaises(ValueError):
            self.policy.require_ci({'workflow_runs': [old, newer]}, SHA)

    def test_manifest_requires_both_real_linux_architectures(self):
        manifest = {'manifests': [{'platform': {'os': 'linux', 'architecture': arch}, 'digest': 'sha256:' + str(i) * 64}
                                 for i, arch in enumerate(['amd64', 'arm64'], 1)]}
        self.policy.require_platforms(manifest)
        for invalid in [{'manifests': manifest['manifests'][:1]},
                        {'manifests': manifest['manifests'] * 2},
                        {'manifests': [{'platform': {'os': 'windows', 'architecture': 'amd64'}}, manifest['manifests'][1]]}]:
            with self.assertRaises(ValueError):
                self.policy.require_platforms(invalid)

    def test_provenance_is_retained_and_must_reference_both_native_images(self):
        manifest = {'manifests': []}
        for i, arch in enumerate(['amd64', 'arm64'], 1):
            digest = 'sha256:' + str(i) * 64
            manifest['manifests'].append({'platform': {'os': 'linux', 'architecture': arch}, 'digest': digest})
            manifest['manifests'].append({'platform': {'os': 'unknown', 'architecture': 'unknown'},
                                         'digest': 'sha256:' + str(i + 2) * 64,
                                         'annotations': {'vnd.docker.reference.type': 'attestation-manifest',
                                                         'vnd.docker.reference.digest': digest}})
        self.policy.require_platforms(manifest, require_provenance=True)
        missing = copy.deepcopy(manifest);missing['manifests'].pop()
        with self.assertRaises(ValueError):self.policy.require_platforms(missing, require_provenance=True)
        wrong = copy.deepcopy(manifest)
        wrong['manifests'][-1]['annotations']['vnd.docker.reference.digest'] = 'sha256:' + '9' * 64
        with self.assertRaises(ValueError):self.policy.require_platforms(wrong, require_provenance=True)

    def test_private_package_gate_is_fail_closed(self):
        self.policy.require_private_package(200, {'visibility': 'private'}, allow_absent=False)
        self.policy.require_private_package(404, {'message': 'Not Found'}, allow_absent=True)
        for status, data, allow_absent in [(200, {'visibility': 'public'}, True),
                                           (200, {'visibility': 'internal'}, True),
                                           (403, {'message': 'Forbidden'}, True),
                                           (404, {'message': 'Not Found'}, False),
                                           (200, {}, True)]:
            with self.subTest(status=status, data=data), self.assertRaises(ValueError):
                self.policy.require_private_package(status, data, allow_absent=allow_absent)

    def test_github_response_status_is_not_inferred_from_command_failure(self):
        status, data = self.policy.parse_api_response('HTTP/2.0 404 Not Found\r\nContent-Type: application/json\r\n\r\n{"message":"Not Found"}\n')
        self.assertEqual((status, data), (404, {'message': 'Not Found'}))
        with self.assertRaises(ValueError):
            self.policy.parse_api_response('authentication failed')

    def test_platform_pins_cannot_reuse_amd64_descriptors_for_arm64(self):
        pins = {'release_platforms': {
            'linux/amd64': {'rust': 'docker.io/library/rust@sha256:' + '1' * 64,
                            'alpine': 'docker.io/library/alpine@sha256:' + '2' * 64},
            'linux/arm64': {'rust': 'docker.io/library/rust@sha256:' + '3' * 64,
                            'alpine': 'docker.io/library/alpine@sha256:' + '4' * 64}}}
        self.assertEqual(self.policy.platform_inputs(pins, 'arm64'), pins['release_platforms']['linux/arm64'])
        duplicate = copy.deepcopy(pins)
        duplicate['release_platforms']['linux/arm64'] = duplicate['release_platforms']['linux/amd64']
        with self.assertRaises(ValueError):
            self.policy.platform_inputs(duplicate, 'arm64')
        with self.assertRaises(ValueError):
            self.policy.platform_inputs(pins, 'unknown')

    def test_cli_checks_lockfile_notes_and_ci_before_emitting_outputs(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / 'Cargo.toml').write_text('[package]\nname="fixture"\nversion="0.1.0-rc.1"\n')
            (root / 'Cargo.lock').write_text('version=4\n[[package]]\nname="fixture"\nversion="0.1.0-rc.1"\n')
            (root / 'docs/releases').mkdir(parents=True)
            (root / 'docs/releases/0.1.0-rc.1.md').write_text('Candidate, not stable.\n')
            (root / 'ci.json').write_text(json.dumps({'workflow_runs': [self.successful_run()]}))
            command = [sys.executable, str(SCRIPT), 'metadata', '--root', str(root),
                       '--tag', 'v0.1.0-rc.1', '--sha', SHA, '--repository', 'Owner/Repo',
                       '--ci-json', str(root / 'ci.json'), '--github-output', str(root / 'output')]
            result = subprocess.run(command, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            values = dict(line.split('=', 1) for line in (root / 'output').read_text().splitlines())
            self.assertEqual(values['prerelease'], 'true')
            self.assertEqual(values['latest'], 'false')
            self.assertEqual(values['image'], 'ghcr.io/owner/repo')
            self.assertEqual(values['commit_sha'], SHA)
            (root / 'output').unlink()
            (root / 'Cargo.lock').write_text('version=4\n[[package]]\nname="fixture"\nversion="0.0.4"\n')
            result = subprocess.run(command, capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse((root / 'output').exists())

    def test_prepared_project_accepts_its_planned_release_tag(self):
        package = tomllib.loads((ROOT / 'Cargo.toml').read_text())['package']['name']
        version = {'polsat-proxy': '0.1.0-rc.1', 'tvp-vod-proxy': '0.0.5'}[package]
        with tempfile.TemporaryDirectory() as temp:
            ci = Path(temp) / 'ci.json'
            ci.write_text(json.dumps({'workflow_runs': [self.successful_run()]}))
            result = subprocess.run([sys.executable, str(SCRIPT), 'metadata', '--root', str(ROOT),
                                     '--tag', 'v' + version, '--sha', SHA, '--repository', 'fixture/repository',
                                     '--ci-json', str(ci)], capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(json.loads(result.stdout)['version'], version)


if __name__ == '__main__':
    unittest.main()
