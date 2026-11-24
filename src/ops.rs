//! Операции с файлами через IFileOperation: свой прогресс, отмена, точные пути результата (для Ctrl+Z).

use serde_json::{json, Value};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::core::{implement, AsImpl, Ref, HRESULT, HSTRING, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::System::Com::*;
use windows::Win32::UI::Shell::*;

/// Одно действие пакета.
pub enum Item {
    Copy { src: String, dst: String },
    Move { src: String, dst: String, name: Option<String> },
    Delete { src: String, recycle: bool },
    Rename { src: String, name: String },
}

pub struct Done {
    pub aborted: bool,
    /// (исходный путь, новый путь) для каждого выполненного действия
    pub pairs: Vec<(String, String, &'static str)>,
}

static CANCEL: OnceLock<Mutex<HashMap<u64, Arc<AtomicBool>>>> = OnceLock::new();

fn cancels() -> &'static Mutex<HashMap<u64, Arc<AtomicBool>>> {
    CANCEL.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn cancel(op: u64) {
    if let Some(c) = cancels().lock().unwrap().get(&op) {
        c.store(true, Ordering::Relaxed);
    }
}

fn path_of(it: Ref<IShellItem>) -> String {
    it.as_ref()
        .and_then(|i| unsafe { i.GetDisplayName(SIGDN_FILESYSPATH).ok() })
        .map(webview2_com::take_pwstr)
        .unwrap_or_default()
}

fn aborted() -> windows::core::Error {
    windows::core::Error::from(ERROR_CANCELLED.to_hresult())
}

type Emit = Box<dyn Fn(Value)>;

#[implement(IFileOperationProgressSink)]
struct Sink {
    op: u64,
    stop: Arc<AtomicBool>,
    emit: Emit,
    cur: RefCell<String>,
    last: Cell<Option<Instant>>,
    pairs: RefCell<Vec<(String, String, &'static str)>>,
}

impl Sink {
    fn pre(&self, it: Ref<IShellItem>) -> windows::core::Result<()> {
        if self.stop.load(Ordering::Relaxed) {
            return Err(aborted());
        }
        let p = path_of(it);
        *self.cur.borrow_mut() = p.rsplit('\\').next().unwrap_or("").to_string();
        Ok(())
    }
    fn post(&self, kind: &'static str, src: Ref<IShellItem>, hr: HRESULT, new: Ref<IShellItem>) {
        if hr.is_ok() {
            let (a, b) = (path_of(src), path_of(new));
            if !a.is_empty() {
                self.pairs.borrow_mut().push((a, b, kind));
            }
        }
    }
}

impl IFileOperationProgressSink_Impl for Sink_Impl {
    fn StartOperations(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn FinishOperations(&self, _hr: HRESULT) -> windows::core::Result<()> {
        Ok(())
    }
    fn PreRenameItem(&self, _f: u32, it: Ref<IShellItem>, _n: &PCWSTR) -> windows::core::Result<()> {
        self.pre(it)
    }
    fn PostRenameItem(&self, _f: u32, it: Ref<IShellItem>, _n: &PCWSTR, hr: HRESULT, new: Ref<IShellItem>) -> windows::core::Result<()> {
        self.post("rename", it, hr, new);
        Ok(())
    }
    fn PreMoveItem(&self, _f: u32, it: Ref<IShellItem>, _d: Ref<IShellItem>, _n: &PCWSTR) -> windows::core::Result<()> {
        self.pre(it)
    }
    fn PostMoveItem(&self, _f: u32, it: Ref<IShellItem>, _d: Ref<IShellItem>, _n: &PCWSTR, hr: HRESULT, new: Ref<IShellItem>) -> windows::core::Result<()> {
        self.post("move", it, hr, new);
        Ok(())
    }
    fn PreCopyItem(&self, _f: u32, it: Ref<IShellItem>, _d: Ref<IShellItem>, _n: &PCWSTR) -> windows::core::Result<()> {
        self.pre(it)
    }
    fn PostCopyItem(&self, _f: u32, it: Ref<IShellItem>, _d: Ref<IShellItem>, _n: &PCWSTR, hr: HRESULT, new: Ref<IShellItem>) -> windows::core::Result<()> {
        self.post("copy", it, hr, new);
        Ok(())
    }
    fn PreDeleteItem(&self, _f: u32, it: Ref<IShellItem>) -> windows::core::Result<()> {
        self.pre(it)
    }
    fn PostDeleteItem(&self, _f: u32, it: Ref<IShellItem>, hr: HRESULT, new: Ref<IShellItem>) -> windows::core::Result<()> {
        self.post("delete", it, hr, new);
        Ok(())
    }
    fn PreNewItem(&self, _f: u32, _d: Ref<IShellItem>, _n: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }
    fn PostNewItem(&self, _f: u32, _d: Ref<IShellItem>, _n: &PCWSTR, _t: &PCWSTR, _a: u32, _hr: HRESULT, _i: Ref<IShellItem>) -> windows::core::Result<()> {
        Ok(())
    }
    fn UpdateProgress(&self, total: u32, sofar: u32) -> windows::core::Result<()> {
        let now = Instant::now();
        let due = self.last.get().map(|t| now.duration_since(t) > Duration::from_millis(120)).unwrap_or(true);
        if due || sofar >= total {
            self.last.set(Some(now));
            (self.emit)(json!({"ev": "progress", "op": self.op, "total": total, "done": sofar, "item": *self.cur.borrow()}));
        }
        if self.stop.load(Ordering::Relaxed) {
            return Err(aborted());
        }
        Ok(())
    }
    fn ResetTimer(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn PauseTimer(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn ResumeTimer(&self) -> windows::core::Result<()> {
        Ok(())
    }
}

fn item(p: &str) -> Result<IShellItem, String> {
    unsafe { SHCreateItemFromParsingName(&HSTRING::from(p), None) }.map_err(|e| {
        let m = e.message();
        if m.is_empty() { format!("Не найдено: {p}") } else { format!("{m} ({p})") }
    })
}

fn parent_of(p: &str) -> String {
    std::path::Path::new(p).parent().map(|x| x.to_string_lossy().trim_end_matches('\\').to_lowercase()).unwrap_or_default()
}

/// Выполняет пакет в текущем (STA) потоке. `emit` получает события прогресса.
pub fn run(hwnd: HWND, op: u64, items: Vec<Item>, emit: Emit) -> Result<Done, String> {
    let stop = Arc::new(AtomicBool::new(false));
    cancels().lock().unwrap().insert(op, stop.clone());
    let r = run_inner(hwnd, op, items, emit, stop);
    cancels().lock().unwrap().remove(&op);
    r
}

fn run_inner(hwnd: HWND, op: u64, items: Vec<Item>, emit: Emit, stop: Arc<AtomicBool>) -> Result<Done, String> {
    unsafe {
        let fo: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL).map_err(|e| e.message())?;
        let _ = fo.SetOwnerWindow(hwnd);
        // своё окно прогресса — системное прячем; вопросы о конфликтах имён остаются системными
        let mut fl = FOF_ALLOWUNDO.0 | FOF_NOCONFIRMMKDIR.0 | FOF_SILENT.0 | FOFX_EARLYFAILURE.0 as u32;
        let only_delete = items.iter().all(|i| matches!(i, Item::Delete { .. }));
        if only_delete {
            let recycle = items.iter().all(|i| matches!(i, Item::Delete { recycle: true, .. }));
            fl = FOF_NOCONFIRMATION.0 | FOF_SILENT.0;
            if recycle {
                fl |= FOF_ALLOWUNDO.0 | FOFX_RECYCLEONDELETE.0 as u32 | FOF_WANTNUKEWARNING.0;
            }
        }
        // вставка в ту же папку — «имя - копия», как в Проводнике
        let dup = items.iter().any(|i| matches!(i, Item::Copy { src, dst } if parent_of(src) == dst.trim_end_matches('\\').to_lowercase()));
        if dup {
            fl |= FOF_RENAMEONCOLLISION.0;
        }
        fo.SetOperationFlags(FILEOPERATION_FLAGS(fl)).map_err(|e| e.message())?;
        let sink: IFileOperationProgressSink = Sink {
            op,
            stop,
            emit,
            cur: RefCell::new(String::new()),
            last: Cell::new(None),
            pairs: RefCell::new(vec![]),
        }
        .into();
        let cookie = fo.Advise(&sink).ok();
        let mut queued = 0;
        for it in &items {
            match it {
                Item::Copy { src, dst } => {
                    fo.CopyItem(&item(src)?, &item(dst)?, PCWSTR::null(), None).map_err(|e| e.message())?;
                }
                Item::Move { src, dst, name } => {
                    if name.is_none() && parent_of(src) == dst.trim_end_matches('\\').to_lowercase() {
                        continue;
                    }
                    let n = name.as_ref().map(|n| HSTRING::from(n.as_str()));
                    let np = n.as_ref().map(|n| PCWSTR(n.as_ptr())).unwrap_or(PCWSTR::null());
                    fo.MoveItem(&item(src)?, &item(dst)?, np, None).map_err(|e| e.message())?;
                }
                Item::Delete { src, .. } => {
                    fo.DeleteItem(&item(src)?, None).map_err(|e| e.message())?;
                }
                Item::Rename { src, name } => {
                    fo.RenameItem(&item(src)?, &HSTRING::from(name.as_str()), None).map_err(|e| e.message())?;
                }
            }
            queued += 1;
        }
        if queued == 0 {
            return Ok(Done { aborted: false, pairs: vec![] });
        }
        let r = fo.PerformOperations();
        if let Some(c) = cookie {
            let _ = fo.Unadvise(c);
        }
        let aborted = fo.GetAnyOperationsAborted().map(|b| b.as_bool()).unwrap_or(false);
        let pairs = {
            let s: &Sink = sink.as_impl();
            std::mem::take(&mut *s.pairs.borrow_mut())
        };
        match r {
            Ok(()) => Ok(Done { aborted, pairs }),
            Err(e) if e.code() == HRESULT(0x80270000u32 as i32) || e.code() == ERROR_CANCELLED.to_hresult() => Ok(Done { aborted: true, pairs }),
            Err(e) => {
                if pairs.is_empty() {
                    Err(e.message())
                } else {
                    Ok(Done { aborted: true, pairs })
                }
            }
        }
    }
}

/// После восстановления из корзины убираем осиротевший файл сведений $I….
pub fn drop_recycle_info(recycled: &str) {
    let p = std::path::Path::new(recycled);
    if let Some(name) = p.file_name().map(|n| n.to_string_lossy().into_owned()) {
        if let Some(rest) = name.strip_prefix("$R") {
            let _ = std::fs::remove_file(p.with_file_name(format!("$I{rest}")));
        }
    }
}

pub fn parse(v: &Value) -> Vec<Item> {
    let s = |x: &Value| x.as_str().unwrap_or("").to_string();
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| match x["k"].as_str()? {
                    "copy" => Some(Item::Copy { src: s(&x["src"]), dst: s(&x["dst"]) }),
                    "move" => Some(Item::Move { src: s(&x["src"]), dst: s(&x["dst"]), name: x["name"].as_str().map(String::from) }),
                    "delete" => Some(Item::Delete { src: s(&x["src"]), recycle: true }),
                    "purge" => Some(Item::Delete { src: s(&x["src"]), recycle: false }),
                    "rename" => Some(Item::Rename { src: s(&x["src"]), name: s(&x["name"]) }),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}
