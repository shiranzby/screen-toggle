//! Keyboard/mouse trigger collection stays separate from desktop hit testing.
//! Hooks never suppress input or perform RPC; unknown desktop canvases fail closed.
use sog::config::Hotkey;
use std::{
    cell::RefCell,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, SyncSender},
        Arc, Condvar, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows::{
    core::{Interface, VARIANT},
    Win32::{
        Foundation::{BOOL, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::Gdi::{
            CreateRectRgn, DeleteObject, GetWindowRgn, MonitorFromPoint, PtInRegion,
            ScreenToClient, COMPLEXREGION, MONITOR_DEFAULTTONULL, SIMPLEREGION,
        },
        System::{
            Com::{
                CoCancelCall, CoDisableCallCancellation, CoEnableCallCancellation, CoInitializeEx,
                CoUninitialize, COINIT_MULTITHREADED,
            },
            Threading::{GetCurrentProcessId, GetCurrentThreadId},
        },
        UI::{
            Accessibility::{
                AccessibleObjectFromWindow, IAccessible, ObjectFromLresult, ROLE_SYSTEM_LIST,
            },
            Input::KeyboardAndMouse::{GetAsyncKeyState, GetDoubleClickTime},
            WindowsAndMessaging::*,
        },
    },
};

const MAX_AGE: Duration = Duration::from_millis(250);
const RPC_TIMEOUT_MS: u32 = 60;
const HIT_TEST_DEADLINE: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Bindings {
    pub global_mouse: Option<(Hotkey, bool)>,
    pub desktop: Option<(Hotkey, bool)>,
}
impl Bindings {
    fn at(self, index: usize) -> Option<(Hotkey, bool)> {
        if index == 0 {
            self.global_mouse
        } else {
            self.desktop
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Position {
    x: i32,
    y: i32,
}
impl From<POINT> for Position {
    fn from(p: POINT) -> Self {
        Self { x: p.x, y: p.y }
    }
}
impl From<Position> for POINT {
    fn from(p: Position) -> Self {
        Self { x: p.x, y: p.y }
    }
}
fn near(a: Position, b: Position, distance: Position) -> bool {
    (i64::from(a.x) - i64::from(b.x)).abs() <= i64::from(distance.x)
        && (i64::from(a.y) - i64::from(b.y)).abs() <= i64::from(distance.y)
}
fn gesture_tolerance(binding: Hotkey, system: Position) -> Position {
    // Squeezing a side/middle/right button commonly nudges the sensor. These
    // are not Explorer's left-button file drag; endpoints still must be blank
    // on the same desktop/monitor, and motion outside this small radius fails.
    if binding.is_mouse() && binding.key != 1 {
        Position {
            x: system.x.max(16),
            y: system.y.max(16),
        }
    } else {
        system
    }
}
#[derive(Clone, Copy)]
struct Click {
    scope: usize,
    down: Position,
    up: Position,
    ordinal: u64,
    timestamp: u32,
    received: Instant,
    valid: bool,
    keyboard: bool,
    foreground: usize,
    presentation: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Hit {
    desktop: usize,
    monitor: usize,
}
struct Detector {
    first: Option<(Click, Hit)>,
    interval: u32,
    distance: Position,
}
impl Detector {
    fn accept(&mut self, click: Click, hit: Option<Hit>, double: bool) -> bool {
        let Some(hit) = hit.filter(|_| click.valid && click.received.elapsed() <= MAX_AGE) else {
            self.first = None;
            return false;
        };
        if !double {
            self.first = None;
            return true;
        }
        if let Some((first, previous)) = self.first.take() {
            if previous == hit
                && first.scope == click.scope
                && first.presentation == click.presentation
                && click.ordinal == first.ordinal.wrapping_add(1)
                && click.timestamp.wrapping_sub(first.timestamp) <= self.interval
                && (click.keyboard || near(first.up, click.up, self.distance))
            {
                return true;
            }
        }
        self.first = Some((click, hit));
        false
    }
}
#[derive(Clone, Copy)]
struct Press {
    point: Position,
    timestamp: u32,
    dragged: bool,
    injected: bool,
    ordinal: u64,
    modifiers: u32,
    foreground: usize,
    presentation: u64,
}
struct Sample {
    point: Position,
    timestamp: u32,
    injected: bool,
    modifiers: u32,
    foreground: usize,
}
struct HookState {
    events: SyncSender<Click>,
    bindings: Bindings,
    ordinal: u64,
    presses: [Option<Press>; 2],
    keys_down: [bool; 256],
    drag: Position,
    interval: u32,
    presentation: Arc<AtomicU64>,
}
thread_local! { static HOOK: RefCell<Option<HookState>> = const { RefCell::new(None) }; }
fn button_message(message: u32, data: u32) -> Option<(u32, bool)> {
    match message {
        WM_LBUTTONDOWN => Some((1, true)),
        WM_LBUTTONUP => Some((1, false)),
        WM_RBUTTONDOWN => Some((2, true)),
        WM_RBUTTONUP => Some((2, false)),
        WM_MBUTTONDOWN => Some((4, true)),
        WM_MBUTTONUP => Some((4, false)),
        WM_XBUTTONDOWN | WM_XBUTTONUP => match data >> 16 {
            1 => Some((5, message == WM_XBUTTONDOWN)),
            2 => Some((6, message == WM_XBUTTONDOWN)),
            _ => None,
        },
        _ => None,
    }
}
fn modifiers() -> u32 {
    unsafe {
        u32::from(GetAsyncKeyState(0x12) < 0)
            | (u32::from(GetAsyncKeyState(0x11) < 0) << 1)
            | (u32::from(GetAsyncKeyState(0x10) < 0) << 2)
            | (u32::from(GetAsyncKeyState(0x5b) < 0 || GetAsyncKeyState(0x5c) < 0) << 3)
    }
}
fn modifier_key(key: u32) -> bool {
    matches!(key, 0x10..=0x12 | 0x5b..=0x5c | 0xa0..=0xa5)
}
impl HookState {
    fn input(&mut self, key: u32, down: bool, keyboard: bool, sample: Sample) {
        let Sample {
            point,
            timestamp,
            injected,
            modifiers,
            foreground,
        } = sample;
        if keyboard {
            if modifier_key(key) || key >= 256 {
                return;
            }
            let held = &mut self.keys_down[key as usize];
            if down && *held {
                return;
            } // Ignore keyboard auto-repeat.
            if !down && !*held {
                return;
            }
            *held = down;
        }
        if down {
            self.ordinal = self.ordinal.wrapping_add(1);
        }
        for index in 0..2 {
            let Some((binding, _)) = self.bindings.at(index) else {
                continue;
            };
            if binding.key != key || binding.is_mouse() == keyboard {
                if down {
                    self.presses[index] = None;
                }
                continue;
            }
            if down {
                self.presses[index] = Some(Press {
                    point,
                    timestamp,
                    dragged: false,
                    injected,
                    ordinal: self.ordinal,
                    modifiers,
                    foreground,
                    presentation: self.presentation.load(Ordering::Acquire),
                });
            } else if let Some(press) = self.presses[index].take() {
                let valid = !press.dragged
                    && !press.injected
                    && !injected
                    && press.presentation == self.presentation.load(Ordering::Acquire)
                    && press.modifiers == binding.modifiers
                    && (keyboard
                        || near(press.point, point, gesture_tolerance(binding, self.drag)))
                    && timestamp.wrapping_sub(press.timestamp) <= self.interval;
                #[cfg(test)]
                eprintln!("INPUT_SAMPLE scope={index} key={key} keyboard={keyboard} valid={valid} down=[{},{}] up=[{},{}] modifiers={} injected={} ordinal={}",
                    press.point.x, press.point.y, point.x, point.y, press.modifiers,
                    press.injected || injected, press.ordinal);
                let _ = self.events.try_send(Click {
                    scope: index,
                    down: press.point,
                    up: point,
                    ordinal: press.ordinal,
                    timestamp,
                    received: Instant::now(),
                    valid,
                    keyboard,
                    foreground: press.foreground,
                    presentation: press.presentation,
                });
            }
        }
    }
    fn motion(&mut self, point: Position) {
        for (index, press) in self.presses.iter_mut().enumerate() {
            if self
                .bindings
                .at(index)
                .is_some_and(|(key, _)| key.is_mouse())
            {
                if let Some(press) = press {
                    let limit = gesture_tolerance(self.bindings.at(index).unwrap().0, self.drag);
                    press.dragged |= !near(press.point, point, limit);
                }
            }
        }
    }
}
unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let event = &*(lparam.0 as *const MSLLHOOKSTRUCT);
        HOOK.with(|cell| {
            if let Some(state) = cell.borrow_mut().as_mut() {
                if wparam.0 as u32 == WM_MOUSEMOVE {
                    state.motion(event.pt.into());
                } else if let Some((key, down)) = button_message(wparam.0 as u32, event.mouseData) {
                    state.input(
                        key,
                        down,
                        false,
                        Sample {
                            point: event.pt.into(),
                            timestamp: event.time,
                            injected: event.flags & 3 != 0,
                            modifiers: modifiers(),
                            foreground: 0,
                        },
                    );
                } else if matches!(wparam.0 as u32, WM_MOUSEWHEEL | WM_MOUSEHWHEEL) {
                    state.ordinal = state.ordinal.wrapping_add(1);
                    state.presses = [None, None];
                }
            }
        });
    }
    CallNextHookEx(None, code, wparam, lparam)
}
unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let event = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        let message = wparam.0 as u32;
        if matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP) {
            let mut point = POINT::default();
            if GetCursorPos(&mut point).is_ok() {
                HOOK.with(|cell| {
                    if let Some(state) = cell.borrow_mut().as_mut() {
                        state.input(
                            event.vkCode,
                            matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN),
                            true,
                            Sample {
                                point: point.into(),
                                timestamp: event.time,
                                injected: event.flags.0 & (2 | 16) != 0,
                                modifiers: modifiers(),
                                foreground: GetForegroundWindow().0 as usize,
                            },
                        );
                    }
                });
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

fn class_name(hwnd: HWND) -> String {
    let mut name = [0u16; 128];
    let count = unsafe { GetClassNameW(hwnd, &mut name) };
    String::from_utf16_lossy(&name[..count.max(0) as usize])
}
fn process(hwnd: HWND) -> u32 {
    let mut pid = 0;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    pid
}

/// An explanatory status only. Gesture admission always performs its own
/// positive ownership/hit checks, even if this enumeration fails or changes.
pub fn compatibility_note() -> Option<&'static str> {
    struct State {
        shell_pid: u32,
        custom_canvas: bool,
    }
    unsafe extern "system" fn child(hwnd: HWND, data: LPARAM) -> BOOL {
        if class_name(hwnd) == "SysListView32" && IsWindowVisible(hwnd).as_bool() {
            let foreign = process(hwnd) != (*(data.0 as *const State)).shell_pid;
            if foreign
                && GetParent(hwnd)
                    .ok()
                    .is_some_and(|parent| class_name(parent) == "SHELLDLL_DefView")
            {
                (*(data.0 as *mut State)).custom_canvas = true;
                return BOOL(0);
            }
        }
        BOOL(1)
    }
    unsafe extern "system" fn top(hwnd: HWND, data: LPARAM) -> BOOL {
        if matches!(class_name(hwnd).as_str(), "Progman" | "WorkerW")
            && process(hwnd) == (*(data.0 as *const State)).shell_pid
        {
            let _ = EnumChildWindows(hwnd, Some(child), data);
        }
        BOOL::from(!(*(data.0 as *const State)).custom_canvas)
    }
    unsafe {
        let mut state = State {
            shell_pid: process(GetShellWindow()),
            custom_canvas: false,
        };
        if state.shell_pid == 0 {
            return Some("未能确认 Windows 桌面；鼠标触发暂停，键盘热键仍可用");
        }
        let _ = EnumWindows(Some(top), LPARAM((&mut state as *mut State) as isize));
        state
            .custom_canvas
            .then_some("检测到桌面整理软件，无法确认抽屉边界时暂停鼠标触发；键盘热键仍可用")
    }
}

fn own_overlay(hwnd: HWND) -> bool {
    class_name(hwnd) == "SOGRustOverlay" && process(hwnd) == unsafe { GetCurrentProcessId() }
}

/// Transparent overlays may be omitted by WindowFromPoint. Wake only inside
/// a visible overlay owned by this process, including its selected-screen
/// window region; the contents hidden underneath do not need to be desktop.
fn overlay_hit(point: Position) -> Option<Hit> {
    struct Search {
        point: Position,
        found: Option<Hit>,
    }
    unsafe extern "system" fn visit(hwnd: HWND, data: LPARAM) -> BOOL {
        let search = &mut *(data.0 as *mut Search);
        if !own_overlay(hwnd) || !IsWindowVisible(hwnd).as_bool() {
            return BOOL(1);
        }
        let mut bounds = RECT::default();
        if GetWindowRect(hwnd, &mut bounds).is_err()
            || search.point.x < bounds.left
            || search.point.x >= bounds.right
            || search.point.y < bounds.top
            || search.point.y >= bounds.bottom
        {
            return BOOL(1);
        }
        let region = CreateRectRgn(0, 0, 0, 0);
        if region.0.is_null() {
            return BOOL(1);
        }
        let kind = GetWindowRgn(hwnd, region);
        let inside = matches!(kind, SIMPLEREGION | COMPLEXREGION)
            && PtInRegion(
                region,
                search.point.x - bounds.left,
                search.point.y - bounds.top,
            )
            .as_bool();
        let _ = DeleteObject(region);
        if inside {
            let monitor = MonitorFromPoint(search.point.into(), MONITOR_DEFAULTTONULL);
            if !monitor.0.is_null() {
                search.found = Some(Hit {
                    desktop: hwnd.0 as usize,
                    monitor: monitor.0 as usize,
                });
            }
        }
        BOOL::from(search.found.is_none())
    }
    let mut search = Search { point, found: None };
    unsafe {
        let _ = EnumWindows(Some(visit), LPARAM((&mut search as *mut Search) as isize));
    }
    search.found
}

fn presentation_is_dark(presentation: u64) -> bool {
    presentation & 1 != 0
}
fn set_presentation(presentation: &AtomicU64, dark: bool) {
    // Keep the mode and its generation in one atomic snapshot. A queued input
    // from before a quick dark/light/dark round trip must not become current.
    let mut previous = presentation.load(Ordering::Acquire);
    while presentation_is_dark(previous) != dark {
        match presentation.compare_exchange_weak(
            previous,
            previous.wrapping_add(1),
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => break,
            Err(current) => previous = current,
        }
    }
}

/// WindowFromPoint can return our transparent overlay. Walk its actual lower
/// Z-order, then descend the first visible window containing this point. Never
/// substitute the shell for an ordinary application underneath the overlay.
fn window_below_overlay(point: Position) -> Option<HWND> {
    unsafe {
        let mut window = WindowFromPoint(point.into());
        if !own_overlay(window) {
            return (!window.0.is_null()).then_some(window);
        }
        for _ in 0..256 {
            window = GetWindow(window, GW_HWNDNEXT).ok()?;
            let mut bounds = RECT::default();
            if !IsWindowVisible(window).as_bool()
                || own_overlay(window)
                || GetWindowRect(window, &mut bounds).is_err()
                || point.x < bounds.left
                || point.x >= bounds.right
                || point.y < bounds.top
                || point.y >= bounds.bottom
            {
                continue;
            }
            for _ in 0..16 {
                let mut local = POINT::from(point);
                if !ScreenToClient(window, &mut local).as_bool() {
                    return None;
                }
                let child = ChildWindowFromPointEx(window, local, CWP_SKIPINVISIBLE);
                if child.0.is_null() || child == window {
                    return Some(window);
                }
                window = child;
            }
            return None;
        }
        None
    }
}

/// Positive native shell ownership is required before any accessibility call.
fn native_desktop(point: Position) -> Option<Hit> {
    unsafe {
        let hwnd = window_below_overlay(point)?;
        if hwnd.0.is_null()
            || class_name(hwnd) != "SysListView32"
            || !IsWindowVisible(hwnd).as_bool()
        {
            return None;
        }
        let shell = GetShellWindow();
        let pid = process(shell);
        if pid == 0 || process(hwnd) != pid {
            return None;
        }
        let parent = GetParent(hwnd).ok()?;
        if class_name(parent) != "SHELLDLL_DefView" || process(parent) != pid {
            return None;
        }
        let root = GetAncestor(hwnd, GA_ROOT);
        if !matches!(class_name(root).as_str(), "Progman" | "WorkerW") || process(root) != pid {
            return None;
        }
        let monitor = MonitorFromPoint(point.into(), MONITOR_DEFAULTTONULL);
        if monitor.0.is_null() {
            return None;
        }
        Some(Hit {
            desktop: hwnd.0 as usize,
            monitor: monitor.0 as usize,
        })
    }
}

#[derive(Default)]
struct RpcWatchState {
    stop: bool,
    sequence: u64,
    active: Option<(u64, Instant)>,
    cancelled: u64,
}
type RpcWatch = Arc<(Mutex<RpcWatchState>, Condvar)>;

/// Only the accessibility worker enables cancellation. The hook and UI threads
/// never wait for a provider; the watchdog cancels one expired outbound call.
struct RpcDeadline {
    state: RpcWatch,
    watcher: Option<JoinHandle<()>>,
}
impl RpcDeadline {
    unsafe fn new() -> Result<Self, String> {
        CoEnableCallCancellation(None).map_err(|e| e.to_string())?;
        let state = Arc::new((Mutex::new(RpcWatchState::default()), Condvar::new()));
        let watched = state.clone();
        let thread_id = GetCurrentThreadId();
        let (ready, initialized) = mpsc::sync_channel(1);
        let watcher = thread::spawn(move || {
            let apartment = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() };
            let _ = ready.send(apartment.as_ref().map(|_| ()).map_err(|e| e.to_string()));
            if apartment.is_err() {
                return;
            }
            let (lock, changed) = &*watched;
            let mut state = lock.lock().unwrap();
            while !state.stop {
                if let Some((sequence, deadline)) = state.active {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        // Serialize cancellation with guard teardown, so a late
                        // timeout can never cancel the next point's RPC.
                        let _ = unsafe { CoCancelCall(thread_id, 0) };
                        state.cancelled = sequence;
                        state.active = None;
                    } else {
                        state = changed.wait_timeout(state, left).unwrap().0;
                    }
                } else {
                    state = changed.wait(state).unwrap();
                }
            }
            drop(state);
            unsafe {
                CoUninitialize();
            }
        });
        if !matches!(initialized.recv_timeout(Duration::from_secs(1)), Ok(Ok(()))) {
            {
                let mut status = state.0.lock().unwrap();
                status.stop = true;
                state.1.notify_one();
            }
            let _ = watcher.join();
            let _ = CoDisableCallCancellation(None);
            return Err("桌面命中超时保护未就绪".into());
        }
        Ok(Self {
            state,
            watcher: Some(watcher),
        })
    }
    fn begin(&self) -> RpcGuard<'_> {
        let mut state = self.state.0.lock().unwrap();
        state.sequence = state.sequence.wrapping_add(1).max(1);
        let sequence = state.sequence;
        state.active = Some((sequence, Instant::now() + HIT_TEST_DEADLINE));
        self.state.1.notify_one();
        RpcGuard {
            deadline: self,
            sequence,
            started: Instant::now(),
        }
    }
}
impl Drop for RpcDeadline {
    fn drop(&mut self) {
        {
            let mut state = self.state.0.lock().unwrap();
            state.stop = true;
            self.state.1.notify_one();
        }
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
        let _ = unsafe { CoDisableCallCancellation(None) };
    }
}
struct RpcGuard<'a> {
    deadline: &'a RpcDeadline,
    sequence: u64,
    started: Instant,
}
impl RpcGuard<'_> {
    fn within_deadline(&self) -> bool {
        self.started.elapsed() <= HIT_TEST_DEADLINE
            && self.deadline.state.0.lock().unwrap().cancelled != self.sequence
    }
}
impl Drop for RpcGuard<'_> {
    fn drop(&mut self) {
        let mut state = self.deadline.state.0.lock().unwrap();
        if state
            .active
            .is_some_and(|(sequence, _)| sequence == self.sequence)
        {
            state.active = None;
            self.deadline.state.1.notify_one();
        }
    }
}

