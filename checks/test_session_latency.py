"""Native backend acceptance with bounded BM_CLICK, private data and restoration.

measure: briefly dims monitors and requires exact baseline restoration.
cursor: leaves monitor brightness unchanged and checks the real system arrow mask.
Programmatic control dispatch is reported separately from real SendInput UI coverage.
"""
import argparse
import ctypes as C
from ctypes import wintypes as W
import hashlib
import json
from pathlib import Path
import subprocess
import time
import test_windows_ui as ui

class IconInfo(C.Structure):
    _fields_=[('icon',W.BOOL),('x',W.DWORD),('y',W.DWORD),('mask',W.HANDLE),('color',W.HANDLE)]
class Bitmap(C.Structure):
    _fields_=[('kind',W.LONG),('width',W.LONG),('height',W.LONG),('stride',W.LONG),('planes',W.WORD),('bpp',W.WORD),('bits',C.c_void_p)]
g=C.WinDLL('gdi32',use_last_error=True)
ui.u.LoadCursorW.argtypes=[W.HINSTANCE,C.c_void_p];ui.u.LoadCursorW.restype=W.HANDLE
ui.u.GetIconInfo.argtypes=[W.HANDLE,C.POINTER(IconInfo)]
g.GetObjectW.argtypes=[W.HANDLE,C.c_int,C.c_void_p]
g.GetBitmapBits.argtypes=[W.HANDLE,W.LONG,C.c_void_p]
g.DeleteObject.argtypes=[W.HANDLE]

def cursor_mask():
    cursor=ui.u.LoadCursorW(None,C.c_void_p(32512));assert cursor
    info=IconInfo();assert ui.u.GetIconInfo(cursor,C.byref(info))
    try:
        bitmap=Bitmap();assert g.GetObjectW(info.mask,C.sizeof(bitmap),C.byref(bitmap))
        size=bitmap.stride*bitmap.height;assert 0<size<1024*1024
        data=C.create_string_buffer(size);assert g.GetBitmapBits(info.mask,size,data)==size
        raw=data.raw;half=size//2
        transparent=not info.color and raw[:half]==b'\xff'*half and raw[half:]==b'\0'*half
        return {'transparent':bool(transparent),'width':bitmap.width,'height':bitmap.height,'color':bool(info.color),'mask_sha256':hashlib.sha256(raw).hexdigest()}
    finally:
        if info.mask:g.DeleteObject(info.mask)
        if info.color:g.DeleteObject(info.color)

