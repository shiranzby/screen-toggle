// 构建期把 icon.ico 嵌入 exe 资源(桌面 exe 显示图标 + 窗口类默认图标来源)
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        // 优先项目内 icon.ico, 缺失时静默跳过(不阻断构建)
        let ico = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("icon.ico");
        if ico.exists() {
            res.set_icon(ico.to_str().unwrap());
        }
        res.compile().expect("winresource compile failed");
    }
}
