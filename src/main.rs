//! # touchpad-raw
//!
//! Reads raw multi-touch contact data directly from a Windows Precision
//! Touchpad (PTP) and draws the live finger positions as an ASCII grid in
//! the terminal.
//!
//! ## Why this needs to exist
//!
//! Windows deliberately does NOT hand a touchpad's raw finger positions to
//! every app. By default a touchpad only drives the mouse cursor and a
//! small set of OS-recognized gestures (two-finger scroll, pinch-zoom).
//! Apps that want the *raw* multi-finger data (drawing programs doing
//! two-finger rotate, for example) have to opt in explicitly using the
//! **Raw Input API**, then decode what comes back themselves using the
//! **HID (Human Interface Device) parsing API**. That's what this whole
//! file does, in two layers:
//!
//! 1. **Win32 plumbing** (`main`, `wndproc`, `enable_ansi`): the ceremony
//!    every Windows desktop app needs just to receive *any* input message.
//!    None of this is touchpad-specific -- it's the same shape for a
//!    keyboard, a mouse, a joystick, anything.
//! 2. **HID parsing** (`handle_raw_input`): the touchpad-specific part.
//!    A touchpad reports itself over HID (the same generic protocol mice,
//!    keyboards, and game controllers use), and its exact byte layout is
//!    NOT standardized across devices/manufacturers. So instead of reading
//!    fixed byte offsets, we ask the device itself, at runtime, "where do
//!    you put X? where's the tip-switch bit?" via its *report descriptor*.
//!
//! If you're new to this: skim part 1 once, then spend your time in part 2
//! (`handle_raw_input`) -- that's where the actual interesting logic is.

#![allow(non_snake_case)]

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::mem::size_of;

use windows::Win32::Devices::HumanInterfaceDevice::*;
use windows::Win32::Foundation::*;
use windows::Win32::System::Console::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

mod debug_render;

// ============================================================================
// HID usage-table constants
// ============================================================================
//
// These numbers are NOT Windows API constants -- they come from the USB-IF
// "HID Usage Tables" specification, a public document that defines a
// universal vocabulary for what a field in a HID report *means*.
//
// Every value in a HID report is tagged with two numbers:
//   - a "usage page" (a broad category, e.g. 0x01 = "Generic Desktop",
//     0x0D = "Digitizer")
//   - a "usage" (a specific meaning within that page, e.g. 0x30 = "X axis")
//
// This is how a generic OS can understand a touchpad, a joystick, and a
// barcode scanner through the same driver stack: the device's report
// descriptor declares "this byte range is usage page 0x01, usage 0x30 (X)"
// and the OS/HID parser looks it up rather than needing device-specific
// code. windows-rs doesn't ship these as constants (they're spec numbers,
// not Win32 APIs), so we hardcode the handful we need.
const HID_USAGE_PAGE_GENERIC: u16 = 0x01; // "Generic Desktop" page
const HID_USAGE_PAGE_DIGITIZER: u16 = 0x0D; // "Digitizer" page (touch/pen devices)

const HID_USAGE_GENERIC_X: u16 = 0x30; // X axis, lives on the Generic page
const HID_USAGE_GENERIC_Y: u16 = 0x31; // Y axis, lives on the Generic page

const HID_USAGE_DIGITIZER_TOUCH_PAD: u16 = 0x05; // "this whole device is a touchpad"
const HID_USAGE_DIGITIZER_TIP_SWITCH: u16 = 0x42; // "is this finger currently touching down?" (boolean)
const HID_USAGE_DIGITIZER_CONTACT_ID: u16 = 0x51; // device's own numbering for a finger, resets/reassigns over time

/// Everything we know about one finger, for one report (one instant in time).
///
/// `x`/`y` are in the touchpad's own "logical units" (not pixels, not
/// millimeters) -- whatever raw range the specific hardware reports.
/// `render_grid` rescales them using the device's own reported min/max.
#[derive(Default, Debug, Clone, Copy)]
struct Contact {
    x: u32,
    y: u32,
    /// Nonzero while the finger is actually touching the pad. A finger can
    /// exist in a report with tip == 0 (e.g. hovering, or the collection
    /// slot is simply unused this frame) -- always check this before
    /// trusting x/y.
    tip: u32,
    /// The touchpad's own contact numbering. NOT guaranteed stable or
    /// unique across time -- some devices reuse/reset it. We mainly use it
    /// as a display label, not as a reliable finger identity.
    id: u32,
}

