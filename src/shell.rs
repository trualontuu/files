//! Windows Shell: открытие, операции с файлами, буфер обмена, системное контекстное меню.

use std::cell::RefCell;
use windows::core::{w, Interface, HSTRING, PCSTR, PCWSTR, PSTR};
use windows::Win32::Foundation::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Memory::*;
use windows::Win32::System::Ole::CF_HDROP;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

fn parent_of(p: &str) -> String {
    std::path::Path::new(p).parent().map(|x| x.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Открыть файл/папку программой по умолчанию.
pub fn open(hwnd: HWND, p: &str) -> Result<(), String> {
    let dir = parent_of(p);
    let r = unsafe { ShellExecuteW(Some(hwnd), w!("open"), &HSTRING::from(p), PCWSTR::null(), &HSTRING::from(dir.as_str()), SW_SHOWNORMAL) };
    if r.0 as isize > 32 {
        Ok(())
    } else if r.0 as isize == 31 {
        // нет связанной программы — системный диалог «Открыть с помощью»
        open_with(hwnd, p)
    } else {
        Err("Не удалось открыть".into())
    }
}

pub fn open_with(hwnd: HWND, p: &str) -> Result<(), String> {
    let hp = HSTRING::from(p);
    let info = OPENASINFO { pcszFile: PCWSTR(hp.as_ptr()), pcszClass: PCWSTR::null(), oaifInFlags: OAIF_EXEC | OAIF_ALLOW_REGISTRATION };
    unsafe { SHOpenWithDialog(Some(hwnd), &info).map_err(|e| e.message()) }
}

pub fn properties(hwnd: HWND, p: &str) {
    unsafe {
        let _ = SHObjectProperties(Some(hwnd), SHOP_FILEPATH, &HSTRING::from(p), PCWSTR::null());
    }
}

pub fn reveal(p: &str) {
    use std::os::windows::process::CommandExt;
    crate::agent::allow_explorer();
    let _ = std::process::Command::new("explorer.exe").raw_arg(format!("/select,\"{p}\"")).spawn();
}

pub fn explorer(p: &str) {
    crate::agent::allow_explorer();
    let _ = std::process::Command::new("explorer.exe").arg(p).spawn();
}

pub fn terminal(dir: &str) {
    use std::os::windows::process::CommandExt;
    let ok = std::process::Command::new("wt.exe").args(["-d", dir]).spawn().is_ok();
    if !ok {
        let _ = std::process::Command::new("powershell.exe").current_dir(dir).creation_flags(0x10).spawn();
    }
}

// ---------------------------------------------------------------- буфер обмена

fn effect_fmt() -> u32 {
    unsafe { RegisterClipboardFormatW(w!("Preferred DropEffect")) }
}

unsafe fn global_bytes(bytes: &[u8]) -> Option<HGLOBAL> {
    unsafe {
        let g = GlobalAlloc(GMEM_MOVEABLE, bytes.len()).ok()?;
        let p = GlobalLock(g) as *mut u8;
        if p.is_null() {
            return None;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        let _ = GlobalUnlock(g);
        Some(g)
    }
}

fn open_clip(hwnd: HWND) -> bool {
    // буфер может быть занят другой программой — несколько попыток
    for _ in 0..10 {
        if unsafe { OpenClipboard(Some(hwnd)).is_ok() } {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
    false
}

/// Кладёт файлы в системный буфер (совместимо с Проводником, Telegram и т.д.).
pub fn clip_set_files(hwnd: HWND, paths: &[String], cut: bool) -> bool {
    // DROPFILES { pFiles, pt.x, pt.y, fNC, fWide } + имена UTF-16 через \0 + \0
    let mut bytes: Vec<u8> = vec![];
    bytes.extend_from_slice(&20u32.to_le_bytes());
    bytes.extend_from_slice(&0i32.to_le_bytes());
    bytes.extend_from_slice(&0i32.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&1u32.to_le_bytes());
    for p in paths {
        for u in p.encode_utf16().chain(std::iter::once(0)) {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
    }
    bytes.extend_from_slice(&0u16.to_le_bytes());
    let effect: u32 = if cut { 2 } else { 5 };
    unsafe {
        if !open_clip(hwnd) {
            return false;
        }
        let _ = EmptyClipboard();
        let mut ok = false;
        if let Some(g) = global_bytes(&bytes) {
            ok = SetClipboardData(CF_HDROP.0 as u32, Some(HANDLE(g.0))).is_ok();
        }
        if let Some(g) = global_bytes(&effect.to_le_bytes()) {
            let _ = SetClipboardData(effect_fmt(), Some(HANDLE(g.0)));
        }
        let _ = CloseClipboard();
        ok
    }
}

/// Файлы из буфера: (пути, вырезаны?).
pub fn clip_get_files(hwnd: HWND) -> Option<(Vec<String>, bool)> {
    unsafe {
        if IsClipboardFormatAvailable(CF_HDROP.0 as u32).is_err() {
            return None;
        }
        if !open_clip(hwnd) {
            return None;
        }
        let mut out = vec![];
        let mut cut = false;
        if let Ok(h) = GetClipboardData(CF_HDROP.0 as u32) {
            let hd = HDROP(h.0);
            let n = DragQueryFileW(hd, 0xFFFF_FFFF, None);
            for i in 0..n {
                let len = DragQueryFileW(hd, i, None) as usize;
                let mut buf = vec![0u16; len + 1];
                DragQueryFileW(hd, i, Some(&mut buf));
                out.push(String::from_utf16_lossy(&buf[..len]));
            }
        }
        if let Ok(h) = GetClipboardData(effect_fmt()) {
            let g = HGLOBAL(h.0);
            let p = GlobalLock(g) as *const u32;
            if !p.is_null() {
                let e = *p;
                cut = e & 2 != 0 && e & 1 == 0;
                let _ = GlobalUnlock(g);
            }
        }
        let _ = CloseClipboard();
        if out.is_empty() { None } else { Some((out, cut)) }
    }
}

pub fn clip_count() -> usize {
    unsafe { if IsClipboardFormatAvailable(CF_HDROP.0 as u32).is_ok() { 1 } else { 0 } }
}

pub fn clip_clear(hwnd: HWND) {
    unsafe {
        if open_clip(hwnd) {
            let _ = EmptyClipboard();
            let _ = CloseClipboard();
        }
    }
}

pub fn clip_set_text(hwnd: HWND, text: &str) {
    let mut bytes: Vec<u8> = vec![];
    for u in text.encode_utf16().chain(std::iter::once(0)) {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    unsafe {
        if !open_clip(hwnd) {
            return;
        }
        let _ = EmptyClipboard();
        if let Some(g) = global_bytes(&bytes) {
            let _ = SetClipboardData(13, Some(HANDLE(g.0)));
        }
        let _ = CloseClipboard();
    }
}

// ---------------------------------------------------------------- системное меню

thread_local! {
    static CM: RefCell<Option<IContextMenu>> = const { RefCell::new(None) };
}

/// Пересылка сообщений меню (подменю «Отправить», «Открыть с помощью» рисуются владельцем).
pub fn menu_msg(msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
    CM.with(|c| {
        let c = c.borrow();
        let cm = c.as_ref()?;
        unsafe {
            if let Ok(c3) = cm.cast::<IContextMenu3>() {
                let mut r = LRESULT(0);
                if c3.HandleMenuMsg2(msg, wp, lp, Some(&mut r)).is_ok() {
                    return Some(r);
                }
            } else if let Ok(c2) = cm.cast::<IContextMenu2>() {
                if c2.HandleMenuMsg(msg, wp, lp).is_ok() {
                    return Some(LRESULT(0));
                }
            }
        }
        None
    })
}

/// Показывает родное меню Проводника для файлов. Возвращает глагол, который должен обработать интерфейс (rename).
pub fn native_menu(hwnd: HWND, paths: &[String], pt: POINT, extended: bool) -> Option<String> {
    unsafe {
        let (arr, pidls) = shell_array(paths)?;
        let free = |v: &Vec<*mut ITEMIDLIST>| {
            for &p in v {
                ILFree(Some(p));
            }
        };
        let Ok(cm) = arr.BindToHandler::<_, IContextMenu>(None, &BHID_SFUIObject) else {
            free(&pidls);
            return None;
        };
        let Ok(menu) = CreatePopupMenu() else {
            free(&pidls);
            return None;
        };
        let mut fl = CMF_NORMAL | CMF_CANRENAME | CMF_EXPLORE;
        if extended {
            fl |= CMF_EXTENDEDVERBS;
        }
        let _ = cm.QueryContextMenu(menu, 0, 1, 0x7FFF, fl);
        CM.with(|c| *c.borrow_mut() = Some(cm.clone()));
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenuEx(menu, (TPM_RETURNCMD | TPM_RIGHTBUTTON).0, pt.x, pt.y, hwnd, None);
        CM.with(|c| *c.borrow_mut() = None);
        let mut result = None;
        if cmd.0 > 0 {
            let id = (cmd.0 - 1) as usize;
            let mut verb = [0u8; 64];
            let v = if cm.GetCommandString(id, GCS_VERBA, None, PSTR(verb.as_mut_ptr()), verb.len() as u32).is_ok() {
                let n = verb.iter().position(|&b| b == 0).unwrap_or(0);
                String::from_utf8_lossy(&verb[..n]).to_lowercase()
            } else {
                String::new()
            };
            if v == "rename" {
                result = Some("rename".to_string());
            } else {
                let info = CMINVOKECOMMANDINFOEX {
                    cbSize: std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32,
                    fMask: 0x4000 /* CMIC_MASK_UNICODE */ | CMIC_MASK_PTINVOKE,
                    hwnd,
                    lpVerb: PCSTR(id as *const u8),
                    lpVerbW: PCWSTR(id as *const u16),
                    nShow: SW_SHOWNORMAL.0,
                    ptInvoke: pt,
                    ..Default::default()
                };
                let _ = cm.InvokeCommand(&info as *const _ as *const CMINVOKECOMMANDINFO);
            }
        }
        let _ = DestroyMenu(menu);
        free(&pidls);
        result
    }
}

/// Тёмные системные меню (недокументированный uxtheme #135 SetPreferredAppMode).
pub fn dark_menus() {
    unsafe {
        if let Ok(ux) = LoadLibraryW(w!("uxtheme.dll")) {
            if let Some(f) = GetProcAddress(ux, PCSTR(135 as *const u8)) {
                let set: extern "system" fn(i32) -> i32 = std::mem::transmute(f);
                set(2);
            }
            if let Some(f) = GetProcAddress(ux, PCSTR(136 as *const u8)) {
                let flush: extern "system" fn() = std::mem::transmute(f);
                flush();
            }
        }
    }
}

/// Массив элементов оболочки из путей (+ PIDL, которые надо освободить).
unsafe fn shell_array(paths: &[String]) -> Option<(IShellItemArray, Vec<*mut ITEMIDLIST>)> {
    unsafe {
        let mut pidls: Vec<*mut ITEMIDLIST> = vec![];
        for p in paths {
            let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
            if SHParseDisplayName(&HSTRING::from(p.as_str()), None, &mut pidl, 0, None).is_ok() && !pidl.is_null() {
                pidls.push(pidl);
            }
        }
        if pidls.is_empty() {
            return None;
        }
        let cpidls: Vec<*const ITEMIDLIST> = pidls.iter().map(|&p| p as *const _).collect();
        match SHCreateShellItemArrayFromIDLists(&cpidls) {
            Ok(a) => Some((a, pidls)),
            Err(_) => {
                for &p in &pidls {
                    ILFree(Some(p));
                }
                None
            }
        }
    }
}

// ---------------------------------------------------------------- перетаскивание наружу

/// Системное перетаскивание файлов (в Проводник, Telegram, браузер…). Блокирует до отпускания кнопки.
pub fn drag_out(hwnd: HWND, paths: &[String]) -> u32 {
    use windows::Win32::System::Ole::*;
    unsafe {
        let Some((arr, pidls)) = shell_array(paths) else { return 0 };
        let mut eff = 0u32;
        if let Ok(dobj) = arr.BindToHandler::<_, IDataObject>(None, &BHID_DataObject) {
            let _ = windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture();
            if let Ok(e) = SHDoDragDrop(Some(hwnd), &dobj, None::<&IDropSource>, DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK) {
                eff = e.0;
            }
        }
        for &p in &pidls {
            ILFree(Some(p));
        }
        eff
    }
}

// ---------------------------------------------------------------- вместо Проводника

const WIN_E: &str = r"Software\Classes\CLSID\{52205fd8-5dfb-447d-801a-d0b52f2e83e1}";

fn reg_set(key: &str, name: Option<&str>, val: &str) -> bool {
    use windows::Win32::System::Registry::*;
    let data: Vec<u16> = val.encode_utf16().chain(std::iter::once(0)).collect();
    let hname = name.map(HSTRING::from);
    unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            &HSTRING::from(key),
            hname.as_ref().map(|n| PCWSTR(n.as_ptr())).unwrap_or(PCWSTR::null()),
            REG_SZ.0,
            Some(data.as_ptr() as _),
            (data.len() * 2) as u32,
        )
        .is_ok()
    }
}

fn reg_del_tree(key: &str) {
    use windows::Win32::System::Registry::*;
    unsafe {
        let _ = RegDeleteTreeW(HKEY_CURRENT_USER, &HSTRING::from(key));
        let _ = RegDeleteKeyW(HKEY_CURRENT_USER, &HSTRING::from(key));
    }
}

fn reg_get(key: &str) -> Option<String> {
    use windows::Win32::System::Registry::*;
    let mut buf = [0u16; 1024];
    let mut len = (buf.len() * 2) as u32;
    unsafe {
        RegGetValueW(HKEY_CURRENT_USER, &HSTRING::from(key), PCWSTR::null(), RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr() as _), Some(&mut len))
            .ok()
            .ok()?;
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(0);
    Some(String::from_utf16_lossy(&buf[..n]))
}

/// Открывать папки и Win+E в Files (только для текущего пользователя, обратимо).
const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

pub fn set_default(on: bool) -> bool {
    let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    if on {
        let cmd = format!("\"{exe}\" \"%1\"");
        let mut ok = true;
        for cls in ["Directory", "Drive"] {
            ok &= reg_set(&format!(r"Software\Classes\{cls}\shell"), None, "openinfiles");
            ok &= reg_set(&format!(r"Software\Classes\{cls}\shell\openinfiles"), None, "Открыть в Files");
            ok &= reg_set(&format!(r"Software\Classes\{cls}\shell\openinfiles\command"), None, &cmd);
        }
        // Win+E и окна Проводника из других программ ловит фоновый помощник; он же стартует при входе
        reg_del_tree(WIN_E);
        ok &= reg_set(RUN, Some("Files"), &format!("\"{exe}\" --agent"));
        crate::agent::spawn();
        ok
    } else {
        use windows::Win32::System::Registry::*;
        for cls in ["Directory", "Drive"] {
            reg_del_tree(&format!(r"Software\Classes\{cls}\shell\openinfiles"));
            unsafe {
                let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, &HSTRING::from(format!(r"Software\Classes\{cls}\shell").as_str()), PCWSTR::null());
            }
        }
        reg_del_tree(WIN_E);
        unsafe {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, &HSTRING::from(RUN), w!("Files"));
        }
        crate::agent::stop();
        true
    }
}

pub fn is_default() -> bool {
    reg_get(r"Software\Classes\Directory\shell\openinfiles\command").map(|c| c.to_lowercase().contains("files")).unwrap_or(false)
}

// ---------------------------------------------------------------- окна

/// Рабочая область монитора, на котором окно.
pub fn work_area(hwnd: HWND) -> RECT {
    use windows::Win32::Graphics::Gdi::*;
    unsafe {
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(mon, &mut mi);
        mi.rcWork
    }
}
