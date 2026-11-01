"""Exercise the built Windows app with bounded Win32 calls and real SendInput.

Owns only the process it starts. --startup-only is safe against a deadlocked build.
"""
import argparse
import ctypes as C
from ctypes import wintypes as W
import json
import os
from pathlib import Path
import subprocess
import time
import hashlib
import traceback
import uuid
import winreg
from PIL import ImageGrab

u = C.WinDLL("user32", use_last_error=True)
u.EnumWindows.argtypes = [C.c_void_p, W.LPARAM]
u.GetWindowThreadProcessId.argtypes = [W.HWND, C.POINTER(W.DWORD)]
u.GetClassNameW.argtypes = [W.HWND, W.LPWSTR, C.c_int]
u.SendMessageTimeoutW.argtypes = [W.HWND, W.UINT, W.WPARAM, W.LPARAM, W.UINT, W.UINT, C.POINTER(C.c_size_t)]
u.SendMessageTimeoutW.restype = C.c_ssize_t
u.EnumChildWindows.argtypes = [W.HWND, C.c_void_p, W.LPARAM]
u.GetDlgItem.argtypes = [W.HWND, C.c_int]
u.GetDlgItem.restype = W.HWND
u.IsWindowVisible.argtypes = [W.HWND]
u.GetWindowRect.argtypes = [W.HWND, C.POINTER(W.RECT)]
u.GetClientRect.argtypes = [W.HWND, C.POINTER(W.RECT)]
u.ClientToScreen.argtypes = [W.HWND, C.POINTER(W.POINT)]
u.GetDlgCtrlID.argtypes = [W.HWND]
u.GetParent.argtypes = [W.HWND]
u.GetParent.restype = W.HWND
u.SetForegroundWindow.argtypes = [W.HWND]
u.IsZoomed.argtypes = [W.HWND]
u.GetAncestor.argtypes = [W.HWND, W.UINT]
u.GetAncestor.restype = W.HWND
u.WindowFromPoint.argtypes = [W.POINT]
u.WindowFromPoint.restype = W.HWND
u.SetCursorPos.argtypes = [C.c_int, C.c_int]
u.GetCursorPos.argtypes = [C.POINTER(W.POINT)]
u.PostMessageW.argtypes = [W.HWND, W.UINT, W.WPARAM, W.LPARAM]
u.GetDpiForWindow.argtypes = [W.HWND]
u.GetDpiForWindow.restype = W.UINT
u.IsWindow.argtypes = [W.HWND]
u.SetWindowPos.argtypes = [W.HWND,W.HWND,C.c_int,C.c_int,C.c_int,C.c_int,W.UINT]
u.RegisterHotKey.argtypes = [W.HWND,C.c_int,W.UINT,W.UINT]
u.UnregisterHotKey.argtypes = [W.HWND,C.c_int]
u.SetProcessDpiAwarenessContext.argtypes = [C.c_void_p]
u.SetProcessDpiAwarenessContext(C.c_void_p(-4))
CALLBACK = C.WINFUNCTYPE(W.BOOL, W.HWND, W.LPARAM)

class Mouse(C.Structure):
    _fields_=[("dx",W.LONG),("dy",W.LONG),("mouseData",W.DWORD),("dwFlags",W.DWORD),("time",W.DWORD),("extra",C.c_size_t)]
class Keyboard(C.Structure):
    _fields_=[("vk",W.WORD),("scan",W.WORD),("flags",W.DWORD),("time",W.DWORD),("extra",C.c_size_t)]
class InputUnion(C.Union):
    _fields_=[("mouse",Mouse),("key",Keyboard)]
class Input(C.Structure):
    _anonymous_=("data",)
    _fields_=[("type",W.DWORD),("data",InputUnion)]
u.SendInput.argtypes=[W.UINT,C.POINTER(Input),C.c_int]

def send(*items):
    events=(Input*len(items))(*items)
    assert u.SendInput(len(events),events,C.sizeof(Input))==len(events),f"SendInput failed: {C.get_last_error()}"

def key(vk,up=False):
    return Input(type=1,key=Keyboard(vk=vk,flags=2 if up else 0))

def press(*keys):
    send(*[key(v) for v in keys],*[key(v,True) for v in reversed(keys)])
    time.sleep(.12)

def bounds(hwnd):
    r=W.RECT();assert u.GetWindowRect(hwnd,C.byref(r));return r

def window_text(hwnd):
    buf=C.create_unicode_buffer(2048)
    message(hwnd,0xD,2048,C.addressof(buf))
    return buf.value

