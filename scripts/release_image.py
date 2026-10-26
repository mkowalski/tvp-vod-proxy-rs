#!/usr/bin/env python3
"""Validate an isolated native release image, never a deployed container."""
import argparse
import json
from pathlib import Path
import platform
import re
import subprocess
import sys
import time
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parents[1]
ISOLATION = ['--rm', '--pull=never', '--read-only', '--cap-drop=ALL',
             '--security-opt=no-new-privileges:true',
             '--tmpfs', '/tmp:rw,noexec,nosuid,size=32m,mode=1777']
MEDIA = '''
ffmpeg -hide_banner -nostdin -v error -f lavfi -i color=c=black:s=320x180:r=25:d=2 \
  -f lavfi -i sine=frequency=1000:sample_rate=48000:duration=2 \
  -map 0:v:0 -map 1:a:0 -c:v libx264 -preset ultrafast -pix_fmt yuv420p \
  -g 25 -c:a aac -ac 2 -shortest -f mpegts /tmp/fixture.ts
ffmpeg -hide_banner -nostdin -v error -err_detect explode -xerror \
  -i /tmp/fixture.ts -map 0:v:0 -map 0:a:0 -f null -
ffprobe -v error -count_frames -show_streams -of json /tmp/fixture.ts
'''


def verify_media(probe):
    streams = probe.get('streams', [])
    video = [s for s in streams if s.get('codec_type') == 'video']
    audio = [s for s in streams if s.get('codec_type') == 'audio']
    if len(streams) != 2 or len(video) != 1 or len(audio) != 1:
        raise ValueError('synthetic media must contain exactly one video/audio pair')
    v, a = video[0], audio[0]
    if (v.get('codec_name'), v.get('width'), v.get('height'), v.get('nb_read_frames')) != ('h264', 320, 180, '50'):
        raise ValueError('incomplete or unexpected generated H.264 video')
    if a.get('codec_name') != 'aac' or a.get('channels') != 2 or a.get('sample_rate') != '48000' or not 90 <= int(a.get('nb_read_frames', '0')) <= 100:
        raise ValueError('incomplete or unexpected generated stereo AAC')


def verify_identity(info, arch, kind, uid):
    expected = {'polsat': {'10001', '10001:10001'}, 'tvp': {'proxy', '10001', '10001:10001'}}
    if info.get('Os') != 'linux' or info.get('Architecture') != arch or uid != 10001:
        raise ValueError('release image must match the requested Linux architecture and UID10001')
    if kind not in expected or info.get('Config', {}).get('User') not in expected[kind]:
        raise ValueError('release image must declare its unprivileged default user')


def verify_idle_tvp(text):
    samples = {}
    for line in text.splitlines():
        if not line or line.startswith('#'):
            continue
        key, value = line.rsplit(' ', 1)
        if key in samples:
            raise ValueError('duplicate telemetry sample')
        samples[key] = float(value)
    expected = {'tvp_stream_limit': 5, 'tvp_stream_slots_in_use': 0,
                'tvp_stream_requests_in_flight': 0,
                'tvp_stream_requests_total{outcome="started"}': 0}
    if any(samples.get(key) != value for key, value in expected.items()):
        raise ValueError('metadata requests allocated media work or incorrect capacity')
    children = [value for key, value in samples.items() if key.startswith('tvp_ffmpeg_processes_active{')]
    if not children or any(value != 0 for value in children):
        raise ValueError('metadata requests left owned media children')


def docker(*args):
    return subprocess.run(['docker', *args], capture_output=True, text=True, timeout=90, check=True).stdout.strip()


