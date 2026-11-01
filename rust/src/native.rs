//! Windows resources stay on their owning worker thread. No raw handle enters a journal.
use crate::engine::{
    confirm_change_after_snapshot, Backend, Change, ChangeResult, Device, Inventory, Kind,
};
use std::{collections::BTreeMap, mem::size_of, thread, time::Instant};
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

struct Physical(HANDLE);
impl Drop for Physical {
    fn drop(&mut self) {
        unsafe {
            let _ = display::DestroyPhysicalMonitor(self.0);
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
    _apartment: Apartment,
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
    fn values(&self) -> Result<Vec<(String, u32)>, String> {
        self.query("SELECT * FROM WmiMonitorBrightness WHERE Active = TRUE")?
            .iter()
            .map(|o| {
                let value = u32::try_from(&Self::property(o, "CurrentBrightness")?)
                    .map_err(|e| e.to_string())?;
                if value > 100 {
                    return Err("WMI 亮度不在 0–100 范围".into());
                }
                Ok((Self::text(o, "InstanceName")?, value))
            })
            .collect()
    }
    fn set(
        &self,
        instance: &str,
        value: u32,
        current: &(dyn Fn() -> bool + Sync),
    ) -> Result<(), String> {
        unsafe {
            if value > 100 {
                return Err("WMI 目标亮度无效".into());
            }
            let mut path = None;
            for object in self.query("SELECT * FROM WmiMonitorBrightnessMethods")? {
                if Self::text(&object, "InstanceName")? == instance {
                    path = Some(Self::text(&object, "__PATH")?);
                    break;
                }
            }
            let path = path.ok_or("未找到原内置屏实例")?;
            let mut class = None;
            self.svc
                .GetObject(
                    &BSTR::from("WmiMonitorBrightnessMethods"),
                    wmi::WBEM_GENERIC_FLAG_TYPE(0),
                    None,
                    Some(&mut class),
                    None,
                )
                .map_err(|e| e.to_string())?;
            let mut input = None;
            let mut output = None;
            class
                .ok_or("未找到 WMI 方法类")?
                .GetMethod(w!("WmiSetBrightness"), 0, &mut input, &mut output)
                .map_err(|e| e.to_string())?;
            let params = input
                .ok_or("WMI 方法没有输入参数")?
                .SpawnInstance(0)
                .map_err(|e| e.to_string())?;
            params
                // WMI represents CIM uint32 with VT_I4 (not VT_UI4).
                .Put(w!("Timeout"), 0, &VARIANT::from(0i32), 0)
                .map_err(|e| format!("WMI Timeout 参数：{e}"))?;
            params
                .Put(w!("Brightness"), 0, &VARIANT::from(value as u8), 0)
                .map_err(|e| format!("WMI Brightness 参数：{e}"))?;
            let mut returned = None;
            // Method discovery above can block after a newer generation arrives.
            if !current() {
                return Ok(());
            }
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
            // Some monitor providers return S_OK without an output object (also
            // reproduced with Invoke-CimMethod). Engine always verifies actual brightness.
            if let Some(output) = returned {
                let code = u32::try_from(&Self::property(&output, "ReturnValue")?)
                    .map_err(|e| e.to_string())?;
                if code != 0 {
                    return Err(format!("WmiSetBrightness 返回错误 {code}"));
                }
            }
            Ok(())
        }
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

pub struct Native {
    physical: BTreeMap<String, Physical>,
    ranges: BTreeMap<String, (u32, u32)>,
    targets: BTreeMap<String, Target>,
    wmi: Option<Wmi>,
    wmi_error: Option<String>,
    wmi_attempted: bool,
    pub test_mode: bool,
}
impl Native {
    pub fn new(test_mode: bool) -> Self {
        Self {
            physical: BTreeMap::new(),
            ranges: BTreeMap::new(),
            targets: BTreeMap::new(),
            wmi: None,
            wmi_error: None,
            wmi_attempted: false,
            test_mode,
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
    fn write_current(
        &mut self,
        id: &str,
        kind: Kind,
        value: u32,
        current: &(dyn Fn() -> bool + Sync),
    ) -> Result<(), String> {
        if self.test_mode {
            return Err("测试模式禁止硬件写入".into());
        }
        if !current() {
            return Ok(());
        }
        match kind {
            Kind::External => {
                let physical = self
                    .physical
                    .get(id)
                    .ok_or("原外接屏已断开或无法唯一识别")?;
                let &(min, max) = self.ranges.get(id).ok_or("外接屏范围未确认")?;
                ddc_write(physical.0, value, min, max)
            }
            Kind::Internal => self.wmi.as_ref().ok_or("WMI 不可用")?.set(
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
        }
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
impl Backend for Native {
    fn scan(&mut self) -> Result<Inventory, String> {
        self.physical.clear();
        self.ranges.clear();
        self.targets.clear();
        let mut inventory = Inventory::default();
        if self.test_mode {
            inventory
                .warnings
                .push("测试模式：未修改亮度、HDR 或系统光标".into());
            return Ok(inventory);
        }
        let topology = match targets() {
            Ok(t) => t,
            Err(e) => {
                inventory.warnings.push(e);
                vec![]
            }
        };
        for target in &topology {
            match hdr_value(target) {
                Ok(Some(value)) => {
                    let id = format!("hdr:{}", target.id);
                    inventory.devices.push(Device {
                        id: id.clone(),
                        name: format!("{} HDR", target.name),
                        kind: Kind::Hdr,
                        min: 0,
                        value,
                        max: 1,
                    });
                    self.targets.insert(id, target.clone());
                }
                Ok(None) => {}
                Err(e) => inventory.warnings.push(format!("{}：{e}", target.name)),
            }
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
                    inventory.warnings.push(os_error("逻辑屏读取失败"));
                    continue;
                }
                let source = from_wide(&info.szDevice);
                let matching: Vec<_> = topology.iter().filter(|t| t.source == source).collect();
                let mut count = 0;
                if display::GetNumberOfPhysicalMonitorsFromHMONITOR(monitor, &mut count).is_err()
                    || count == 0
                {
                    continue;
                }
                if count > 64 {
                    inventory.warnings.push("物理屏数量异常，已跳过".into());
                    continue;
                }
                let mut monitors = vec![display::PHYSICAL_MONITOR::default(); count as usize];
                if display::GetPhysicalMonitorsFromHMONITOR(monitor, &mut monitors).is_err() {
                    inventory.warnings.push(os_error("物理屏枚举失败"));
                    continue;
                }
                // Own every returned handle immediately, including unsupported and ambiguous monitors.
                let owned: Vec<_> = monitors
                    .iter()
                    .map(|m| Physical(m.hPhysicalMonitor))
                    .collect();
                if count != 1 || matching.len() != 1 {
                    inventory
                        .warnings
                        .push(format!("{source}：无法唯一识别物理屏，跳过 DDC"));
                    continue;
                }
                if matching[0].internal {
                    continue;
                }
                let id = format!("ddc:{}", matching[0].id);
                let mut iter = owned.into_iter();
                let physical = iter.next().ok_or("缺少物理屏句柄")?;
                candidates.insert(id, (matching[0].name.clone(), physical));
            }
        }
        // Only integer handle borrows cross into DDC threads. The owning
        // Physical values stay in candidates and cannot drop until every join.
        let jobs = candidates
            .values()
            .map(|(_, physical)| physical.0 .0 as usize)
            .collect();
        let (ddc, internal) = parallel_with_local(
            jobs,
            |raw| Self::ddc_read(HANDLE(raw as *mut _)),
            || {
                // Connect/use/drop COM only on this original hardware worker.
                // Lazy connection now overlaps external baseline reads.
                if !self.wmi_attempted {
                    self.wmi_attempted = true;
                    match Wmi::connect() {
                        Ok(wmi) => self.wmi = Some(wmi),
                        Err(error) => self.wmi_error = Some(error),
                    }
                }
                match &self.wmi {
                    Some(wmi) => wmi.values(),
                    None => Err(format!(
                        "WMI 连接不可用：{}",
                        self.wmi_error.as_deref().unwrap_or("未知错误")
                    )),
                }
            },
        );
        for ((id, (name, physical)), result) in candidates.into_iter().zip(ddc) {
            match result.unwrap_or_else(|_| Err("DDC 扫描线程异常结束".into())) {
                Ok((min, value, max)) => {
                    inventory.devices.push(Device {
                        id: id.clone(),
                        name,
                        kind: Kind::External,
                        min,
                        value,
                        max,
                    });
                    self.ranges.insert(id.clone(), (min, max));
                    self.physical.insert(id, physical);
                }
                Err(e) => inventory.warnings.push(format!("{name}：{e}")),
            }
        }
        match internal {
            Ok(values) => {
                for (id, value) in values {
                    inventory.devices.push(Device {
                        id: format!("wmi:{id}"),
                        name: "内置屏".into(),
                        kind: Kind::Internal,
                        min: 0,
                        value,
                        max: 100,
                    });
                }
            }
            Err(e) => inventory.warnings.push(format!("内置屏调光不可用：{e}")),
        }
        Ok(inventory)
    }
    fn apply(
        &mut self,
        changes: &[Change],
        current: &(dyn Fn() -> bool + Sync),
    ) -> Vec<ChangeResult> {
        let batch_start = Instant::now();
        let mut grouped: BTreeMap<&str, Vec<&Change>> = BTreeMap::new();
        let mut results = Vec::new();
        for change in changes.iter().filter(|c| c.device.kind == Kind::External) {
            grouped.entry(&change.device.id).or_default().push(change);
        }
        let mut jobs = Vec::new();
        for (id, group) in grouped {
            match (self.physical.get(id), self.ranges.get(id)) {
                (Some(physical), Some(&(min, max))) if !self.test_mode => {
                    // A group keeps duplicate commands for one physical screen
                    // serial while independent screens can overlap.
                    jobs.push((physical.0 .0 as usize, min, max, group));
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
        let (external, local) = parallel_with_local(
            jobs,
            |(raw, min, max, group)| {
                let mut handle = HANDLE(raw as *mut _);
                group
                    .into_iter()
                    .map(|change| {
                        confirm_change_after_snapshot(
                            &mut handle,
                            change,
                            current,
                            batch_start,
                            |handle| Self::ddc_read(*handle).map(|(_, value, _)| value),
                            |handle, value| ddc_write(*handle, value, min, max),
                        )
                    })
                    .collect::<Vec<_>>()
            },
            || {
                changes
                    .iter()
                    .filter(|change| change.device.kind != Kind::External)
                    .map(|change| {
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
        results
    }
    fn write(&mut self, id: &str, kind: Kind, value: u32) -> Result<(), String> {
        self.write_current(id, kind, value, &|| true)
    }
    fn read(&mut self, id: &str, kind: Kind) -> Result<u32, String> {
        match kind {
            Kind::External => {
                Self::ddc_read(self.physical.get(id).ok_or("原外接屏已断开")?.0).map(|(_, v, _)| v)
            }
            Kind::Internal => self
                .wmi
                .as_ref()
                .ok_or("WMI 不可用")?
                .values()?
                .into_iter()
                .find(|(key, _)| Some(key.as_str()) == id.strip_prefix("wmi:"))
                .map(|(_, v)| v)
                .ok_or("原内置屏已断开".into()),
            Kind::Hdr => hdr_value(self.targets.get(id).ok_or("原 HDR 屏已断开")?)?
                .ok_or("HDR 已不可用".into()),
        }
    }
    fn cursor(&mut self, hidden: bool) -> Result<(), String> {
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
