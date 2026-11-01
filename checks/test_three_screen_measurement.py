"""Opt-in real two-DDC/one-WMI acceptance of the exact application EXE.

Hidden controller messages only; no UI-test/simulated backend, foreground or
input injection. Always request normal restoration/exit; never force-kill.
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
from test_session_latency import cursor_mask

k=C.WinDLL('kernel32',use_last_error=True)
k.CreateFileW.argtypes=[ui.W.LPCWSTR,ui.W.DWORD,ui.W.DWORD,C.c_void_p,ui.W.DWORD,ui.W.DWORD,ui.W.HANDLE]
k.CreateFileW.restype=ui.W.HANDLE
k.ReadFile.argtypes=[ui.W.HANDLE,C.c_void_p,ui.W.DWORD,C.POINTER(ui.W.DWORD),C.c_void_p]
k.CloseHandle.argtypes=[ui.W.HANDLE]


def read_shared(path):
    handle=k.CreateFileW(str(path),0x80000000,7,None,3,0x80,None)
    if handle in (None,C.c_void_p(-1).value):return None
    try:
        buf=C.create_string_buffer(262144);size=ui.W.DWORD()
        if not k.ReadFile(handle,buf,len(buf),C.byref(size),None):return None
        return json.loads(buf.raw[:size.value])
    finally:k.CloseHandle(handle)


def values(probe):
    return {(d['id'],d['kind']):d['value'] for d in probe['inventory']['devices']}


def main():
    parser=argparse.ArgumentParser()
    parser.add_argument('--exe',required=True)
    parser.add_argument('--output',required=True)
    parser.add_argument('--rounds',type=int,default=3,choices=range(1,4))
    args=parser.parse_args()
    exe=str(Path(args.exe).resolve());out=Path(args.output).resolve();out.mkdir(parents=True,exist_ok=True)
    data=out/'data';data.mkdir(exist_ok=True)
    assert not (Path(os.environ['APPDATA'])/'ScreenOffGuard/recovery.json').exists(),'Daily hardware operation active'
    assert not (data/'recovery.json').exists(),'Restore the previous test first'
    assert not (data/'hardware-measurement.json').exists(),'Use a fresh evidence directory'
    cfg={'schema_version':1,'hotkey':{'modifiers':3,'key':120},'close_action':'exit','autostart':False,
        'dim_internal':True,'dim_external':True,'hdr_off':False,'cursor_mode':'keep',
        'dim_order':'black_first','restore_mode':'ready','delay_ms':0}
    (data/'settings.json').write_text(json.dumps(cfg),encoding='utf8')
    result={'sha256':hashlib.sha256(Path(exe).read_bytes()).hexdigest(),'pass':False,'rounds':[],
        'scope':'Authorized native three-screen brightness only; HDR unchanged',
        'dispatch':'WM_PANEL Measure on hidden controller; exact EXE real Native backend, private data'}
    process=None;owner=None;before=None
    def probe(name):
        path=out/(name+'.json')
        subprocess.run([exe,'--probe',str(path)],check=True)
        return json.loads(path.read_text(encoding='utf8'))
    try:
        before=probe('before')
        devices=[d for d in before['inventory']['devices'] if d['kind'] in ('external','internal')]
        assert len(devices)==3 and sum(d['kind']=='external' for d in devices)==2,devices
        expected={(d['id'],d['kind']) for d in devices}
        result['cursor_before']=cursor_mask()
        process=subprocess.Popen([exe,'--data-dir',str(data),'--silent'],creationflags=0x08000000)
        owner=ui.until(lambda:next((h for h,c in ui.windows(process.pid) if c.startswith('SOGController')),None),timeout=10)
        time.sleep(.3)
        assert process.poll() is None
        assert not any(c=='SOGRustSettings' for h,c in ui.windows(process.pid))
        last_generation=0
        for number in range(1,args.rounds+1):
            begin=time.perf_counter()
            ui.message(owner,0x8002,2071)
            def measured():
                value=read_shared(data/'hardware-measurement.json')
                return value if value and value['generation']>last_generation else None
            trace=ui.until(measured,timeout=15)
            wall_ms=(time.perf_counter()-begin)*1000
            last_generation=trace['generation']
            (out/f'round-{number}.json').write_text(json.dumps(trace,ensure_ascii=False,indent=2),encoding='utf8')
            assert not trace.get('dark_error') and not trace.get('light_error'),trace
            phases={}
            for name in ['dark','light']:
                phase=trace[name]
                assert phase and not phase['warnings'] and not phase['superseded'],trace
                assert phase['channels']==3 and len(phase['changes'])==3,trace
                changes=phase['changes']
                assert {(c['id'],c['kind']) for c in changes}==expected
                assert all(c['result']=={'Ok':True} and c['stage']=='brightness' and c['pre_read_ms']==0 and c['verify_reads']>=1 for c in changes),trace
                latest_start=max(c['started_ms'] for c in changes)
                earliest_finish=min(c['started_ms']+c['elapsed_ms'] for c in changes)
                assert latest_start<earliest_finish,('Hardware lanes did not all overlap',trace)
                phases[name]={'hardware_ms':phase['elapsed_ms'],'write_ms':phase['write_ms'],
                    'prepare_ms':phase['prepare_ms'],'total_ms':phase['total_ms'],
                    'start_spread_ms':latest_start-min(c['started_ms'] for c in changes),
                    'all_three_overlap_ms':earliest_finish-latest_start,'channels':changes}
            assert not (data/'recovery.json').exists(),'Restore still pending'
            expected_delay=min(500,max(trace['dark']['elapsed_ms'],trace['light']['elapsed_ms']))
            ui.until(lambda:(v:=read_shared(data/'settings.json')) and v.get('delay_ms')==expected_delay,timeout=8)
            after=probe(f'after-round-{number}')
            assert values(after)==values(before),'Per-screen brightness/HDR baseline mismatch'
            assert cursor_mask()==result['cursor_before'],'Measurement changed the system arrow'
            result['rounds'].append({'round':number,'pass':True,'observed_wall_ms':wall_ms,
                'saved_delay_ms':expected_delay,'dark':phases['dark'],'light':phases['light'],'baselines_equal':True})
            (out/'results.json').write_text(json.dumps(result,ensure_ascii=False,indent=2),encoding='utf8')
        result['pass']=True
    except Exception as error:
        result['error']=str(error)
        raise
    finally:
        if owner and ui.u.IsWindow(owner):ui.u.PostMessageW(owner,0x10,0,0)
        if process:
            try:process.wait(timeout=20)
            except subprocess.TimeoutExpired:result['restoring_pid']=process.pid
            result['exit_code']=process.poll()
        result['pending_recovery']=(data/'recovery.json').exists()
        if before and (not process or process.poll() is not None):
            after=probe('after-exit')
            result['baselines_equal_after_exit']=values(after)==values(before)
        result['pass'] &= result.get('exit_code')==0 and not result['pending_recovery'] and result.get('baselines_equal_after_exit',False)
        (out/'results.json').write_text(json.dumps(result,ensure_ascii=False,indent=2),encoding='utf8')
        print(json.dumps({**result,'rounds':[{k:v for k,v in r.items() if k not in ('dark','light')} for r in result['rounds']]},ensure_ascii=False))
    if not result['pass']:raise SystemExit(1)


if __name__=='__main__':main()
