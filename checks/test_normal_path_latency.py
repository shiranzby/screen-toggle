"""Opt-in native three-screen Dark/Light acceptance of an instrumented EXE.

Uses normal WM_COMMAND toggle requests on a hidden controller. No measurement
operation, simulated backend, foreground change, screenshot, or input injection.
Window observation measures presentation, never physical panel response. Always
requests ordinary restoration/exit; a restoring process is never force-killed.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import statistics
import subprocess
import time

import test_windows_ui as ui
from test_session_latency import cursor_mask
from test_three_screen_measurement import read_shared, values


def visible_overlay(pid):
    return any(cls == 'SOGRustOverlay' and ui.u.IsWindowVisible(hwnd)
               for hwnd, cls in ui.windows(pid))


def save(path, value):
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2), encoding='utf-8')


def run_phase(process, owner, data, operation, last_generation, expected, cursor):
    dark = operation == 'dark'
    assert visible_overlay(process.pid) != dark, 'Unexpected pre-toggle overlay state'
    started = time.perf_counter()
    ui.message(owner, 0x111, 1001)
    dispatch_return_ms = (time.perf_counter() - started) * 1000
    samples = []
    observed_presentation_ms = None
    trace = None
    while time.perf_counter() - started < 15:
        assert process.poll() is None, 'Native test process exited during transition'
        window = visible_overlay(process.pid)
        window_ms = (time.perf_counter() - started) * 1000
        if window == dark and observed_presentation_ms is None:
            observed_presentation_ms = window_ms
        pointer = cursor_mask()
        pointer_ms = (time.perf_counter() - started) * 1000
        journal = read_shared(data / 'recovery.json')
        samples.append({'window_observed_ms': window_ms,
                        'cursor_observed_ms': pointer_ms,
                        'overlay': window, 'cursor_transparent': pointer['transparent'],
                        'journal_entries': len(journal['entries']) if journal else None})
        value = read_shared(data / 'hardware-operation.json')
        if value and value['generation'] > last_generation and value['operation'] == operation:
            trace = value
            break
        time.sleep(.002)
    assert trace is not None, f'Normal {operation} timed out; restore the private data directory'
    trace_observed_ms = (time.perf_counter() - started) * 1000
    assert not trace.get('error'), trace
    outcome = trace['outcome']
    assert outcome and not outcome['warnings'] and not outcome['superseded'], trace
    changes = outcome['changes']
    assert outcome['channels'] == len(expected) and len(changes) == len(expected), trace
    assert {(change['id'], change['kind']) for change in changes} == expected, trace
    assert all(change['result'] == {'Ok': True} and change['stage'] == 'brightness'
               and change['pre_read_ms'] == 0 and change['verify_reads'] >= 1
               for change in changes), trace
    latest_start = max(change['started_ms'] for change in changes)
    earliest_finish = min(change['started_ms'] + change['elapsed_ms'] for change in changes)
    assert latest_start < earliest_finish, ('Three hardware lanes did not overlap', trace)
    assert observed_presentation_ms is not None, ('Presentation never observed', trace)
    assert visible_overlay(process.pid) == dark, trace
    assert cursor_mask()['transparent'] == (dark and cursor == 'hide'), trace
    assert trace['pending'] == dark, trace
    assert (data / 'recovery.json').exists() == dark, trace
    assert outcome.get('presentation_ms') is not None, 'Instrumented presentation timestamp missing'
    return {
        'operation': operation, 'generation': trace['generation'], 'pass': True,
        'dispatch_return_ms': dispatch_return_ms,
        'observed_presentation_ms': observed_presentation_ms,
        'trace_observed_ms': trace_observed_ms,
        'presentation_ms': outcome['presentation_ms'],
        'requested_to_complete_ms': trace['requested_to_complete_ms'],
        'hardware_ms': outcome['elapsed_ms'], 'write_ms': outcome['write_ms'],
        'prepare_ms': outcome['prepare_ms'], 'total_ms': outcome['total_ms'],
        'start_spread_ms': latest_start - min(change['started_ms'] for change in changes),
        'all_three_overlap_ms': earliest_finish - latest_start,
        'trace': trace, 'samples': samples,
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--exe', required=True)
    parser.add_argument('--output', required=True)
    parser.add_argument('--rounds', type=int, default=3, choices=range(1, 5))
    parser.add_argument('--cursor', choices=['hide', 'keep'], default='hide')
    parser.add_argument('--restore-mode', choices=['ready', 'delay'], default='ready')
    parser.add_argument('--delay-ms', type=int, default=0, choices=range(501))
    parser.add_argument('--restrict-id', help='fail-closed native device identity for a safe single-screen run')
    args = parser.parse_args()
    exe = str(Path(args.exe).resolve())
    out = Path(args.output).resolve()
    out.mkdir(parents=True, exist_ok=True)
    data = out / 'data'
    data.mkdir(exist_ok=True)
    assert not (Path(os.environ['APPDATA']) / 'ScreenOffGuard/recovery.json').exists(), \
        'Daily hardware operation active'
    assert not any(data.iterdir()), 'Use a fresh evidence directory; restore any previous test first'
    cfg = {'schema_version': 1, 'hotkey': {'modifiers': 3, 'key': 120},
           'close_action': 'exit', 'autostart': False, 'dim_internal': True,
           'dim_external': True, 'hdr_off': False, 'cursor_mode': args.cursor,
           'dim_order': 'black_first', 'restore_mode': args.restore_mode,
           'delay_ms': args.delay_ms}
    save(data / 'settings.json', cfg)
    result = {'exe': exe, 'sha256': hashlib.sha256(Path(exe).read_bytes()).hexdigest(),
              'pass': False, 'rounds': [], 'config': cfg,
              'scope': 'Authorized native normal Dark/Light; optional fail-closed device restriction; HDR unchanged',
              'dispatch': 'Bounded WM_COMMAND 1001 on hidden controller; private data; no input',
              'timing_boundaries': {
                  'presentation_ms': 'Engine start to UI acknowledgement; not physical panel response',
                  'observed_presentation_ms': 'Toggle dispatch to first target overlay visibility sample',
                  'requested_to_complete_ms': 'Application request to completed Engine outcome',
                  'trace_observed_ms': 'Toggle dispatch to completed-operation trace readable, including observer overhead',
                  'hardware_ms': 'Hardware batch wall time including exact read-back; no discovery/journal',
                  'write_ms': 'Slowest write return in hardware batch; not read-back completion'}}
    process = None
    owner = None
    before = None

    def probe(name):
        path = out / (name + '.json')
        subprocess.run([exe, '--probe', str(path)], check=True, timeout=20)
        return json.loads(path.read_text(encoding='utf-8'))

    try:
        before = probe('before')
        devices = [device for device in before['inventory']['devices']
                   if device['kind'] in ('external', 'internal')]
        if args.restrict_id:
            devices = [device for device in devices if device['id'].lower() == args.restrict_id.lower()]
            assert len(devices) == 1, devices
        else:
            assert len(devices) == 3 and sum(device['kind'] == 'external' for device in devices) == 2, devices
        expected = {(device['id'], device['kind']) for device in devices}
        result['cursor_before'] = cursor_mask()
        assert not result['cursor_before']['transparent'], 'Cursor already hidden before the test'
        command = [exe, '--data-dir', str(data), '--silent']
        if args.restrict_id:
            command += ['--restrict-id', args.restrict_id]
        process = subprocess.Popen(command, creationflags=0x08000000)
        owner = ui.until(lambda: next((hwnd for hwnd, cls in ui.windows(process.pid)
                                     if cls.startswith('SOGController')), None), timeout=10)
        ui.message(owner, 0, timeout=3000)
        time.sleep(.5)
        assert process.poll() is None
        assert not any(cls == 'SOGRustSettings' for _, cls in ui.windows(process.pid))
        assert not visible_overlay(process.pid)
        last_generation = 0
        for number in range(1, args.rounds + 1):
            phases = {}
            for operation in ('dark', 'light'):
                phase = run_phase(process, owner, data, operation, last_generation,
                                  expected, args.cursor)
                last_generation = phase['generation']
                save(out / f'round-{number}-{operation}.json', phase)
                phases[operation] = {key: value for key, value in phase.items()
                                     if key not in ('trace', 'samples')}
            after = probe(f'after-round-{number}')
            assert values(after) == values(before), 'Per-screen brightness/HDR baseline mismatch'
            assert cursor_mask() == result['cursor_before'], 'System arrow baseline mismatch'
            result['rounds'].append({'round': number, 'pass': True,
                                     'baselines_equal': True, **phases})
            save(out / 'results.json', result)
        presentations = read_shared(data / 'presentation-trace.json')
        assert presentations and len(presentations) == args.rounds * 2, presentations
        assert all(event['dark'] == (index % 2 == 0) for index, event in enumerate(presentations)), presentations
        assert all(not event['simulated'] and abs(event['window_ms'] - event['cursor_ms']) < 25
                   for event in presentations), presentations
        result['presentation_trace'] = presentations
        result['medians'] = {
            operation: {metric: statistics.median(round_[operation][metric] for round_ in result['rounds'])
                        for metric in ('presentation_ms', 'observed_presentation_ms',
                                       'requested_to_complete_ms', 'hardware_ms', 'write_ms',
                                       'prepare_ms', 'total_ms')}
            for operation in ('dark', 'light')}
        result['pass'] = True
    except Exception as error:
        result['error'] = str(error)
        raise
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
            try:
                result['baselines_equal_after_exit'] = values(probe('after-exit')) == values(before)
                result['cursor_equal_after_exit'] = cursor_mask() == result.get('cursor_before')
            except Exception as error:
                result['after_exit_error'] = str(error)
        result['pass'] &= (result.get('exit_code') == 0 and not result['pending_recovery']
                           and result.get('baselines_equal_after_exit', False)
                           and result.get('cursor_equal_after_exit', False))
        save(out / 'results.json', result)
        print(json.dumps({key: value for key, value in result.items()
                          if key not in ('rounds', 'presentation_trace')}, ensure_ascii=False), flush=True)
    if not result['pass']:
        raise SystemExit(1)


if __name__ == '__main__':
    main()
