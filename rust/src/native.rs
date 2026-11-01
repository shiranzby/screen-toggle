//! Windows resources stay on their owning worker thread. No raw handle enters a journal.
use crate::engine::{
    confirm_change_after_snapshot, confirm_change_after_snapshot_with_ack, Backend, Change,
    ChangeResult, Device, Inventory, Kind, ScanRequest, WriteAcknowledgements,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    mem::size_of,
    thread,
    time::Instant,
};
use windows::{
    core::{w, BSTR, PCWSTR, VARIANT},
    Win32::{
        Devices::Display as display,
        Foundation::{BOOL, HANDLE, HINSTANCE, LPARAM, LUID, RECT},
        Graphics::Gdi::{self as gdi, HDC, HMONITOR, MONITORINFOEXW},
        System::{Com as com, Wmi as wmi},
        UI::WindowsAndMessaging as wm,
    },
};

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
pub fn from_wide(s: &[u16]) -> String {
    String::from_utf16_lossy(&s[..s.iter().position(|&v| v == 0).unwrap_or(s.len())])
}
fn os_error(context: &str) -> String {
    format!("{context}：{}", windows::core::Error::from_win32())
}

// Physical monitor handles have no COM/thread affinity. Integer ownership lets
// discovery return them from its joined worker; every acquired handle still has
// exactly one RAII owner, including unsuccessful/ambiguous discovery results.
struct Physical(usize);
impl Physical {
    fn handle(&self) -> HANDLE {
        HANDLE(self.0 as *mut _)
    }
}
impl Drop for Physical {
    fn drop(&mut self) {
        unsafe {
            let _ = display::DestroyPhysicalMonitor(self.handle());
        }
    }
}
struct Apartment;
impl Apartment {
    fn enter() -> Result<Self, String> {
        unsafe {
            com::CoInitializeEx(None, com::COINIT_MULTITHREADED)
                .ok()
                .map_err(|e| e.to_string())?;
        }
        Ok(Self)
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe {
            com::CoUninitialize();
        }
    }
}
struct Wmi {
    svc: wmi::IWbemServices,
    paths: BTreeMap<String, WmiPaths>,
    input: Option<wmi::IWbemClassObject>,
    preparation_error: Option<String>,
    _apartment: Apartment,
}
struct WmiPaths {
    brightness: String,
    methods: String,
}
impl Wmi {
    fn connect() -> Result<Self, String> {
        unsafe {
            let apartment = Apartment::enter()?;
            let locator: wmi::IWbemLocator =
                com::CoCreateInstance(&wmi::WbemLocator, None, com::CLSCTX_INPROC_SERVER)
                    .map_err(|e| e.to_string())?;
            let svc = locator
                .ConnectServer(
                    &BSTR::from("ROOT\\WMI"),
                    &BSTR::new(),
                    &BSTR::new(),
                    &BSTR::new(),
                    0,
                    &BSTR::new(),
                    None,
                )
                .map_err(|e| e.to_string())?;
            // NTLM / no authorization service; let COM own its BSTR/VARIANT memory.
            com::CoSetProxyBlanket(
                &svc,
                10,
                0,
                PCWSTR::null(),
                com::RPC_C_AUTHN_LEVEL_CALL,
                com::RPC_C_IMP_LEVEL_IMPERSONATE,
                None,
                com::EOAC_NONE,
            )
            .map_err(|e| e.to_string())?;
            Ok(Self {
                svc,
                paths: BTreeMap::new(),
                input: None,
                preparation_error: None,
                _apartment: apartment,
            })
        }
    }
    fn query(&self, query: &str) -> Result<Vec<wmi::IWbemClassObject>, String> {
        unsafe {
            let en = self
                .svc
                .ExecQuery(
                    &BSTR::from("WQL"),
                    &BSTR::from(query),
                    wmi::WBEM_FLAG_FORWARD_ONLY | wmi::WBEM_FLAG_RETURN_IMMEDIATELY,
                    None,
                )
                .map_err(|e| e.to_string())?;
            let mut result = Vec::new();
            for _ in 0..64 {
                let mut objects = [None];
                let mut count = 0;
                let status = en.Next(500, &mut objects, &mut count);
                status.ok().map_err(|e| e.to_string())?;
                if count == 0 {
                    if status.0 == 0x40004 {
                        return Err("WMI 枚举超时".into());
                    }
                    return Ok(result);
                }
                if let Some(object) = objects[0].take() {
                    result.push(object);
                }
            }
            Err("WMI 返回了过多屏幕实例".into())
        }
    }
    fn property(object: &wmi::IWbemClassObject, name: &str) -> Result<VARIANT, String> {
        unsafe {
            let mut v = VARIANT::default();
            object
                .Get(PCWSTR(wide(name).as_ptr()), 0, &mut v, None, None)
                .map_err(|e| e.to_string())?;
            Ok(v)
        }
    }
    fn text(object: &wmi::IWbemClassObject, name: &str) -> Result<String, String> {
        // TryFrom clones the string. from_raw would steal the VARIANT's BSTR and double free.
        BSTR::try_from(&Self::property(object, name)?)
            .map(|s| s.to_string())
            .map_err(|e| e.to_string())
    }
    fn get_object(&self, path: &str) -> Result<wmi::IWbemClassObject, String> {
        unsafe {
            let mut object = None;
            self.svc
                .GetObject(
                    &BSTR::from(path),
                    wmi::WBEM_GENERIC_FLAG_TYPE(0),
                    None,
                    Some(&mut object),
                    None,
                )
                .map_err(|e| e.to_string())?;
            object.ok_or_else(|| "WMI 实例已不可用".into())
        }
    }
    fn brightness(object: &wmi::IWbemClassObject) -> Result<u32, String> {
        let value = u32::try_from(&Self::property(object, "CurrentBrightness")?)
            .map_err(|e| e.to_string())?;
        if value > 100 {
            return Err("WMI 亮度不在 0–100 范围".into());
        }
        Ok(value)
    }
    fn invalidate(&mut self) {
        self.paths.clear();
        self.input = None;
        self.preparation_error = None;
    }
    fn discover_paths(
        &mut self,
        access: &DeviceAccess,
        read_brightness: bool,
    ) -> Result<Vec<wmi::IWbemClassObject>, String> {
        self.paths.clear();
        self.preparation_error = None;
        let result = (|| {
            // Recovery needs fresh active identities and method paths, not a
            // second sample of the brightness that is about to be overwritten.
            let query = if read_brightness {
                "SELECT * FROM WmiMonitorBrightness WHERE Active = TRUE"
            } else {
                "SELECT InstanceName FROM WmiMonitorBrightness WHERE Active = TRUE"
            };
            let mut objects = Vec::new();
            for object in self.query(query)? {
                let instance = Self::text(&object, "InstanceName")?;
                if !access.allows(&format!("wmi:{instance}"), Kind::Internal) {
                    continue;
                }
                let key = normalized(&instance);
                let paths = WmiPaths {
                    brightness: Self::text(&object, "__PATH")?,
                    methods: String::new(),
                };
                if self.paths.insert(key, paths).is_some() {
                    return Err("内置屏实例身份不唯一".into());
                }
                objects.push(object);
            }
            if !objects.is_empty() {
                let prepared = (|| -> Result<(), String> {
                    // Resolve paths/signature once while external discovery runs.
                    // Each write still receives a fresh parameter instance.
                    for object in self.query("SELECT * FROM WmiMonitorBrightnessMethods")? {
                        let instance = Self::text(&object, "InstanceName")?;
                        if let Some(paths) = self.paths.get_mut(&normalized(&instance)) {
                            if !paths.methods.is_empty() {
                                return Err("内置屏方法实例身份不唯一".into());
                            }
                            paths.methods = Self::text(&object, "__PATH")?;
                        }
                    }
                    if self.paths.values().any(|p| p.methods.is_empty()) {
                        return Err("未找到原内置屏方法实例".into());
                    }
                    if self.input.is_none() {
                        let class = self.get_object("WmiMonitorBrightnessMethods")?;
                        unsafe {
                            let mut input = None;
                            class
                                .GetMethod(
                                    w!("WmiSetBrightness"),
                                    0,
                                    &mut input,
                                    std::ptr::null_mut(),
                                )
                                .map_err(|e| e.to_string())?;
                            self.input = Some(input.ok_or("WMI 方法没有输入参数")?);
                        }
                    }
                    Ok(())
                })();
                if let Err(error) = prepared {
                    // Brightness discovery succeeded. Keep this device in the
                    // journal/changes so metadata failure is reported as a
                    // failed write, never a silently missing test channel.
                    self.input = None;
                    self.preparation_error = Some(error);
                }
            }
            Ok(objects)
        })();
        if result.is_err() {
            self.invalidate();
        }
        result
    }
    fn values(&mut self, access: &DeviceAccess) -> Result<Vec<(String, u32)>, String> {
        let result: Result<Vec<_>, String> = self
            .discover_paths(access, true)?
            .iter()
            .map(|object| {
                Ok((
                    Self::text(object, "InstanceName")?,
                    Self::brightness(object)?,
                ))
            })
            .collect();
        if result.is_err() {
            self.invalidate();
        }
        result
    }
    fn read(&mut self, instance: &str) -> Result<u32, String> {
        let result = (|| {
            let path = &self
                .paths
                .get(&normalized(instance))
                .ok_or("原内置屏身份未确认")?
                .brightness;
            // GetObject fetches fresh values; only the object path is cached.
            let object = self.get_object(path)?;
            if !Self::text(&object, "InstanceName")?.eq_ignore_ascii_case(instance)
                || !bool::try_from(&Self::property(&object, "Active")?)
                    .map_err(|e| e.to_string())?
            {
                return Err("原内置屏已断开或实例身份变化".into());
            }
            Self::brightness(&object)
        })();
        if result.is_err() {
            self.invalidate();
        }
        result
    }
    fn set(
        &mut self,
        instance: &str,
        value: u32,
        current: &(dyn Fn() -> bool + Sync),
    ) -> Result<(), String> {
        let result = (|| unsafe {
            if let Some(error) = &self.preparation_error {
                return Err(format!("WMI 方法预备失败：{error}"));
            }
            if value > 100 {
                return Err("WMI 目标亮度无效".into());
            }
            let path = &self
                .paths
                .get(&normalized(instance))
                .ok_or("原内置屏方法身份未确认")?
                .methods;
            let params = self
                .input
                .as_ref()
                .ok_or("WMI 方法签名未确认")?
                .SpawnInstance(0)
                .map_err(|e| e.to_string())?;
            params
                // WMI represents CIM uint32 with VT_I4 (not VT_UI4).
                .Put(w!("Timeout"), 0, &VARIANT::from(0i32), 0)
                .map_err(|e| format!("WMI Timeout 参数：{e}"))?;
            params
                .Put(w!("Brightness"), 0, &VARIANT::from(value as u8), 0)
                .map_err(|e| format!("WMI Brightness 参数：{e}"))?;
            if !current() {
                return Ok(());
            }
            let mut returned = None;
            self.svc
                .ExecMethod(
                    &BSTR::from(path),
                    &BSTR::from("WmiSetBrightness"),
                    wmi::WBEM_GENERIC_FLAG_TYPE(0),
                    None,
                    &params,
                    Some(&mut returned),
                    None,
                )
                .map_err(|e| format!("WmiSetBrightness 调用：{e}"))?;
            // Some providers return S_OK without an output object. The Engine
            // independently verifies the resulting brightness in either case.
            if let Some(output) = returned {
                let code = u32::try_from(&Self::property(&output, "ReturnValue")?)
                    .map_err(|e| e.to_string())?;
                if code != 0 {
                    return Err(format!("WmiSetBrightness 返回错误 {code}"));
                }
            }
            Ok(())
        })();
        if result.is_err() {
            self.invalidate();
        }
        result
    }
}