fn exact_i32(value: &VARIANT) -> Option<i32> {
    // Converting a VT_EMPTY/VT_DISPATCH via VariantToInt32 would erase the
    // distinction between unknown, a child object, and CHILDID_SELF.
    (unsafe { value.as_raw().Anonymous.Anonymous.vt } == 3)
        .then(|| i32::try_from(value).ok())
        .flatten()
}
fn blank_result(
    role: Option<i32>,
    child_count: Option<i32>,
    native_count: usize,
    hit: Option<i32>,
) -> bool {
    role == Some(ROLE_SYSTEM_LIST as i32)
        && child_count.is_some_and(|count| count >= 0 && native_count <= count as usize)
        && hit == Some(0)
}
struct DesktopHitTest {
    deadline: RpcDeadline,
}
impl DesktopHitTest {
    unsafe fn new() -> Result<Self, String> {
        Ok(Self {
            deadline: RpcDeadline::new()?,
        })
    }
    /// Fast point query, independent of the total number of desktop icons.
    /// This method requires a separately verified Explorer desktop HWND.
    fn blank_on_list(&self, hwnd: HWND, point: Position) -> Option<bool> {
        let guard = self.deadline.begin();
        unsafe {
            let mut object = 0usize;
            if SendMessageTimeoutW(
                hwnd,
                WM_GETOBJECT,
                WPARAM(0),
                LPARAM(OBJID_CLIENT.0 as isize),
                SMTO_ABORTIFHUNG | SMTO_BLOCK,
                RPC_TIMEOUT_MS,
                Some(&mut object),
            )
            .0 == 0
            {
                return None;
            }
            let mut raw = std::ptr::null_mut();
            if object == 0 {
                // System controls may expose their standard MSAA proxy rather
                // than return a custom WM_GETOBJECT marshal token.
                AccessibleObjectFromWindow(
                    hwnd,
                    OBJID_CLIENT.0 as u32,
                    &IAccessible::IID,
                    &mut raw,
                )
                .ok()?;
            } else {
                ObjectFromLresult(
                    LRESULT(object as isize),
                    &IAccessible::IID,
                    WPARAM(0),
                    &mut raw,
                )
                .ok()?;
            }
            if raw.is_null() {
                return None;
            }
            let accessible = IAccessible::from_raw(raw);
            let role = exact_i32(&accessible.get_accRole(&VARIANT::from(0i32)).ok()?);
            let count = accessible.accChildCount().ok();
            let mut native_count = 0usize;
            if SendMessageTimeoutW(
                hwnd,
                0x1004,
                WPARAM(0),
                LPARAM(0),
                SMTO_ABORTIFHUNG | SMTO_BLOCK,
                RPC_TIMEOUT_MS,
                Some(&mut native_count),
            )
            .0 == 0
            {
                return None;
            }
            let hit = exact_i32(&accessible.accHitTest(point.x, point.y).ok()?);
            let blank = blank_result(role, count, native_count, hit);
            guard.within_deadline().then_some(blank)
        }
    }
    fn blank(&self, point: Position) -> Option<Hit> {
        let native = native_desktop(point)?;
        if !self.blank_on_list(HWND(native.desktop as *mut _), point)? {
            return None;
        }
        // The real topmost owner is rechecked after RPC, including Coodesker.
        (native_desktop(point)? == native).then_some(native)
    }
}