// ============================================================================
// PART 1: Win32 plumbing
// ============================================================================
//
// Windows input delivery is fundamentally message-based: the OS posts
// events (key presses, mouse moves, and -- what we care about -- raw HID
// reports) to a *window*, and your program picks them up in a loop and
// hands each one to a callback function ("window procedure" / WndProc).
// This is true even for console apps with no visible UI: you still need a
// window to be the addressee of these messages.

fn main() -> Result<()> {
    unsafe {
        // Let \x1B[...] ANSI escape codes work in this console, so
        // render_grid's cursor-home/redraw trick actually does something.
        enable_ansi();

        // --- Step 1: register a "window class" -----------------------------
        // A window class is a template Windows uses to create windows from:
        // mainly, which function (WndProc) should handle its messages.
        // hInstance identifies "which running program owns this class" --
        // GetModuleHandleW(None) means "this .exe itself".
        let hmodule = GetModuleHandleW(None)?;
        let hinstance = HINSTANCE::from(hmodule);
        let class_name = w!("TouchpadRawDemoWindowClass"); // w!() makes a UTF-16 wide string literal, which Win32 requires

        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc), // <- our callback, called for every message this window receives
            hInstance: hmodule.into(),
            lpszClassName: class_name,
            ..Default::default() // zero out everything else (icon, cursor, background brush) -- we don't render anything, so we don't need them
        };
        RegisterClassW(&wc);

        // --- Step 2: actually create a window from that class --------------
        // HWND_MESSAGE is a special parent handle meaning "message-only
        // window": it never appears on screen, has no taskbar entry, and
        // can't be interacted with -- but it CAN receive messages, which is
        // literally all we need. This is the standard trick for a
        // background service/console app that still needs to opt into
        // Win32 input APIs.
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            w!("touchpad-raw"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,                  // position/size: irrelevant, it's never shown
            Some(HWND_MESSAGE), // parent = the message-only pseudo-parent
            None,               // no menu
            Some(hinstance),
            None, // no extra creation data
        )?;

        // --- Step 3: subscribe to raw touchpad input ------------------------
        // This is the one call that's actually ABOUT the touchpad. Without
        // this registration, Windows only tells us about mouse-cursor
        // movement (post-processed, single point) -- never the raw
        // multi-finger contact data. RAWINPUTDEVICE says, in effect:
        // "for HID usage page 0x0D usage 0x05 (i.e. any Precision
        // Touchpad), send raw reports to my window (hwndTarget), and keep
        // sending them even while my window isn't focused/foreground
        // (RIDEV_INPUTSINK)."
        let rid = RAWINPUTDEVICE {
            usUsagePage: HID_USAGE_PAGE_DIGITIZER,
            usUsage: HID_USAGE_DIGITIZER_TOUCH_PAD,
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: hwnd,
        };
        RegisterRawInputDevices(&[rid], size_of::<RAWINPUTDEVICE>() as u32)?;

        print!("\x1B[2J"); // clear the terminal once, before the first frame draws

        // --- Step 4: the message loop --------------------------------------
        // This is the heartbeat of every classic Win32 app. GetMessageW
        // blocks (sleeps) until a message shows up for one of this thread's
        // windows, fills `msg` with it, and returns true -- or returns
        // false only on WM_QUIT, which is our cue to exit the loop.
        // DispatchMessageW is what actually calls `wndproc` with the
        // message. TranslateMessage handles keyboard-specific translation
        // (WM_KEYDOWN -> WM_CHAR); it's a no-op for our WM_INPUT messages
        // but is standard boilerplate to include.
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

/// Turn on ANSI escape-code interpretation for this console.
///
/// Modern terminals (Windows Terminal) usually default to this already, but
/// the classic `cmd.exe`/`conhost.exe` console historically didn't -- so
/// without this, our `\x1B[2J` (clear screen) and `\x1B[H` (cursor home)
/// sequences in `render_grid` would just print as literal garbage
/// characters instead of doing anything. This flips the one console mode
/// bit that enables VT100-style escape sequences.
fn enable_ansi() {
    if let Ok(handle) = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) } {
        let mut mode = CONSOLE_MODE(0);
        if unsafe { GetConsoleMode(handle, &mut mode).is_ok() } {
            let _ = unsafe { SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) };
        }
    }
}

