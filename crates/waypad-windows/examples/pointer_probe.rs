//! Prints what Windows reports about the desktop and where the pointer
//! actually lands, so absolute-coordinate maths can be checked against reality.

#[cfg(windows)]
fn main() {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_MOVE,
        MOUSEEVENTF_VIRTUALDESK, MOUSEINPUT, SendInput,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetCursorPos, GetSystemMetrics, SM_CMONITORS, SM_CXSCREEN, SM_CXVIRTUALSCREEN, SM_CYSCREEN,
        SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    };

    unsafe {
        println!(
            "primary        : {}x{}",
            GetSystemMetrics(SM_CXSCREEN),
            GetSystemMetrics(SM_CYSCREEN)
        );
        println!("monitors       : {}", GetSystemMetrics(SM_CMONITORS));
        println!(
            "virtual desktop: origin=({},{}) size={}x{}",
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN)
        );

        {
            use windows::Win32::System::StationsAndDesktops::{
                GetProcessWindowStation, GetThreadDesktop, GetUserObjectInformationW, UOI_NAME,
            };
            use windows::Win32::System::Threading::GetCurrentThreadId;
            let mut buffer = [0u16; 256];
            let mut needed = 0u32;
            let station = GetProcessWindowStation().unwrap();
            let _ = GetUserObjectInformationW(
                windows::Win32::Foundation::HANDLE(station.0),
                UOI_NAME,
                Some(buffer.as_mut_ptr().cast()),
                (buffer.len() * 2) as u32,
                Some(&mut needed),
            );
            let station_name =
                String::from_utf16_lossy(&buffer[..(needed as usize / 2).saturating_sub(1)]);
            let mut dbuf = [0u16; 256];
            let mut dneeded = 0u32;
            let desktop = GetThreadDesktop(GetCurrentThreadId()).unwrap();
            let _ = GetUserObjectInformationW(
                windows::Win32::Foundation::HANDLE(desktop.0),
                UOI_NAME,
                Some(dbuf.as_mut_ptr().cast()),
                (dbuf.len() * 2) as u32,
                Some(&mut dneeded),
            );
            let desktop_name =
                String::from_utf16_lossy(&dbuf[..(dneeded as usize / 2).saturating_sub(1)]);
            println!("window station : {station_name}");
            println!("desktop        : {desktop_name}");
            // The thread desktop is not necessarily the one receiving input.
            // When the workstation is locked the input desktop is Winlogon,
            // and SendInput from Default is accepted and then discarded.
            use windows::Win32::System::StationsAndDesktops::{
                DESKTOP_ACCESS_FLAGS, OpenInputDesktop,
            };
            match OpenInputDesktop(Default::default(), false, DESKTOP_ACCESS_FLAGS(0x0001)) {
                Ok(input_desktop) => {
                    let mut ibuf = [0u16; 256];
                    let mut ineeded = 0u32;
                    let _ = GetUserObjectInformationW(
                        windows::Win32::Foundation::HANDLE(input_desktop.0),
                        UOI_NAME,
                        Some(ibuf.as_mut_ptr().cast()),
                        (ibuf.len() * 2) as u32,
                        Some(&mut ineeded),
                    );
                    println!(
                        "input desktop  : {}",
                        String::from_utf16_lossy(&ibuf[..(ineeded as usize / 2).saturating_sub(1)])
                    );
                }
                Err(err) => println!("input desktop  : cannot open ({err})"),
            }
        }

        let mut start = POINT::default();
        GetCursorPos(&mut start).unwrap();
        println!("cursor now     : ({},{})", start.x, start.y);

        for norm in [0i32, 16384, 32768, 49152, 65535] {
            let event = INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx: norm,
                        dy: norm,
                        mouseData: 0,
                        dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            };
            let sent = SendInput(&[event], std::mem::size_of::<INPUT>() as i32);
            std::thread::sleep(std::time::Duration::from_millis(80));
            let mut landed = POINT::default();
            GetCursorPos(&mut landed).unwrap();
            println!(
                "norm {norm:>5} -> sent={sent} landed=({},{})",
                landed.x, landed.y
            );
        }

        // Put it back where it was found, in the same absolute space the
        // probe just used rather than as a relative nudge.
        let span_x = (GetSystemMetrics(SM_CXVIRTUALSCREEN) - 1).max(1);
        let span_y = (GetSystemMetrics(SM_CYVIRTUALSCREEN) - 1).max(1);
        let restore = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: (start.x - GetSystemMetrics(SM_XVIRTUALSCREEN)) * 65535 / span_x,
                    dy: (start.y - GetSystemMetrics(SM_YVIRTUALSCREEN)) * 65535 / span_y,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        SendInput(&[restore], std::mem::size_of::<INPUT>() as i32);
        println!("cursor restored to ({},{})", start.x, start.y);
    }
}

#[cfg(not(windows))]
fn main() {}
