//! Фоновый помощник `Files.exe --agent` (запускается при входе, если включено «Открывать папки в Files»).
//!
//! 1. Win+E — перехват клавиш (WH_KEYBOARD_LL): открывает Files вместо Проводника.
//! 2. Окна Проводника, открытые другими программами («Показать в папке» в браузере,
//!    `explorer.exe <папка>`), — пока окно невидимо, узнаём папку и выделенный файл,
//!    закрываем его и открываем то же место в Files. Системные места (Этот компьютер,
//!    Корзина, Панель управления…) остаются в Проводнике.
//!
//! Без опроса: события окон приходят от системы (SetWinEventHook), в простое 0% процессора.

use std::cell::RefCell;
use std::time::{Duration, Instant};
use windows::core::{w, Interface, BSTR, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

pub const CLASS: PCWSTR = w!("MonoFilesAgent");
/// Files сам открывает Проводник («Открыть в Проводнике») — пропустить окна ближайших секунд.
pub const WM_ALLOW: u32 = WM_APP + 2;
const WM_WINE: u32 = WM_APP + 3;
const WM_NEWWIN: u32 = WM_APP + 4;
const TIMER: usize = 7;

struct Pending {
    hwnd: HWND,
    since: Instant,
    /// когда впервые узнали папку — выделение ставится чуть позже, ждём его
    seen: Option<(Instant, String)>,
}

thread_local! {
    static PENDING: RefCell<Vec<Pending>> = const { RefCell::new(vec![]) };
    static ALLOW_UNTIL: RefCell<Option<Instant>> = const { RefCell::new(None) };
    static WIN_DOWN: RefCell<bool> = const { RefCell::new(false) };
    static SWALLOW_E: RefCell<bool> = const { RefCell::new(false) };
}
static mut AGENT: HWND = HWND(std::ptr::null_mut());

fn agent_hwnd() -> HWND {
    unsafe { AGENT }
}

/// Найти окно запущенного помощника (оно message-only, FindWindow его не видит).
pub fn find() -> Option<HWND> {
    unsafe { FindWindowExW(Some(HWND_MESSAGE), None, CLASS, PCWSTR::null()).ok().filter(|h| !h.is_invalid()) }
}

pub fn allow_explorer() {
    if let Some(h) = find() {
        unsafe {
            let _ = SendMessageW(h, WM_ALLOW, None, None);
        }
    }
}

pub fn stop() {
    if let Some(h) = find() {
        unsafe {
            let _ = PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
}

pub fn spawn() {
    if find().is_none() {
        if let Ok(exe) = std::env::current_exe() {
            let _ = std::process::Command::new(exe).arg("--agent").spawn();
        }
    }
}

fn open_in_files(arg: Option<&str>) {
    let Ok(exe) = std::env::current_exe() else { return };
    let mut c = std::process::Command::new(exe);
    if let Some(a) = arg {
        c.arg(a);
    }
    let _ = c.spawn();
}

pub fn run() {
    // переменные тест-режима не должны доставаться окнам, которые откроет помощник
    for (k, _) in std::env::vars() {
        if k.starts_with("FILES_") {
            unsafe { std::env::remove_var(&k) };
        }
    }
    unsafe {
        // один помощник на сеанс
        let _m = CreateMutexW(None, true, w!("Local\\MonoFilesAgent"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let hinst = GetModuleHandleW(None).map(|h| HINSTANCE(h.0)).unwrap_or_default();
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: hinst, lpszClassName: CLASS, ..Default::default() };
        RegisterClassW(&wc);
        let Ok(hw) = CreateWindowExW(WINDOW_EX_STYLE(0), CLASS, w!(""), WINDOW_STYLE(0), 0, 0, 0, 0, Some(HWND_MESSAGE), None, Some(hinst), None) else {
            return;
        };
        AGENT = hw;
        let _kb = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard), Some(hinst), 0);
        let _ev = SetWinEventHook(EVENT_OBJECT_SHOW, EVENT_OBJECT_SHOW, None, Some(on_event), 0, 0, WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

// ---------------------------------------------------------------- Win+E

fn key_down(vk: VIRTUAL_KEY) -> bool {
    unsafe { GetAsyncKeyState(vk.0 as i32) as u16 & 0x8000 != 0 }
}

unsafe extern "system" fn keyboard(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if code == HC_ACTION as i32 {
            let k = &*(lp.0 as *const KBDLLHOOKSTRUCT);
            let down = wp.0 as u32 == WM_KEYDOWN || wp.0 as u32 == WM_SYSKEYDOWN;
            let vk = VIRTUAL_KEY(k.vkCode as u16);
            if vk == VK_LWIN || vk == VK_RWIN {
                WIN_DOWN.with(|w| *w.borrow_mut() = down);
            } else if vk == VK_E {
                let win = WIN_DOWN.with(|w| *w.borrow());
                if down && win && !key_down(VK_CONTROL) && !key_down(VK_MENU) && !key_down(VK_SHIFT) {
                    // «маскирующая» клавиша, иначе отпускание Win откроет меню «Пуск»
                    let mask = [
                        INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VIRTUAL_KEY(0xE8), ..Default::default() } } },
                        INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VIRTUAL_KEY(0xE8), dwFlags: KEYEVENTF_KEYUP, ..Default::default() } } },
                    ];
                    SendInput(&mask, std::mem::size_of::<INPUT>() as i32);
                    SWALLOW_E.with(|s| *s.borrow_mut() = true);
                    let _ = PostMessageW(Some(agent_hwnd()), WM_WINE, WPARAM(0), LPARAM(0));
                    return LRESULT(1);
                }
                if !down && SWALLOW_E.with(|s| std::mem::take(&mut *s.borrow_mut())) {
                    return LRESULT(1);
                }
            }
        }
        CallNextHookEx(None, code, wp, lp)
    }
}