#[derive(Clone)]
struct Target {
    id: String,
    source: String,
    name: String,
    adapter: LUID,
    target: u32,
    internal: bool,
}
fn header(
    kind: display::DISPLAYCONFIG_DEVICE_INFO_TYPE,
    size: usize,
    adapter: LUID,
    id: u32,
) -> display::DISPLAYCONFIG_DEVICE_INFO_HEADER {
    display::DISPLAYCONFIG_DEVICE_INFO_HEADER {
        r#type: kind,
        size: size as u32,
        adapterId: adapter,
        id,
    }
}
fn targets() -> Result<Vec<Target>, String> {
    unsafe {
        for _ in 0..3 {
            let (mut count, mut modes) = (0, 0);
            let status = display::GetDisplayConfigBufferSizes(
                display::QDC_ONLY_ACTIVE_PATHS,
                &mut count,
                &mut modes,
            );
            if status.0 != 0 {
                return Err(format!("显示拓扑不可用（{}）", status.0));
            }
            if count > 256 || modes > 4096 {
                return Err("显示拓扑大小异常".into());
            }
            let mut paths = vec![display::DISPLAYCONFIG_PATH_INFO::default(); count as usize];
            let mut mode = vec![display::DISPLAYCONFIG_MODE_INFO::default(); modes as usize];
            let status = display::QueryDisplayConfig(
                display::QDC_ONLY_ACTIVE_PATHS,
                &mut count,
                paths.as_mut_ptr(),
                &mut modes,
                mode.as_mut_ptr(),
                None,
            );
            if status.0 == 122 {
                continue;
            }
            if status.0 != 0 {
                return Err(format!("查询显示拓扑失败（{}）", status.0));
            }
            paths.truncate(count as usize);
            let mut result = Vec::new();
            for path in paths {
                let mut source = display::DISPLAYCONFIG_SOURCE_DEVICE_NAME {
                    header: header(
                        display::DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                        size_of::<display::DISPLAYCONFIG_SOURCE_DEVICE_NAME>(),
                        path.sourceInfo.adapterId,
                        path.sourceInfo.id,
                    ),
                    ..Default::default()
                };
                let mut target = display::DISPLAYCONFIG_TARGET_DEVICE_NAME {
                    header: header(
                        display::DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
                        size_of::<display::DISPLAYCONFIG_TARGET_DEVICE_NAME>(),
                        path.targetInfo.adapterId,
                        path.targetInfo.id,
                    ),
                    ..Default::default()
                };
                if display::DisplayConfigGetDeviceInfo(&mut source.header) != 0
                    || display::DisplayConfigGetDeviceInfo(&mut target.header) != 0
                {
                    continue;
                }
                let id = from_wide(&target.monitorDevicePath).to_lowercase();
                if id.is_empty() {
                    continue;
                }
                let technology = path.targetInfo.outputTechnology;
                let internal = technology == display::DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL
                    || technology == display::DISPLAYCONFIG_OUTPUT_TECHNOLOGY_LVDS
                    || technology == display::DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED
                    || technology == display::DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EMBEDDED;
                result.push(Target {
                    id,
                    source: from_wide(&source.viewGdiDeviceName),
                    name: from_wide(&target.monitorFriendlyDeviceName),
                    adapter: path.targetInfo.adapterId,
                    target: path.targetInfo.id,
                    internal,
                });
            }
            return Ok(result);
        }
        Err("显示拓扑连续变化，请稍后重试".into())
    }
}
fn hdr_value(target: &Target) -> Result<Option<u32>, String> {
    unsafe {
        let mut info = display::DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO {
            header: header(
                display::DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
                size_of::<display::DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO>(),
                target.adapter,
                target.target,
            ),
            ..Default::default()
        };
        let code = display::DisplayConfigGetDeviceInfo(&mut info.header);
        if code != 0 {
            return Err(format!("HDR 状态不可读（{code}）"));
        }
        let bits = info.Anonymous.value;
        // wideColorEnforced means WCG is mandatory: do not treat it as a user HDR toggle.
        // wingdi.h: bit 0 supported, bit 1 enabled, bit 2 WCG, bit 3 policy disabled.
        if bits & 1 == 0 || bits & (4 | 8) != 0 {
            return Ok(None);
        }
        Ok(Some(u32::from(bits & 2 != 0)))
    }
}
unsafe extern "system" fn collect_monitor(
    monitor: HMONITOR,
    _dc: HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> BOOL {
    // EnumDisplayMonitors is synchronous and this vector lives throughout the call.
    unsafe {
        (*(data.0 as *mut Vec<HMONITOR>)).push(monitor);
    }
    BOOL(1)
}

fn normalized(id: &str) -> String {
    id.to_ascii_lowercase()
}
fn id_prefix(kind: Kind) -> &'static str {
    match kind {
        Kind::External => "ddc:",
        Kind::Internal => "wmi:",
        Kind::Hdr => "hdr:",
    }
}
#[derive(Clone, Default)]
struct DeviceAccess(Option<BTreeSet<String>>);
impl DeviceAccess {
    fn restricted(ids: Vec<String>) -> Result<Self, String> {
        if ids.is_empty()
            || ids.iter().any(|id| {
                id.trim() != id
                    || ![Kind::External, Kind::Internal, Kind::Hdr]
                        .iter()
                        .any(|kind| {
                            id.starts_with(id_prefix(*kind)) && id.len() > id_prefix(*kind).len()
                        })
            })
        {
            return Err("限定设备需要非空的完整 ddc:/wmi:/hdr: 身份列表".into());
        }
        Ok(Self(Some(ids.iter().map(|id| normalized(id)).collect())))
    }
    fn allows(&self, id: &str, kind: Kind) -> bool {
        id.starts_with(id_prefix(kind))
            && id.len() > id_prefix(kind).len()
            && self
                .0
                .as_ref()
                .is_none_or(|ids| ids.contains(&normalized(id)))
    }
    fn includes(&self, kind: Kind) -> bool {
        self.0
            .as_ref()
            .is_none_or(|ids| ids.iter().any(|id| id.starts_with(id_prefix(kind))))
    }
    fn intersect(&self, entries: &[Device]) -> Self {
        Self(Some(
            entries
                .iter()
                .filter(|d| self.allows(&d.id, d.kind))
                .map(|d| normalized(&d.id))
                .collect(),
        ))
    }
}
type TopologyKey = Vec<(String, String, u32, i32, u32, bool)>;
fn topology_key(topology: &[Target]) -> TopologyKey {
    let mut key: Vec<_> = topology
        .iter()
        .map(|t| {
            (
                t.id.clone(),
                t.source.clone(),
                t.adapter.LowPart,
                t.adapter.HighPart,
                t.target,
                t.internal,
            )
        })
        .collect();
    key.sort();
    key
}
fn reusable_snapshot(
    previous: Option<&TopologyKey>,
    current: &TopologyKey,
    entries: &[Device],
    has_resource: impl Fn(&Device) -> bool,
) -> bool {
    previous == Some(current)
        && entries
            .iter()
            .all(|d| d.kind != Kind::Hdr && has_resource(d))
}
fn validate_recovery_entries(access: &DeviceAccess, entries: &[Device]) -> Result<(), String> {
    let mut identities = BTreeSet::new();
    for d in entries {
        if !access.allows(&d.id, d.kind) {
            return Err("设备不在允许的身份列表中，已拒绝硬件请求".into());
        }
        if !identities.insert((d.kind, normalized(&d.id))) {
            return Err("恢复记录存在重复设备身份，已拒绝硬件请求".into());
        }
        if d.min > d.value
            || d.value > d.max
            || match d.kind {
                Kind::External => false,
                Kind::Internal => d.min != 0 || d.max != 100,
                Kind::Hdr => d.min != 0 || d.max != 1,
            }
        {
            return Err("恢复记录的亮度范围无效，已拒绝硬件请求".into());
        }
    }
    Ok(())
}
#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct NativeStats {
    pub full_scans: u32,
    pub reused_restores: u32,
    pub restore_rebinds: u32,
}
#[derive(Default)]
struct ExternalScan {
    inventory: Inventory,
    physical: BTreeMap<String, Physical>,
    ranges: BTreeMap<String, (u32, u32)>,
    targets: BTreeMap<String, Target>,
}

