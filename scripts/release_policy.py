#!/usr/bin/env python3
"""Fail-closed release gates; uses only the standard library and existing gh CLI."""
import argparse
import json
from pathlib import Path
import re
import subprocess
import sys
import tomllib

VERSION = re.compile(r'(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?')


def release_info(tag, version):
    match = VERSION.fullmatch(version)
    if not match or len(version) > 128 or tag != 'v' + version:
        raise ValueError('tag must exactly match a Docker-compatible package version')
    prerelease = match[4] is not None
    if prerelease and any(part.isdigit() and len(part) > 1 and part[0] == '0' for part in match[4].split('.')):
        raise ValueError('numeric prerelease identifiers cannot have leading zeroes')
    return {'version': version, 'prerelease': prerelease, 'latest': not prerelease}


def require_ci(payload, sha):
    if not re.fullmatch(r'[0-9a-f]{40}', sha):
        raise ValueError('a full commit SHA is required')
    runs = payload.get('workflow_runs')
    if not isinstance(runs, list):
        raise ValueError('CI response is not a workflow-run list')
    matching = [run for run in runs if run.get('head_sha') == sha
                and run.get('head_branch') == 'main' and run.get('event') == 'push']
    if not matching or any(type(run.get('id')) is not int or run['id'] <= 0 for run in matching):
        raise ValueError('no verified main-push CI for this exact commit')
    latest = max(matching, key=lambda run: (run['id'], run.get('run_attempt', 1)))
    if latest.get('status') != 'completed' or latest.get('conclusion') != 'success':
        raise ValueError('latest main-push CI for this exact commit must succeed first')
    return latest['id']


def require_platforms(manifest, *, require_provenance=False):
    entries = manifest.get('manifests', [])
    native = [entry for entry in entries if entry.get('platform', {}).get('os') == 'linux']
    actual = [entry.get('platform', {}).get('architecture') for entry in native]
    digests = {entry.get('digest') for entry in native}
    if len(actual) != 2 or set(actual) != {'amd64', 'arm64'} or len(digests) != 2:
        raise ValueError('release manifest must contain exactly linux/amd64 and linux/arm64')
    if any(not isinstance(d, str) or not re.fullmatch(r'sha256:[0-9a-f]{64}', d) for d in digests):
        raise ValueError('native manifest digests are required')
    attested = set()
    for entry in entries:
        if entry in native:
            continue
        annotations = entry.get('annotations', {})
        platform = entry.get('platform', {})
        digest = entry.get('digest', '')
        reference = annotations.get('vnd.docker.reference.digest')
        if platform != {'architecture': 'unknown', 'os': 'unknown'} or annotations.get('vnd.docker.reference.type') != 'attestation-manifest' or reference not in digests or not re.fullmatch(r'sha256:[0-9a-f]{64}', digest):
            raise ValueError('unexpected or incorrectly associated attestation descriptor')
        attested.add(reference)
    if require_provenance and attested != digests:
        raise ValueError('both release architectures must retain BuildKit attestations')


def require_private_package(status, payload, *, allow_absent):
    if status == 404 and allow_absent:
        return
    if status != 200 or payload.get('visibility') != 'private':
        raise ValueError('package privacy could not be verified; refuse publication/promotion')


def parse_api_response(text):
    headers, separator, body = text.replace('\r\n', '\n').partition('\n\n')
    first = headers.splitlines()[0] if headers else ''
    match = re.fullmatch(r'HTTP/\S+ (\d{3})(?: .*)?', first)
    if not match or not separator:
        raise ValueError('GitHub returned no verifiable HTTP status')
    try:
        data = json.loads(body)
    except ValueError as error:
        raise ValueError('GitHub returned invalid JSON') from error
    if not isinstance(data, dict):
        raise ValueError('unexpected GitHub response shape')
    return int(match[1]), data


def repository_parts(repository):
    parts = repository.lower().split('/')
    if len(parts) != 2 or not all(re.fullmatch(r'[a-z0-9]+(?:[._-]+[a-z0-9]+)*', part) for part in parts):
        raise ValueError('invalid registry repository identity')
    return parts


def github_get(path):
    # gh inherits its standard CI authentication; never retrieve/print a token.
    result = subprocess.run(['gh', 'api', '--include', path], capture_output=True, text=True, timeout=30)
    status, data = parse_api_response(result.stdout)
    if (result.returncode == 0) != (200 <= status < 300):
        raise ValueError('GitHub request did not complete consistently')
    return status, data