// ---------------------------------------------------------------- окна Проводника

unsafe extern "system" fn on_event(_h: HWINEVENTHOOK, _ev: u32, hwnd: HWND, obj: i32, _child: i32, _t: u32, _ms: u32) {
    if obj != OBJID_WINDOW.0 || hwnd.is_invalid() {
        return;
    }
    let mut cls = [0u16; 64];
    let n = unsafe { GetClassNameW(hwnd, &mut cls) } as usize;
    if String::from_utf16_lossy(&cls[..n]) != "CabinetWClass" {
        return;
    }
    unsafe {
        let _ = PostMessageW(Some(agent_hwnd()), WM_NEWWIN, WPARAM(hwnd.0 as usize), LPARAM(0));
    }
}

fn set_hidden(hwnd: HWND, hide: bool) {
    // прозрачность, а не SW_HIDE: окно не прыгает по панели задач и не теряет фокус
    unsafe {
        let ex = GetWindowLongW(hwnd, GWL_EXSTYLE);
        if hide {
            let _ = SetWindowLongW(hwnd, GWL_EXSTYLE, ex | WS_EX_LAYERED.0 as i32);
            let _ = windows::Win32::UI::WindowsAndMessaging::SetLayeredWindowAttributes(hwnd, COLORREF(0), 0, LWA_ALPHA);
        } else {
            let _ = SetWindowLongW(hwnd, GWL_EXSTYLE, ex & !(WS_EX_LAYERED.0 as i32));
            use windows::Win32::Graphics::Gdi::*;
            let _ = RedrawWindow(Some(hwnd), None, None, RDW_ERASE | RDW_INVALIDATE | RDW_FRAME | RDW_ALLCHILDREN);
        }
    }
}

