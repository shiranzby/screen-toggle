// 构建期把 icon.ico 嵌入 exe 资源(桌面 exe 显示图标 + 窗口类默认图标来源)
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_manifest(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
 <assemblyIdentity version="0.2.0.0" processorArchitecture="*" name="ScreenOffGuard.Rust" type="win32"/>
 <dependency><dependentAssembly><assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0" processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/></dependentAssembly></dependency>
 <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3"><security><requestedPrivileges><requestedExecutionLevel level="asInvoker" uiAccess="false"/></requestedPrivileges></security></trustInfo>
 <application xmlns="urn:schemas-microsoft-com:asm.v3"><windowsSettings><dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness></windowsSettings></application>
 <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1"><application><supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/></application></compatibility>
</assembly>"#);
        // 优先项目内 icon.ico, 缺失时静默跳过(不阻断构建)
        let ico = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("icon.ico");
        if ico.exists() {
            res.set_icon(ico.to_str().unwrap());
        }
        res.compile().expect("winresource compile failed");
    }
}
