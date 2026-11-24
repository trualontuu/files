//! Файловая система: список папки, диски, известные папки, поиск, размер папки.

use serde_json::{json, Value};
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use windows::core::HSTRING;
use windows::Win32::Storage::FileSystem::*;
use windows::Win32::UI::Shell::*;

const A_DIR: u32 = 0x10;
const A_HIDDEN: u32 = 0x2;
const A_SYSTEM: u32 = 0x4;
const A_REPARSE: u32 = 0x400;

// флаги записи, которые уходят в интерфейс
pub const F_DIR: u32 = 1;
pub const F_HIDDEN: u32 = 2;
pub const F_SYSTEM: u32 = 4;
pub const F_LINK: u32 = 8;

fn ms(t: std::io::Result<SystemTime>) -> i64 {
    t.ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_millis() as i64).unwrap_or(0)
}

pub fn err_text(e: &std::io::Error) -> String {
    use std::io::ErrorKind::*;
    match e.kind() {
        PermissionDenied => "Нет доступа к этой папке".into(),
        NotFound => "Папка не найдена".into(),
        NotADirectory => "Это не папка".into(),
        _ => match e.raw_os_error() {
            Some(21) => "Устройство не готово".into(),
            Some(53) | Some(67) => "Сетевой путь не найден".into(),
            _ => e.to_string(),
        },
    }
}

fn flags(attr: u32) -> u32 {
    let mut f = 0;
    if attr & A_DIR != 0 {
        f |= F_DIR;
    }
    if attr & A_HIDDEN != 0 {
        f |= F_HIDDEN;
    }
    if attr & A_SYSTEM != 0 {
        f |= F_SYSTEM;
    }
    if attr & A_REPARSE != 0 {
        f |= F_LINK;
    }
    f
}

/// Содержимое папки: `[[имя, флаги, размер, изменён_мс], …]`.
pub fn list(dir: &str) -> Result<Value, String> {
    let rd = std::fs::read_dir(dir).map_err(|e| err_text(&e))?;
    let mut out = Vec::with_capacity(256);
    for e in rd.flatten() {
        // на Windows metadata() берётся из данных FindNextFile — без лишних обращений к диску
        let Ok(md) = e.metadata() else { continue };
        let f = flags(md.file_attributes());
        let size = if f & F_DIR != 0 { 0 } else { md.file_size() };
        out.push(json!([e.file_name().to_string_lossy(), f, size, ms(md.modified())]));
    }
    Ok(Value::Array(out))
}

/// Сведения об одном пути (для адресной строки и «открыть путь»).
pub fn stat(p: &str) -> Value {
    match std::fs::metadata(p) {
        Ok(md) => json!({"ok": true, "dir": md.is_dir(), "size": md.len(), "mtime": ms(md.modified())}),
        Err(e) => json!({"ok": false, "e": err_text(&e)}),
    }
}

pub fn drives() -> Value {
    let mut out = vec![];
    let mask = unsafe { GetLogicalDrives() };
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let root = format!("{letter}:\\");
        let hroot = HSTRING::from(root.as_str());
        let kind = unsafe { GetDriveTypeW(&hroot) };
        let kind_s = match kind {
            2 => "usb",
            3 => "disk",
            4 => "net",
            5 => "cd",
            6 => "ram",
            _ => "disk",
        };
        let mut label = [0u16; 261];
        let mut fs = [0u16; 64];
        let ok = unsafe { GetVolumeInformationW(&hroot, Some(&mut label), None, None, None, Some(&mut fs)).is_ok() };
        if !ok && (kind == 2 || kind == 5) {
            continue; // пустой картридер / дисковод
        }
        let (mut free, mut total) = (0u64, 0u64);
        unsafe {
            let _ = GetDiskFreeSpaceExW(&hroot, None, Some(&mut total), Some(&mut free));
        }
        let lab = String::from_utf16_lossy(&label[..label.iter().position(|&c| c == 0).unwrap_or(0)]);
        out.push(json!({"root": root, "letter": letter.to_string(), "label": lab, "kind": kind_s, "free": free, "total": total}));
    }
    Value::Array(out)
}

pub fn known() -> Value {
    let list: [(&str, &windows::core::GUID, &str); 7] = [
        ("desktop", &FOLDERID_Desktop, "Рабочий стол"),
        ("downloads", &FOLDERID_Downloads, "Загрузки"),
        ("documents", &FOLDERID_Documents, "Документы"),
        ("pictures", &FOLDERID_Pictures, "Изображения"),
        ("music", &FOLDERID_Music, "Музыка"),
        ("videos", &FOLDERID_Videos, "Видео"),
        ("home", &FOLDERID_Profile, "Домашняя"),
    ];
    let mut out = vec![];
    for (key, id, name) in list {
        if let Ok(p) = unsafe { SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None) } {
            let s = webview2_com::take_pwstr(p);
            if !s.is_empty() {
                out.push(json!({"key": key, "name": name, "path": s}));
            }
        }
    }
    Value::Array(out)
}