pub struct Native {
    physical: BTreeMap<String, Physical>,
    ranges: BTreeMap<String, (u32, u32)>,
    targets: BTreeMap<String, Target>,
    wmi: Option<Wmi>,
    access: DeviceAccess,
    snapshot: Option<TopologyKey>,
    stats: NativeStats,
    pub test_mode: bool,
}
impl Native {
    pub fn new(test_mode: bool) -> Self {
        Self {
            physical: BTreeMap::new(),
            ranges: BTreeMap::new(),
            targets: BTreeMap::new(),
            wmi: None,
            access: DeviceAccess::default(),
            snapshot: None,
            stats: NativeStats::default(),
            test_mode,
        }
    }
    /// A fail-closed hardware test backend. Restrictions apply before physical
    /// enumeration as well as to every direct read/write/apply entry point.
    pub fn restricted_to(ids: Vec<String>) -> Result<Self, String> {
        Ok(Self {
            access: DeviceAccess::restricted(ids)?,
            ..Self::new(false)
        })
    }
    pub fn stats(&self) -> NativeStats {
        self.stats
    }
    fn ensure_allowed(&self, id: &str, kind: Kind) -> Result<(), String> {
        if self.access.allows(id, kind) {
            Ok(())
        } else {
            Err("设备不在允许的身份列表中，已拒绝硬件请求".into())
        }
    }
    fn ddc_read(handle: HANDLE) -> Result<(u32, u32, u32), String> {
        unsafe {
            let (mut min, mut value, mut max) = (0, 0, 0);
            if display::GetMonitorBrightness(handle, &mut min, &mut value, &mut max) == 0 {
                return Err(os_error("DDC 亮度读取失败"));
            }
            Ok((min, value, max))
        }
    }
    fn scan_internal(
        &mut self,
        access: &DeviceAccess,
        requested: bool,
    ) -> Result<Vec<(String, u32)>, String> {
        if !requested {
            return Ok(Vec::new());
        }
        if self.wmi.is_none() {
            self.wmi = Some(Wmi::connect()?);
        }
        let result = self.wmi.as_mut().ok_or("WMI 不可用")?.values(access);
        if result.is_err() {
            // Reconnect on the next scan; a stale provider connection is not a
            // permanent failed cache and COM is released on this same worker.
            self.wmi = None;
        }
        result
    }
    fn scan_with_access(
        &mut self,
        mut request: ScanRequest,
        access: DeviceAccess,
    ) -> Result<Inventory, String> {
        self.stats.full_scans = self.stats.full_scans.saturating_add(1);
        self.snapshot = None;
        self.physical.clear();
        self.ranges.clear();
        self.targets.clear();
        if let Some(wmi) = &mut self.wmi {
            wmi.paths.clear();
        }
        let mut inventory = Inventory::default();
        if self.test_mode {
            inventory
                .warnings
                .push("测试模式：未修改亮度、HDR 或系统光标".into());
            return Ok(inventory);
        }
        request.external &= access.includes(Kind::External);
        request.internal &= access.includes(Kind::Internal);
        request.hdr &= access.includes(Kind::Hdr);
        if !request.external && !request.internal && !request.hdr {
            return Ok(inventory);
        }
        let (topology, verified) = match targets() {
            Ok(t) => (t, true),
            Err(error) => {
                inventory.warnings.push(error);
                (Vec::new(), false)
            }
        };
        let jobs = if request.external || request.hdr {
            vec![request]
        } else {
            Vec::new()
        };
        // Start discovery/HDR/physical enumeration before touching COM. The
        // original worker prepares WMI concurrently, without moving COM objects.
        let (external, internal) = parallel_with_local(
            jobs,
            |request| discover_external(&topology, request, &access),
            || self.scan_internal(&access, request.internal),
        );
        if let Some(external) = external.into_iter().next() {
            let external = external.map_err(|_| "外屏扫描线程异常结束")??;
            inventory.devices.extend(external.inventory.devices);
            inventory.warnings.extend(external.inventory.warnings);
            self.physical = external.physical;
            self.ranges = external.ranges;
            self.targets = external.targets;
        }
        match internal {
            Ok(values) => {
                inventory
                    .devices
                    .extend(values.into_iter().map(|(id, value)| Device {
                        id: format!("wmi:{id}"),
                        name: "内置屏".into(),
                        kind: Kind::Internal,
                        min: 0,
                        value,
                        max: 100,
                    }));
            }
            Err(error) => inventory
                .warnings
                .push(format!("内置屏调光不可用：{error}")),
        }
        if verified {
            self.snapshot = Some(topology_key(&topology));
        }
        Ok(inventory)
    }
    fn snapshot_covers(&self, entries: &[Device], current: &TopologyKey) -> bool {
        reusable_snapshot(self.snapshot.as_ref(), current, entries, |d| {
            self.access.allows(&d.id, d.kind)
                && match d.kind {
                    Kind::External => {
                        self.physical.contains_key(&d.id)
                            && self
                                .ranges
                                .get(&d.id)
                                .is_some_and(|&(min, max)| min <= d.value && d.value <= max)
                    }
                    Kind::Internal => {
                        self.wmi.as_ref().is_some_and(|wmi| {
                            wmi.preparation_error.is_none()
                                && wmi.input.is_some()
                                && d.id.strip_prefix("wmi:").is_some_and(|instance| {
                                    wmi.paths.get(&normalized(instance)).is_some_and(|p| {
                                        !p.brightness.is_empty() && !p.methods.is_empty()
                                    })
                                })
                        }) && d.value <= 100
                    }
                    // HDR may invalidate handles without changing target IDs.
                    Kind::Hdr => false,
                }
        })
    }
    fn write_current(
        &mut self,
        id: &str,
        kind: Kind,
        value: u32,
        current: &(dyn Fn() -> bool + Sync),
    ) -> Result<(), String> {
        self.ensure_allowed(id, kind)?;
        if self.test_mode {
            return Err("测试模式禁止硬件写入".into());
        }
        if !current() {
            return Ok(());
        }
        let result = (|| match kind {
            Kind::External => {
                let physical = self
                    .physical
                    .get(id)
                    .ok_or("原外接屏已断开或无法唯一识别")?;
                let &(min, max) = self.ranges.get(id).ok_or("外接屏范围未确认")?;
                ddc_write(physical.handle(), value, min, max)
            }
            Kind::Internal => self.wmi.as_mut().ok_or("WMI 不可用")?.set(
                id.strip_prefix("wmi:").ok_or("内置屏身份无效")?,
                value,
                current,
            ),
            Kind::Hdr => {
                let target = self.targets.get(id).ok_or("原 HDR 屏已断开")?;
                let state = display::DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE {
                    header: header(
                        display::DISPLAYCONFIG_DEVICE_INFO_SET_ADVANCED_COLOR_STATE,
                        size_of::<display::DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE>(),
                        target.adapter,
                        target.target,
                    ),
                    Anonymous: display::DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE_0 {
                        value: u32::from(value != 0),
                    },
                };
                let code = unsafe { display::DisplayConfigSetDeviceInfo(&state.header) };
                if code != 0 {
                    return Err(format!("HDR 写入失败（{code}）"));
                }
                Ok(())
            }
        })();
        if result.is_err() || kind == Kind::Hdr {
            self.snapshot = None;
        }
        result
    }
}
fn ddc_write(handle: HANDLE, value: u32, min: u32, max: u32) -> Result<(), String> {
    if value < min || value > max {
        return Err("原亮度超出现设备范围，保留恢复记录".into());
    }
    if unsafe { display::SetMonitorBrightness(handle, value) } == 0 {
        return Err(os_error("DDC 调光失败"));
    }
    Ok(())
}
/// DDC jobs run on scoped threads; COM work stays on the calling worker.
/// Returning joins every job, including when a sibling returns an error.
fn parallel_with_local<J: Send, R: Send, L>(
    jobs: Vec<J>,
    work: impl Fn(J) -> R + Sync,
    local: impl FnOnce() -> L,
) -> (Vec<thread::Result<R>>, L) {
    thread::scope(|scope| {
        let work = &work;
        let pending: Vec<_> = jobs
            .into_iter()
            .map(|job| thread::Builder::new().spawn_scoped(scope, move || work(job)))
            .collect();
        let local = local();
        let results = pending
            .into_iter()
            .map(|job| match job {
                Ok(job) => job.join(),
                Err(error) => Err(Box::new(error) as Box<dyn std::any::Any + Send>),
            })
            .collect();
        (results, local)
    })
}
fn discover_external(
    topology: &[Target],
    request: ScanRequest,
    access: &DeviceAccess,
) -> Result<ExternalScan, String> {
    let mut out = ExternalScan::default();
    if request.hdr {
        for target in topology {
            let id = format!("hdr:{}", target.id);
            if !access.allows(&id, Kind::Hdr) {
                continue;
            }
            match hdr_value(target) {
                Ok(Some(value)) => {
                    out.inventory.devices.push(Device {
                        id: id.clone(),
                        name: format!("{} HDR", target.name),
                        kind: Kind::Hdr,
                        min: 0,
                        value,
                        max: 1,
                    });
                    out.targets.insert(id, target.clone());
                }
                Ok(None) => {}
                Err(error) => out
                    .inventory
                    .warnings
                    .push(format!("{}：{error}", target.name)),
            }
        }
    }
    if !request.external {
        return Ok(out);
    }
    let mut logical = Vec::<HMONITOR>::new();
    unsafe {
        gdi::EnumDisplayMonitors(
            None,
            None,
            Some(collect_monitor),
            LPARAM((&mut logical as *mut Vec<HMONITOR>) as isize),
        )
        .ok()
        .map_err(|e| e.to_string())?;
    }
    let mut candidates = BTreeMap::new();
    for monitor in logical {
        unsafe {
            let mut info = MONITORINFOEXW::default();
            info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
            if !gdi::GetMonitorInfoW(monitor, &mut info.monitorInfo).as_bool() {
                out.inventory.warnings.push(os_error("逻辑屏读取失败"));
                continue;
            }
            let source = from_wide(&info.szDevice);
            let matching: Vec<_> = topology.iter().filter(|t| t.source == source).collect();
            if matching.len() != 1 {
                out.inventory
                    .warnings
                    .push(format!("{source}：无法唯一识别物理屏，跳过 DDC"));
                continue;
            }
            let target = matching[0];
            let id = format!("ddc:{}", target.id);
            // Exclude built-in and unauthorized screens before *any* physical
            // monitor API. Filtering the returned inventory would be too late.
            if target.internal || !access.allows(&id, Kind::External) {
                continue;
            }
            let mut count = 0;
            if display::GetNumberOfPhysicalMonitorsFromHMONITOR(monitor, &mut count).is_err()
                || count == 0
            {
                continue;
            }
            if count > 64 {
                out.inventory.warnings.push("物理屏数量异常，已跳过".into());
                continue;
            }
            let mut monitors = vec![display::PHYSICAL_MONITOR::default(); count as usize];
            if display::GetPhysicalMonitorsFromHMONITOR(monitor, &mut monitors).is_err() {
                out.inventory.warnings.push(os_error("物理屏枚举失败"));
                continue;
            }
            let owned: Vec<_> = monitors
                .iter()
                .map(|m| Physical(m.hPhysicalMonitor.0 as usize))
                .collect();
            if count != 1 {
                out.inventory
                    .warnings
                    .push(format!("{source}：无法唯一识别物理屏，跳过 DDC"));
                continue;
            }
            let physical = owned.into_iter().next().ok_or("缺少物理屏句柄")?;
            candidates.insert(id, (target.name.clone(), physical));
        }
    }
    // Owners remain in candidates until every read joins. Returning out then
    // transfers unique integer-backed ownership to the original worker.
    let jobs = candidates.values().map(|(_, p)| p.0).collect();
    let (reads, ()) =
        parallel_with_local(jobs, |raw| Native::ddc_read(HANDLE(raw as *mut _)), || ());
    for ((id, (name, physical)), read) in candidates.into_iter().zip(reads) {
        match read.unwrap_or_else(|_| Err("DDC 扫描线程异常结束".into())) {
            Ok((min, value, max)) => {
                out.inventory.devices.push(Device {
                    id: id.clone(),
                    name,
                    kind: Kind::External,
                    min,
                    value,
                    max,
                });
                out.ranges.insert(id.clone(), (min, max));
                out.physical.insert(id, physical);
            }
            Err(error) => out.inventory.warnings.push(format!("{name}：{error}")),
        }
    }
    Ok(out)
}
impl Backend for Native {
    fn scan(&mut self) -> Result<Inventory, String> {
        self.scan_required(ScanRequest::all())
    }
    fn scan_required(&mut self, request: ScanRequest) -> Result<Inventory, String> {
        self.scan_with_access(request, self.access.clone())
    }
    fn prepare_restore(
        &mut self,
        entries: &[Device],
        allow_reuse: bool,
    ) -> Result<Vec<String>, String> {
        validate_recovery_entries(&self.access, entries)?;
        if entries.is_empty() {
            return Ok(Vec::new());
        }
        if allow_reuse && !self.test_mode && entries.iter().all(|d| d.kind != Kind::Hdr) {
            if let Ok(topology) = targets() {
                if self.snapshot_covers(entries, &topology_key(&topology)) {
                    self.stats.reused_restores = self.stats.reused_restores.saturating_add(1);
                    // Consume reuse permission. A later restore must refresh,
                    // including a retry after any failed/superseded operation.
                    self.snapshot = None;
                    return Ok(Vec::new());
                }
            }
        }
        self.scan_with_access(
            ScanRequest::from_devices(entries),
            self.access.intersect(entries),
        )
        .map(|inventory| inventory.warnings)
    }
    fn apply_internal(
        &mut self,
        changes: &[Change],
        current: &(dyn Fn() -> bool + Sync),
        all_written: Option<&(dyn Fn() + Sync)>,
    ) -> Vec<ChangeResult> {
        let batch_start = Instant::now();
        let acknowledgements = all_written
            .map(|callback| WriteAcknowledgements::new(changes.len(), current, callback));
        let mut grouped: BTreeMap<&str, Vec<&Change>> = BTreeMap::new();
        let mut results = Vec::new();
        let mut authorized = Vec::new();
        for change in changes {
            match self.ensure_allowed(&change.device.id, change.device.kind) {
                Ok(()) => authorized.push(change),
                Err(error) => results.push(ChangeResult::completed(
                    change,
                    batch_start,
                    Instant::now(),
                    Err(error),
                )),
            }
        }
        for change in authorized
            .iter()
            .copied()
            .filter(|c| c.device.kind == Kind::External)
        {
            grouped.entry(&change.device.id).or_default().push(change);
        }
        let mut jobs = Vec::new();
        for (id, group) in grouped {
            match (self.physical.get(id), self.ranges.get(id)) {
                (Some(physical), Some(&(min, max))) if !self.test_mode => {
                    // A group keeps duplicate commands for one physical screen
                    // serial while independent screens can overlap.
                    jobs.push((physical.0, min, max, group));
                }
                _ => {
                    for change in group {
                        results.push(ChangeResult::completed(
                            change,
                            batch_start,
                            Instant::now(),
                            Err(format!(
                                "{}：原外接屏已断开或无法唯一识别",
                                change.device.name
                            )),
                        ));
                    }
                }
            }
        }
        let identities: Vec<_> = jobs.iter().map(|(_, _, _, group)| group.clone()).collect();
        // physical/ranges remain owned by this Native until all scoped threads
        // join. Local WMI/HDR work cannot clear or replace either map.
        let acknowledgements = acknowledgements.as_ref();
        let (external, local) = parallel_with_local(
            jobs,
            |(raw, min, max, group)| {
                let mut handle = HANDLE(raw as *mut _);
                group
                    .into_iter()
                    .map(|change| {
                        if let Some(ack) = acknowledgements {
                            confirm_change_after_snapshot_with_ack(
                                &mut handle,
                                change,
                                current,
                                batch_start,
                                |handle| Self::ddc_read(*handle).map(|(_, value, _)| value),
                                |handle, value| ddc_write(*handle, value, min, max),
                                &|| ack.success(),
                            )
                        } else {
                            confirm_change_after_snapshot(
                                &mut handle,
                                change,
                                current,
                                batch_start,
                                |handle| Self::ddc_read(*handle).map(|(_, value, _)| value),
                                |handle, value| ddc_write(*handle, value, min, max),
                            )
                        }
                    })
                    .collect::<Vec<_>>()
            },
            || {
                authorized
                    .iter()
                    .copied()
                    .filter(|change| change.device.kind != Kind::External)
                    .map(|change| {
                        if let Some(ack) = acknowledgements {
                            confirm_change_after_snapshot_with_ack(
                                self,
                                change,
                                current,
                                batch_start,
                                |backend| backend.read(&change.device.id, change.device.kind),
                                |backend, value| {
                                    backend.write_current(
                                        &change.device.id,
                                        change.device.kind,
                                        value,
                                        current,
                                    )
                                },
                                &|| ack.success(),
                            )
                        } else {
                            confirm_change_after_snapshot(
                                self,
                                change,
                                current,
                                batch_start,
                                |backend| backend.read(&change.device.id, change.device.kind),
                                |backend, value| {
                                    backend.write_current(
                                        &change.device.id,
                                        change.device.kind,
                                        value,
                                        current,
                                    )
                                },
                            )
                        }
                    })
                    .collect::<Vec<_>>()
            },
        );
        results.extend(local);
        for (result, group) in external.into_iter().zip(identities) {
            match result {
                Ok(completed) => results.extend(completed),
                Err(_) => {
                    for change in group {
                        results.push(ChangeResult::completed(
                            change,
                            batch_start,
                            Instant::now(),
                            Err(format!(
                                "{}：DDC 调光线程异常结束，保留恢复记录",
                                change.device.name
                            )),
                        ));
                    }
                }
            }
        }
        if results
            .iter()
            .any(|r| r.kind == Kind::Hdr || !matches!(r.result, Ok(true)))
        {
            self.snapshot = None;
        }
        results
    }
    fn apply(
        &mut self,
        changes: &[Change],
        current: &(dyn Fn() -> bool + Sync),
    ) -> Vec<ChangeResult> {
        self.apply_internal(changes, current, None)
    }
    fn apply_with_write_ack(
        &mut self,
        changes: &[Change],
        current: &(dyn Fn() -> bool + Sync),
        all_written: &(dyn Fn() + Sync),
    ) -> Vec<ChangeResult> {
        self.apply_internal(changes, current, Some(all_written))
    }
    fn write(&mut self, id: &str, kind: Kind, value: u32) -> Result<(), String> {
        let result = self.write_current(id, kind, value, &|| true);
        if result.is_err() || kind == Kind::Hdr {
            self.snapshot = None;
        }
        result
    }
    fn read(&mut self, id: &str, kind: Kind) -> Result<u32, String> {
        self.ensure_allowed(id, kind)?;
        let result = (|| match kind {
            Kind::External => {
                Self::ddc_read(self.physical.get(id).ok_or("原外接屏已断开")?.handle())
                    .map(|(_, v, _)| v)
            }
            Kind::Internal => self
                .wmi
                .as_mut()
                .ok_or("WMI 不可用")?
                .read(id.strip_prefix("wmi:").ok_or("内置屏身份无效")?),
            Kind::Hdr => hdr_value(self.targets.get(id).ok_or("原 HDR 屏已断开")?)?
                .ok_or("HDR 已不可用".into()),
        })();
        if result.is_err() {
            self.snapshot = None;
        }
        result
    }
    fn cursor(&mut self, hidden: bool) -> Result<(), String> {
        if self.access.0.is_some() {
            return Err("限定设备测试不修改系统光标".into());
        }
        if self.test_mode {
            Ok(())
        } else {
            set_cursor_hidden(hidden)
        }
    }
    fn remote_active(&self) -> bool {
        remote_active()
    }
}

