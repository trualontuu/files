//! ZIP: просмотр как папки, распаковка, сжатие.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};

fn cp866(b: &[u8]) -> String {
    b.iter()
        .map(|&c| match c {
            0..=0x7F => c as char,
            0x80..=0xAF => char::from_u32(0x410 + (c as u32 - 0x80)).unwrap_or('?'),
            0xE0..=0xEF => char::from_u32(0x440 + (c as u32 - 0xE0)).unwrap_or('?'),
            0xF0 => 'Ё',
            0xF1 => 'ё',
            _ => '_',
        })
        .collect()
}

/// Имя записи: UTF-8 если корректно, иначе — CP866 (архивы, сжатые Проводником в русской Windows).
fn entry_name(f: &zip::read::ZipFile<'_, BufReader<File>>) -> String {
    let raw = f.name_raw();
    let s = match std::str::from_utf8(raw) {
        Ok(s) => s.to_string(),
        Err(_) => cp866(raw),
    };
    s.replace('\\', "/")
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn ms(dt: Option<zip::DateTime>) -> i64 {
    let Some(d) = dt else { return 0 };
    // время в ZIP — местное: переводим в UTC с учётом пояса и летнего времени на ту дату
    use windows::Win32::Foundation::SYSTEMTIME;
    use windows::Win32::System::Time::TzSpecificLocalTimeToSystemTime;
    let local = SYSTEMTIME { wYear: d.year(), wMonth: d.month() as u16, wDay: d.day() as u16, wHour: d.hour() as u16, wMinute: d.minute() as u16, wSecond: d.second() as u16, ..Default::default() };
    let mut u = local;
    let _ = unsafe { TzSpecificLocalTimeToSystemTime(None, &local, &mut u) };
    let days = days_from_civil(u.wYear as i64, u.wMonth as i64, u.wDay as i64);
    ((days * 86400 + u.wHour as i64 * 3600 + u.wMinute as i64 * 60 + u.wSecond as i64) * 1000).max(0)
}

fn open(zip: &str) -> Result<zip::ZipArchive<BufReader<File>>, String> {
    let f = File::open(zip).map_err(|e| crate::fsx::err_text(&e))?;
    zip::ZipArchive::new(BufReader::new(f)).map_err(|e| format!("Не удалось открыть архив: {e}"))
}

/// Содержимое папки `inner` внутри архива: `[[имя, флаги, размер, изменён], …]`.
pub fn list(zip: &str, inner: &str) -> Result<Value, String> {
    let mut a = open(zip)?;
    let pre = if inner.is_empty() { String::new() } else { format!("{}/", inner.trim_matches('/').replace('\\', "/")) };
    let mut out: BTreeMap<String, (u32, u64, i64)> = BTreeMap::new();
    for i in 0..a.len() {
        let Ok(f) = a.by_index_raw(i) else { continue };
        let name = entry_name(&f);
        let Some(rest) = name.strip_prefix(&pre) else { continue };
        if rest.is_empty() {
            continue;
        }
        let (head, tail) = match rest.split_once('/') {
            Some((h, t)) => (h.to_string(), t),
            None => (rest.to_string(), ""),
        };
        if head.is_empty() {
            continue;
        }
        let is_dir = !tail.is_empty() || f.is_dir();
        let e = out.entry(head).or_insert((if is_dir { 1 } else { 0 }, 0, 0));
        if is_dir {
            e.0 = 1;
        } else {
            e.1 = f.size();
        }
        e.2 = e.2.max(ms(f.last_modified()));
    }
    Ok(Value::Array(out.into_iter().map(|(n, (fl, s, t))| json!([n, fl, s, t])).collect()))
}

fn safe_join(base: &Path, rel: &str) -> Option<PathBuf> {
    let mut p = base.to_path_buf();
    for part in rel.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." || part.contains(':') {
            return None; // защита от «zip slip»
        }
        p.push(part);
    }
    Some(p)
}

/// Распаковывает записи (`items` — пути внутри архива, пусто = всё) в `dst`. Возвращает созданные пути верхнего уровня.
pub fn extract(zip: &str, items: &[String], dst: &str) -> Result<Vec<String>, String> {
    let mut a = open(zip)?;
    let want: Vec<String> = items.iter().map(|s| s.trim_matches('/').replace('\\', "/")).collect();
    let mut tops: Vec<String> = vec![];
    let base = Path::new(dst);
    for i in 0..a.len() {
        let Ok(mut f) = a.by_index(i) else { continue };
        let name = entry_name(&f);
        let rel = if want.is_empty() {
            Some(name.clone())
        } else {
            want.iter().find_map(|w| {
                let parent = w.rsplit_once('/').map(|x| x.0).unwrap_or("");
                if name == *w || name.starts_with(&format!("{w}/")) {
                    Some(name[if parent.is_empty() { 0 } else { parent.len() + 1 }..].to_string())
                } else {
                    None
                }
            })
        };
        let Some(rel) = rel else { continue };
        let Some(out) = safe_join(base, &rel) else { continue };
        let top = safe_join(base, rel.split('/').next().unwrap_or("")).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        if !tops.contains(&top) {
            tops.push(top);
        }
        if f.is_dir() || name.ends_with('/') {
            let _ = std::fs::create_dir_all(&out);
            continue;
        }
        if let Some(par) = out.parent() {
            std::fs::create_dir_all(par).map_err(|e| crate::fsx::err_text(&e))?;
        }
        let mut w = File::create(&out).map_err(|e| crate::fsx::err_text(&e))?;
        std::io::copy(&mut f, &mut w).map_err(|e| e.to_string())?;
    }
    Ok(tops)
}

/// Сжимает файлы и папки в новый ZIP (UTF-8 имена).
pub fn create(src: &[String], dst: &str) -> Result<(), String> {
    let f = File::create(dst).map_err(|e| crate::fsx::err_text(&e))?;
    let mut z = zip::ZipWriter::new(std::io::BufWriter::new(f));
    let opt = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated).large_file(false);
    let dst_l = dst.to_lowercase();
    let mut buf = vec![0u8; 256 * 1024];
    for s in src {
        let root = Path::new(s);
        let base = root.parent().unwrap_or(root);
        let mut stack = vec![root.to_path_buf()];
        while let Some(p) = stack.pop() {
            if p.to_string_lossy().to_lowercase() == dst_l {
                continue;
            }
            let rel = p.strip_prefix(base).unwrap_or(&p).to_string_lossy().replace('\\', "/");
            if p.is_dir() {
                z.add_directory(format!("{rel}/"), opt).map_err(|e| e.to_string())?;
                if let Ok(rd) = std::fs::read_dir(&p) {
                    for e in rd.flatten() {
                        stack.push(e.path());
                    }
                }
            } else {
                let big = std::fs::metadata(&p).map(|m| m.len() > 0xFFFF_0000).unwrap_or(false);
                z.start_file(rel, opt.large_file(big)).map_err(|e| e.to_string())?;
                let mut r = File::open(&p).map_err(|e| crate::fsx::err_text(&e))?;
                loop {
                    let n = r.read(&mut buf).map_err(|e| e.to_string())?;
                    if n == 0 {
                        break;
                    }
                    z.write_all(&buf[..n]).map_err(|e| e.to_string())?;
                }
            }
        }
    }
    z.finish().map_err(|e| e.to_string())?;
    Ok(())
}

/// Временная папка для файлов, открытых из архива.
pub fn temp_dir(zip: &str) -> PathBuf {
    let name = Path::new(zip).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "zip".into());
    let d = std::env::temp_dir().join("Files").join(name);
    let _ = std::fs::create_dir_all(&d);
    d
}