// ---------------------------------------------------------------- поиск

fn wild(pat: &[char], s: &[char]) -> bool {
    // простая маска: * и ?
    let (mut p, mut i, mut star, mut mark) = (0, 0, usize::MAX, 0);
    while i < s.len() {
        if p < pat.len() && (pat[p] == '?' || pat[p] == s[i]) {
            p += 1;
            i += 1;
        } else if p < pat.len() && pat[p] == '*' {
            star = p;
            mark = i;
            p += 1;
        } else if star != usize::MAX {
            p = star + 1;
            mark += 1;
            i = mark;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == '*' {
        p += 1;
    }
    p == pat.len()
}

const SKIP: &[&str] = &["$recycle.bin", "system volume information", ".git", "windows\\winsxs", "$windows.~ws", "$winreagent"];

/// Рекурсивный поиск по имени. Результаты отдаются пачками через `emit(items, done)`.
pub fn search(root: &str, query: &str, seq: u64, cur: &AtomicU64, show_hidden: bool, mut emit: impl FnMut(Vec<Value>, bool, usize)) {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        emit(vec![], true, 0);
        return;
    }
    let qc: Vec<char> = q.chars().collect();
    let is_wild = q.contains('*') || q.contains('?');
    // несколько слов — все должны встретиться в имени
    let words: Vec<String> = q.split_whitespace().map(String::from).collect();
    let mut stack: Vec<PathBuf> = vec![PathBuf::from(root)];
    let mut batch = vec![];
    let mut found = 0usize;
    let mut scanned = 0usize;
    let mut last = Instant::now();
    while let Some(dir) = stack.pop() {
        if cur.load(Ordering::Relaxed) != seq {
            return;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let Ok(md) = e.metadata() else { continue };
            let f = flags(md.file_attributes());
            if !show_hidden && f & (F_HIDDEN | F_SYSTEM) != 0 {
                continue;
            }
            scanned += 1;
            let name = e.file_name().to_string_lossy().into_owned();
            let ln = name.to_lowercase();
            let hit = if is_wild {
                let lc: Vec<char> = ln.chars().collect();
                wild(&qc, &lc)
            } else {
                words.iter().all(|w| ln.contains(w.as_str()))
            };
            let path = e.path();
            if hit {
                found += 1;
                let size = if f & F_DIR != 0 { 0 } else { md.file_size() };
                batch.push(json!([path.to_string_lossy(), f, size, ms(md.modified())]));
            }
            if f & F_DIR != 0 && f & F_LINK == 0 {
                let lp = path.to_string_lossy().to_lowercase();
                if !SKIP.iter().any(|s| lp.ends_with(s)) {
                    stack.push(path);
                }
            }
        }
        if !batch.is_empty() && (batch.len() >= 200 || last.elapsed() > Duration::from_millis(120)) {
            emit(std::mem::take(&mut batch), false, scanned);
            last = Instant::now();
        }
        if found >= 5000 {
            break;
        }
    }
    if cur.load(Ordering::Relaxed) == seq {
        emit(batch, true, scanned);
    }
}

/// Размер папки: (файлов, папок, байт). `tick` вызывается примерно раз в 300 мс.
pub fn dir_size(root: &str, seq: u64, cur: &AtomicU64, mut tick: impl FnMut(u64, u64, u64)) -> Option<(u64, u64, u64)> {
    let (mut files, mut dirs, mut bytes) = (0u64, 0u64, 0u64);
    let mut stack = vec![PathBuf::from(root)];
    let mut last = Instant::now();
    while let Some(d) = stack.pop() {
        if cur.load(Ordering::Relaxed) != seq {
            return None;
        }
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let Ok(md) = e.metadata() else { continue };
            let f = flags(md.file_attributes());
            if f & F_DIR != 0 {
                dirs += 1;
                if f & F_LINK == 0 {
                    stack.push(e.path());
                }
            } else {
                files += 1;
                bytes += md.file_size();
            }
        }
        if last.elapsed() > Duration::from_millis(300) {
            tick(files, dirs, bytes);
            last = Instant::now();
        }
    }
    Some((files, dirs, bytes))
}

/// Начало текстового файла для предпросмотра.
pub fn read_text(p: &str) -> Result<Value, String> {
    use std::io::Read;
    let mut f = std::fs::File::open(p).map_err(|e| err_text(&e))?;
    let total = f.metadata().map(|m| m.len()).unwrap_or(0);
    let mut buf = vec![0u8; 96 * 1024];
    let n = f.read(&mut buf).map_err(|e| err_text(&e))?;
    buf.truncate(n);
    // UTF-16 с BOM
    if buf.len() >= 2 && buf[0] == 0xFF && buf[1] == 0xFE {
        let u: Vec<u16> = buf[2..].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        return Ok(json!({"text": String::from_utf16_lossy(&u), "cut": (n as u64) < total}));
    }
    let body = if buf.starts_with(&[0xEF, 0xBB, 0xBF]) { &buf[3..] } else { &buf[..] };
    let nul = body.iter().take(8192).filter(|&&b| b == 0).count();
    if nul > 0 {
        return Ok(json!({"binary": true}));
    }
    let text = match std::str::from_utf8(body) {
        Ok(s) => s.to_string(),
        Err(e) if e.valid_up_to() + 4 >= body.len() => String::from_utf8_lossy(&body[..e.valid_up_to()]).into_owned(),
        Err(_) => cp1251(body),
    };
    Ok(json!({"text": text, "cut": (n as u64) < total}))
}