def tvp_metadata(image):
    container = None
    try:
        container = docker('run', '--detach', *ISOLATION, '--network=bridge',
                           '--name', 'tvp-release-smoke-' + uuid.uuid4().hex,
                           '--publish', '127.0.0.1::8080', '--env', 'PORT=8080',
                           '--env', 'MAX_STREAMS=5', '--env', 'TVP_API=http://127.0.0.1:9', image)
        if not re.fullmatch(r'[0-9a-f]{64}', container):
            raise ValueError('invalid owned container identity')
        info = json.loads(docker('inspect', container))[0]
        bindings = info['NetworkSettings']['Ports']['8080/tcp']
        if len(bindings) != 1 or bindings[0]['HostIp'] != '127.0.0.1' or bindings[0]['HostPort'] in ['', '0', '38099', '38100']:
            raise ValueError('smoke listener must use a nonproduction localhost-only port')
        origin = 'http://127.0.0.1:' + bindings[0]['HostPort']
        deadline = time.monotonic() + 10
        while True:
            try:
                with urllib.request.urlopen(origin + '/healthz', timeout=1) as response:
                    if response.status == 200:
                        break
            except OSError:
                if time.monotonic() >= deadline:
                    raise ValueError('image startup deadline')
                time.sleep(.05)
        with urllib.request.urlopen(urllib.request.Request(origin + '/tvp/399700.ts', method='HEAD'), timeout=2) as response:
            if response.status != 200 or response.headers.get('Content-Length') is not None:
                raise ValueError('incorrect endless-stream HEAD metadata')
        with urllib.request.urlopen(origin + '/metrics', timeout=2) as response:
            verify_idle_tvp(response.read().decode())
        if 'ffmpeg' in docker('exec', container, 'ps', '-o', 'comm').splitlines():
            raise ValueError('metadata smoke started a media process')
    finally:
        if container is not None:
            docker('stop', '--time', '5', container)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--image', required=True)
    parser.add_argument('--arch', choices=['amd64', 'arm64'], required=True)
    parser.add_argument('--kind', choices=['tvp', 'polsat'], required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if not re.fullmatch(r'sha256:[0-9a-f]{64}', args.image) or args.output.exists():
        raise ValueError('an immutable local image and new report path are required')
    native = {'x86_64': 'amd64', 'aarch64': 'arm64', 'arm64': 'arm64'}.get(platform.machine())
    if platform.system() != 'Linux' or native != args.arch:
        raise ValueError('validate on the native requested Linux architecture, not emulation')
    image = json.loads(docker('image', 'inspect', args.image))[0]
    uid = int(docker('run', *ISOLATION, '--network=none', '--entrypoint', 'id', args.image, '-u'))
    verify_identity(image, args.arch, args.kind, uid)
    probe = json.loads(docker('run', *ISOLATION, '--network=none', '--entrypoint', 'sh', args.image, '-ec', MEDIA))
    verify_media(probe)
    if args.kind == 'tvp':
        tvp_metadata(args.image)
    else:
        result = args.output.with_name(args.output.stem + '-metadata.json')
        subprocess.run([sys.executable, str(ROOT / 'scripts/verify_container.py'), '--image', args.image,
                        '--config', str(ROOT / 'config/polsat.json'), '--output', str(result)], check=True, timeout=60)
        if not json.loads(result.read_text())['passed']:
            raise ValueError('Polsat isolated metadata contract failed')
    report = {'passed': True, 'scope': 'native isolated image/codec/metadata smoke, not live sources or DVR',
              'image_id': args.image, 'platform': 'linux/' + args.arch, 'uid': uid,
              'ffmpeg_version_line': docker('run', *ISOLATION, '--network=none', '--entrypoint', 'ffmpeg', args.image, '-version').splitlines()[0],
              'apk_inventory': sorted(docker('run', *ISOLATION, '--network=none', '--entrypoint', 'apk', args.image, 'info', '-v').splitlines()),
              'synthetic_video_frames': 50, 'synthetic_audio_channels': 2, 'metadata_started_no_media': True}
    with args.output.open('x') as out:
        json.dump(report, out, indent=2)
        out.write('\n')
    print(json.dumps({key: value for key, value in report.items() if key != 'apk_inventory'}))


if __name__ == '__main__':
    try:
        main()
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError):
        print('Release image validation failed; inspect the isolated CI image and required tools.', file=sys.stderr)
        sys.exit(1)
