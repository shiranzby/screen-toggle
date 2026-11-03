"""Isolated settings persistence for the optional desktop mouse gesture.

Bounded Windows messages, simulated hardware, Type-C placement, no input.
Native gesture acceptance is tested separately from this settings contract.
"""
import argparse
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
    out = Path(args.output).resolve()
    (out / 'data').mkdir(parents=True, exist_ok=True)
    (out / 'data/settings.json').write_text(json.dumps({
        'theme': 'dark', 'autostart': False,
        'hotkey': {'modifiers': 3, 'key': 120},
    }), encoding='utf-8')
    result = {'sha256': hashlib.sha256(exe.read_bytes()).hexdigest(),
              'pass': False, 'cases': [], 'scope': 'settings messages; simulated hardware; no native mouse input'}
    driver = ui.Driver(str(exe), out, result)
    try:
        driver.start()
        assert not driver.saved()['desktop_trigger_enabled']
        assert not driver.saved()['hotkey_double_click']
        assert driver.saved()['desktop_double_click']
        assert driver.saved()['desktop_hotkey'] == {'modifiers': 0, 'key': 5}
        def capture(control, field, key, modifiers, label):
            ui.message(driver.ctl(control), 0xf5)
            ui.until(lambda: driver.status()['capture'])
            ui.message(driver.owner, 0x8006, key, modifiers)
            driver.wait_saved(field, {'modifiers': modifiers, 'key': key})
            ui.until(lambda: not driver.status()['capture'])
            ui.message(driver.owner, 0x8007)
            assert label in ui.window_text(driver.ctl(control))
        names = {1: '鼠标左键', 2: '鼠标右键', 4: '鼠标中键（滚轮按下）',
                 5: '鼠标侧键 1', 6: '鼠标侧键 2'}
        for key, label in names.items():
            capture(2001, 'hotkey', key, 0, label)
        capture(2001, 'hotkey', 0x82, 7, 'Ctrl + Alt + Shift + F19')
        global_hotkey = driver.saved()['hotkey']
        ui.message(driver.ctl(2136), 0xf5)
        driver.wait_saved('hotkey_double_click', True)
        ui.message(driver.ctl(2131), 0xf5)
        driver.wait_saved('desktop_trigger_enabled', True)
        for key, label in names.items():
            if key == 5:
                continue
            capture(2132, 'desktop_hotkey', key, 0, label)
            assert driver.saved()['hotkey'] == global_hotkey
        capture(2132, 'desktop_hotkey', 0x83, 3, 'Ctrl + Alt + F20')
        ui.message(driver.ctl(2133), 0xf5)
        driver.wait_saved('desktop_double_click', False)
        assert driver.saved()['hotkey_double_click']
        capture(2132, 'desktop_hotkey', 6, 0, names[6])
        # Reject an enabled duplicate without silently replacing either binding.
        ui.message(driver.ctl(2132), 0xf5)
        ui.message(driver.owner, 0x8006, 0x82, 7)
        ui.until(lambda: '不能相同' in driver.status()['message'])
        assert driver.status()['capture']
        ui.message(driver.owner, 0x8006, 0x1b, 0)
        ui.until(lambda: not driver.status()['capture'])
        assert driver.saved()['desktop_hotkey'] == {'modifiers': 0, 'key': 6}
        driver.shot('two-trigger-groups-dark')
        driver.quit()
        assert driver.process.returncode == 0
        driver.start()
        assert driver.saved()['desktop_trigger_enabled']
        assert driver.saved()['hotkey_double_click']
        assert not driver.saved()['desktop_double_click']
        assert driver.saved()['desktop_hotkey'] == {'modifiers': 0, 'key': 6}
        assert driver.saved()['hotkey'] == global_hotkey
        assert not any(cls == 'SOGRustOverlay' for _, cls in ui.windows(driver.pid))
        ui.message(driver.ctl(2131), 0xf5)
        driver.wait_saved('desktop_trigger_enabled', False)
        assert not (driver.data / 'recovery.json').exists()
        result['cases'].append({'case': 'independent_global_desktop_recording_mouse_keyboard_double_flags_duplicate_rejection_restart', 'pass': True})
        result['pass'] = True
    finally:
        driver.quit()
        result['exit_code'] = driver.process.returncode if driver.process else None
        driver.write()
    print(json.dumps(result, ensure_ascii=False))


if __name__ == '__main__':
    main()
