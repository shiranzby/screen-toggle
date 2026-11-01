"""Read-only display topology and a fail-closed target for Type-C UI tests.

Windows reports the video transport, not the physical USB-C plug. This machine's
unique external DisplayPort path is the candidate corresponding to the user's
Type-C screen. HDMI, embedded panels and ambiguous external DP paths are refused.
No monitor settings, DDC/CI, WMI, input or window visibility are changed here.
"""
import argparse
import ctypes as C
from ctypes import wintypes as W
from dataclasses import dataclass
import json
import os
from pathlib import Path


class Luid(C.Structure):
    _fields_ = [('low', W.DWORD), ('high', W.LONG)]


class Rational(C.Structure):
    _fields_ = [('numerator', W.UINT), ('denominator', W.UINT)]


class SourceInfo(C.Structure):
    _fields_ = [('adapter', Luid), ('id', W.UINT), ('mode', W.UINT), ('flags', W.UINT)]


class TargetInfo(C.Structure):
    _fields_ = [('adapter', Luid), ('id', W.UINT), ('mode', W.UINT),
                ('technology', W.UINT), ('rotation', W.UINT), ('scaling', W.UINT),
                ('refresh', Rational), ('scan_order', W.UINT),
                ('available', W.BOOL), ('flags', W.UINT)]


class DisplayPath(C.Structure):
    _fields_ = [('source', SourceInfo), ('target', TargetInfo), ('flags', W.UINT)]


class ModeInfo(C.Structure):
    # DISPLAYCONFIG_MODE_INFO's 48-byte union is 8-byte aligned.
    _fields_ = [('kind', W.UINT), ('id', W.UINT), ('adapter', Luid), ('data', C.c_uint64 * 6)]


class Header(C.Structure):
    _fields_ = [('kind', W.UINT), ('size', W.UINT), ('adapter', Luid), ('id', W.UINT)]


class SourceName(C.Structure):
    _fields_ = [('header', Header), ('gdi_name', W.WCHAR * 32)]


class TargetName(C.Structure):
    _fields_ = [('header', Header), ('flags', W.UINT), ('technology', W.UINT),
                ('manufacturer', W.WORD), ('product', W.WORD), ('connector', W.UINT),
                ('friendly_name', W.WCHAR * 64), ('device_path', W.WCHAR * 128)]


class MonitorInfo(C.Structure):
    _fields_ = [('size', W.DWORD), ('monitor', W.RECT), ('work', W.RECT),
                ('flags', W.DWORD), ('device', W.WCHAR * 32)]


u = C.WinDLL('user32', use_last_error=True)
u.GetDisplayConfigBufferSizes.argtypes = [W.UINT, C.POINTER(W.UINT), C.POINTER(W.UINT)]
u.GetDisplayConfigBufferSizes.restype = W.LONG
u.QueryDisplayConfig.argtypes = [W.UINT, C.POINTER(W.UINT), C.POINTER(DisplayPath),
                                C.POINTER(W.UINT), C.POINTER(ModeInfo), C.c_void_p]
u.QueryDisplayConfig.restype = W.LONG
u.DisplayConfigGetDeviceInfo.argtypes = [C.POINTER(Header)]
u.DisplayConfigGetDeviceInfo.restype = W.LONG
u.GetMonitorInfoW.argtypes = [W.HANDLE, C.POINTER(MonitorInfo)]
u.GetMonitorInfoW.restype = W.BOOL
MonitorCallback = C.WINFUNCTYPE(W.BOOL, W.HANDLE, W.HDC, C.POINTER(W.RECT), W.LPARAM)
u.EnumDisplayMonitors.argtypes = [W.HDC, C.POINTER(W.RECT), MonitorCallback, W.LPARAM]
u.EnumDisplayMonitors.restype = W.BOOL

TECHNOLOGY = {5: 'HDMI', 6: 'LVDS', 10: 'DISPLAYPORT_EXTERNAL',
              11: 'DISPLAYPORT_EMBEDDED', 13: 'UDI_EMBEDDED',
              16: 'INDIRECT_WIRED', 17: 'INDIRECT_VIRTUAL',
              18: 'DISPLAYPORT_USB_TUNNEL', 0x80000000: 'INTERNAL'}


def _rect(area):
    return [area.left, area.top, area.right, area.bottom]


