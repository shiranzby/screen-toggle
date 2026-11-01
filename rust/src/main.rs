#![windows_subsystem = "windows"]
mod about;
mod antialias;
mod app;
#[cfg(feature = "direct2d")]
mod d2d;
mod panel;
mod theme;
mod win;

fn main() {
    if let Err(error) = app::run() {
        if std::env::args().any(|arg| arg == "--ui-test")
            && std::env::var_os("SOG_TEST_WORKAREA").is_some()
        {
            // A failed background test must not open a modal on the work screen.
            eprintln!("{error}");
            std::process::exit(1);
        }
        unsafe {
            windows::Win32::UI::WindowsAndMessaging::MessageBoxW(
                None,
                windows::core::PCWSTR(win::wide(&error).as_ptr()),
                windows::core::w!("一键熄屏"),
                windows::Win32::UI::WindowsAndMessaging::MB_OK
                    | windows::Win32::UI::WindowsAndMessaging::MB_ICONERROR,
            );
        }
        std::process::exit(1);
    }
}