pub fn set_cursor_hidden(hidden: bool) -> Result<(), String> {
    unsafe {
        if !hidden {
            wm::SystemParametersInfoW(
                wm::SPI_SETCURSORS,
                0,
                None,
                wm::SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
            .map_err(|e| e.to_string())?;
            if let Ok(cursor) = wm::LoadCursorW(None, wm::IDC_ARROW) {
                let _ = wm::SetCursor(cursor);
            }
            return Ok(());
        }
        let and_mask = [255u8; 128];
        let xor_mask = [0u8; 128];
        for id in [
            32512, 32513, 32514, 32515, 32516, 32642, 32643, 32644, 32645, 32646, 32648, 32649,
            32650,
        ] {
            let cursor = match wm::CreateCursor(
                HINSTANCE::default(),
                0,
                0,
                32,
                32,
                and_mask.as_ptr().cast(),
                xor_mask.as_ptr().cast(),
            ) {
                Ok(cursor) => cursor,
                Err(e) => {
                    let _ = wm::SystemParametersInfoW(
                        wm::SPI_SETCURSORS,
                        0,
                        None,
                        wm::SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
                    );
                    return Err(e.to_string());
                }
            };
            if let Err(e) = wm::SetSystemCursor(cursor, wm::SYSTEM_CURSOR_ID(id)) {
                let _ = wm::DestroyCursor(cursor);
                let _ = wm::SystemParametersInfoW(
                    wm::SPI_SETCURSORS,
                    0,
                    None,
                    wm::SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
                );
                return Err(e.to_string());
            }
        }
        Ok(())
    }
}
pub fn remote_active() -> bool {
    crate::remote::detect().active
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::confirm_change;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Condvar, Mutex,
    };
    use std::time::Duration;

    fn device(id: &str, kind: Kind) -> Device {
        Device {
            id: id.into(),
            name: id.into(),
            kind,
            min: 0,
            value: 72,
            max: 100,
        }
    }

    #[test]
    fn restricted_backend_denies_other_devices_before_resource_lookup() {
        assert!(Native::restricted_to(Vec::new()).is_err());
        assert!(Native::restricted_to(vec!["ddc:".into()]).is_err());
        assert!(Native::restricted_to(vec!["display3".into()]).is_err());
        let mut native = Native::restricted_to(vec!["ddc:TYPE_C".into()]).unwrap();
        for (id, kind) in [
            ("ddc:hdmi", Kind::External),
            ("wmi:internal", Kind::Internal),
            ("hdr:type_c", Kind::Hdr),
            ("ddc:type_c", Kind::Internal),
        ] {
            assert!(native.read(id, kind).unwrap_err().contains("允许"));
            assert!(native.write(id, kind, 0).unwrap_err().contains("允许"));
            let change = Change {
                device: device(id, kind),
                value: 0,
            };
            let results = native.apply(&[change], &|| true);
            assert_eq!(results.len(), 1);
            assert!(results[0].result.as_ref().unwrap_err().contains("允许"));
            assert!(native
                .prepare_restore(&[device(id, kind)], true)
                .unwrap_err()
                .contains("允许"));
        }
        assert!(native.cursor(true).unwrap_err().contains("光标"));
        // Authorized, but absent, resources fail without discovering anything.
        assert!(native
            .read("ddc:type_c", Kind::External)
            .unwrap_err()
            .contains("断开"));
        assert_eq!(native.stats().full_scans, 0);
    }

    #[test]
    fn restore_selection_never_broadens_restricted_access_or_kind() {
        let access =
            DeviceAccess::restricted(vec!["ddc:type_c".into(), "wmi:internal".into()]).unwrap();
        assert!(access.includes(Kind::External));
        assert!(access.includes(Kind::Internal));
        assert!(!access.includes(Kind::Hdr));
        let selected = access.intersect(&[
            device("ddc:TYPE_C", Kind::External),
            device("ddc:hdmi", Kind::External),
            device("hdr:type_c", Kind::Hdr),
        ]);
        assert!(selected.allows("ddc:type_c", Kind::External));
        assert!(!selected.allows("ddc:type_c2", Kind::External));
        assert!(!selected.allows("ddc:hdmi", Kind::External));
        assert!(!selected.allows("hdr:type_c", Kind::Hdr));
        assert!(!selected.includes(Kind::Internal));
    }

    #[test]
    fn measurement_snapshot_rejects_topology_resource_and_hdr_changes() {
        let target = Target {
            id: "type_c".into(),
            source: "display3".into(),
            name: "Type_C".into(),
            adapter: LUID {
                LowPart: 12,
                HighPart: 3,
            },
            target: 42,
            internal: false,
        };
        let another = Target {
            id: "hdmi".into(),
            source: "display2".into(),
            ..target.clone()
        };
        let original = topology_key(&[target.clone(), another.clone()]);
        let entries = [device("ddc:type_c", Kind::External)];
        let reordered = topology_key(&[another.clone(), target.clone()]);
        assert!(reusable_snapshot(
            Some(&original),
            &reordered,
            &entries,
            |_| true
        ));
        assert!(!reusable_snapshot(None, &original, &entries, |_| true));
        assert!(!reusable_snapshot(
            Some(&original),
            &original,
            &entries,
            |_| false
        ));
        assert!(!reusable_snapshot(
            Some(&original),
            &original,
            &[device("hdr:type_c", Kind::Hdr)],
            |_| true
        ));
        for change in 0..6 {
            let mut changed = target.clone();
            match change {
                0 => changed.id = "replacement".into(),
                1 => changed.source = "display4".into(),
                2 => changed.adapter.LowPart += 1,
                3 => changed.adapter.HighPart += 1,
                4 => changed.target += 1,
                _ => changed.internal = true,
            }
            let current = topology_key(&[another.clone(), changed]);
            assert!(
                !reusable_snapshot(Some(&original), &current, &entries, |_| true),
                "identity field {change} must invalidate reuse"
            );
        }
    }

    #[test]
    fn resource_failure_invalidates_measurement_snapshot() {
        let mut native = Native::restricted_to(vec!["ddc:type_c".into()]).unwrap();
        native.snapshot = Some(Vec::new());
        assert!(native.read("ddc:type_c", Kind::External).is_err());
        assert!(native.snapshot.is_none());
        native.snapshot = Some(Vec::new());
        assert!(native.write("ddc:type_c", Kind::External, 0).is_err());
        assert!(native.snapshot.is_none());
        native.snapshot = Some(Vec::new());
        let result = native.apply(
            &[Change {
                device: device("ddc:type_c", Kind::External),
                value: 0,
            }],
            &|| true,
        );
        assert!(result[0].result.is_err());
        assert!(native.snapshot.is_none());
    }

    #[test]
    fn ddc_batches_overlap_local_lane_and_join_even_on_device_failure() {
        let owner = thread::current().id();
        let batch_start = Instant::now();
        let (started_tx, started_rx) = mpsc::channel();
        let gate = (Mutex::new(false), Condvar::new());
        let finished = AtomicUsize::new(0);
        let make_change = |id: usize, kind| Change {
            device: Device {
                id: format!("fake-{id}"),
                name: format!("fake-{id}"),
                kind,
                min: 0,
                value: 75,
                max: 100,
            },
            value: 0,
        };
        let (external, local) = parallel_with_local(
            vec![0, 1],
            |id| {
                assert_ne!(thread::current().id(), owner);
                let mut value = 75;
                let result = confirm_change(
                    &mut value,
                    &make_change(id, Kind::External),
                    &|| true,
                    batch_start,
                    |value| Ok(*value),
                    |value, target| {
                        started_tx.send(id).unwrap();
                        let (released, _) = gate
                            .1
                            .wait_timeout_while(
                                gate.0.lock().unwrap(),
                                Duration::from_secs(3),
                                |released| !*released,
                            )
                            .unwrap();
                        assert!(*released, "local lane did not overlap both external jobs");
                        if id == 1 {
                            return Err("simulated disconnected device".into());
                        }
                        *value = target;
                        Ok(())
                    },
                );
                finished.fetch_add(1, Ordering::AcqRel);
                result
            },
            || {
                assert_eq!(thread::current().id(), owner);
                let mut value = 75;
                confirm_change(
                    &mut value,
                    &make_change(2, Kind::Internal),
                    &|| true,
                    batch_start,
                    |value| Ok(*value),
                    |value, target| {
                        let mut ids = [
                            started_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
                            started_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
                        ];
                        ids.sort();
                        assert_eq!(ids, [0, 1]);
                        *gate.0.lock().unwrap() = true;
                        gate.1.notify_all();
                        *value = target;
                        Ok(())
                    },
                )
            },
        );
        assert_eq!(finished.load(Ordering::Acquire), 2);
        assert!(matches!(local.result, Ok(true)));
        let mut completed = external.into_iter().map(Result::unwrap);
        let a = completed.next().unwrap();
        let b = completed.next().unwrap();
        assert!(matches!(a.result, Ok(true)));
        assert!(b.result.is_err());
        assert!(a.started_ms < b.started_ms + b.elapsed_ms);
        assert!(b.started_ms < a.started_ms + a.elapsed_ms);
        assert!(local.started_ms < a.started_ms + a.elapsed_ms);
        assert!(a.started_ms < local.started_ms + local.elapsed_ms);
    }
}