fn cp1251(b: &[u8]) -> String {
    b.iter()
        .map(|&c| match c {
            0..=0x7F => c as char,
            0xC0..=0xFF => char::from_u32(0x410 + (c as u32 - 0xC0)).unwrap_or('?'),
            0xA8 => 'Ё',
            0xB8 => 'ё',
            0xB9 => '№',
            _ => '·',
        })
        .collect()
}

/// Уникальное имя «Новая папка», «Новая папка (2)», …
pub fn unique(dir: &str, base: &str, ext: &str) -> PathBuf {
    let d = Path::new(dir);
    let mut p = d.join(format!("{base}{ext}"));
    let mut i = 2;
    while p.exists() {
        p = d.join(format!("{base} ({i}){ext}"));
        i += 1;
    }
    p
}

// ---------------------------------------------------------------- сеть

/// Общие папки компьютера `\имя` (как подпапки).
pub fn shares(server: &str) -> Result<Value, String> {
    use windows::Win32::NetworkManagement::NetManagement::NetApiBufferFree;
    let mut buf: *mut u8 = std::ptr::null_mut();
    let (mut read, mut total) = (0u32, 0u32);
    let r = unsafe { NetShareEnum(&HSTRING::from(server), 1, &mut buf, u32::MAX, &mut read, &mut total, None) };
    if r != 0 || buf.is_null() {
        return Err(match r {
            5 => "Нет доступа к компьютеру".into(),
            53 | 2250 => "Компьютер не найден в сети".into(),
            _ => format!("Не удалось получить список папок (код {r})"),
        });
    }
    let mut out = vec![];
    unsafe {
        let list = std::slice::from_raw_parts(buf as *const SHARE_INFO_1, read as usize);
        for s in list {
            let t = s.shi1_type.0;
            if t & 0xFF != 0 || t & 0x8000_0000 != 0 {
                continue; // принтеры, IPC$, C$ и прочие служебные
            }
            let name = s.shi1_netname.to_string().unwrap_or_default();
            out.push(json!([name, F_DIR, 0, 0]));
        }
        let _ = NetApiBufferFree(Some(buf as _));
    }
    Ok(Value::Array(out))
}

// ---------------------------------------------------------------- недавние

/// Недавние файлы: свой журнал Files (`own`: путь, время открытия) + системный список Recent\*.lnk.
pub fn recent(own: &[(String, i64)]) -> Value {
    use windows::core::Interface;
    use windows::Win32::System::Com::*;
    let mut cand: Vec<(i64, String)> = own.iter().map(|(p, t)| (*t, p.clone())).collect();
    let dir = std::env::var("APPDATA").map(|a| PathBuf::from(a).join(r"Microsoft\Windows\Recent")).unwrap_or_default();
    let mut lnks: Vec<(PathBuf, i64)> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().extension().map(|x| x.eq_ignore_ascii_case("lnk")).unwrap_or(false))
                .map(|e| (e.path(), ms(e.metadata().and_then(|m| m.modified()))))
                .collect()
        })
        .unwrap_or_default();
    lnks.sort_by(|a, b| b.1.cmp(&a.1));
    unsafe {
        if let Ok(link) = CoCreateInstance::<_, IShellLinkW>(&ShellLink, None, CLSCTX_INPROC_SERVER) {
            if let Ok(pf) = link.cast::<IPersistFile>() {
                for (p, opened) in lnks.into_iter().take(300) {
                    if pf.Load(&HSTRING::from(p.as_os_str()), STGM_READ).is_err() {
                        continue;
                    }
                    let mut buf = [0u16; 1024];
                    if link.GetPath(&mut buf, std::ptr::null_mut(), 0).is_err() {
                        continue;
                    }
                    let n = buf.iter().position(|&c| c == 0).unwrap_or(0);
                    if n > 0 {
                        cand.push((opened, String::from_utf16_lossy(&buf[..n])));
                    }
                }
            }
        }
    }
    cand.sort_by(|a, b| b.0.cmp(&a.0));
    let mut out = vec![];
    let mut seen = std::collections::HashSet::new();
    for (opened, target) in cand {
        if target.is_empty() || !seen.insert(target.to_lowercase()) {
            continue;
        }
        let Ok(md) = std::fs::metadata(&target) else { continue };
        let f = flags(md.file_attributes());
        if f & F_DIR != 0 {
            continue;
        }
        out.push(json!([target, f, md.file_size(), ms(md.modified()), opened]));
        if out.len() >= 150 {
            break;
        }
    }
    Value::Array(out)
}