def control(pid,root,ident):
    return next((h for h,_ in windows(pid,root) if u.GetDlgCtrlID(h)==ident),None)

def click(hwnd):
    assert hwnd,"Missing control"
    parent=u.GetParent(hwnd)
    cls=C.create_unicode_buffer(128);u.GetClassNameW(parent,cls,128)
    if cls.value=="SOGRustSettingsBody":
        for _ in range(25):
            r=bounds(hwnd);b=bounds(parent)
            if r.top>=b.top and r.bottom<=b.bottom:break
            delta=120 if r.top<b.top else -120
            message(parent,0x20A,(delta&0xffff)<<16)
        else:raise AssertionError("Could not scroll control into view")
    root=u.GetAncestor(hwnd,2);u.SetForegroundWindow(root);time.sleep(.05)
    r=bounds(hwnd);point=W.POINT((r.left+r.right)//2,(r.top+r.bottom)//2)
    assert u.SetCursorPos(point.x,point.y), 'Windows rejected local cursor positioning; desktop input is unavailable for this test'
    time.sleep(.05)
    actual=W.POINT();assert u.GetCursorPos(C.byref(actual))
    assert (actual.x,actual.y)==(point.x,point.y), f'Cursor moved before click: expected {(point.x,point.y)}, actual {(actual.x,actual.y)}'
    hit=u.WindowFromPoint(point)
    assert hit==hwnd,f"Click intercepted: expected {hwnd}, hit {hit}"
    send(Input(type=0,mouse=Mouse(dwFlags=2)));time.sleep(.07);send(Input(type=0,mouse=Mouse(dwFlags=4)))
    actual=W.POINT();assert u.GetCursorPos(C.byref(actual))
    assert (actual.x,actual.y)==(point.x,point.y), f'Desktop pointer changed during click: expected {(point.x,point.y)}, actual {(actual.x,actual.y)}'
    time.sleep(.12)

def type_text(hwnd,value):
    click(hwnd);press(0x11,0x41)
    events=[]
    for ch in value:
        events.extend([Input(type=1,key=Keyboard(scan=ord(ch),flags=4)),Input(type=1,key=Keyboard(scan=ord(ch),flags=6))])
    if events:send(*events)
    else:press(0x08)
    time.sleep(.18)


def message(hwnd, msg, w=0, l=0, timeout=600):
    result = C.c_size_t()
    if not u.SendMessageTimeoutW(hwnd, msg, w, l, 2, timeout, C.byref(result)):
        raise AssertionError(f"Window did not answer message 0x{msg:x} within {timeout} ms")
    return result.value


def windows(pid, child=None):
    found = []
    @CALLBACK
    def collect(hwnd, _):
        owner = W.DWORD()
        u.GetWindowThreadProcessId(hwnd, C.byref(owner))
        if pid is None or owner.value == pid:
            cls = C.create_unicode_buffer(128)
            u.GetClassNameW(hwnd, cls, 128)
            found.append((hwnd, cls.value))
        return True
    if child:
        u.EnumChildWindows(child, collect, 0)
    else:
        u.EnumWindows(collect, 0)
    return found


def until(test, timeout=6):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = test()
        if result:
            return result
        time.sleep(.05)
    raise AssertionError("Timed out waiting for expected app state")

class Guid(C.Structure):
    _fields_=[("raw",C.c_ubyte*16)]
class IconId(C.Structure):
    _fields_=[("size",W.DWORD),("hwnd",W.HWND),("id",W.UINT),("guid",Guid)]
shell=C.WinDLL("shell32",use_last_error=True)
shell.Shell_NotifyIconGetRect.argtypes=[C.POINTER(IconId),C.POINTER(W.RECT)]
shell.Shell_NotifyIconGetRect.restype=C.c_long
ole=C.OleDLL('ole32');ole.CoInitialize(None)
oleacc=C.OleDLL('oleacc')
oleacc.AccessibleObjectFromWindow.argtypes=[W.HWND,W.DWORD,C.POINTER(Guid),C.POINTER(C.c_void_p)]
class Variant(C.Structure):
    _fields_=[('vt',W.WORD),('reserved',W.WORD*3),('data',C.c_byte*16)]
oleacc.AccessibleChildren.argtypes=[C.c_void_p,W.LONG,W.LONG,C.POINTER(Variant),C.POINTER(W.LONG)]
aut=C.OleDLL('oleaut32');aut.SysFreeString.argtypes=[C.c_void_p];aut.SysFreeString.restype=None
def accessible_items(hwnd):
    ptr=C.c_void_p();iid=Guid.from_buffer_copy(uuid.UUID('618736e0-3c3d-11cf-810c-00aa00389b71').bytes_le)
    oleacc.AccessibleObjectFromWindow(hwnd,0xFFFFFFFC,C.byref(iid),C.byref(ptr))
    table=C.cast(ptr,C.POINTER(C.POINTER(C.c_void_p))).contents
    method=lambda i,*types:C.WINFUNCTYPE(C.c_long,C.c_void_p,*types)(table[i])
    count=W.LONG();method(8,C.POINTER(W.LONG))(ptr,C.byref(count))
    children=(Variant*count.value)();got=W.LONG();oleacc.AccessibleChildren(ptr,0,count,children,C.byref(got))
    found=[]
    try:
        for child in children[:got.value]:
            if child.vt!=3:continue
            name=C.c_void_p();method(10,Variant,C.POINTER(C.c_void_p))(ptr,child,C.byref(name))
            title=C.wstring_at(name.value) if name.value else '';aut.SysFreeString(name)
            x,y,width,height=(W.LONG() for _ in range(4))
            method(22,C.POINTER(W.LONG),C.POINTER(W.LONG),C.POINTER(W.LONG),C.POINTER(W.LONG),Variant)(ptr,C.byref(x),C.byref(y),C.byref(width),C.byref(height),child)
            found.append((title,W.RECT(x.value,y.value,x.value+width.value,y.value+height.value)))
    finally:method(2)(ptr)
    return found
u.GetMenu.argtypes=[W.HWND];u.GetMenu.restype=W.HMENU
u.GetMenuItemRect.argtypes=[W.HWND,W.HMENU,W.UINT,C.POINTER(W.RECT)]
u.GetMenuItemID.argtypes=[W.HMENU,C.c_int];u.GetMenuItemID.restype=W.UINT
u.GetMenuItemCount.argtypes=[W.HMENU]
u.GetSubMenu.argtypes=[W.HMENU,C.c_int];u.GetSubMenu.restype=W.HMENU

class Driver:
    def __init__(self,exe,output,results):
        self.exe=exe;self.output=output;self.data=output/"data";self.results=results;self.process=None
        self.covered=set();self.screens=[];self._last_status={}
        self.registry_key=None
        self.display=None
    def start(self,*flags):
        from display_target import configure_typec
        self.display=configure_typec()
        self.results["test_display"]=self.display.record
        self.process=subprocess.Popen([self.exe,"--ui-test","--data-dir",str(self.data),*flags])
        self.pid=self.process.pid
        self.owner=until(lambda:next((h for h,c in windows(self.pid) if c.startswith("SOGController")),None))
        cls=next(c for h,c in windows(self.pid) if h==self.owner)
        self.registry_key='Software\\ScreenOffGuard\\Tests\\'+cls.removeprefix('SOGController')
        until(lambda:self.status().get("pid")==self.pid and self.status().get("ready"))
        self.hwnd=next((h for h,c in windows(self.pid) if c=="SOGRustSettings"),None)
        if self.hwnd and u.IsWindowVisible(self.hwnd):
            message(self.hwnd,0,timeout=3000)
            # Pin only this disposable test window so captures cannot silently
            # contain an unrelated app when foreground activation is refused.
            u.SetWindowPos(self.hwnd,C.c_void_p(-1),0,0,0,0,0x13)
            self.display.assert_contains(tuple(getattr(bounds(self.hwnd), key) for key in ["left","top","right","bottom"]))
            time.sleep(.3)
    def status(self):
        try:self._last_status=json.loads((self.data/"status.json").read_text(encoding="utf-8"))
        except (OSError,ValueError):pass
        return self._last_status
    def saved(self):return self.status().get("saved",{})
    def ctl(self,ident):return control(self.pid,self.hwnd,ident)
    def tap(self,ident):
        click(self.ctl(ident));self.covered.add(ident);message(self.hwnd,0)
    def wait_saved(self,field,value):
        until(lambda:self.saved().get(field)==value and not self.status().get("saving"))
        data=json.loads((self.data/"settings.json").read_text(encoding="utf-8"))
        assert data[field]==value,(field,value,data[field])
    def edit(self,ident,value):
        type_text(self.ctl(ident),value);self.covered.add(ident)
    def shot(self,name,window=None):
        root=window or self.hwnd
        if root and u.IsWindowVisible(root):
            if self.display:self.display.assert_contains(tuple(getattr(bounds(root),key) for key in ["left","top","right","bottom"]))
            time.sleep(.14);r=bounds(root)
            path=self.output/f"{name}.png"
            if path.exists():path=self.output/f"{name}-{len(self.screens)+1}.png"
            ImageGrab.grab(bbox=(r.left,r.top,r.right,r.bottom),all_screens=True).save(path)
            self.screens.append(path.name)
    def case(self,name,fn):
        before=len(self.screens)
        try:
            fn();self.results["cases"].append({"case":name,"pass":True,"screenshots":self.screens[before:]})
            print("PASS",name,flush=True)
        except Exception as exc:
            self.shot("FAIL-"+name)
            self.results["cases"].append({"case":name,"pass":False,"error":str(exc),"trace":traceback.format_exc(),"screenshots":self.screens[before:]})
            raise
        finally:self.write()
    def write(self):
        self.results["covered_controls"]=sorted(self.covered)
        self.results["screenshots"]=self.screens
        (self.output/"results.json").write_text(json.dumps(self.results,ensure_ascii=False,indent=2),encoding="utf-8")
    def quit(self):
        if self.process and self.process.poll() is None:
            u.PostMessageW(self.owner,0x10,0,0)
            try:self.process.wait(timeout=8)
            except subprocess.TimeoutExpired:self.process.terminate();self.process.wait(timeout=5)
    def restored(self):
        try:
            data=json.loads((self.data/"simulated-hardware.json").read_text(encoding="utf-8"))
            values={d['id']:d['value'] for d in data['devices']}
            return values=={'sim-external':72,'sim-internal':38,'sim-hdr':1} and not data['cursor_hidden'] and not (self.data/'recovery.json').exists()
        except (OSError,ValueError):return False
    def icon_rect(self):
        v=json.loads((self.data/"tray.json").read_text(encoding="utf-8"))
        assert v['success'],"Tray registration failed"
        n=IconId(size=C.sizeof(IconId),hwnd=self.owner,id=1,guid=Guid.from_buffer_copy(uuid.UUID(v['guid']).bytes_le))
        r=W.RECT();assert shell.Shell_NotifyIconGetRect(C.byref(n),C.byref(r))==0
        return r
    def overflow(self):
        visible=lambda:[h for h,c in windows(None) if c=='NotifyIconOverflowWindow' and u.IsWindowVisible(h)]
        taskbar=next(h for h,c in windows(None) if c=='Shell_TrayWnd')
        chevron=next(h for h,c in windows(None,taskbar) if window_text(h) in ['通知 V 形','显示隐藏的图标','Notification Chevron','Show hidden icons'])
        def toggle_overflow():
            # Activating the taskbar separately can dismiss an existing overflow asynchronously.
            # Click the chevron itself, which supplies the real activation and toggle.
            area=bounds(chevron);point=W.POINT((area.left+area.right)//2,(area.top+area.bottom)//2)
            u.SetCursorPos(point.x,point.y);time.sleep(.08)
            assert u.WindowFromPoint(point)==chevron,'Notification chevron is covered'
            send(Input(type=0,mouse=Mouse(dwFlags=2)));time.sleep(.07);send(Input(type=0,mouse=Mouse(dwFlags=4)))
            time.sleep(.2)
        toggle_overflow()
        if not visible():toggle_overflow()
        all_windows=until(visible)
        for overflow in all_windows:
            for child,cls in windows(None,overflow):
                if cls!='ToolbarWindow32':continue
                for title,r in accessible_items(child):
                    if title.startswith(f'一键熄屏 · 测试 PID {self.pid} ·') and 0<r.right-r.left<100:
                        self.shot('tray-overflow-icon',overflow)
                        return r
        raise AssertionError('App icon missing from visible notification overflow')
    def tray_menu(self):
        r=self.overflow();u.SetCursorPos((r.left+r.right)//2,(r.top+r.bottom)//2)
        time.sleep(.15)
        hit=u.WindowFromPoint(W.POINT((r.left+r.right)//2,(r.top+r.bottom)//2))
        cls=C.create_unicode_buffer(128);u.GetClassNameW(hit,cls,128)
        assert cls.value=='ToolbarWindow32',f'Tray click intercepted by {cls.value}'
        send(Input(type=0,mouse=Mouse(dwFlags=8)));time.sleep(.07);send(Input(type=0,mouse=Mouse(dwFlags=16)))
        menu=until(lambda:next((h for h,c in windows(self.pid) if c=="#32768" and u.IsWindowVisible(h)),None))
        self.shot("tray-menu",menu)
        return menu
    def shot_menu(self,name):
        menus=[bounds(h) for h,c in windows(self.pid) if c=='#32768' and u.IsWindowVisible(h)]
        assert len(menus)>=2,'Submenu did not open'
        path=self.output/f'{name}.png'
        ImageGrab.grab(bbox=(min(r.left for r in menus),min(r.top for r in menus),max(r.right for r in menus),max(r.bottom for r in menus)),all_screens=True).save(path)
        self.screens.append(path.name)

def run_cases(d,startup_only):
    def startup():
        d.start();message(d.hwnd,0)
        ids={u.GetDlgCtrlID(h) for h,_ in windows(d.pid,d.hwnd)}
        required={2001,2002,2003,2004,2011,2012,2013,2021,2022,2031,2041,2042,2043,2051,2061,2062,2071,2081,2082,2083,2091,2092,2093,2094,2095,2096,2097,2098}
        assert required<=ids,required-ids
        assert not (d.data/'settings.json').exists(),"Initialization saved unintended defaults"
        assert not (d.data/'tray-error.json').exists(),"Tray registration error"
        assert '已被占用' not in d.status()['message'],'Default global hotkey registration failed'
        d.results['dpi']=u.GetDpiForWindow(d.hwnd);d.shot('settings-initial')
    d.case('normal_startup_complete_no_config_write',startup)
    if startup_only:return
    def toggles():
        for ident,field in [(2002,'dim_external'),(2003,'dim_internal'),(2004,'hdr_off'),(2041,'schedule_enabled'),(2051,'autostart')]:
            old=d.saved()[field];d.tap(ident);d.wait_saved(field,not old);d.shot(f'toggle-{ident}-changed')
            if ident==2051:
                with winreg.OpenKey(winreg.HKEY_CURRENT_USER,d.registry_key) as reg:
                    command=winreg.QueryValueEx(reg,'ScreenOffGuardRust')[0]
                    assert command.startswith('"'+d.exe+'"') and '--silent' in command and str(d.data) in command,command
            d.tap(ident);d.wait_saved(field,old)
    d.case('all_switches_real_click_and_persist',toggles)
    def radios():
        for field,choices in [
            ('cursor_mode',[(2082,'hide'),(2083,'keep'),(2081,'auto')]),
            ('dim_order',[(2012,'dim_first'),(2013,'fade'),(2011,'black_first')]),
            ('restore_mode',[(2022,'delay'),(2021,'ready')]),
            ('close_action',[(2062,'exit'),(2061,'hide')])]:
            for ident,value in choices:
                d.tap(ident);d.wait_saved(field,value)
                assert message(d.ctl(ident),0xF0)==1
                d.shot(f'radio-{field}-{value}')
    d.case('all_radio_groups_and_persist',radios)
    def edits():
        d.edit(2031,'270');d.wait_saved('delay_ms',270)
        assert message(d.ctl(2091),0x400)==270
        d.tap(2091);press(0x27);d.wait_saved('delay_ms',271)
        assert window_text(d.ctl(2031))=='271'
        # WM_USER trackbar messages do not marshal pointer arguments across
        # processes. Use the real client bounds and current scalar position.
        slider=d.ctl(2091);client=W.RECT();assert u.GetClientRect(slider,C.byref(client))
        gutter=round(12*u.GetDpiForWindow(slider)/96)
        point=W.POINT(gutter+round((client.right-2*gutter)*271/500),client.bottom//2);assert u.ClientToScreen(slider,C.byref(point))
        u.SetCursorPos(point.x,point.y);time.sleep(.06);send(Input(type=0,mouse=Mouse(dwFlags=2)));time.sleep(.07)
        try:
            assert message(slider,0x400)==271,'Slider drag must start on the current thumb'
            u.SetCursorPos(point.x+30,point.y);time.sleep(.12)
        finally:send(Input(type=0,mouse=Mouse(dwFlags=4)))
        dragged=message(slider,0x400);assert 271<dragged<=500;d.wait_saved('delay_ms',dragged)
        assert window_text(d.ctl(2031))==str(dragged);d.shot('slider-mouse-dragged')
        d.edit(2031,'271');d.wait_saved('delay_ms',271)
        d.edit(2031,'501');time.sleep(.2);assert d.saved()['delay_ms']==271
        assert window_text(d.ctl(2031))=='501';d.shot('invalid-delay-draft')
        d.edit(2031,'140');d.wait_saved('delay_ms',140)
        d.edit(2042,'22:45');d.wait_saved('dark_at',{'hour':22,'minute':45})
        d.edit(2043,'0635');d.wait_saved('light_at',{'hour':6,'minute':35})
        for partial in ['22:','22:4','24:00']:
            d.edit(2042,partial);time.sleep(.15);assert d.saved()['dark_at']=={'hour':22,'minute':45}
            assert window_text(d.ctl(2042))==partial
        d.shot('invalid-time-draft');d.edit(2042,'23:00');d.wait_saved('dark_at',{'hour':23,'minute':0})
        press(0x0D);assert window_text(d.ctl(2042))=='23:00'
        d.shot('inputs-saved')
    d.case('delay_slider_time_valid_invalid_and_partial',edits)
    def saving_failure():
        file=d.data/'settings.json';original=file.read_bytes();backup=d.data/'settings.original.json';file.rename(backup);file.mkdir()
        try:
            d.edit(2031,'222');until(lambda:'保存失败' in d.status().get('message',''))
            assert d.saved()['delay_ms']==140
            assert window_text(d.ctl(2031))=='222'
            assert backup.read_bytes()==original;d.shot('save-failed-draft-retained')
        finally:file.rmdir();backup.rename(file)
        d.tap(2097);d.wait_saved('delay_ms',222);d.shot('save-retry-success')
        d.edit(2031,'140');d.wait_saved('delay_ms',140)
    d.case('save_failure_preserves_state_and_retry',saving_failure)
    def hotkeys():
        original=d.saved()['hotkey'].copy()
        press(0x11,0x12,original['key']);until(lambda:d.status()['phase']=='dark')
        press(0x11,0x12,original['key']);until(lambda:d.status()['phase']=='light');until(d.restored)
        generation=d.status()['generation']
        d.tap(2001);press(0x11,0x12,original['key']);assert d.status()['generation']==generation
        press(0x1B);until(lambda:not d.status()['capture']);assert d.saved()['hotkey']==original
        d.tap(2001);d.shot('hotkey-capture');d.tap(2092)
        dialog=until(lambda:next((h for h,c in windows(d.pid) if c=='SOGRustAbout'),None));d.shot('about-dialog',dialog)
        press(0x0D);until(lambda:not u.IsWindow(dialog));assert not d.status()['capture'];message(d.hwnd,0)
        assert u.RegisterHotKey(None,909,3|0x4000,0x59)
        try:
            d.tap(2001);press(0x11,0x12,0x59)
            until(lambda:'已被占用' in d.status().get('message',''))
            assert d.saved()['hotkey']==original;assert d.status()['generation']==generation
            d.shot('hotkey-conflict-retains-old');press(0x1B)
        finally:u.UnregisterHotKey(None,909)
        d.tap(2001);press(0x11,0x12,0x4B);d.wait_saved('hotkey',{'modifiers':3,'key':0x4B})
        d.shot('hotkey-new-saved')
        press(0x11,0x12,original['key']);time.sleep(.2);assert d.status()['generation']==generation
        # A held shortcut and injected key repeat must create only one intent.
        send(key(0x11),key(0x12),key(0x4B));time.sleep(.1)
        send(key(0x4B),key(0x4B),key(0x4B));time.sleep(.2)
        send(key(0x4B,True),key(0x12,True),key(0x11,True))
        until(lambda:d.status()['phase']=='dark');assert d.status()['generation']==generation+1
        press(0x11,0x12,0x4B);until(lambda:d.status()['phase']=='light');assert d.restored()
    d.case('hotkey_cancel_conflict_replace_no_repeat',hotkeys)
    def hardware():
        d.tap(2004);d.wait_saved('hdr_off',True)
        for ident,order in [(2011,'black_first'),(2012,'dim_first'),(2013,'fade')]:
            d.tap(ident);d.wait_saved('dim_order',order)
            d.tap(2095);until(lambda:d.status()['phase']=='dark')
            simulated=json.loads((d.data/'simulated-hardware.json').read_text())
            assert all(x['value']==0 for x in simulated['devices']);d.shot('simulated-dark-'+order)
            d.tap(2095);until(lambda:d.status()['phase']=='light');assert d.restored()
        d.tap(2022);d.wait_saved('restore_mode','delay');d.tap(2095);until(lambda:d.status()['phase']=='dark')
        d.tap(2095);until(lambda:d.status()['phase']=='light');until(d.restored)
        d.tap(2021);d.wait_saved('restore_mode','ready')
        generation=d.status()['generation']
        for _ in range(6):
            # Rapid intents intentionally arrive before the slow simulation has finished.
            u.PostMessageW(d.owner,0x111,1001,0)
        until(lambda:d.status()['generation']==generation+6 and d.status()['phase']=='light');until(d.restored)
        d.tap(2071);until(lambda:d.status()['phase']=='light');until(d.restored)
        until(lambda:not d.status()['saving']);assert 0<d.saved()['delay_ms']<=500
        d.shot('measurement-restored');d.tap(2011);d.wait_saved('dim_order','black_first')
    d.case('dim_orders_restore_modes_measure_and_rapid_reverse_simulated',hardware)
    def window_lifecycle():
        d.tap(2098);until(lambda:u.IsZoomed(d.hwnd));d.shot('window-maximized')
        d.tap(2098);until(lambda:not u.IsZoomed(d.hwnd));d.shot('window-restored')
        old=bounds(d.hwnd);u.SetWindowPos(d.hwnd,None,old.left+15,old.top+15,800,650,4);time.sleep(.2);message(d.hwnd,0)
        d.shot('window-small-scrolled')
        # Test all standard resize hit regions against actual geometry.
        r=bounds(d.hwnd)
        points=[(r.left+2,r.top+2,13),(r.right-2,r.top+2,14),(r.left+2,r.bottom-2,16),(r.right-2,r.bottom-2,17),
                (r.left+2,(r.top+r.bottom)//2,10),(r.right-2,(r.top+r.bottom)//2,11),((r.left+r.right)//2,r.top+2,12),((r.left+r.right)//2,r.bottom-2,15)]
        for x,y,expected in points:assert message(d.hwnd,0x84,0,(x&65535)|((y&65535)<<16))==expected
        # Move by a real title bar drag.
        x,y=r.left+180,r.top+20;u.SetCursorPos(x,y);send(Input(type=0,mouse=Mouse(dwFlags=2)));time.sleep(.08);u.SetCursorPos(x+35,y+25);time.sleep(.12);send(Input(type=0,mouse=Mouse(dwFlags=4)));time.sleep(.15)
        moved=bounds(d.hwnd);assert abs(moved.left-r.left-35)<5 and abs(moved.top-r.top-25)<5
        d.shot('window-title-dragged')
        d.tap(2093);until(lambda:not u.IsWindowVisible(d.hwnd));assert d.process.poll() is None
        second=subprocess.Popen([d.exe,'--ui-test','--data-dir',str(d.data)]);assert second.wait(timeout=5)==0
        until(lambda:u.IsWindowVisible(d.hwnd));assert d.process.poll() is None;d.shot('single-instance-restores')
        d.tap(2094);until(lambda:not u.IsWindowVisible(d.hwnd));assert d.process.poll() is None
        u.PostMessageW(d.owner,0x8005,0,0);until(lambda:u.IsWindowVisible(d.hwnd));d.shot('close-hide-restores')
    d.case('maximize_restore_drag_resize_minimize_single_instance_close_hide',window_lifecycle)
    def scale_keyboard():
        assert message(d.owner,0x11)==1,'App vetoed session end'
        message(d.owner,0x16,0);assert not d.status()['exiting']
        # This scales app layout only; it does not change the user's display configuration.
        r=bounds(d.hwnd)
        for dpi in [96,120,144,192]:
            message(d.hwnd,0x801e,dpi)
            u.SetWindowPos(d.hwnd,None,r.left,24,int(724*dpi/96),min(1320,int(1040*dpi/96)),4)
            time.sleep(.12);message(d.hwnd,0)
            d.tap(2112);d.wait_saved('theme','dark')
            d.tap(2111);d.wait_saved('theme','light')
            d.shot(f'layout-{dpi}-dpi-simulated')
        message(d.hwnd,0x801e,d.results['dpi'])
        u.SetWindowPos(d.hwnd,None,r.left,r.top,905,1300,4)
        # Native tab traversal must enter the scrolling child container.
        d.tap(2001);press(0x1b);press(0x09)
        # Space follows the native radio/check semantics; screenshot retains focus cue.
        press(0x20);d.wait_saved('cursor_mode','auto');d.shot('keyboard-tab-focus')
    d.case('layout_scaling_keyboard_and_session_query',scale_keyboard)
    def tray_menu():
        d.tray_menu();press(0x1B)
        d.tap(2094);until(lambda:not u.IsWindowVisible(d.hwnd))
        r=d.overflow();u.SetCursorPos((r.left+r.right)//2,(r.top+r.bottom)//2)
        time.sleep(.2)
        hit=u.WindowFromPoint(W.POINT((r.left+r.right)//2,(r.top+r.bottom)//2))
        cls=C.create_unicode_buffer(128);u.GetClassNameW(hit,cls,128)
        assert cls.value=='ToolbarWindow32',f'Tray left click intercepted by {cls.value}'
        send(Input(type=0,mouse=Mouse(dwFlags=2)));time.sleep(.07);send(Input(type=0,mouse=Mouse(dwFlags=4)))
        until(lambda:u.IsWindowVisible(d.hwnd));d.shot('tray-left-click-restores')
        # Menu commands are selected through the actual popup keyboard path.
        d.tray_menu();press(0x28);press(0x0D);until(lambda:d.status()['phase']=='dark')
        d.tray_menu();press(0x28);press(0x0D);until(lambda:d.status()['phase']=='light');until(d.restored)
        d.tray_menu();press(0x28);press(0x28);press(0x0D);until(lambda:u.IsWindowVisible(d.hwnd))
        # Cursor submenu: third menu item, right, second submenu choice.
        d.tray_menu();press(0x28);press(0x28);press(0x28);press(0x27);time.sleep(.15)
        menus=[h for h,c in windows(d.pid) if c=='#32768' and u.IsWindowVisible(h)]
        if menus:d.shot_menu('tray-cursor-submenu')
        press(0x28);press(0x0D);d.wait_saved('cursor_mode','hide')
        d.tap(2081);d.wait_saved('cursor_mode','auto')
        d.tray_menu();press(0x28);press(0x28);press(0x28);press(0x28);press(0x27);time.sleep(.1)
        menus=[h for h,c in windows(d.pid) if c=='#32768' and u.IsWindowVisible(h)]
        if menus:d.shot_menu('tray-order-submenu')
        press(0x28);press(0x0D);d.wait_saved('dim_order','dim_first')
        d.tap(2011);d.wait_saved('dim_order','black_first')
        d.tray_menu();press(0x28);press(0x28);press(0x28);press(0x28);press(0x28);press(0x0D)
        dialog=until(lambda:next((h for h,c in windows(d.pid) if c=='SOGRustAbout'),None));d.shot('tray-about',dialog);press(0x0D)
    d.case('real_tray_icon_menu_submenus_and_actions',tray_menu)
    def recovery():
        d.tap(2095);until(lambda:d.status()['phase']=='dark');assert (d.data/'recovery.json').exists()
        d.process.terminate();d.process.wait(timeout=5)
        d.start();until(d.restored);d.shot('crash-restart-recovered')
        d.tap(2095);until(lambda:d.status()['phase']=='dark')
        d.quit();assert d.process.returncode==0;assert d.restored()
        d.start();d.quit();d.start('--silent')
        assert not d.hwnd,'Silent startup opened a panel'
        u.PostMessageW(d.owner,0x8005,0,0)
        d.hwnd=until(lambda:next((h for h,c in windows(d.pid) if c=='SOGRustSettings'),None))
        message(d.hwnd,0,timeout=3000)
        u.SetWindowPos(d.hwnd,C.c_void_p(-1),0,0,0,0,3)
        d.shot('silent-start-restored')
        d.tap(2111);d.wait_saved('theme','light')
        d.tap(2062);d.wait_saved('close_action','exit');d.shot('close-exit-selected')
        (d.data/'save-delay-ms').write_text('650')
        d.tap(2002);until(lambda:d.status()['saving']);d.tap(2003)
        d.tap(2094);assert d.process.wait(timeout=8)==0;assert d.restored()
        persisted=json.loads((d.data/'settings.json').read_text(encoding='utf-8'))
        assert persisted['dim_external']==False and persisted['dim_internal']==False,'Exit dropped edits during save'
        (d.data/'save-delay-ms').unlink()
        d.start();d.tray_menu()
        for _ in range(6):press(0x28)
        press(0x0D);assert d.process.wait(timeout=8)==0
    d.case('crash_recovery_dark_exit_silent_start_close_exit',recovery)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--exe", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--startup-only", action="store_true")
    args = parser.parse_args()
    output = Path(args.output).resolve()
    output.mkdir(parents=True, exist_ok=True)
    exe=str(Path(args.exe).resolve())
    results = {"exe":exe,"sha256":hashlib.sha256(Path(exe).read_bytes()).hexdigest(),"cases":[],"hardware":"simulated; actual windows and SendInput","driver":"Win32 native (computer-use pipe unavailable)"}
    driver=Driver(exe,output,results)
    try:
        run_cases(driver,args.startup_only)
    except Exception:
        print(traceback.format_exc(),flush=True)
    finally:
        driver.quit()
    results["pass"] = all(c["pass"] for c in results["cases"])
    driver.write()
    print(json.dumps(results, ensure_ascii=False))
    raise SystemExit(0 if results["pass"] else 1)


if __name__ == "__main__":
    main()