/// Папка и выделенный файл окна Проводника. `None` — окно ещё не готово.
fn location(hwnd: HWND) -> Option<(String, Option<String>)> {
    unsafe {
        let sw: IShellWindows = CoCreateInstance(&ShellWindows, None, CLSCTX_ALL).ok()?;
        let n = sw.Count().ok()?;
        for i in 0..n {
            let Ok(d) = sw.Item(&VARIANT::from(i)) else { continue };
            let Ok(wb) = d.cast::<IWebBrowser2>() else { continue };
            if wb.HWND().ok().map(|h| h.0 as isize) != Some(hwnd.0 as isize) {
                continue;
            }
            let view: IShellFolderViewDual = wb.Document().ok()?.cast().ok()?;
            let folder: Folder2 = view.Folder().ok()?.cast().ok()?;
            let path = folder.Self_().ok()?.Path().ok()?.to_string();
            let sel = view.SelectedItems().ok().and_then(|items| {
                if items.Count().ok()? > 0 {
                    items.Item(&VARIANT::from(0)).ok()?.Path().ok().map(|b: BSTR| b.to_string())
                } else {
                    None
                }
            });
            return Some((path, sel));
        }
        None
    }
}

/// Обычная папка на диске или в сети — такие забираем себе.
fn is_fs(p: &str) -> bool {
    let b = p.as_bytes();
    let drive = b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\';
    let unc = p.starts_with(r"\\") && !p.starts_with(r"\\?\");
    (drive || unc) && !p.to_lowercase().contains(r"\$recycle.bin") && std::path::Path::new(p).is_dir()
}

fn check_pending(hw: HWND) {
    let now = Instant::now();
    PENDING.with(|pl| {
        pl.borrow_mut().retain_mut(|p| {
            if !unsafe { IsWindow(Some(p.hwnd)) }.as_bool() {
                return false;
            }
            let loc = location(p.hwnd);
            match (&loc, &p.seen) {
                (Some((path, sel)), _) if !is_fs(path) => {
                    let _ = sel;
                    set_hidden(p.hwnd, false);
                    false
                }
                (Some((path, sel)), seen) => {
                    // выделение ставится после открытия папки — даём ему до 400 мс
                    let first = seen.as_ref().map(|s| s.0).unwrap_or(now);
                    if p.seen.is_none() {
                        p.seen = Some((now, path.clone()));
                    }
                    if sel.is_none() && now.duration_since(first) < Duration::from_millis(400) {
                        return true;
                    }
                    unsafe {
                        let _ = PostMessageW(Some(p.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                    }
                    open_in_files(Some(sel.as_deref().unwrap_or(path)));
                    false
                }
                (None, _) if now.duration_since(p.since) > Duration::from_secs(3) => {
                    set_hidden(p.hwnd, false);
                    false
                }
                _ => true,
            }
        });
        unsafe {
            if pl.borrow().is_empty() {
                let _ = KillTimer(Some(hw), TIMER);
            }
        }
    });
}

unsafe extern "system" fn wndproc(hw: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_WINE => {
                open_in_files(None);
                LRESULT(0)
            }
            WM_ALLOW => {
                ALLOW_UNTIL.with(|a| *a.borrow_mut() = Some(Instant::now() + Duration::from_secs(4)));
                LRESULT(1)
            }
            WM_NEWWIN => {
                let h = HWND(wp.0 as _);
                let allowed = ALLOW_UNTIL.with(|a| a.borrow().map(|t| Instant::now() < t).unwrap_or(false));
                let known = PENDING.with(|p| p.borrow().iter().any(|x| x.hwnd == h));
                if !allowed && !known {
                    set_hidden(h, true);
                    PENDING.with(|p| p.borrow_mut().push(Pending { hwnd: h, since: Instant::now(), seen: None }));
                    SetTimer(Some(hw), TIMER, 60, None);
                    check_pending(hw);
                }
                LRESULT(0)
            }
            WM_TIMER => {
                check_pending(hw);
                LRESULT(0)
            }
            WM_CLOSE => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hw, msg, wp, lp),
        }
    }
}