def inventory():
    """Return current active video paths joined to desktop work areas by GDI name."""
    assert (C.sizeof(DisplayPath), C.sizeof(ModeInfo), C.sizeof(SourceName),
            C.sizeof(TargetName)) == (72, 64, 84, 420)
    # This sets only the helper's coordinate interpretation, never an OS preference.
    u.SetProcessDpiAwarenessContext.argtypes = [C.c_void_p]
    u.SetProcessDpiAwarenessContext(C.c_void_p(-4))
    monitors = {}
    callback_errors = []

    @MonitorCallback
    def collect(handle, dc, rectangle, unused):
        info = MonitorInfo(size=C.sizeof(MonitorInfo))
        if not u.GetMonitorInfoW(handle, C.byref(info)):
            callback_errors.append(C.get_last_error())
            return False
        monitors[info.device.lower()] = {'monitor_rect': _rect(info.monitor),
                                         'work_rect': _rect(info.work),
                                         'primary': bool(info.flags & 1)}
        return True

    if not u.EnumDisplayMonitors(None, None, collect, 0) or callback_errors:
        raise OSError(f'Cannot enumerate monitor work areas: {callback_errors}')
    for _ in range(3):
        path_count, mode_count = W.UINT(), W.UINT()
        error = u.GetDisplayConfigBufferSizes(2, C.byref(path_count), C.byref(mode_count))
        if error:
            raise OSError(error, 'GetDisplayConfigBufferSizes failed')
        if path_count.value > 256 or mode_count.value > 4096:
            raise RuntimeError('Unexpected display topology size')
        paths = (DisplayPath * path_count.value)()
        modes = (ModeInfo * mode_count.value)()
        error = u.QueryDisplayConfig(2, C.byref(path_count), paths, C.byref(mode_count), modes, None)
        if error == 122:
            continue
        if error:
            raise OSError(error, 'QueryDisplayConfig failed')
        result = []
        for path in paths[:path_count.value]:
            source = SourceName(header=Header(1, C.sizeof(SourceName), path.source.adapter, path.source.id))
            target = TargetName(header=Header(2, C.sizeof(TargetName), path.target.adapter, path.target.id))
            for request in (source, target):
                error = u.DisplayConfigGetDeviceInfo(C.byref(request.header))
                if error:
                    raise OSError(error, 'DisplayConfigGetDeviceInfo failed')
            area = monitors.get(source.gdi_name.lower())
            if area is None:
                raise RuntimeError(f'Active path lacks monitor rectangle: {source.gdi_name}')
            result.append({'gdi_device': source.gdi_name,
                'friendly_name': target.friendly_name,
                'device_path': target.device_path,
                'source_id': path.source.id, 'target_id': path.target.id,
                'output_technology': path.target.technology,
                'connection': TECHNOLOGY.get(path.target.technology, str(path.target.technology)),
                **area})
        return result
    raise RuntimeError('Display topology kept changing; UI tests must wait')


@dataclass(frozen=True)
class TestDisplay:
    record: dict

    @property
    def work(self):
        return tuple(self.record['work_rect'])

    @property
    def environment_value(self):
        return ','.join(str(v) for v in self.work)

    def rectangle(self, width, height, margin=24):
        """Return an XYWH rectangle centered and wholly within this work area."""
        left, top, right, bottom = self.work
        width = min(width, right-left-2*margin)
        height = min(height, bottom-top-2*margin)
        if width <= 0 or height <= 0:
            raise ValueError('No space for a test window on the allowed display')
        x, y = left+(right-left-width)//2, top+(bottom-top-height)//2
        self.assert_contains((x, y, x+width, y+height))
        return x, y, width, height

    def assert_contains(self, bounds):
        """Reject LTRB bounds that could touch the HDMI or another display."""
        left, top, right, bottom = bounds
        wl, wt, wr, wb = self.work
        if not (wl <= left < right <= wr and wt <= top < bottom <= wb):
            raise AssertionError(f'Test rectangle {tuple(bounds)} leaves allowed Type-C work area {self.work}')


def select_typec(displays=None):
    displays = inventory() if displays is None else displays
    choices = [d for d in displays if d['output_technology'] in (10, 18)]
    if len(choices) != 1:
        raise RuntimeError(f'Expected one external DisplayPort/USB tunnel Type-C candidate; found {len(choices)}. Refusing UI tests.')
    selected = choices[0]
    # Mirrored/shared source regions would allow a test to appear on HDMI too.
    if any(d is not selected and d['gdi_device'].lower() == selected['gdi_device'].lower()
           for d in displays):
        raise RuntimeError('Type-C candidate shares its desktop source with another display; refusing UI tests')
    left, top, right, bottom = selected['work_rect']
    if any(d is not selected
           and max(left, d['monitor_rect'][0]) < min(right, d['monitor_rect'][2])
           and max(top, d['monitor_rect'][1]) < min(bottom, d['monitor_rect'][3])
           for d in displays):
        raise RuntimeError('Type-C work area overlaps another display; refusing UI tests')
    return TestDisplay(selected)


def configure_typec():
    """Set the child-process-only placement contract used by --ui-test startup."""
    selected = select_typec()
    os.environ['SOG_TEST_WORKAREA'] = selected.environment_value
    return selected


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--output', required=True)
    args = parser.parse_args()
    displays = inventory()
    target = select_typec(displays)
    evidence = {'read_only': True, 'api': 'QueryDisplayConfig + EnumDisplayMonitors',
                'physical_connector_note': 'Windows reports video transport; the unique external DP path is the Type-C candidate supplied by the user.',
                'displays': displays, 'selected': target.record,
                'SOG_TEST_WORKAREA': target.environment_value,
                'sample_xywh': target.rectangle(940, 1120)}
    output = Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(evidence, ensure_ascii=False, indent=2), encoding='utf-8')
    print(json.dumps(evidence, ensure_ascii=False, indent=2))


if __name__ == '__main__':
    main()
