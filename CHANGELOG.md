# Changelog

## 0.4.0 · 2026-11-04

- 逐屏保存熄屏范围，黑窗、背光和 HDR 一起限定；断连不回退至其它屏幕，恢复按原始记录进行。
- 全局／桌面两套独立键盘或鼠标热键与单双击开关，支持中键、侧键及修饰键。
- 桌面空白改用 MSAA 单点命中，容许侧键自然小位移；酷呆接管或未知桌面继续暂停。
- 黑屏状态可用同一桌面手势亮屏，只接受实际黑窗区域，拒绝未选屏和区域空隙。
- 内外屏背光开关合并为一行，部分屏幕熄屏时保留指针。
- 修复 HDR 重试依赖、资源重绑、WMI 投影空路径和可选返回参数引起的恢复问题；保留持久化基线、精确回读与写入 ACK。
- “关于”显示当前版本、作者 shiranzby 和可打开的仓库地址；统一 EXE 元数据。
- 添加与 Twinkle Tray 的完整比较和扩展路线图；规划能力与当前功能分开说明。

## 0.3.0

- Add write-ack presentation for normal Ready recovery.
- Restore the overlay and cursor as soon as all requested writes succeed while exact read-back continues.
- Keep HDR as a barrier and retain recovery records for failed or superseded devices.
- Merge startup settings into “launch silently at login”.
- Add light/dark/system themes, silent About window, hidden scrollbar and resize repaint fixes.

## 0.2.0

- Complete Rust/Win32 reconstruction of the screen-off utility.
- Parallelize independent DDC/WMI hardware lanes.
- Add durable per-device recovery records, generation cancellation and precise read-back.
- Add Fluent settings UI, scheduling and theme support.

## 0.1.0

- First Rust implementation based on the original screen-off workflow.
