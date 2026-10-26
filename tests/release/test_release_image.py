"""Release-image validation must reject wrong architectures and incomplete media."""
import copy
import importlib.util
from pathlib import Path
import unittest

SCRIPT = Path(__file__).resolve().parents[2] / 'scripts/release_image.py'


class ReleaseImageTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        spec = importlib.util.spec_from_file_location('release_image', SCRIPT)
        cls.smoke = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.smoke)

    def media(self):
        return {'streams': [
            {'codec_type': 'video', 'codec_name': 'h264', 'width': 320, 'height': 180, 'nb_read_frames': '50'},
            {'codec_type': 'audio', 'codec_name': 'aac', 'channels': 2, 'sample_rate': '48000', 'nb_read_frames': '95'}]}

    def test_full_generated_av_is_required(self):
        self.smoke.verify_media(self.media())
        invalid = []
        missing = self.media();missing['streams'].pop();invalid.append(missing)
        extra = self.media();extra['streams'].append({'codec_type': 'subtitle'});invalid.append(extra)
        for index, key, value in [(0, 'nb_read_frames', '49'), (0, 'codec_name', 'mpeg2video'),
                                  (1, 'nb_read_frames', '1'), (1, 'channels', 6),
                                  (1, 'sample_rate', '44100')]:
            changed = copy.deepcopy(self.media());changed['streams'][index][key] = value;invalid.append(changed)
        for probe in invalid:
            with self.assertRaises(ValueError):self.smoke.verify_media(probe)

    def test_image_must_match_requested_architecture_and_default_user(self):
        info = {'Os': 'linux', 'Architecture': 'arm64', 'Config': {'User': '10001'}}
        self.smoke.verify_identity(info, 'arm64', 'polsat', 10001)
        for change, arch, uid in [({'Architecture': 'amd64'}, 'arm64', 10001),
                                  ({'Os': 'windows'}, 'arm64', 10001),
                                  ({}, 'arm64', 0), ({'Config': {'User': 'root'}}, 'arm64', 10001)]:
            changed={**info,**change}
            with self.assertRaises(ValueError):self.smoke.verify_identity(changed,arch,'polsat',uid)
        self.smoke.verify_identity({'Os':'linux','Architecture':'amd64','Config':{'User':'proxy'}},'amd64','tvp',10001)

    def test_idle_tvp_metadata_cannot_hide_media_work(self):
        text='''tvp_stream_limit 5
tvp_stream_slots_in_use 0
tvp_stream_requests_in_flight 0
tvp_stream_requests_total{outcome="started"} 0
tvp_ffmpeg_processes_active{channel="other"} 0
'''
        self.smoke.verify_idle_tvp(text)
        for changed in [text.replace('tvp_stream_slots_in_use 0','tvp_stream_slots_in_use 1'),
                        text.replace('outcome="started"} 0','outcome="started"} 1'),
                        text.replace('channel="other"} 0','channel="other"} 1')]:
            with self.assertRaises(ValueError):self.smoke.verify_idle_tvp(changed)


if __name__ == '__main__':
    unittest.main()
