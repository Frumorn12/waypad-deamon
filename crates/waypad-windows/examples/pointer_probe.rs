//! Maps normalised SendInput coordinates onto where the pointer actually lands.
//!
//! Absolute pointing is the one part of the input backend whose correctness
//! cannot be established from the API docs alone: DPI awareness and the
//! virtual-desktop flag interact, and a multi-monitor host with mixed scaling
//! is where that shows up. Run it on such a host to see the real mapping.

#[cfg(windows)]
fn main() {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::HiDpi::GetProcessDpiAwareness;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_MOUSE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_MOVE,
        MOUSEEVENTF_VIRTUALDESK, MOUSEINPUT, SendInput,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetCursorPos, GetSystemMetrics, SM_CMONITORS, SM_CXSCREEN, SM_CXVIRTUALSCREEN, SM_CYSCREEN,
        SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    };

    unsafe {
        let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        println!(
            "primary {}x{}  monitors {}  virtual origin=({vx},{vy}) size={vw}x{vh}",
            GetSystemMetrics(SM_CXSCREEN),
            GetSystemMetrics(SM_CYSCREEN),
            GetSystemMetrics(SM_CMONITORS)
        );
        match GetProcessDpiAwareness(None) {
            Ok(awareness) => println!(
                "process dpi awareness: {} (0=unaware, 1=system, 2=per-monitor)",
                awareness.0
            ),
            Err(err) => println!("process dpi awareness: unknown ({err})"),
        }

        let mut start = POINT::default();
        GetCursorPos(&mut start).unwrap();
        println!("cursor starts at ({},{})", start.x, start.y);

        let move_to = |nx: i32, ny: i32, flags: MOUSE_EVENT_FLAGS| {
            let event = INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx: nx,
                        dy: ny,
                        mouseData: 0,
                        dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | flags,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            };
            SendInput(&[event], std::mem::size_of::<INPUT>() as i32);
            std::thread::sleep(std::time::Duration::from_millis(50));
            let mut landed = POINT::default();
            GetCursorPos(&mut landed).unwrap();
            (landed.x, landed.y)
        };

        for (label, flags) in [
            ("with VIRTUALDESK", MOUSEEVENTF_VIRTUALDESK),
            ("without VIRTUALDESK", MOUSE_EVENT_FLAGS(0)),
        ] {
            println!("--- {label} (flag bits {:#06x}) ---", flags.0);
            for norm in [0i32, 16384, 32768, 49152, 65535] {
                let (x, y) = move_to(norm, norm, flags);
                println!("  norm {norm:>5} -> ({x},{y})");
            }
        }

        // Put it back where it was found.
        let span_x = (vw - 1).max(1);
        let span_y = (vh - 1).max(1);
        move_to(
            (start.x - vx) * 65535 / span_x,
            (start.y - vy) * 65535 / span_y,
            MOUSEEVENTF_VIRTUALDESK,
        );
        println!("cursor restored towards ({},{})", start.x, start.y);
    }
}

#[cfg(not(windows))]
fn main() {}
