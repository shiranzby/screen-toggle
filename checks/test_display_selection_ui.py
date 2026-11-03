"""Display selection persistence and compact controls, isolated Type-C UI only.

Bounded window messages with the simulated backend; no input or hardware writes.
"""
import argparse
import ctypes as C
import hashlib
import json
from pathlib import Path
import test_windows_ui as ui


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--exe', required=True)
    parser.add_argument('--output', required=True)
    args = parser.parse_args()
    exe = Path(args.exe).resolve()
    output = Path(args.output).resolve()
    (output / 'data').mkdir(parents=True, exist_ok=True)
    profile = output / 'data/settings.json'
    profile.write_text(json.dumps({'theme': 'dark', 'autostart': False,
                                  'hotkey': {'modifiers': 3, 'key': 120}}), encoding='utf-8')
    result = {'sha256': hashlib.sha256(exe.read_bytes()).hexdigest(), 'pass': False,
              'cases': [], 'scope': 'isolated settings messages; simulated backend; Type-C only; no input'}
    driver = ui.Driver(str(exe), output, result)
    ui.u.IsWindowEnabled.argtypes = [ui.W.HWND]

    def click(hwnd, ident):
        ui.message(ui.control(driver.pid, hwnd, ident), 0xf5)

    def open_picker():
        click(driver.hwnd, 2141)
        hwnd = ui.until(lambda: next((h for h, cls in ui.windows(driver.pid)
                                     if cls == 'SOGRustDisplayPicker'), None))
        assert not ui.u.IsWindowEnabled(driver.hwnd)
        return hwnd

    def close_picker(hwnd, save=False):
        click(hwnd, 1 if save else 2)
        ui.until(lambda: not ui.u.IsWindow(hwnd))
        ui.until(lambda: ui.u.IsWindowEnabled(driver.hwnd))

    def check(hwnd, ident):
        return ui.message(ui.control(driver.pid, hwnd, ident), 0xf0) == 1

    def rows(hwnd):
        return [(ui.u.GetDlgCtrlID(h), ui.window_text(h))
                for h, _ in ui.windows(driver.pid, hwnd)
                if 3100 <= ui.u.GetDlgCtrlID(h) < 3105 and ui.u.IsWindowVisible(h)]

    try:
        driver.start()
        assert driver.saved()['selected_displays'] is None
        displays = driver.status()['displays']
        chosen = [d for d in displays if d['name'] == 'Type_C']
        assert len(chosen) == 1, displays
        target = chosen[0]['id']
        ext, internal = ui.bounds(driver.ctl(2002)), ui.bounds(driver.ctl(2003))
        assert (ext.top, ext.bottom) == (internal.top, internal.bottom)
        assert ext.right < internal.left
        click(driver.hwnd, 2002)
        driver.wait_saved('dim_external', False)
        click(driver.hwnd, 2003)
        driver.wait_saved('dim_internal', False)
        click(driver.hwnd, 2002)
        driver.wait_saved('dim_external', True)
        click(driver.hwnd, 2003)
        driver.wait_saved('dim_internal', True)
        result['cases'].append({'case': 'same_row_independent_external_internal_switches', 'pass': True})

        picker = open_picker()
        assert check(picker, 3001)
        click(picker, 3001)
        row_ids = rows(picker)
        assert len(row_ids) == len(displays), row_ids
        for ident, caption in row_ids:
            if 'Type_C' not in caption:
                click(picker, ident)
        driver.shot('display-picker-dark-one-selected', picker)
        close_picker(picker)
        assert driver.saved()['selected_displays'] is None, 'Cancel changed selection'

        picker = open_picker()
        click(picker, 3001)
        for ident, caption in rows(picker):
            if 'Type_C' not in caption:
                click(picker, ident)
        close_picker(picker, save=True)
        driver.wait_saved('selected_displays', [target])
        assert '1' in ui.window_text(driver.ctl(2141))
        driver.quit()
        assert driver.process.returncode == 0
        driver.start()
        assert driver.saved()['selected_displays'] == [target]
        picker = open_picker()
        assert not check(picker, 3001)
        assert all(check(picker, ident) == ('Type_C' in caption) for ident, caption in rows(picker))
        # A zero-selection draft cannot be saved.
        target_row = next(ident for ident, caption in rows(picker) if 'Type_C' in caption)
        click(picker, target_row)
        assert not ui.u.IsWindowEnabled(ui.control(driver.pid, picker, 1))
        close_picker(picker)
        assert driver.saved()['selected_displays'] == [target]
        result['cases'].append({'case': 'cancel_no_write_save_stable_identity_restart_zero_selection_rejected', 'pass': True})

        # Resize and use the light theme without moving off the allowed display.
        click(driver.hwnd, 2111)
        driver.wait_saved('theme', 'light')
        area = driver.display.rectangle(1150, 1500)
        ui.u.SetWindowPos(driver.hwnd, None, *area, 0x14)
        ui.message(driver.hwnd, 0x115, 2)  # top of body
        driver.shot('compact-screen-controls-light')
        picker = open_picker()
        driver.shot('display-picker-light', picker)
        click(picker, 3001)
        close_picker(picker, save=True)
        driver.wait_saved('selected_displays', None)
        driver.quit()
        driver.start()
        assert driver.saved()['selected_displays'] is None
        assert not (driver.data / 'recovery.json').exists()
        driver.quit()

        # An unavailable chosen screen remains selected and cannot dim another.
        cfg = json.loads(profile.read_text(encoding='utf-8'))
        missing = r'\\?\DISPLAY#MISSING#unplugged#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}'
        cfg['selected_displays'] = [missing]
        profile.write_text(json.dumps(cfg), encoding='utf-8')
        driver.start()
        click(driver.hwnd, 2095)
        ui.until(lambda: '未连接' in driver.status()['message'])
        assert not driver.status()['desired_dark']
        assert not any(cls == 'SOGRustOverlay' for _, cls in ui.windows(driver.pid))
        assert not (driver.data / 'recovery.json').exists()
        picker = open_picker()
        assert any('未连接' in caption for _, caption in rows(picker))
        close_picker(picker)
        assert driver.saved()['selected_displays'] == [missing]
        result['cases'].append({'case': 'all_mode_light_theme_disconnected_identity_kept_no_fallback', 'pass': True})
        driver.quit()
        cfg = json.loads(profile.read_text(encoding='utf-8'))
        offline = [missing.replace('unplugged', f'unplugged-{index}') for index in range(8)]
        cfg['selected_displays'] = offline
        profile.write_text(json.dumps(cfg), encoding='utf-8')
        driver.start()
        picker = open_picker()
        seen = []
        pages = 0
        while True:
            seen.extend(caption for _, caption in rows(picker))
            pages += 1
            assert pages <= 6
            if not ui.u.IsWindowEnabled(ui.control(driver.pid, picker, 3003)):
                break
            click(picker, 3003)
        assert len(seen) == len(displays) + len(offline) and pages >= 2, seen
        close_picker(picker, save=True)
        driver.wait_saved('selected_displays', offline)
        result['cases'].append({'case': 'many_display_pages_preserve_all_selected_identities', 'pass': True})
        result['pass'] = True
    finally:
        driver.quit()
        result['exit_code'] = driver.process.returncode if driver.process else None
        driver.write()
    print(json.dumps(result, ensure_ascii=False))


if __name__ == '__main__':
    main()