/// The window procedure: Windows calls this for every message posted to our
/// window. We only care about two:
///   - WM_INPUT: a raw input event arrived (this is our touchpad data)
///   - WM_DESTROY: the window is closing, so tell the message loop to stop
///     (PostQuitMessage causes the next GetMessageW to return false)
/// Everything else we don't handle ourselves, so we hand it to
/// DefWindowProcW, Windows' default handling -- required for a well-behaved
/// window even one this minimal.
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_INPUT {
        if let Err(e) = unsafe { handle_raw_input(lparam) } {
            eprintln!("[wndproc] raw input error: {e:?}");
        }
        return LRESULT(0);
    }
    if msg == WM_DESTROY {
        unsafe { PostQuitMessage(0) };
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

// ============================================================================
// PART 2: HID parsing -- this is the actual touchpad-specific logic
// ============================================================================

/// Decode one WM_INPUT message into a set of touch contacts and draw them.
///
/// The overall shape of this function is: "unwrap layers of Windows
/// generality until we reach touchpad-specific meaning."
///   RAW_INPUT (could be mouse/keyboard/HID)
///     -> HID report                (could be ANY HID device: could be a game controller)
///       -> a specific device's report descriptor  (tells us THIS device's byte layout)
///         -> per-field values via HidP_*           (finally: real X/Y/tip/id numbers)
unsafe fn handle_raw_input(lparam: LPARAM) -> Result<()> {
    // lparam is documented by Windows to actually be an HRAWINPUT handle,
    // just passed through the generic LPARAM slot. We reinterpret it as one.
    let hrawinput = HRAWINPUT(lparam.0 as *mut c_void);

    // --- Fetch the raw event payload -----------------------------------
    // GetRawInputData uses the extremely common Win32 "two-call idiom":
    // call once with a null buffer to ask "how many bytes do you need?",
    // allocate that much, then call again to actually fill it. This avoids
    // the API having to guess a buffer size up front.
    let mut size: u32 = 0;
    unsafe {
        GetRawInputData(
            hrawinput,
            RID_INPUT,
            None, // null buffer = "just tell me the size"
            &mut size,
            size_of::<RAWINPUTHEADER>() as u32,
        )
    };
    if size == 0 {
        return Ok(());
    }

    let mut buffer = vec![0u8; size as usize];
    let copied = unsafe {
        GetRawInputData(
            hrawinput,
            RID_INPUT,
            Some(buffer.as_mut_ptr() as *mut c_void), // now give it somewhere to write
            &mut size,
            size_of::<RAWINPUTHEADER>() as u32,
        )
    };
    if copied == u32::MAX {
        return Ok(()); // call failed; bail quietly rather than crash on bad data
    }

    // `buffer` now holds a RAWINPUT struct: a small header (what kind of
    // device? mouse/keyboard/HID?) followed by a union of possible payload
    // shapes. We reinterpret the raw bytes as that struct.
    let raw = unsafe { &*(buffer.as_ptr() as *const RAWINPUT) };
    if raw.header.dwType != RIM_TYPEHID.0 {
        // Could theoretically be RIM_TYPEMOUSE/RIM_TYPEKEYBOARD if some
        // other raw input registration exists; we only registered for HID
        // (the touchpad), so this should always be true in practice.
        return Ok(());
    }

    // --- The HID payload itself -----------------------------------------
    // `hid.bRawData` is the actual bytes the touchpad's firmware sent,
    // completely opaque to us right now -- just a blob. dwCount is how many
    // separate reports are batched into this one message (usually 1);
    // dwSizeHid is the byte length of ONE report.
    let hid = unsafe { &raw.data.hid };
    let report_count = hid.dwCount;
    let report_size = hid.dwSizeHid;
    if report_count == 0 || report_size == 0 {
        return Ok(());
    }
    // bRawData is declared in the Windows headers as a 1-element array
    // because C (and this binding) has no true "flexible array member".
    // Its address is still the correct start of the real, larger buffer --
    // we already know the true total size from dwSizeHid * dwCount, so we
    // just do our own pointer arithmetic below instead of indexing past
    // the nominal length (which Rust would otherwise refuse).
    let raw_data_ptr = hid.bRawData.as_ptr();

    // --- Ask the device how to interpret its own bytes -------------------
    // This is the crux of HID: every device manufacturer ships a "report
    // descriptor" describing their own byte layout (which bits are X,
    // which are Y, how many fingers, etc). Windows parses that descriptor
    // once and hands us an opaque "preparsed data" blob; we then query it
    // via the HidP_* functions instead of ever touching raw byte offsets
    // ourselves. Same two-call size-then-fill idiom as before.
    let hdevice = raw.header.hDevice;
    let mut pp_size: u32 = 0;
    unsafe { GetRawInputDeviceInfoW(Some(hdevice), RIDI_PREPARSEDDATA, None, &mut pp_size) };
    if pp_size == 0 {
        return Ok(());
    }
    let mut pp_data = vec![0u8; pp_size as usize];
    unsafe {
        GetRawInputDeviceInfoW(
            Some(hdevice),
            RIDI_PREPARSEDDATA,
            Some(pp_data.as_mut_ptr() as *mut c_void),
            &mut pp_size,
        )
    };
    let preparsed = PHIDP_PREPARSED_DATA(pp_data.as_mut_ptr() as isize);

    // HIDP_CAPS ("capabilities") is a summary of the descriptor: how many
    // value fields exist, how many button fields exist, etc. We mainly
    // want NumberInputValueCaps so we know how big an array to allocate
    // for the next step.
    let mut caps = HIDP_CAPS::default();
    unsafe { HidP_GetCaps(preparsed, &mut caps) };

    // --- Enumerate every "value" field the descriptor defines ------------
    // HID splits fields into two kinds:
    //   - VALUE fields: multi-bit numbers (X, Y, ContactID, ScanTime, ...)
    //   - BUTTON fields: single-bit booleans (TipSwitch, Confidence, ...)
    // HidP_GetValueCaps only returns the VALUE ones. (We handle the button
    // kind separately, further down, for TipSwitch.)
    //
    // Each HIDP_VALUE_CAPS entry tells us, for ONE field: which usage page
    // /usage it is, which "link collection" it belongs to (see below), and
    // its logical (raw) min/max range.
    let mut value_caps: Vec<HIDP_VALUE_CAPS> =
        vec![HIDP_VALUE_CAPS::default(); caps.NumberInputValueCaps as usize];
    let mut value_caps_len = caps.NumberInputValueCaps;
    unsafe {
        HidP_GetValueCaps(
            HidP_Input, // we only care about INPUT reports (device -> PC); HID also has Output/Feature reports we don't use
            value_caps.as_mut_ptr(),
            &mut value_caps_len,
            preparsed,
        )
    };

    // Note on "link collection": a HID report descriptor can group related
    // fields into nested collections. A Precision Touchpad's descriptor
    // typically has one collection per possible finger, so "link
    // collection 3" and "usage X within link collection 3" together mean
    // "the X position of the 3rd possible finger slot." Collection 0 is
    // special -- it's the outermost/root collection wrapping everything
    // else, not a finger of its own (this trips us up further down, and
    // we explicitly filter it out).

    // --- Find X/Y's real reported range for THIS device -------------------
    // vc.LogicalMin/LogicalMax tell us the raw numeric range this specific
    // hardware actually uses for X and Y (touchpads vary in resolution/
    // reporting units) -- we need this later to scale raw coordinates onto
    // our fixed-size ASCII grid correctly.
    let mut x_min = 0i32;
    let mut x_max = 1i32;
    let mut y_min = 0i32;
    let mut y_max = 1i32;
    for vc in &value_caps {
        // Each value cap describes either a single usage (the common case)
        // or a *range* of usages sharing one definition. IsRange tells us
        // which union field is valid to read -- reading the wrong one is
        // undefined data, so we always branch on it first.
        let is_range = vc.IsRange.into();
        let usage = if is_range {
            unsafe { vc.Anonymous.Range.UsageMin }
        } else {
            unsafe { vc.Anonymous.NotRange.Usage }
        };
        if vc.UsagePage == HID_USAGE_PAGE_GENERIC && usage == HID_USAGE_GENERIC_X {
            x_min = vc.LogicalMin;
            x_max = vc.LogicalMax;
        }
        if vc.UsagePage == HID_USAGE_PAGE_GENERIC && usage == HID_USAGE_GENERIC_Y {
            y_min = vc.LogicalMin;
            y_max = vc.LogicalMax;
        }
    }

    // --- Decode each individual report in this batch ----------------------
    // Almost always report_count == 1; the loop exists because Windows CAN
    // batch multiple reports into a single WM_INPUT if several arrived
    // faster than the app processed them.
    for i in 0..report_count {
        // Slice out just this one report's bytes from the raw buffer.
        let report_ptr = unsafe { raw_data_ptr.add((i * report_size) as usize) };
        let mut report =
            unsafe { std::slice::from_raw_parts(report_ptr, report_size as usize).to_vec() };

        let mut contacts: BTreeMap<u16, Contact> = BTreeMap::new();

        // For every VALUE field the descriptor defines, ask "what's this
        // field's actual value in THIS specific report's bytes?" via
        // HidP_GetUsageValue. If the field genuinely isn't present in this
        // report (some devices omit unused finger slots entirely), the
        // call fails and we just skip it -- that's expected, not an error.
        for vc in &value_caps {
            let usage_page = vc.UsagePage;
            let is_range = vc.IsRange.into();
            let usage = if is_range {
                unsafe { vc.Anonymous.Range.UsageMin }
            } else {
                unsafe { vc.Anonymous.NotRange.Usage }
            };
            let link_collection = vc.LinkCollection; // which "finger slot" this field belongs to

            let mut value: u32 = 0;
            let status = unsafe {
                HidP_GetUsageValue(
                    HidP_Input,
                    usage_page,
                    Some(link_collection), // restrict the lookup to just this collection/finger
                    usage,
                    &mut value,
                    preparsed,
                    &mut report, // the actual bytes to decode
                )
            };
            if status.is_err() {
                continue;
            }

            // `.entry(...).or_default()` means: "get the Contact for this
            // finger slot, creating a fresh zeroed one the first time we
            // see this link_collection in this report."
            let entry = contacts.entry(link_collection).or_default();
            match (usage_page, usage) {
                (HID_USAGE_PAGE_GENERIC, HID_USAGE_GENERIC_X) => entry.x = value,
                (HID_USAGE_PAGE_GENERIC, HID_USAGE_GENERIC_Y) => entry.y = value,
                (HID_USAGE_PAGE_DIGITIZER, HID_USAGE_DIGITIZER_TIP_SWITCH) => entry.tip = value,
                (HID_USAGE_PAGE_DIGITIZER, HID_USAGE_DIGITIZER_CONTACT_ID) => entry.id = value,
                _ => {} // some other field we don't care about (ContactCount, ScanTime, ...)
            }
        }

        // link_collection 0 is the outermost/root collection (see the note
        // above), not a real finger -- it has no X/Y of its own. We drop it
        // here so it never gets treated as a phantom 6th finger.
        contacts.remove(&0);

        // --- TipSwitch: the one field that needs a different API call -----
        // TipSwitch ("is this finger actually pressing down?") is a BUTTON
        // usage (1 bit: on/off), not a VALUE usage, so it never appeared in
        // the value_caps loop above -- HidP_GetUsageValue simply can't see
        // it. To read boolean/button usages we instead call HidP_GetUsages,
        // which -- given a link collection -- returns the list of every
        // boolean usage that's currently "on" (1) within that collection.
        // If TipSwitch's usage code is in that list, the finger is down.
        //
        // Subtlety: HidP_GetUsages for a given link collection returns
        // usages set anywhere in that collection OR any collection nested
        // inside it. Since we already removed link 0 (the root collection
        // that contains every finger nested inside it), we don't
        // accidentally aggregate every finger's tip state into one place.
        let links: Vec<u16> = contacts.keys().copied().collect();
        for link_collection in links {
            let mut usage_list = [0u16; 8]; // generous fixed buffer; a touchpad reports far fewer than 8 button usages per finger
            let mut usage_len: u32 = usage_list.len() as u32; // input: capacity; output: actual count filled
            let status = unsafe {
                HidP_GetUsages(
                    HidP_Input,
                    HID_USAGE_PAGE_DIGITIZER,
                    Some(link_collection),
                    usage_list.as_mut_ptr(),
                    &mut usage_len,
                    preparsed,
                    &mut report,
                )
            };
            if status.is_ok() {
                let tip_on =
                    usage_list[..usage_len as usize].contains(&HID_USAGE_DIGITIZER_TIP_SWITCH);
                if let Some(entry) = contacts.get_mut(&link_collection) {
                    entry.tip = tip_on as u32;
                }
            }
        }

        debug_render::render_grid(&contacts, x_min, x_max, y_min, y_max);
    }

    Ok(())
}