/// Only active bindings install hooks. Drop always unhooks before joining.
pub struct DesktopMouse {
    stop: Arc<AtomicBool>,
    presentation: Arc<AtomicU64>,
    hook_thread: u32,
    hook: Option<JoinHandle<()>>,
    validator: Option<JoinHandle<()>>,
}
fn keyboard_foreground_matches(foreground: usize) -> bool {
    let window = HWND(foreground as *mut _);
    unsafe {
        window == GetForegroundWindow()
            && process(window) == process(GetShellWindow())
            && matches!(
                class_name(window).as_str(),
                "Progman" | "WorkerW" | "SHELLDLL_DefView" | "SysListView32"
            )
    }
}
impl DesktopMouse {
    /// Call after changing the visible overlay, and once after listener setup.
    /// Every overlay needs an explicit window region, including all-screen mode.
    pub fn set_dark(&self, dark: bool) {
        set_presentation(&self.presentation, dark);
    }
    pub fn start(
        owner: HWND,
        message: u32,
        bindings: Bindings,
        token: usize,
    ) -> Result<Self, String> {
        if bindings.global_mouse.is_none() && bindings.desktop.is_none() {
            return Err("没有启用的鼠标或桌面触发热键".into());
        }
        if bindings
            .global_mouse
            .is_some_and(|(key, _)| !key.is_mouse())
        {
            return Err("全局键盘热键由窗口注册处理".into());
        }
        for (key, _) in [bindings.global_mouse, bindings.desktop]
            .into_iter()
            .flatten()
        {
            key.validate()?;
        }
        let owner = owner.0 as usize;
        let stop = Arc::new(AtomicBool::new(false));
        let presentation = Arc::new(AtomicU64::new(0));
        let (events, received) = mpsc::sync_channel::<Click>(16);
        let (validator_ready, ready) = mpsc::sync_channel(1);
        let validator_stop = stop.clone();
        let validator_presentation = presentation.clone();
        let validator = thread::spawn(move || unsafe {
            let initialized = bindings.desktop.is_some();
            if initialized {
                if let Err(error) = CoInitializeEx(None, COINIT_MULTITHREADED).ok() {
                    let _ = validator_ready.send(Err(error.to_string()));
                    return;
                }
            }
            {
                let prepared = if initialized {
                    DesktopHitTest::new().map(Some)
                } else {
                    Ok(None)
                };
                let _ = validator_ready.send(prepared.as_ref().map(|_| ()).map_err(Clone::clone));
                if let Ok(check) = prepared {
                    let make_detector = |scope| Detector {
                        first: None,
                        interval: GetDoubleClickTime(),
                        distance: gesture_tolerance(
                            bindings.at(scope).map(|(key, _)| key).unwrap_or(Hotkey {
                                modifiers: 0,
                                key: 1,
                            }),
                            Position {
                                x: (GetSystemMetrics(SM_CXDOUBLECLK) / 2).max(1),
                                y: (GetSystemMetrics(SM_CYDOUBLECLK) / 2).max(1),
                            },
                        ),
                    };
                    let mut detectors = [make_detector(0), make_detector(1)];
                    while !validator_stop.load(Ordering::Acquire) {
                        let click = match received.recv_timeout(Duration::from_millis(40)) {
                            Ok(click) => click,
                            Err(mpsc::RecvTimeoutError::Timeout) => continue,
                            Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        };
                        let Some((_, double)) = bindings.at(click.scope) else {
                            continue;
                        };
                        let current_presentation = validator_presentation.load(Ordering::Acquire);
                        let hit = if !click.valid
                            || click.received.elapsed() > MAX_AGE
                            || click.presentation != current_presentation
                        {
                            None
                        } else if click.scope == 0 {
                            let monitor = MonitorFromPoint(click.up.into(), MONITOR_DEFAULTTONULL);
                            (!monitor.0.is_null()).then_some(Hit {
                                desktop: 0,
                                monitor: monitor.0 as usize,
                            })
                        } else if presentation_is_dark(current_presentation) {
                            overlay_hit(click.down)
                                .and_then(|down| overlay_hit(click.up).filter(|up| *up == down))
                        } else if click.keyboard && !keyboard_foreground_matches(click.foreground) {
                            None
                        } else {
                            check.as_ref().and_then(|check| {
                                check.blank(click.down).and_then(|down| {
                                    if click.down == click.up {
                                        Some(down)
                                    } else {
                                        check.blank(click.up).filter(|up| *up == down)
                                    }
                                })
                            })
                        };
                        let hit = hit.filter(|_| {
                            validator_presentation.load(Ordering::Acquire) == current_presentation
                        });
                        let accepted = detectors[click.scope].accept(click, hit, double);
                        #[cfg(test)]
                        eprintln!("DESKTOP_GATE scope={} point=[{},{}] hit={hit:?} accepted={accepted} age_ms={}",
                            click.scope, click.up.x, click.up.y, click.received.elapsed().as_millis());
                        if accepted
                            && !validator_stop.load(Ordering::Acquire)
                            && validator_presentation.load(Ordering::Acquire)
                                == current_presentation
                        {
                            let _ = PostMessageW(
                                HWND(owner as *mut _),
                                message,
                                WPARAM(token),
                                LPARAM(click.timestamp as isize),
                            );
                        }
                    }
                }
            } // Release cached COM objects before the apartment.
            if initialized {
                CoUninitialize();
            }
        });
        match ready.recv_timeout(Duration::from_secs(2)) {
            Ok(Ok(())) => {}
            result => {
                stop.store(true, Ordering::Release);
                drop(events);
                let _ = validator.join();
                return Err(match result {
                    Ok(Err(e)) => e,
                    _ => "桌面命中检查未就绪".into(),
                });
            }
        }
        let (hook_ready, ready) = mpsc::sync_channel(1);
        let hook_thread_id = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let published_thread_id = hook_thread_id.clone();
        let hook_stop = stop.clone();
        let hook_presentation = presentation.clone();
        let hook = thread::spawn(move || unsafe {
            let mut message = MSG::default();
            let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE);
            published_thread_id.store(GetCurrentThreadId(), Ordering::Release);
            HOOK.with(|cell| {
                *cell.borrow_mut() = Some(HookState {
                    events,
                    bindings,
                    ordinal: 0,
                    presses: [None, None],
                    keys_down: [false; 256],
                    drag: Position {
                        x: (GetSystemMetrics(SM_CXDRAG) / 2).max(1),
                        y: (GetSystemMetrics(SM_CYDRAG) / 2).max(1),
                    },
                    interval: GetDoubleClickTime(),
                    presentation: hook_presentation,
                })
            });
            let installed = (|| {
                let mouse = if bindings.global_mouse.is_some()
                    || bindings.desktop.is_some_and(|(key, _)| key.is_mouse())
                {
                    Some(
                        SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), None, 0)
                            .map_err(|e| e.to_string())?,
                    )
                } else {
                    None
                };
                let keyboard = if bindings.desktop.is_some_and(|(key, _)| !key.is_mouse()) {
                    match SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), None, 0) {
                        Ok(hook) => Some(hook),
                        Err(error) => {
                            if let Some(mouse) = mouse {
                                let _ = UnhookWindowsHookEx(mouse);
                            }
                            return Err(error.to_string());
                        }
                    }
                } else {
                    None
                };
                Ok((mouse, keyboard))
            })();
            let _ = hook_ready.send(
                installed
                    .as_ref()
                    .map(|_| GetCurrentThreadId())
                    .map_err(Clone::clone),
            );
            if let Ok((mouse, keyboard)) = installed {
                while !hook_stop.load(Ordering::Acquire)
                    && GetMessageW(&mut message, None, 0, 0).0 > 0
                {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
                for hook in [mouse, keyboard].into_iter().flatten() {
                    let _ = UnhookWindowsHookEx(hook);
                }
            }
            HOOK.with(|cell| *cell.borrow_mut() = None);
        });
        match ready.recv_timeout(Duration::from_secs(2)) {
            Ok(Ok(hook_thread)) => Ok(Self {
                stop,
                presentation,
                hook_thread,
                hook: Some(hook),
                validator: Some(validator),
            }),
            result => {
                stop.store(true, Ordering::Release);
                let thread_id = hook_thread_id.load(Ordering::Acquire);
                if thread_id != 0 {
                    let _ = unsafe { PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
                }
                let _ = hook.join();
                let _ = validator.join();
                Err(match result {
                    Ok(Err(e)) => e,
                    _ => "热键监听未就绪".into(),
                })
            }
        }
    }
}
impl Drop for DesktopMouse {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        unsafe {
            let _ = PostThreadMessageW(self.hook_thread, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(hook) = self.hook.take() {
            let _ = hook.join();
        }
        if let Some(validator) = self.validator.take() {
            let _ = validator.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn point_hit_requires_known_list_self_and_complete_provider() {
        assert!(blank_result(Some(33), Some(178), 177, Some(0)));
        assert!(blank_result(Some(33), Some(0), 0, Some(0)));
        for (role, count, native, hit) in [
            (Some(33), Some(178), 177, Some(1)),
            (Some(33), Some(178), 177, None),
            (Some(10), Some(0), 177, Some(0)),
            (Some(33), Some(0), 177, Some(0)),
            (None, Some(178), 177, Some(0)),
            (Some(33), None, 177, Some(0)),
        ] {
            assert!(!blank_result(role, count, native, hit));
        }
        assert_eq!(exact_i32(&VARIANT::new()), None);
        assert_eq!(exact_i32(&VARIANT::from(0i32)), Some(0));
        assert_eq!(exact_i32(&VARIANT::from("0")), None);
    }
    #[test]
    #[ignore = "shows a tiny nonactivating overlay on the explicit Type-C test work area"]
    fn native_overlay_region_wake_probe() -> Result<(), String> {
        use windows::{
            core::{w, PCWSTR},
            Win32::{
                Foundation::{COLORREF, HINSTANCE},
                Graphics::Gdi::{CombineRgn, GetStockObject, SetWindowRgn, BLACK_BRUSH, RGN_OR},
                System::LibraryLoader::GetModuleHandleW,
            },
        };
        unsafe extern "system" fn procedure(
            hwnd: HWND,
            message: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        struct ProbeWindow {
            hwnd: HWND,
            instance: HINSTANCE,
            class: PCWSTR,
        }
        impl Drop for ProbeWindow {
            fn drop(&mut self) {
                unsafe {
                    let _ = DestroyWindow(self.hwnd);
                    let _ = UnregisterClassW(self.class, self.instance);
                }
            }
        }
        // The caller must first use the existing Type-C topology guard to set
        // SOG_TEST_WORKAREA. No fallback onto the primary/work monitor is allowed.
        let area = crate::panel::test_workarea(true)?.ok_or("缺少显式 Type-C 测试工作区")?;
        if area.right - area.left < 288 || area.bottom - area.top < 128 {
            return Err("Type-C 测试工作区太小".into());
        }
        unsafe {
            let instance: HINSTANCE = GetModuleHandleW(None).map_err(|e| e.to_string())?.into();
            let class = w!("SOGRustOverlay");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(procedure),
                hInstance: instance,
                hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH(GetStockObject(BLACK_BRUSH).0),
                lpszClassName: class,
                ..Default::default()
            };
            if RegisterClassW(&wc) == 0 {
                return Err(windows::core::Error::from_win32().to_string());
            }
            let mut probe = ProbeWindow {
                hwnd: HWND::default(),
                instance,
                class,
            };
            let left = area.left + 24;
            let top = area.top + 24;
            probe.hwnd = CreateWindowExW(
                WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW
                    | WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_NOACTIVATE,
                class,
                w!("仅 Type-C · 黑窗唤醒区域探针"),
                WS_POPUP,
                left,
                top,
                240,
                80,
                None,
                None,
                instance,
                None,
            )
            .map_err(|e| e.to_string())?;
            let union = CreateRectRgn(0, 0, 80, 80);
            let right = CreateRectRgn(160, 0, 240, 80);
            assert!(!union.0.is_null() && !right.0.is_null());
            let combined = CombineRgn(union, union, right, RGN_OR);
            let _ = DeleteObject(right);
            if combined != COMPLEXREGION || SetWindowRgn(probe.hwnd, union, false) == 0 {
                let _ = DeleteObject(union);
                return Err("测试黑窗区域设置失败".into());
            }
            SetLayeredWindowAttributes(probe.hwnd, COLORREF(0), 255, LWA_ALPHA)
                .map_err(|e| e.to_string())?;
            let _ = ShowWindow(probe.hwnd, SW_SHOWNOACTIVATE);
            let point = |x| Position {
                x: left + x,
                y: top + 40,
            };
            let first = overlay_hit(point(40)).ok_or("第一个已选区域未能唤醒")?;
            let second = overlay_hit(point(200)).ok_or("第二个已选区域未能唤醒")?;
            assert_eq!(first.desktop, probe.hwnd.0 as usize);
            assert_eq!(second.desktop, probe.hwnd.0 as usize);
            assert!(
                overlay_hit(point(120)).is_none(),
                "Unselected region hole must reject"
            );
            assert!(
                overlay_hit(point(-1)).is_none(),
                "Outside overlay must reject"
            );
            let _ = ShowWindow(probe.hwnd, SW_HIDE);
            assert!(
                overlay_hit(point(40)).is_none(),
                "Hidden overlay must reject"
            );
            eprintln!("NATIVE_OVERLAY_WAKE selected_regions=2 hole_rejected=true outside_rejected=true hidden_rejected=true no_dimming=true no_input=true");
        }
        Ok(())
    }
    #[test]
    #[ignore = "read-only diagnostic of current desktop point geometry"]
    fn native_desktop_provider_probe() {
        unsafe extern "system" fn find(hwnd: HWND, data: LPARAM) -> BOOL {
            if class_name(hwnd) == "SysListView32" && process(hwnd) == process(GetShellWindow()) {
                (*(data.0 as *mut Vec<usize>)).push(hwnd.0 as usize);
            }
            BOOL(1)
        }
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok().unwrap();
            {
                let check = DesktopHitTest::new().unwrap();
                let mut lists = Vec::<usize>::new();
                let _ = EnumChildWindows(
                    GetShellWindow(),
                    Some(find),
                    LPARAM((&mut lists as *mut Vec<usize>) as isize),
                );
                assert_eq!(lists.len(), 1);
                let hwnd = HWND(lists[0] as *mut _);
                for (x, y, expected) in [
                    (-1919, 64, false),
                    (-1919, 215, false),
                    (-1919, 366, false),
                    (-1000, 3000, true),
                    (-1900, 2000, true),
                ] {
                    let start = Instant::now();
                    let blank = check.blank_on_list(hwnd, Position { x, y });
                    eprintln!(
                        "point=[{x},{y}] blank={blank:?} actual_front_desktop={} elapsed_ms={:.3}",
                        native_desktop(Position { x, y }).is_some(),
                        start.elapsed().as_secs_f64() * 1000.0
                    );
                    assert_eq!(blank, Some(expected));
                }
            }
            CoUninitialize();
        }
    }
    #[test]
    #[ignore = "operator performs a physical side-button double click; no hardware operations"]
    fn native_side_button_double_click_probe_without_dimming() {
        use windows::{core::w, Win32::System::LibraryLoader::GetModuleHandleW};
        struct Probe {
            fired: bool,
        }
        const EVENT: u32 = WM_APP + 42;
        unsafe extern "system" fn receiver(
            hwnd: HWND,
            message: u32,
            w: WPARAM,
            l: LPARAM,
        ) -> LRESULT {
            if message == WM_NCCREATE {
                let create = &*(l.0 as *const CREATESTRUCTW);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
            }
            if message == EVENT && w.0 == 123 {
                let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Probe;
                if !state.is_null() {
                    (*state).fired = true;
                }
                eprintln!(
                    "PHYSICAL_DESKTOP_TRIGGER token={} timestamp={}",
                    w.0, l.0 as u32
                );
                PostQuitMessage(0);
                return LRESULT(0);
            }
            if message == WM_TIMER {
                PostQuitMessage(1);
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, message, w, l)
        }
        unsafe {
            let instance = GetModuleHandleW(None).unwrap();
            let class = w!("SOGDesktopProbeMessageOnly");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(receiver),
                hInstance: instance.into(),
                lpszClassName: class,
                ..Default::default()
            };
            assert_ne!(RegisterClassW(&wc), 0);
            let mut state = Box::new(Probe { fired: false });
            let owner = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class,
                class,
                WINDOW_STYLE::default(),
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                None,
                instance,
                Some((&mut *state as *mut Probe).cast()),
            )
            .unwrap();
            let listener = DesktopMouse::start(
                owner,
                EVENT,
                Bindings {
                    global_mouse: None,
                    desktop: Some((mouse(6), true)),
                },
                123,
            )
            .unwrap();
            SetTimer(owner, 1, 120_000, None);
            eprintln!(
                "READY_FOR_PHYSICAL_SIDE_BUTTON_2_DOUBLE_CLICK timeout_seconds=120 no_dimming=true"
            );
            let mut message = MSG::default();
            while GetMessageW(&mut message, None, 0, 0).0 > 0 {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
            let _ = KillTimer(owner, 1);
            drop(listener);
            let _ = DestroyWindow(owner);
            let _ = UnregisterClassW(class, instance);
            assert!(
                state.fired,
                "No accepted physical desktop gesture in the observation interval"
            );
        }
    }
    fn mouse(key: u32) -> Hotkey {
        Hotkey { modifiers: 0, key }
    }
    fn click(ordinal: u64, timestamp: u32, x: i32, y: i32) -> Click {
        Click {
            scope: 1,
            down: Position { x, y },
            up: Position { x, y },
            ordinal,
            timestamp,
            received: Instant::now(),
            valid: true,
            keyboard: false,
            foreground: 0,
            presentation: 0,
        }
    }
    fn detector() -> Detector {
        Detector {
            first: None,
            interval: 500,
            distance: Position { x: 4, y: 4 },
        }
    }
    fn hit(monitor: usize) -> Option<Hit> {
        Some(Hit {
            desktop: 1,
            monitor,
        })
    }
    #[test]
    fn presentation_change_rejects_cross_state_double_and_round_trip_events() {
        let presentation = AtomicU64::new(0);
        let mut gesture = detector();
        assert!(!gesture.accept(click(1, 100, 10, 10), hit(1), true));
        set_presentation(&presentation, true);
        let dark = presentation.load(Ordering::Acquire);
        assert!(presentation_is_dark(dark));
        let mut second = click(2, 150, 10, 10);
        second.presentation = dark;
        assert!(!gesture.accept(second, hit(1), true));
        let mut third = click(3, 200, 10, 10);
        third.presentation = dark;
        assert!(gesture.accept(third, hit(1), true));

        set_presentation(&presentation, true);
        assert_eq!(presentation.load(Ordering::Acquire), dark);
        set_presentation(&presentation, false);
        assert!(!presentation_is_dark(presentation.load(Ordering::Acquire)));
        set_presentation(&presentation, true);
        assert!(presentation_is_dark(presentation.load(Ordering::Acquire)));
        assert_ne!(presentation.load(Ordering::Acquire), dark);
    }
    #[test]
    fn press_held_across_overlay_change_is_not_a_valid_click() {
        let (mut hook, events) = state(Bindings {
            global_mouse: None,
            desktop: Some((mouse(6), false)),
        });
        let sample = |timestamp| Sample {
            point: Position { x: -1500, y: 800 },
            timestamp,
            injected: false,
            modifiers: 0,
            foreground: 0,
        };
        hook.input(6, true, false, sample(100));
        set_presentation(&hook.presentation, true);
        hook.input(6, false, false, sample(150));
        assert!(!events.try_recv().unwrap().valid);
        hook.input(6, true, false, sample(200));
        hook.input(6, false, false, sample(250));
        let click = events.try_recv().unwrap();
        assert!(click.valid);
        assert!(presentation_is_dark(click.presentation));
    }
    #[test]
    fn single_and_double_modes_are_independent_and_triple_toggles_once() {
        let mut single = detector();
        let mut double = detector();
        for (n, time) in [(1, 100), (2, 250), (3, 400)] {
            assert!(single.accept(click(n, time, -1500, 800), hit(2), false));
            assert_eq!(
                double.accept(click(n, time, -1500, 800), hit(2), true),
                n == 2
            );
        }
    }
    #[test]
    fn unknown_scope_monitor_time_distance_lost_and_stale_events_reject() {
        for (second, target) in [
            (click(2, 150, 1, 1), None),
            (click(2, 150, 1, 1), hit(2)),
            (click(2, 601, 1, 1), hit(1)),
            (click(2, 150, 6, 1), hit(1)),
            (click(3, 150, 1, 1), hit(1)),
        ] {
            let mut d = detector();
            assert!(!d.accept(click(1, 100, 1, 1), hit(1), true));
            assert!(!d.accept(second, target, true));
        }
        let mut d = detector();
        let mut stale = click(1, 100, 1, 1);
        stale.received = Instant::now() - MAX_AGE - Duration::from_millis(1);
        assert!(!d.accept(stale, hit(1), false));
        let mut invalid = click(1, 100, 1, 1);
        invalid.valid = false;
        assert!(!d.accept(invalid, hit(1), false));
    }
    fn state(bindings: Bindings) -> (HookState, mpsc::Receiver<Click>) {
        let (events, received) = mpsc::sync_channel(16);
        (
            HookState {
                events,
                bindings,
                ordinal: 0,
                presses: [None, None],
                keys_down: [false; 256],
                drag: Position { x: 4, y: 4 },
                interval: 500,
                presentation: Arc::new(AtomicU64::new(0)),
            },
            received,
        )
    }
    #[test]
    fn keyboard_autorepeat_modifiers_and_input_scope_are_distinct() {
        let keyboard = Hotkey {
            modifiers: 2,
            key: 0x4b,
        };
        let (mut s, rx) = state(Bindings {
            global_mouse: Some((mouse(5), false)),
            desktop: Some((keyboard, true)),
        });
        let p = Position { x: -1500, y: 800 };
        s.input(
            0x4b,
            true,
            true,
            Sample {
                point: p,
                timestamp: 100,
                injected: false,
                modifiers: 2,
                foreground: 1,
            },
        );
        s.input(
            0x4b,
            true,
            true,
            Sample {
                point: p,
                timestamp: 110,
                injected: false,
                modifiers: 2,
                foreground: 1,
            },
        );
        s.input(
            0x4b,
            false,
            true,
            Sample {
                point: p,
                timestamp: 120,
                injected: false,
                modifiers: 2,
                foreground: 1,
            },
        );
        let key = rx.try_recv().unwrap();
        assert!(key.valid);
        assert_eq!(key.scope, 1);
        assert_eq!(key.ordinal, 1);
        assert!(rx.try_recv().is_err());
        s.input(
            0x4b,
            true,
            true,
            Sample {
                point: p,
                timestamp: 200,
                injected: false,
                modifiers: 0,
                foreground: 1,
            },
        );
        s.input(
            0x4b,
            false,
            true,
            Sample {
                point: p,
                timestamp: 220,
                injected: false,
                modifiers: 0,
                foreground: 1,
            },
        );
        assert!(!rx.try_recv().unwrap().valid);
        s.input(
            5,
            true,
            false,
            Sample {
                point: p,
                timestamp: 300,
                injected: false,
                modifiers: 0,
                foreground: 0,
            },
        );
        s.input(
            5,
            false,
            false,
            Sample {
                point: p,
                timestamp: 320,
                injected: false,
                modifiers: 0,
                foreground: 0,
            },
        );
        let mouse = rx.try_recv().unwrap();
        assert!(mouse.valid);
        assert_eq!(mouse.scope, 0);
    }
    #[test]
    fn side_button_sensor_nudge_is_allowed_but_left_drag_and_large_motion_are_not() {
        for (key, distance, expected) in
            [(6, 11, true), (6, 17, false), (1, 3, true), (1, 5, false)]
        {
            let (mut hook, events) = state(Bindings {
                global_mouse: None,
                desktop: Some((mouse(key), true)),
            });
            let from = Position { x: -1024, y: 1968 };
            let to = Position {
                x: from.x + distance,
                y: from.y,
            };
            hook.input(
                key,
                true,
                false,
                Sample {
                    point: from,
                    timestamp: 100,
                    injected: false,
                    modifiers: 0,
                    foreground: 0,
                },
            );
            hook.motion(to);
            hook.input(
                key,
                false,
                false,
                Sample {
                    point: to,
                    timestamp: 140,
                    injected: false,
                    modifiers: 0,
                    foreground: 0,
                },
            );
            assert_eq!(events.try_recv().unwrap().valid, expected);
        }
        let system = Position { x: 2, y: 2 };
        assert_eq!(gesture_tolerance(mouse(1), system), system);
        assert!(near(
            Position { x: -1027, y: 1968 },
            Position { x: -1023, y: 1967 },
            gesture_tolerance(mouse(6), system)
        ));
    }
    #[test]
    fn drag_return_and_injected_events_cannot_trigger() {
        let (mut s, rx) = state(Bindings {
            global_mouse: None,
            desktop: Some((mouse(5), true)),
        });
        let p = Position { x: -1500, y: 900 };
        s.input(
            5,
            true,
            false,
            Sample {
                point: p,
                timestamp: 100,
                injected: false,
                modifiers: 0,
                foreground: 0,
            },
        );
        s.motion(Position { x: -1400, y: 900 });
        s.motion(p);
        s.input(
            5,
            false,
            false,
            Sample {
                point: p,
                timestamp: 140,
                injected: false,
                modifiers: 0,
                foreground: 0,
            },
        );
        assert!(!rx.try_recv().unwrap().valid);
        s.input(
            5,
            true,
            false,
            Sample {
                point: p,
                timestamp: 200,
                injected: true,
                modifiers: 0,
                foreground: 0,
            },
        );
        s.input(
            5,
            false,
            false,
            Sample {
                point: p,
                timestamp: 220,
                injected: false,
                modifiers: 0,
                foreground: 0,
            },
        );
        assert!(!rx.try_recv().unwrap().valid);
        assert_eq!(button_message(WM_RBUTTONUP, 0), Some((2, false)));
        assert_eq!(button_message(WM_MBUTTONDOWN, 0), Some((4, true)));
        assert_eq!(button_message(WM_XBUTTONUP, 2 << 16), Some((6, false)));
    }
    #[test]
    #[ignore = "installs native hooks; run explicitly on an interactive desktop"]
    fn native_listener_starts_and_stops_without_sending_input() {
        for bindings in [
            Bindings {
                global_mouse: Some((mouse(6), true)),
                desktop: None,
            },
            Bindings {
                global_mouse: None,
                desktop: Some((
                    Hotkey {
                        modifiers: 2,
                        key: 0x4b,
                    },
                    true,
                )),
            },
        ] {
            for _ in 0..3 {
                drop(DesktopMouse::start(HWND::default(), WM_APP + 8, bindings, 123).unwrap());
            }
        }
    }
}
