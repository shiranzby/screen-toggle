# Changelog

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