def main():
    parser=argparse.ArgumentParser();parser.add_argument('--exe',required=True);parser.add_argument('--output',required=True)
    parser.add_argument('--mode',choices=['measure','cursor'],required=True);parser.add_argument('--expect-remote',type=int,choices=[0,1])
    args=parser.parse_args();exe=str(Path(args.exe).resolve());out=Path(args.output).resolve();out.mkdir(parents=True,exist_ok=True)
    data=out/'data';data.mkdir(exist_ok=True)
    assert not (data/'recovery.json').exists(),'Use a fresh evidence directory or recover the previous run first'
    cfg={'schema_version':1,'hotkey':{'modifiers':3,'key':120},'close_action':'exit','delay_ms':0,
         'cursor_mode':'auto' if args.mode=='cursor' else 'keep','dim_internal':args.mode=='measure','dim_external':args.mode=='measure'}
    (data/'settings.json').write_text(json.dumps(cfg),encoding='utf-8')
    results={'exe':exe,'sha256':hashlib.sha256(Path(exe).read_bytes()).hexdigest(),'mode':args.mode,'dispatch':'bounded BM_CLICK','pass':False}
    def probe(name):
        path=out/(name+'.json');subprocess.run([exe,'--probe',str(path)],timeout=10,check=True)
        return json.loads(path.read_text(encoding='utf-8'))
    before=probe('before');results['remote']=before.get('remote')
    if args.expect_remote is not None:
        assert before['remote']['active']==bool(args.expect_remote),before['remote']
    baseline=lambda value:{(d['id'],d['kind']):d['value'] for d in value['inventory']['devices']}
    process=subprocess.Popen([exe,'--data-dir',str(data)]);root=None
    try:
        root=ui.until(lambda:next((h for h,c in ui.windows(process.pid) if c=='SOGRustSettings'),None),timeout=10)
        status=lambda:ui.window_text(ui.control(process.pid,root,2096))
        ui.until(lambda:status()=='就绪',timeout=10)
        results['cursor_before']=cursor_mask()
        if args.mode=='measure':
            ui.message(ui.control(process.pid,root,2071),0xF5)
            ui.until(lambda:(data/'hardware-measurement.json').exists(),timeout=15)
            trace=json.loads((data/'hardware-measurement.json').read_text(encoding='utf-8'))
            ui.until(lambda:status()=='已保存',timeout=6)
            saved=json.loads((data/'settings.json').read_text(encoding='utf-8'))['delay_ms']
            phases=[trace['dark'],trace['light']]
            assert all(phase and not phase['warnings'] and not phase['superseded'] for phase in phases),trace
            assert all(change['result']=={'Ok':True} for phase in phases for change in phase['changes']),trace
            # The full confirmation time remains the saved value; write-only time
            # must never be silently substituted to manufacture a smaller number.
            assert saved==min(500,max(phase['elapsed_ms'] for phase in phases)),(saved,trace)
            for phase in phases:
                brightness=[c for c in phase['changes'] if c['kind'] in ('external','internal')]
                assert len(brightness)==3 and sum(c['kind']=='external' for c in brightness)==2, 'This acceptance requires both DDC screens and the WMI screen'
                assert max(c['started_ms'] for c in brightness)<min(c['started_ms']+c['elapsed_ms'] for c in brightness), 'All three hardware lanes must overlap'
                assert all(c['pre_read_ms']==0 and c['verify_reads']>=1 for c in phase['changes']),trace
            results.update(measurement=trace,saved_delay_ms=saved)
        else:
            assert not results['cursor_before']['transparent'],'Pointer was already hidden before the test'
            ui.message(ui.control(process.pid,root,2095),0xF5)
            ui.until(lambda:status().startswith('已熄屏'),timeout=10)
            journal=json.loads((data/'recovery.json').read_text(encoding='utf-8'))
            results['cursor_dark']=cursor_mask();results['journal_cursor_hidden']=journal['cursor_hidden']
            expected=not bool(args.expect_remote)
            assert journal['entries']==[],'Cursor test changed monitor brightness'
            assert journal['cursor_hidden']==expected
            assert results['cursor_dark']['transparent']==expected,results
            ui.message(ui.control(process.pid,root,2095),0xF5)
            ui.until(lambda:status()=='屏幕已恢复',timeout=10)
        assert not (data/'recovery.json').exists(),'Recovery journal still pending'
        after=probe('after');assert baseline(before)==baseline(after),'Monitor baseline was not restored'
        results['cursor_after']=cursor_mask();assert not results['cursor_after']['transparent']
        results['baselines_equal']=True
        ui.u.SetWindowPos(root,C.c_void_p(-1),0,0,0,0,3)
        r=ui.bounds(root);ui.ImageGrab.grab(bbox=(r.left,r.top,r.right,r.bottom),all_screens=True).save(out/'completed.png')
        results['pass']=True
    except Exception as error:
        results['error']=str(error)
        if root and ui.u.IsWindow(root):results['status_on_failure']=status()
        raise
    finally:
        if root and ui.u.IsWindow(root):ui.u.PostMessageW(root,0x10,0,0)
        try:process.wait(timeout=20)
        except subprocess.TimeoutExpired:results['restoring_pid']=process.pid
        results['exit_code']=process.poll()
        (out/'results.json').write_text(json.dumps(results,ensure_ascii=False,indent=2),encoding='utf-8')
        print(json.dumps(results,ensure_ascii=False))

if __name__=='__main__':main()
