"""Read working set, private commit and GUI handle counts for our own app.

Uses real Native initialization, no simulated backend and no dimming commands.
No working-set trimming: memory figures describe normal resource ownership.
"""
import argparse
import ctypes as C
from ctypes import wintypes as W
import hashlib,json,subprocess,time
from pathlib import Path
import test_windows_ui as ui

k=C.WinDLL('kernel32',use_last_error=True);ps=C.WinDLL('psapi',use_last_error=True)
k.OpenProcess.argtypes=[W.DWORD,W.BOOL,W.DWORD];k.OpenProcess.restype=W.HANDLE
k.CloseHandle.argtypes=[W.HANDLE]
k.GetProcessHandleCount.argtypes=[W.HANDLE,C.POINTER(W.DWORD)]
ui.u.GetGuiResources.argtypes=[W.HANDLE,W.DWORD];ui.u.GetGuiResources.restype=W.DWORD
class Counters(C.Structure):
    _fields_=[('size',W.DWORD),('faults',W.DWORD),('peak_working',C.c_size_t),('working',C.c_size_t),('peak_paged_pool',C.c_size_t),('paged_pool',C.c_size_t),('peak_nonpaged_pool',C.c_size_t),('nonpaged_pool',C.c_size_t),('pagefile',C.c_size_t),('peak_pagefile',C.c_size_t),('private',C.c_size_t)]
ps.GetProcessMemoryInfo.argtypes=[W.HANDLE,C.POINTER(Counters),W.DWORD]
def main():
    parser=argparse.ArgumentParser();parser.add_argument('--exe',required=True);parser.add_argument('--output',required=True);args=parser.parse_args()
    exe=str(Path(args.exe).resolve());out=Path(args.output).resolve();out.mkdir(parents=True,exist_ok=True)
    data=out/'data';data.mkdir(exist_ok=True)
    p=subprocess.Popen([exe,'--silent','--data-dir',str(data)])
    handle=k.OpenProcess(0x1000|0x10,False,p.pid);assert handle
    result={'exe':exe,'sha256':hashlib.sha256(Path(exe).read_bytes()).hexdigest(),'native_backend':True,'measurements':[]}
    def sample(name):
        memory=Counters(size=C.sizeof(Counters));assert ps.GetProcessMemoryInfo(handle,C.byref(memory),memory.size)
        count=W.DWORD();assert k.GetProcessHandleCount(handle,C.byref(count))
        result['measurements'].append({'scenario':name,'working_set_bytes':memory.working,'private_commit_bytes':memory.private,'gdi_objects':ui.u.GetGuiResources(handle,0),'user_objects':ui.u.GetGuiResources(handle,1),'handles':count.value})
    try:
        owner=ui.until(lambda:next((h for h,c in ui.windows(p.pid) if c.startswith('SOGController')),None))
        time.sleep(1);sample('tray_only_cold')
        ui.u.PostMessageW(owner,0x8005,0,0)
        panel=ui.until(lambda:next((h for h,c in ui.windows(p.pid) if c=='SOGRustSettings'),None))
        time.sleep(.3);sample('settings_visible')
        r=ui.bounds(panel);ui.ImageGrab.grab(bbox=(r.left,r.top,r.right,r.bottom),all_screens=True).save(out/'settings.png')
        for _ in range(50):
            ui.u.PostMessageW(panel,0x10,0,0);ui.until(lambda:not ui.u.IsWindowVisible(panel))
            ui.u.PostMessageW(owner,0x8005,0,0);ui.until(lambda:ui.u.IsWindowVisible(panel))
        time.sleep(.3);sample('settings_after_50_hide_show')
        ui.u.PostMessageW(panel,0x10,0,0);ui.until(lambda:not ui.u.IsWindowVisible(panel));time.sleep(.3);sample('tray_after_settings_hidden')
        result['pass']=True
    finally:
        if p.poll() is None:
            ui.u.PostMessageW(owner,0x10,0,0);p.wait(timeout=15)
        k.CloseHandle(handle)
        (out/'results.json').write_text(json.dumps(result,ensure_ascii=False,indent=2),encoding='utf-8')
        print(json.dumps(result,ensure_ascii=False))
if __name__=='__main__':main()
