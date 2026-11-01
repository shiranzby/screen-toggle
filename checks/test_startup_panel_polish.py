"""Merged startup, manual launch, scrollbar-free scrolling and quiet About.

Uses the real UI/native registry code with isolated data and a private Run branch.
No user Run key, login session, hardware brightness or system sound is changed.
"""
import argparse
import ctypes as C
import hashlib
import json
from pathlib import Path
import subprocess
import time
import winreg
import test_windows_ui as ui


def main():
    parser=argparse.ArgumentParser()
    parser.add_argument('--exe',required=True)
    parser.add_argument('--output',required=True)
    args=parser.parse_args()
    exe=str(Path(args.exe).resolve());out=Path(args.output).resolve();out.mkdir(parents=True,exist_ok=True)
    data=out/'data';data.mkdir(exist_ok=True)
    (data/'settings.json').write_text(json.dumps({'theme':'dark','autostart':False,'silent_start':True,
        'hotkey':{'modifiers':3,'key':120},'close_action':'hide'}),encoding='utf-8')
    result={'sha256':hashlib.sha256(Path(exe).read_bytes()).hexdigest(),'pass':False,'cases':[],
            'dispatch':'bounded BM_CLICK / Windows messages; actual subprocess startup command',
            'registry':'private HKCU/Software/ScreenOffGuard/Tests branch'}
    d=ui.Driver(exe,out,result)
    ui.u.GetWindowLongPtrW.argtypes=[ui.W.HWND,C.c_int];ui.u.GetWindowLongPtrW.restype=C.c_ssize_t
    ui.u.IsWindowEnabled.argtypes=[ui.W.HWND]
    command=lambda ident:ui.message(d.ctl(ident),0xf5)
    def current_run():
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER,d.registry_key) as key:
            return winreg.QueryValueEx(key,'ScreenOffGuardRust')[0]
    def attach_started(process):
        d.process=process;d.pid=process.pid
        d.owner=ui.until(lambda:next((h for h,c in ui.windows(d.pid) if c.startswith('SOGController')),None))
        ui.until(lambda:d.status().get('pid')==d.pid and d.status().get('ready'))
        d.hwnd=next((h for h,c in ui.windows(d.pid) if c=='SOGRustSettings'),None)
    def about(name):
        command(2092)
        dialog=ui.until(lambda:next((h for h,c in ui.windows(d.pid) if c=='SOGRustAbout'),None))
        assert not ui.u.IsWindowEnabled(d.hwnd)
        title=ui.window_text(ui.control(d.pid,dialog,10))
        body=ui.window_text(ui.control(d.pid,dialog,11))
        assert title=='一键熄屏 · Rust 0.3.0'
        assert all(word in body for word in ['Rust','DDC/CI','WMI','黑窗','恢复'])
        assert not any(word in body for word in ['Fluent','WinUI','风格'])
        d.shot(name,dialog)
        return dialog
    try:
        d.start()
        assert d.hwnd and ui.u.IsWindowVisible(d.hwnd), 'Legacy silent_start hid a manual launch'
        ids={ui.u.GetDlgCtrlID(h) for h,c in ui.windows(d.pid,d.hwnd)}
        assert 2051 in ids and 2052 not in ids
        command(2051);d.wait_saved('autostart',True)
        stored=json.loads((data/'settings.json').read_text(encoding='utf-8'))
        assert 'silent_start' not in stored
        registered=ui.until(lambda:current_run() if '--silent' in current_run() else None)
        assert registered.startswith('"'+exe+'"') and str(data) in registered and '--ui-test' in registered
        result['startup_command']=registered
        d.shot('merged-startup-panel')
        d.quit();assert d.process.returncode==0

        # Launch exactly the registered startup string, including its private profile.
        attach_started(subprocess.Popen(registered))
        assert not d.hwnd, 'Autostart opened a settings panel'
        ui.message(d.owner,0x111,1003)  # About from the silent tray controller
        tray_about=ui.until(lambda:next((h for h,c in ui.windows(d.pid) if c=='SOGRustAbout'),None))
        d.shot('about-from-silent-tray-typec',tray_about)
        ui.message(tray_about,0x10)
        ui.until(lambda:not ui.u.IsWindow(tray_about))
        pid=d.pid
        assert subprocess.run(registered,timeout=10).returncode==0
        assert not any(c=='SOGRustSettings' for h,c in ui.windows(pid)), 'Repeated silent launch revealed the panel'
        assert subprocess.run([exe,'--ui-test','--data-dir',str(data)],timeout=10).returncode==0
        d.hwnd=ui.until(lambda:next((h for h,c in ui.windows(pid) if c=='SOGRustSettings'),None))
        ui.until(lambda:ui.u.IsWindowVisible(d.hwnd))
        ui.message(d.hwnd,0,timeout=3000)
        ui.u.SetWindowPos(d.hwnd,C.c_void_p(-1),0,0,0,0,0x13)
        d.shot('manual-launch-reveals-running-tray')
        d.quit();d.start()
        assert d.hwnd and ui.u.IsWindowVisible(d.hwnd), 'Autostart preference hid a new manual launch'
        result['cases'].append({'case':'merged_autostart_silent_only_on_startup_command_manual_always_visible','pass':True})

        # Native bar styles stay absent through scrolling, resize and theme changes.
        r=ui.bounds(d.hwnd)
        geometry=d.display.rectangle(1150,1400)
        ui.u.SetWindowPos(d.hwnd,None,*geometry,0x14)
        ui.message(d.hwnd,0,timeout=3000)
        body=next(h for h,c in ui.windows(d.pid,d.hwnd) if c=='SOGRustSettingsBody')
        assert ui.u.GetWindowLongPtrW(body,-16)&0x300000==0
        before=ui.bounds(d.ctl(2001)).top
        ui.message(body,0x20a,((-120)&0xffff)<<16)
        after=ui.bounds(d.ctl(2001)).top
        assert after<before
        ui.message(body,0x100,0x23)  # End
        d.shot('hidden-scrollbar-bottom-dark')
        assert ui.u.GetWindowLongPtrW(body,-16)&0x300000==0
        ui.message(body,0x100,0x24)  # Home
        assert ui.bounds(d.ctl(2001)).top==before
        command(2111);d.wait_saved('theme','light')
        assert ui.u.GetWindowLongPtrW(body,-16)&0x300000==0
        d.shot('hidden-scrollbar-light')
        result['cases'].append({'case':'no_scrollbar_with_wheel_keyboard_and_theme_switch','pass':True})

        dialog=about('about-light')
        ui.message(ui.control(d.pid,dialog,1),0xf5)
        ui.until(lambda:not ui.u.IsWindow(dialog))
        assert ui.u.IsWindowEnabled(d.hwnd)
        command(2112);d.wait_saved('theme','dark')
        dialog=about('about-dark')
        ui.u.PostMessageW(dialog,0x100,0x1b,0)
        ui.until(lambda:not ui.u.IsWindow(dialog))
        assert ui.u.IsWindowEnabled(d.hwnd)
        # Opening/closing the quiet custom window releases fonts/brushes.
        kernel=C.WinDLL('kernel32');kernel.OpenProcess.argtypes=[ui.W.DWORD,ui.W.BOOL,ui.W.DWORD];kernel.OpenProcess.restype=ui.W.HANDLE
        kernel.CloseHandle.argtypes=[ui.W.HANDLE];ui.u.GetGuiResources.argtypes=[ui.W.HANDLE,ui.W.DWORD]
        handle=kernel.OpenProcess(0x400,False,d.pid);assert handle
        try:
            gdi_before=ui.u.GetGuiResources(handle,0)
            for _ in range(15):
                command(2092)
                dialog=ui.until(lambda:next((h for h,c in ui.windows(d.pid) if c=='SOGRustAbout'),None))
                ui.u.PostMessageW(dialog,0x10,0,0);ui.until(lambda:not ui.u.IsWindow(dialog))
            time.sleep(.1);gdi_after=ui.u.GetGuiResources(handle,0)
        finally:kernel.CloseHandle(handle)
        assert gdi_after<=gdi_before+2,(gdi_before,gdi_after)
        result['cases'].append({'case':'quiet_about_content_ok_escape_close_and_resource_release','pass':True,'gdi_before':gdi_before,'gdi_after':gdi_after,
                                'sound_evidence':'About uses a custom window, not MessageBox/MessageBeep; audio loopback not recorded'})
        command(2051);d.wait_saved('autostart',False)
        try:current_run()
        except FileNotFoundError:pass
        else:raise AssertionError('Disabling autostart left its private Run value')
        result['pass']=True
    except Exception as error:
        result['error']=str(error)
        if d.hwnd and ui.u.IsWindow(d.hwnd):d.shot('FAIL-current-state')
        raise
    finally:
        d.quit();result['exit_code']=d.process.returncode if d.process else None;d.write()
        print(json.dumps(result,ensure_ascii=False))


if __name__=='__main__':main()
