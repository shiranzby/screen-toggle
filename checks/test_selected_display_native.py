"""Native selected-screen round trip, guarded to the user's Type-C screen.

The same stable display identity controls the saved selection, native allowlist,
and black-window region. No input injection or HDMI brightness operations.
"""
import argparse
import ctypes as C
import hashlib
import json
import os
from pathlib import Path
import subprocess
import time
import test_windows_ui as ui
from display_target import configure_typec
from test_three_screen_measurement import read_shared, values
from test_session_latency import cursor_mask


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--exe', required=True)
    parser.add_argument('--output', required=True)
    args = parser.parse_args()
    exe = str(Path(args.exe).resolve())
    output = Path(args.output).resolve()
    output.mkdir(parents=True, exist_ok=True)
    data = output / 'data'
    data.mkdir(exist_ok=True)
    assert not any(data.iterdir()), 'Use fresh test data; retain any pending recovery'
    assert not (Path(os.environ['APPDATA']) / 'ScreenOffGuard/recovery.json').exists()
    target = configure_typec()
    display_id = target.record['device_path'].lower()
    hardware_id = 'ddc:' + display_id
    config = {'theme': 'dark', 'autostart': False, 'hotkey': {'modifiers': 3, 'key': 120},
              'selected_displays': [display_id], 'dim_external': True, 'dim_internal': True,
              'hdr_off': False, 'cursor_mode': 'auto', 'dim_order': 'black_first',
              'restore_mode': 'ready', 'delay_ms': 0}
    (data / 'settings.json').write_text(json.dumps(config), encoding='utf-8')
    result = {'sha256': hashlib.sha256(Path(exe).read_bytes()).hexdigest(),
              'pass': False, 'scope': 'Type-C only, exact Native EXE, normal Dark/Light, no input',
              'selected_display': target.record, 'rounds': []}
    process = None
    owner = None
    before = None

    def probe(name):
        path = output / (name + '.json')
        subprocess.run([exe, '--restrict-id', hardware_id, '--probe', str(path)],
                       check=True, timeout=20)
        value = json.loads(path.read_text(encoding='utf-8'))
        assert 'inventory' in value and not value['inventory']['warnings'], value
        assert len(value['inventory']['devices']) == 1, value
        assert value['inventory']['devices'][0]['id'].lower() == hardware_id
        return value

    try:
        before = probe('before')
        cursor = cursor_mask()
        result['cursor_before'] = cursor
        assert not cursor['transparent']
        process = subprocess.Popen([exe, '--data-dir', str(data), '--restrict-id', hardware_id, '--silent'],
                                   creationflags=0x08000000)
        owner = ui.until(lambda: next((h for h, cls in ui.windows(process.pid)
                                     if cls.startswith('SOGController')), None))
        time.sleep(.3)
        generation = 0
        for number in range(1, 3):
            record = {'round': number}
            for operation in ['dark', 'light']:
                started = time.perf_counter()
                ui.message(owner, 0x111, 1001)
                def completed():
                    value = read_shared(data / 'hardware-operation.json')
                    return value if value and value['generation'] > generation else None
                trace = ui.until(completed, timeout=15)
                generation = trace['generation']
                assert trace['operation'] == operation and not trace['error'], trace
                outcome = trace['outcome']
                assert not outcome['warnings'] and not outcome['superseded'], trace
                assert outcome['channels'] == 1 and len(outcome['changes']) == 1, trace
                change = outcome['changes'][0]
                assert change['id'].lower() == hardware_id and change['result'] == {'Ok': True}, trace
                assert change['verify_reads'] >= 1, trace
                overlays = [h for h, cls in ui.windows(process.pid)
                            if cls == 'SOGRustOverlay' and ui.u.IsWindowVisible(h)]
                if operation == 'dark':
                    assert len(overlays) == 1
                    area = ui.bounds(overlays[0])
                    bounds = [area.left, area.top, area.right, area.bottom]
                    assert bounds == target.record['monitor_rect'], bounds
                    journal = read_shared(data / 'recovery.json')
                    assert len(journal['entries']) == 1 and not journal['cursor_hidden']
                else:
                    assert not overlays and not trace['pending']
                    assert not (data / 'recovery.json').exists()
                assert cursor_mask() == cursor, 'Partial-screen switching changed the global cursor'
                record[operation] = {'requested_to_complete_ms': trace['requested_to_complete_ms'],
                                     'hardware_ms': outcome['elapsed_ms'],
                                     'presentation_ms': outcome.get('presentation_ms'),
                                     'trace_observed_ms': (time.perf_counter()-started)*1000,
                                     'trace': trace}
            assert values(probe(f'after-{number}')) == values(before)
            result['rounds'].append(record)
        result['pass'] = True
    finally:
        if owner and ui.u.IsWindow(owner):
            ui.u.PostMessageW(owner, 0x10, 0, 0)
        if process:
            try:
                process.wait(timeout=20)
            except subprocess.TimeoutExpired:
                result['restoring_pid'] = process.pid
            result['exit_code'] = process.poll()
        result['pending_recovery'] = (data / 'recovery.json').exists()
        if before and (not process or process.poll() is not None):
            result['baseline_restored'] = values(probe('after-exit')) == values(before)
        result['pass'] &= result.get('exit_code') == 0 and not result['pending_recovery'] and result.get('baseline_restored', False)
        (output / 'results.json').write_text(json.dumps(result, ensure_ascii=False, indent=2), encoding='utf-8')
        print(json.dumps({k: v for k, v in result.items() if k != 'rounds'}, ensure_ascii=False))


if __name__ == '__main__':
    main()