def platform_inputs(pins, arch):
    if arch not in ('amd64', 'arm64'):
        raise ValueError('unsupported release architecture')
    platforms = pins.get('release_platforms', {})
    if set(platforms) != {'linux/amd64', 'linux/arm64'}:
        raise ValueError('both verified release platforms are required')
    for platform in platforms.values():
        if set(platform) != {'rust', 'alpine'}:
            raise ValueError('unexpected platform input keys')
        for name in ['rust', 'alpine']:
            if not re.fullmatch(r'docker.io/library/' + name + r'@sha256:[0-9a-f]{64}', platform[name]):
                raise ValueError('base images must use verified immutable manifest references')
    if any(platforms['linux/amd64'][name] == platforms['linux/arm64'][name] for name in ['rust', 'alpine']):
        raise ValueError('do not reuse a platform-specific amd64 descriptor on arm64')
    return platforms['linux/' + arch]


def emit(values, path):
    if path:
        with Path(path).open('a') as out:
            for key, value in values.items():
                encoded = str(value).lower() if isinstance(value, bool) else str(value)
                if '\n' in encoded or '\r' in encoded:
                    raise ValueError('invalid multiline output')
                out.write(key + '=' + encoded + '\n')
    print(json.dumps(values, sort_keys=True))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest='command', required=True)
    meta = commands.add_parser('metadata')
    meta.add_argument('--root', type=Path, default=Path('.'))
    meta.add_argument('--tag', required=True)
    meta.add_argument('--sha', required=True)
    meta.add_argument('--repository', required=True)
    meta.add_argument('--ci-json', type=Path, required=True)
    meta.add_argument('--github-output')
    platform = commands.add_parser('platform')
    platform.add_argument('--pins', type=Path, required=True)
    platform.add_argument('--arch', choices=['amd64', 'arm64'], required=True)
    platform.add_argument('--github-output')
    package = commands.add_parser('private-package')
    package.add_argument('--repository', required=True)
    package.add_argument('--allow-absent', action='store_true')
    unreleased = commands.add_parser('unreleased')
    unreleased.add_argument('--repository', required=True)
    unreleased.add_argument('--tag', required=True)
    manifest = commands.add_parser('manifest')
    manifest.add_argument('--json', type=Path, required=True)
    manifest.add_argument('--require-provenance', action='store_true')
    args = parser.parse_args()
    if args.command == 'metadata':
        cargo = tomllib.loads((args.root / 'Cargo.toml').read_text())['package']
        values = release_info(args.tag, cargo['version'])
        lock = tomllib.loads((args.root / 'Cargo.lock').read_text())
        packages = [p for p in lock['package'] if p['name'] == cargo['name']]
        if len(packages) != 1 or packages[0]['version'] != cargo['version']:
            raise ValueError('Cargo.lock package version must match Cargo.toml')
        notes = args.root / 'docs/releases' / (cargo['version'] + '.md')
        if not notes.is_file() or not notes.read_text().strip():
            raise ValueError('version-specific release notes are required')
        values['ci_run_id'] = require_ci(json.loads(args.ci_json.read_text()), args.sha)
        values['commit_sha'] = args.sha
        values['image'] = 'ghcr.io/' + '/'.join(repository_parts(args.repository))
        emit(values, args.github_output)
    elif args.command == 'platform':
        pins = json.loads(args.pins.read_text())
        images = platform_inputs(pins, args.arch)
        ffmpeg = pins['ffmpeg_apk_version']
        if not re.fullmatch(r'[0-9]+\.[0-9]+\.[0-9]+-r[0-9]+', ffmpeg):
            raise ValueError('invalid pinned FFmpeg package version')
        emit({'rust_image': images['rust'], 'alpine_image': images['alpine'], 'ffmpeg_apk_version': ffmpeg}, args.github_output)
    elif args.command == 'private-package':
        owner, name = repository_parts(args.repository)
        status, data = github_get(f'/users/{owner}/packages/container/{name}')
        require_private_package(status, data, allow_absent=args.allow_absent)
        print('Package visibility gate passed; no visibility setting changed.')
    elif args.command == 'unreleased':
        owner, name = repository_parts(args.repository)
        if not args.tag.startswith('v'):
            raise ValueError('release tag must start with v')
        release_info(args.tag, args.tag[1:])
        status, _ = github_get(f'/repos/{owner}/{name}/releases/tags/{args.tag}')
        if status != 404:
            raise ValueError('release already exists or absence could not be verified')
        print('No existing release will be overwritten.')
    else:
        require_platforms(json.loads(args.json.read_text()), require_provenance=args.require_provenance)
        print('Verified both native Linux release architectures.')


if __name__ == '__main__':
    try:
        main()
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as error:
        print('Release policy rejected: ' + str(error), file=sys.stderr)
        sys.exit(1)
