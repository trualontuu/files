#![windows_subsystem = "windows"]
//! Files — ч/б файловый менеджер на Rust поверх WebView2.

mod fsx;
mod ops;
mod shell;
mod thumbs;
mod wv;
mod zipx;

use notify::{RecursiveMode, Watcher};
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicIsize, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::*;
use windows::core::{w, Interface, BOOL, HSTRING, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::MARGINS;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::{SHCreateMemStream, SHCreateStreamOnFileEx};
use windows::Win32::UI::WindowsAndMessaging::*;
use wv::{get_str, h, View};

const STRIP_DIP: f64 = 4.0;
const WM_WAKE: u32 = WM_APP + 1;
const WM_DRAG: u32 = WM_APP + 2;
const WM_DEFER: u32 = WM_APP + 3;
const T_FLUSH: usize = 1;
const T_SNAP: usize = 2;
const HOST: &str = "https://files.local/";
const CLASS: PCWSTR = w!("MonoFiles");
const COPY_MAGIC: usize = 0x46494C45;

static UI_HTML: &str = include_str!("../ui/ui.html");

static MAIN_HWND: AtomicIsize = AtomicIsize::new(0);
static TX: OnceLock<mpsc::Sender<Msg>> = OnceLock::new();
static SEARCH_SEQ: AtomicU64 = AtomicU64::new(0);
static SIZE_SEQ: AtomicU64 = AtomicU64::new(0);
static STATE: OnceLock<Mutex<Value>> = OnceLock::new();
static DATA: OnceLock<PathBuf> = OnceLock::new();

enum Msg {
    Post(String),
    Thumb(u64, Option<Arc<Vec<u8>>>),
    Changed(String),
}

thread_local! {
    static VIEW: RefCell<Option<View>> = const { RefCell::new(None) };
    static ENV: RefCell<Option<ICoreWebView2Environment>> = const { RefCell::new(None) };
    static RX: RefCell<Option<mpsc::Receiver<Msg>>> = const { RefCell::new(None) };
    static PENDING: RefCell<HashMap<u64, (ICoreWebView2WebResourceRequestedEventArgs, ICoreWebView2Deferral)>> = RefCell::new(HashMap::new());
    static NEXT: RefCell<u64> = const { RefCell::new(1) };
    static THUMBS: RefCell<Option<thumbs::Pool>> = const { RefCell::new(None) };
    static WATCH: RefCell<Option<notify::RecommendedWatcher>> = const { RefCell::new(None) };
    static WATCHED: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
    static CHANGED: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
    static DEFER: RefCell<Vec<Box<dyn FnOnce()>>> = RefCell::new(vec![]);
}

fn hwnd() -> HWND {
    HWND(MAIN_HWND.load(Ordering::Relaxed) as _)
}

fn wake() {
    let h = MAIN_HWND.load(Ordering::Relaxed);
    if h != 0 {
        unsafe {
            let _ = PostMessageW(Some(HWND(h as _)), WM_WAKE, WPARAM(0), LPARAM(0));
        }
    }
}

fn send(m: Msg) {
    if let Some(tx) = TX.get() {
        let _ = tx.send(m);
        wake();
    }
}

fn post(v: Value) {
    send(Msg::Post(v.to_string()));
}

fn reply(id: u64, r: std::result::Result<Value, String>) {
    match r {
        Ok(v) => post(json!({"r": id, "v": v})),
        Err(e) => post(json!({"r": id, "e": e})),
    }
}

/// Отложить действие до следующего витка цикла сообщений (не крутить модальные циклы внутри обработчиков WebView2).
fn defer(f: impl FnOnce() + 'static) {
    DEFER.with(|d| d.borrow_mut().push(Box::new(f)));
    unsafe {
        let _ = PostMessageW(Some(hwnd()), WM_DEFER, WPARAM(0), LPARAM(0));
    }
}

fn scale(hwnd: HWND) -> f64 {
    unsafe { GetDpiForWindow(hwnd) as f64 / 96.0 }
}

fn strip(hwnd: HWND) -> i32 {
    if unsafe { IsZoomed(hwnd).as_bool() } { 0 } else { (STRIP_DIP * scale(hwnd)).round() as i32 }
}

fn layout() {
    let hw = hwnd();
    let mut r = RECT::default();
    unsafe {
        let _ = GetClientRect(hw, &mut r);
    }
    r.top += strip(hw);
    VIEW.with(|v| {
        if let Some(v) = v.borrow().as_ref() {
            v.bounds(r);
        }
    });
}

fn data_dir() -> PathBuf {
    if let Ok(d) = std::env::var("FILES_DATA") {
        let d = PathBuf::from(d);
        let _ = std::fs::create_dir_all(&d);
        return d;
    }
    let base = std::env::var("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir());
    let d = base.join("Files");
    let _ = std::fs::create_dir_all(&d);
    d
}

fn state_path() -> PathBuf {
    DATA.get().cloned().unwrap_or_default().join("state.json")
}

fn save_state() {
    if let Some(s) = STATE.get() {
        let txt = s.lock().unwrap().to_string();
        let p = state_path();
        let tmp = p.with_extension("tmp");
        if std::fs::write(&tmp, txt).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }
}

fn secondary() -> bool {
    std::env::args().any(|a| a == "--new-window")
}

fn test_mode() -> bool {
    std::env::var("FILES_TEST").is_ok()
}

fn arr_str(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
}

// ---------------------------------------------------------------- команды интерфейса

fn on_msg(m: Value, files: Vec<String>) {
    let id = m["id"].as_u64().unwrap_or(0);
    let c = m["c"].as_str().unwrap_or("").to_string();
    let p = m["p"].as_str().unwrap_or("").to_string();
    match c.as_str() {
        "list" => {
            std::thread::spawn(move || {
                // \\компьютер — список общих папок
                let server = p.starts_with(r"\\") && !p[2..].trim_end_matches('\\').contains('\\');
                reply(id, if server { fsx::shares(p.trim_end_matches('\\')) } else { fsx::list(&p) })
            });
        }
        "recent" => {
            let own: Vec<(String, i64)> = m["own"].as_array().map(|a| a.iter().filter_map(|x| Some((x[0].as_str()?.to_string(), x[1].as_i64()?))).collect()).unwrap_or_default();
            std::thread::spawn(move || {
                unsafe {
                    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
                }
                reply(id, Ok(fsx::recent(&own)));
            });
        }
        "zlist" => {
            let z = m["zip"].as_str().unwrap_or("").to_string();
            std::thread::spawn(move || reply(id, zipx::list(&z, &p)));
        }
        "zextract" => {
            // распаковать записи архива в папку
            let z = m["zip"].as_str().unwrap_or("").to_string();
            let items = arr_str(&m["items"]);
            let dst = m["dst"].as_str().unwrap_or("").to_string();
            std::thread::spawn(move || reply(id, zipx::extract(&z, &items, &dst).map(|t| json!(t))));
        }
        "zopen" | "zclip" | "zdrag" => {
            // открыть / скопировать / вытащить файл из архива: сначала во временную папку
            let z = m["zip"].as_str().unwrap_or("").to_string();
            let items = arr_str(&m["items"]);
            let cut = c.clone();
            std::thread::spawn(move || {
                let dir = zipx::temp_dir(&z);
                let r = zipx::extract(&z, &items, &dir.to_string_lossy());
                match r {
                    Ok(paths) => {
                        let hw = MAIN_HWND.load(Ordering::Relaxed);
                        match cut.as_str() {
                            "zopen" => {
                                unsafe {
                                    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
                                }
                                for p in &paths {
                                    let _ = shell::open(HWND(hw as _), p);
                                }
                                reply(id, Ok(Value::Null));
                            }
                            "zclip" => {
                                send_ui(move || {
                                    shell::clip_set_files(hwnd(), &paths, false);
                                    reply(id, Ok(json!(paths.len())));
                                });
                            }
                            _ => {
                                send_ui(move || {
                                    defer(move || {
                                        let e = shell::drag_out(hwnd(), &paths);
                                        post(json!({"ev": "dragdone", "effect": e}));
                                    });
                                    reply(id, Ok(Value::Null));
                                });
                            }
                        }
                    }
                    Err(e) => reply(id, Err(e)),
                }
            });
        }
        "zcreate" => {
            let src = arr_str(&m["src"]);
            std::thread::spawn(move || reply(id, zipx::create(&src, &p).map(|_| Value::Null)));
        }
        "stat" => reply(id, Ok(fsx::stat(&p))),
        "drives" => {
            std::thread::spawn(move || reply(id, Ok(fsx::drives())));
        }
        "text" => {
            std::thread::spawn(move || reply(id, fsx::read_text(&p)));
        }
        "open" => {
            let hw = MAIN_HWND.load(Ordering::Relaxed);
            std::thread::spawn(move || {
                unsafe {
                    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
                }
                reply(id, shell::open(HWND(hw as _), &p).map(|_| Value::Null));
            });
        }
        "openwith" => defer(move || {
            let _ = shell::open_with(hwnd(), &p);
        }),
        "props" => defer(move || shell::properties(hwnd(), &p)),
        "reveal" => shell::reveal(&p),
        "explorer" => shell::explorer(&p),
        "term" => shell::terminal(&p),
        "op" => run_items(id, ops::parse(&m["items"]), Value::Null),
        "cancelop" => ops::cancel(m["op"].as_u64().unwrap_or(0)),
        "mkdir" | "mkfile" => {
            let r = if c == "mkdir" {
                let t = fsx::unique(&p, "Новая папка", "");
                std::fs::create_dir(&t).map(|_| t)
            } else {
                let t = fsx::unique(&p, "Новый текстовый документ", ".txt");
                std::fs::write(&t, b"").map(|_| t)
            };
            reply(id, r.map(|t| json!(t.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())).map_err(|e| fsx::err_text(&e)));
        }
        "clip" => {
            let src = arr_str(&m["src"]);
            let cut = m["k"].as_str() == Some("cut");
            reply(id, Ok(json!(shell::clip_set_files(hwnd(), &src, cut))));
        }
        "cliptext" => {
            shell::clip_set_text(hwnd(), m["t"].as_str().unwrap_or(""));
            reply(id, Ok(Value::Null));
        }
        "clipinfo" => reply(id, Ok(json!(shell::clip_count()))),
        "paste" => {
            let Some((src, cut)) = shell::clip_get_files(hwnd()) else {
                return reply(id, Ok(json!({"none": true})));
            };
            if cut {
                shell::clip_clear(hwnd());
            }
            let n = src.len();
            let items = src.into_iter().map(|s| if cut { ops::Item::Move { src: s, dst: p.clone(), name: None } } else { ops::Item::Copy { src: s, dst: p.clone() } }).collect();
            run_items(id, items, json!({"n": n, "cut": cut}));
        }
        "drop" => {
            // файлы, брошенные из Проводника/других программ
            let copy = m["copy"].as_bool().unwrap_or(true);
            let n = files.len();
            if n == 0 {
                return reply(id, Ok(json!({"n": 0})));
            }
            let items = files.into_iter().map(|s| if copy { ops::Item::Copy { src: s, dst: p.clone() } } else { ops::Item::Move { src: s, dst: p.clone(), name: None } }).collect();
            run_items(id, items, json!({"n": n, "cut": !copy}));
        }
        "dragout" => {
            let src = arr_str(&m["src"]);
            defer(move || {
                let e = shell::drag_out(hwnd(), &src);
                post(json!({"ev": "dragdone", "effect": e}));
            });
        }
        "setdefault" => {
            let on = m["on"].as_bool().unwrap_or(false);
            reply(id, Ok(json!(shell::set_default(on) && shell::is_default() == on)));
        }
        "isdefault" => reply(id, Ok(json!(shell::is_default()))),
        "newwin" => {
            // новое окно; side — разложить два окна рядом
            let side = m["side"].as_bool().unwrap_or(false);
            let exe = std::env::current_exe().unwrap_or_default();
            let mut cmd = std::process::Command::new(exe);
            cmd.arg("--new-window");
            if side {
                cmd.arg("--right");
                let hw = hwnd();
                let wa = shell::work_area(hw);
                unsafe {
                    let _ = ShowWindow(hw, SW_RESTORE);
                    let _ = SetWindowPos(hw, None, wa.left, wa.top, (wa.right - wa.left) / 2, wa.bottom - wa.top, SWP_NOZORDER);
                }
            }
            if !p.is_empty() {
                cmd.arg(&p);
            }
            let _ = cmd.spawn();
        }
        "menu" => {
            let src = arr_str(&m["src"]);
            let (x, y) = (m["x"].as_f64().unwrap_or(0.0), m["y"].as_f64().unwrap_or(0.0));
            let ext = m["shift"].as_bool().unwrap_or(false);
            defer(move || {
                let hw = hwnd();
                let s = scale(hw);
                let mut pt = POINT { x: (x * s) as i32, y: (y * s) as i32 + strip(hw) };
                unsafe {
                    let _ = ClientToScreen(hw, &mut pt);
                }
                let v = shell::native_menu(hw, &src, pt, ext);
                post(json!({"ev": "menu", "verb": v}));
            });
        }
        "search" => {
            let q = m["q"].as_str().unwrap_or("").to_string();
            let hidden = m["hidden"].as_bool().unwrap_or(false);
            let seq = SEARCH_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
            let sid = m["sid"].as_u64().unwrap_or(0);
            std::thread::spawn(move || {
                fsx::search(&p, &q, seq, &SEARCH_SEQ, hidden, |items, done, scanned| {
                    post(json!({"ev": "search", "sid": sid, "items": items, "done": done, "scanned": scanned}));
                });
            });
        }
        "cancel" => {
            SEARCH_SEQ.fetch_add(1, Ordering::Relaxed);
        }
        "dirsize" => {
            let seq = SIZE_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
            std::thread::spawn(move || {
                let pp = p.clone();
                let r = fsx::dir_size(&p, seq, &SIZE_SEQ, |f, d, b| {
                    post(json!({"ev": "dirsize", "p": pp, "files": f, "dirs": d, "bytes": b, "done": false}));
                });
                if let Some((f, d, b)) = r {
                    post(json!({"ev": "dirsize", "p": p, "files": f, "dirs": d, "bytes": b, "done": true}));
                }
            });
        }
        "watch" => set_watch(arr_str(&m["dirs"])),
        "save" => {
            if let Some(s) = STATE.get() {
                let mut st = s.lock().unwrap();
                let mut ui = m["state"].clone();
                if secondary() {
                    // настройки общие, а вкладки остаются у главного окна
                    ui["tabs"] = st["ui"]["tabs"].clone();
                    ui["active"] = st["ui"]["active"].clone();
                }
                st["ui"] = ui;
            }
            save_state();
        }
        "win" => {
            let hw = hwnd();
            unsafe {
                match m["k"].as_str().unwrap_or("") {
                    "min" => {
                        let _ = ShowWindow(hw, SW_MINIMIZE);
                    }
                    "max" => {
                        let _ = ShowWindow(hw, if IsZoomed(hw).as_bool() { SW_RESTORE } else { SW_MAXIMIZE });
                    }
                    "close" => {
                        let _ = PostMessageW(Some(hw), WM_CLOSE, WPARAM(0), LPARAM(0));
                    }
                    "drag" => {
                        let _ = PostMessageW(Some(hw), WM_DRAG, WPARAM(0), LPARAM(0));
                    }
                    _ => {}
                }
            }
        }
        "ready" => {
            if std::env::var("FILES_SNAP").is_ok() {
                if let Ok(js) = std::env::var("FILES_EVAL") {
                    VIEW.with(|v| {
                        if let Some(v) = v.borrow().as_ref() {
                            v.eval(&js);
                        }
                    });
                }
                let delay = std::env::var("FILES_SNAP_DELAY").ok().and_then(|s| s.parse().ok()).unwrap_or(1800);
                unsafe {
                    SetTimer(Some(hwnd()), T_SNAP, delay, None);
                }
            }
        }
        "log" if test_mode() => {
            let line = format!("{}\n", m["v"]);
            let p = DATA.get().cloned().unwrap_or_default().join("log.txt");
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
                let _ = f.write_all(line.as_bytes());
            }
        }
        _ => {}
    }
}

/// Пакет операций в отдельном STA-потоке; id запроса служит и номером операции (для прогресса и отмены).
fn run_items(id: u64, items: Vec<ops::Item>, extra: Value) {
    let hw = MAIN_HWND.load(Ordering::Relaxed);
    std::thread::spawn(move || {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
        }
        let r = ops::run(HWND(hw as _), id, items, Box::new(post));
        reply(
            id,
            r.map(|d| {
                // восстановление из корзины: убрать осиротевшие $I-файлы
                for (a, _, k) in &d.pairs {
                    if *k == "move" && a.to_lowercase().contains("\\$recycle.bin\\") {
                        ops::drop_recycle_info(a);
                    }
                }
                json!({"aborted": d.aborted, "pairs": d.pairs.iter().map(|(a, b, k)| json!([a, b, k])).collect::<Vec<_>>(), "x": extra})
            }),
        );
        unsafe {
            CoUninitialize();
        }
    });
}

/// Выполнить замыкание в потоке окна.
fn send_ui(f: impl FnOnce() + Send + 'static) {
    UI_JOBS.lock().unwrap().push(Box::new(f));
    wake();
}
static UI_JOBS: Mutex<Vec<Box<dyn FnOnce() + Send>>> = Mutex::new(vec![]);

fn set_watch(dirs: Vec<String>) {
    let want: HashSet<String> = dirs.into_iter().filter(|d| d.len() >= 3).collect();
    WATCH.with(|w| {
        let mut w = w.borrow_mut();
        if w.is_none() {
            *w = notify::recommended_watcher(|res: notify::Result<notify::Event>| {
                if let Ok(ev) = res {
                    for p in ev.paths {
                        if let Some(par) = p.parent() {
                            send(Msg::Changed(par.to_string_lossy().into_owned()));
                        }
                    }
                }
            })
            .ok();
        }
        let Some(wt) = w.as_mut() else { return };
        WATCHED.with(|cur| {
            let mut cur = cur.borrow_mut();
            for d in cur.iter() {
                if !want.contains(d) {
                    let _ = wt.unwatch(std::path::Path::new(d));
                }
            }
            for d in &want {
                if !cur.contains(d) {
                    let _ = wt.watch(std::path::Path::new(d), RecursiveMode::NonRecursive);
                }
            }
            *cur = want;
        });
    });
}

fn on_wake() {
    let jobs: Vec<Box<dyn FnOnce() + Send>> = std::mem::take(&mut *UI_JOBS.lock().unwrap());
    for j in jobs {
        j();
    }
    let msgs: Vec<Msg> = RX.with(|r| r.borrow().as_ref().map(|r| r.try_iter().collect()).unwrap_or_default());
    for m in msgs {
        match m {
            Msg::Post(s) => VIEW.with(|v| {
                if let Some(v) = v.borrow().as_ref() {
                    v.post(&s);
                }
            }),
            Msg::Thumb(id, data) => finish_thumb(id, data),
            Msg::Changed(d) => {
                CHANGED.with(|c| c.borrow_mut().insert(d));
                unsafe {
                    SetTimer(Some(hwnd()), T_FLUSH, 150, None);
                }
            }
        }
    }
}

fn flush_changed() {
    unsafe {
        let _ = KillTimer(Some(hwnd()), T_FLUSH);
    }
    let dirs: Vec<String> = CHANGED.with(|c| c.borrow_mut().drain().collect());
    if !dirs.is_empty() {
        VIEW.with(|v| {
            if let Some(v) = v.borrow().as_ref() {
                v.post(&json!({"ev": "changed", "dirs": dirs}).to_string());
            }
        });
    }
}

// ---------------------------------------------------------------- ресурсы https://files.local/

fn mime(p: &str) -> &'static str {
    let ext = p.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" | "jfif" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "svg" => "image/svg+xml",
        "avif" => "image/avif",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mkv" => "video/x-matroska",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "wav" => "audio/wav",
        "ogg" | "opus" => "audio/ogg",
        "flac" => "audio/flac",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

fn query(uri: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if let Some(q) = uri.split_once('?').map(|x| x.1) {
        for kv in q.split('&') {
            if let Some((k, v)) = kv.split_once('=') {
                out.insert(k.to_string(), urldec(v));
            }
        }
    }
    out
}

fn urldec(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn respond(args: &ICoreWebView2WebResourceRequestedEventArgs, stream: Option<IStream>, status: i32, headers: &str) {
    ENV.with(|e| {
        if let Some(env) = e.borrow().as_ref() {
            unsafe {
                let reason = if status == 200 { "OK" } else { "Not Found" };
                if let Ok(resp) = env.CreateWebResourceResponse(stream.as_ref(), status, &h(reason), &h(headers)) {
                    let _ = args.SetResponse(&resp);
                }
            }
        }
    });
}

fn on_resource(args: ICoreWebView2WebResourceRequestedEventArgs) {
    let Ok(req) = (unsafe { args.Request() }) else { return };
    let uri = get_str(|p| unsafe { req.Uri(p) });
    let path = uri.strip_prefix(HOST).unwrap_or("");
    let route = path.split('?').next().unwrap_or("");
    let q = query(path);
    match route {
        "" | "index.html" => {
            let s = unsafe { SHCreateMemStream(Some(UI_HTML.as_bytes())) };
            respond(&args, s, 200, "Content-Type: text/html; charset=utf-8");
        }
        "raw" => {
            let p = q.get("p").cloned().unwrap_or_default();
            let s = unsafe { SHCreateStreamOnFileEx(&HSTRING::from(p.as_str()), (STGM_READ | STGM_SHARE_DENY_NONE).0, 0, false, None).ok() };
            if s.is_some() {
                respond(&args, s, 200, &format!("Content-Type: {}\r\nCache-Control: no-store", mime(&p)));
            } else {
                respond(&args, None, 404, "");
            }
        }
        "thumb" => {
            let p = q.get("p").cloned().unwrap_or_default();
            let px: i32 = q.get("s").and_then(|s| s.parse().ok()).unwrap_or(96).clamp(16, 512);
            let icon = q.get("k").map(|k| k == "i").unwrap_or(false);
            let key = format!("{}|{}|{}|{}", p, px, icon, q.get("v").cloned().unwrap_or_default());
            let Ok(def) = (unsafe { args.GetDeferral() }) else { return };
            let id = NEXT.with(|n| {
                let mut n = n.borrow_mut();
                *n += 1;
                *n
            });
            PENDING.with(|pd| pd.borrow_mut().insert(id, (args, def)));
            THUMBS.with(|t| {
                if let Some(t) = t.borrow().as_ref() {
                    t.push(thumbs::Job { id, path: p, px, icon, key });
                }
            });
        }
        _ => respond(&args, None, 404, ""),
    }
}

fn finish_thumb(id: u64, data: Option<Arc<Vec<u8>>>) {
    let Some((args, def)) = PENDING.with(|p| p.borrow_mut().remove(&id)) else { return };
    match data {
        Some(b) => {
            let s = unsafe { SHCreateMemStream(Some(&b)) };
            respond(&args, s, 200, "Content-Type: image/png\r\nCache-Control: max-age=31536000");
        }
        None => respond(&args, None, 404, "Cache-Control: max-age=600"),
    }
    unsafe {
        let _ = def.Complete();
    }
}

// ---------------------------------------------------------------- WebView

fn boot_script(open: Option<String>) -> String {
    let ui = STATE.get().map(|s| s.lock().unwrap()["ui"].clone()).unwrap_or(Value::Null);
    let boot = json!({
        "state": ui,
        "known": fsx::known(),
        "open": open,
        "test": test_mode(),
        "secondary": secondary(),
        "version": env!("CARGO_PKG_VERSION"),
    });
    format!("window.__BOOT={};", boot)
}

fn create_view(env: &ICoreWebView2Environment, open: Option<String>) {
    let boot = boot_script(open);
    let handler = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(move |res, ctl| {
        res?;
        let Some(ctl) = ctl else { return Ok(()) };
        let wv = unsafe { ctl.CoreWebView2()? };
        let view = View { ctl, wv };
        view.set_bg(10, 10, 10);
        unsafe {
            if let Ok(s) = view.wv.Settings() {
                let _ = s.SetIsStatusBarEnabled(false);
                let _ = s.SetAreDefaultContextMenusEnabled(false);
                let _ = s.SetIsZoomControlEnabled(false);
                let _ = s.SetAreDevToolsEnabled(cfg!(debug_assertions) || test_mode());
                if let Ok(s3) = s.cast::<ICoreWebView2Settings3>() {
                    let _ = s3.SetAreBrowserAcceleratorKeysEnabled(false);
                }
                if let Ok(s4) = s.cast::<ICoreWebView2Settings4>() {
                    let _ = s4.SetIsPasswordAutosaveEnabled(false);
                    let _ = s4.SetIsGeneralAutofillEnabled(false);
                }
                if let Ok(s6) = s.cast::<ICoreWebView2Settings6>() {
                    let _ = s6.SetIsSwipeNavigationEnabled(false);
                }
            }
            let mut tok = 0i64;
            let _ = view.wv.AddScriptToExecuteOnDocumentCreated(&h(&boot), None);
            let _ = view.wv.AddWebResourceRequestedFilter(&h(&format!("{HOST}*")), COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL);
            let _ = view.wv.add_WebResourceRequested(
                &WebResourceRequestedEventHandler::create(Box::new(|_, args| {
                    if let Some(a) = args {
                        on_resource(a);
                    }
                    Ok(())
                })),
                &mut tok,
            );
            let _ = view.wv.add_WebMessageReceived(
                &WebMessageReceivedEventHandler::create(Box::new(|_, args| {
                    let Some(args) = args else { return Ok(()) };
                    let json = get_str(|p| args.WebMessageAsJson(p));
                    let mut files = vec![];
                    if let Ok(a2) = args.cast::<ICoreWebView2WebMessageReceivedEventArgs2>() {
                        if let Ok(objs) = a2.AdditionalObjects() {
                            let mut n = 0u32;
                            let _ = objs.Count(&mut n);
                            for i in 0..n {
                                if let Ok(o) = objs.GetValueAtIndex(i) {
                                    if let Ok(f) = o.cast::<ICoreWebView2File>() {
                                        let p = get_str(|p| f.Path(p));
                                        if !p.is_empty() {
                                            files.push(p);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    if let Ok(m) = serde_json::from_str::<Value>(&json) {
                        on_msg(m, files);
                    }
                    Ok(())
                })),
                &mut tok,
            );
            // заголовок окна = текущая папка (видно в панели задач и Alt+Tab)
            let _ = view.wv.add_DocumentTitleChanged(
                &DocumentTitleChangedEventHandler::create(Box::new(|wv, _| {
                    if let Some(wv) = wv {
                        let t = get_str(|p| wv.DocumentTitle(p));
                        if !t.is_empty() {
                            let _ = SetWindowTextW(hwnd(), &HSTRING::from(t.as_str()));
                        }
                    }
                    Ok(())
                })),
                &mut tok,
            );
            let _ = view.wv.add_NewWindowRequested(
                &NewWindowRequestedEventHandler::create(Box::new(|_, args| {
                    if let Some(a) = args {
                        let _ = a.SetHandled(true);
                    }
                    Ok(())
                })),
                &mut tok,
            );
        }
        view.navigate(HOST);
        VIEW.with(|v| *v.borrow_mut() = Some(view.clone()));
        layout();
        view.focus();
        Ok(())
    }));
    unsafe {
        let _ = env.CreateCoreWebView2Controller(hwnd(), &handler);
    }
}

fn snap() {
    unsafe {
        let _ = KillTimer(Some(hwnd()), T_SNAP);
    }
    let out = std::env::var("FILES_SNAP").unwrap_or_default();
    let Ok(stream) = (unsafe { SHCreateStreamOnFileEx(&HSTRING::from(out.as_str()), (STGM_CREATE | STGM_WRITE).0, 0x80, true, None) }) else {
        unsafe { PostQuitMessage(2) };
        return;
    };
    let view = VIEW.with(|v| v.borrow().clone());
    let Some(view) = view else { return };
    let s2 = stream.clone();
    let handler = CapturePreviewCompletedHandler::create(Box::new(move |_res| {
        let _ = &s2;
        unsafe {
            let _ = s2.Commit(STGC_DEFAULT);
            PostMessageW(Some(hwnd()), WM_CLOSE, WPARAM(0), LPARAM(0)).ok();
        }
        Ok(())
    }));
    unsafe {
        let _ = view.wv.CapturePreview(COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG, &stream, &handler);
    }
}

// ---------------------------------------------------------------- окно

extern "system" fn wndproc(hw: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if matches!(msg, WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR) {
            if let Some(r) = shell::menu_msg(msg, wp, lp) {
                return r;
            }
        }
        match msg {
            WM_NCCALCSIZE if wp.0 != 0 => {
                let p = &mut *(lp.0 as *mut NCCALCSIZE_PARAMS);
                let top = p.rgrc[0].top;
                let r = DefWindowProcW(hw, msg, wp, lp);
                p.rgrc[0].top = top;
                if IsZoomed(hw).as_bool() {
                    let dpi = GetDpiForWindow(hw);
                    p.rgrc[0].top += GetSystemMetricsForDpi(SM_CYFRAME, dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi);
                }
                r
            }
            WM_NCHITTEST => {
                let r = DefWindowProcW(hw, msg, wp, lp);
                if r.0 as u32 == HTCLIENT && !IsZoomed(hw).as_bool() {
                    let mut pt = POINT { x: (lp.0 & 0xFFFF) as i16 as i32, y: ((lp.0 >> 16) & 0xFFFF) as i16 as i32 };
                    let _ = ScreenToClient(hw, &mut pt);
                    if pt.y < strip(hw) {
                        return LRESULT(HTTOP as isize);
                    }
                }
                r
            }
            WM_SIZE => {
                layout();
                VIEW.with(|v| {
                    if let Some(v) = v.borrow().as_ref() {
                        let z = IsZoomed(hw).as_bool();
                        v.post(&json!({"ev": "win", "max": z}).to_string());
                    }
                });
                LRESULT(0)
            }
            WM_MOVE | WM_MOVING => {
                VIEW.with(|v| {
                    if let Some(v) = v.borrow().as_ref() {
                        let _ = v.ctl.NotifyParentWindowPositionChanged();
                    }
                });
                DefWindowProcW(hw, msg, wp, lp)
            }
            WM_ACTIVATE => {
                let on = (wp.0 & 0xFFFF) as u32 != WA_INACTIVE;
                VIEW.with(|v| {
                    if let Some(v) = v.borrow().as_ref() {
                        v.post(&json!({"ev": "focus", "on": on}).to_string());
                        if on {
                            v.focus();
                        }
                    }
                });
                DefWindowProcW(hw, msg, wp, lp)
            }
            WM_GETMINMAXINFO => {
                let mi = &mut *(lp.0 as *mut MINMAXINFO);
                let s = scale(hw);
                mi.ptMinTrackSize = POINT { x: (560.0 * s) as i32, y: (360.0 * s) as i32 };
                LRESULT(0)
            }
            WM_DPICHANGED => {
                let r = &*(lp.0 as *const RECT);
                let _ = SetWindowPos(hw, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                LRESULT(0)
            }
            WM_TIMER => {
                match wp.0 {
                    T_FLUSH => flush_changed(),
                    T_SNAP => snap(),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_WAKE => {
                on_wake();
                LRESULT(0)
            }
            WM_DEFER => {
                let list: Vec<Box<dyn FnOnce()>> = DEFER.with(|d| std::mem::take(&mut *d.borrow_mut()));
                for f in list {
                    f();
                }
                LRESULT(0)
            }
            WM_DRAG => {
                let _ = ReleaseCapture();
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let _ = SendMessageW(hw, WM_NCLBUTTONDOWN, Some(WPARAM(HTCAPTION as usize)), Some(LPARAM(((pt.y as isize) << 16) | (pt.x as isize & 0xFFFF))));
                LRESULT(0)
            }
            WM_ERASEBKGND => {
                let dc = HDC(wp.0 as _);
                let mut r = RECT::default();
                let _ = GetClientRect(hw, &mut r);
                let b = CreateSolidBrush(COLORREF(0x000A0A0A));
                FillRect(dc, &r, b);
                let _ = DeleteObject(b.into());
                LRESULT(1)
            }
            WM_COPYDATA => {
                let cds = &*(lp.0 as *const COPYDATASTRUCT);
                if cds.dwData == COPY_MAGIC {
                    let s = std::slice::from_raw_parts(cds.lpData as *const u16, cds.cbData as usize / 2);
                    let p = String::from_utf16_lossy(s);
                    VIEW.with(|v| {
                        if let Some(v) = v.borrow().as_ref() {
                            v.post(&json!({"ev": "open", "p": p}).to_string());
                        }
                    });
                    if IsIconic(hw).as_bool() {
                        let _ = ShowWindow(hw, SW_RESTORE);
                    }
                    let _ = SetForegroundWindow(hw);
                }
                LRESULT(1)
            }
            WM_CLOSE => {
                if !test_mode() && !secondary() {
                    let mut wp = WINDOWPLACEMENT { length: std::mem::size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
                    if GetWindowPlacement(hw, &mut wp).is_ok() {
                        let r = wp.rcNormalPosition;
                        if let Some(s) = STATE.get() {
                            s.lock().unwrap()["win"] = json!([r.left, r.top, r.right, r.bottom, IsZoomed(hw).as_bool() as i32]);
                        }
                    }
                    save_state();
                }
                let _ = DestroyWindow(hw);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hw, msg, wp, lp),
        }
    }
}

fn main() {
    unsafe {
        // OLE (а не просто COM) — нужно для перетаскивания файлов наружу
        let _ = windows::Win32::System::Ole::OleInitialize(None);
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    shell::dark_menus();
    // папка из командной строки
    let arg: Option<String> = std::env::args().skip(1).find(|a| !a.starts_with("--")).map(|a| {
        std::fs::canonicalize(&a).map(|p| p.to_string_lossy().trim_start_matches(r"\\?\").to_string()).unwrap_or(a)
    });
    if let Some(a) = &arg {
        if a.starts_with("::") || a.to_lowercase().starts_with("shell:") {
            shell::explorer(a);
            return;
        }
    }
    let open = arg.or_else(|| std::env::var("FILES_OPEN").ok());
    // один экземпляр: отдаём путь уже открытому окну
    if !test_mode() && !secondary() {
        unsafe {
            if let Ok(other) = FindWindowW(CLASS, PCWSTR::null()) {
                if !other.is_invalid() {
                    let payload: Vec<u16> = open.clone().unwrap_or_default().encode_utf16().collect();
                    let cds = COPYDATASTRUCT { dwData: COPY_MAGIC, cbData: (payload.len() * 2) as u32, lpData: payload.as_ptr() as _ };
                    let _ = SendMessageW(other, WM_COPYDATA, Some(WPARAM(0)), Some(LPARAM(&cds as *const _ as isize)));
                    return;
                }
            }
        }
    }

    let data = data_dir();
    let _ = DATA.set(data.clone());
    let st: Value = std::fs::read_to_string(data.join("state.json")).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_else(|| json!({}));
    let st = if st.is_object() { st } else { json!({}) };
    let win = st["win"].as_array().cloned();
    let _ = STATE.set(Mutex::new(st));

    let (tx, rx) = mpsc::channel();
    let _ = TX.set(tx);
    RX.with(|r| *r.borrow_mut() = Some(rx));
    let pool = thumbs::Pool::new(3, |id, data| send(Msg::Thumb(id, data)));
    THUMBS.with(|t| *t.borrow_mut() = Some(pool));

    let hinst = unsafe { GetModuleHandleW(None).map(|h| HINSTANCE(h.0)).unwrap_or_default() };
    let icon = unsafe { LoadIconW(Some(hinst), PCWSTR(1 as _)).unwrap_or_default() };
    unsafe {
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            hIcon: icon,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: CLASS,
            ..Default::default()
        };
        RegisterClassExW(&wc);
    }

    let (mut x, mut y, mut wdt, mut hgt, mut maxed) = (CW_USEDEFAULT, CW_USEDEFAULT, 1180, 740, false);
    if let Some(v) = win {
        let v: Vec<i32> = v.iter().filter_map(|x| x.as_i64().map(|x| x as i32)).collect();
        if v.len() == 5 && v[2] - v[0] > 300 && v[3] - v[1] > 200 {
            (x, y, wdt, hgt, maxed) = (v[0], v[1], v[2] - v[0], v[3] - v[1], v[4] == 1);
        }
    }
    let test = test_mode();
    if secondary() {
        let wa = shell::work_area(HWND::default());
        if std::env::args().any(|a| a == "--right") {
            (x, y, wdt, hgt, maxed) = (wa.left + (wa.right - wa.left) / 2, wa.top, (wa.right - wa.left) / 2, wa.bottom - wa.top, false);
        } else if x != CW_USEDEFAULT {
            (x, y) = (x + 40, y + 40);
        }
    }
    if test {
        let sz = std::env::var("FILES_SIZE").unwrap_or_else(|_| "1180x740".into());
        let (a, b) = sz.split_once('x').unwrap_or(("1180", "740"));
        (wdt, hgt, maxed) = (a.parse().unwrap_or(1180), b.parse().unwrap_or(740), false);
    }
    let Ok(hw) = (unsafe { CreateWindowExW(WINDOW_EX_STYLE(0), CLASS, w!("Files"), WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN, x, y, wdt, hgt, None, None, Some(hinst), None) }) else {
        return;
    };
    MAIN_HWND.store(hw.0 as isize, Ordering::Relaxed);
    unsafe {
        let _ = SetWindowPos(hw, None, 0, 0, 0, 0, SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        let dark = BOOL(1);
        let _ = DwmSetWindowAttribute(hw, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const _ as _, 4);
        let border: u32 = 0x00262626;
        let _ = DwmSetWindowAttribute(hw, DWMWA_BORDER_COLOR, &border as *const _ as _, 4);
        let m = MARGINS { cxLeftWidth: 0, cxRightWidth: 0, cyTopHeight: 1, cyBottomHeight: 0 };
        let _ = DwmExtendFrameIntoClientArea(hw, &m);
        if test {
            // тестовый прогон: окно за экраном и без фокуса
            let _ = SetWindowPos(hw, Some(HWND_BOTTOM), -4000, 0, wdt, hgt, SWP_NOACTIVATE);
        }
        let _ = ShowWindow(hw, if test { SW_SHOWNOACTIVATE } else if maxed { SW_SHOWMAXIMIZED } else { SW_SHOW });
        let _ = UpdateWindow(hw);
    }

    let options = CoreWebView2EnvironmentOptions::default();
    unsafe {
        options.set_language("ru-RU".into());
        options.set_additional_browser_arguments("--disable-features=msSmartScreenProtection --enable-features=OverlayScrollbar".into());
    }
    let options: ICoreWebView2EnvironmentOptions = options.into();
    let env_handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(move |res, env| {
        if res.is_err() {
            unsafe {
                MessageBoxW(None, w!("Не найден WebView2 Runtime. Установите его с сайта Microsoft."), w!("Files"), MB_ICONERROR);
                PostQuitMessage(1);
            }
            return Ok(());
        }
        let Some(env) = env else { return Ok(()) };
        ENV.with(|e| *e.borrow_mut() = Some(env.clone()));
        create_view(&env, open.clone());
        Ok(())
    }));
    let udf = data.join("profile");
    unsafe {
        let _ = CreateCoreWebView2EnvironmentWithOptions(PCWSTR::null(), &h(&udf.to_string_lossy()), &options, &env_handler);
    }

    let mut msg = MSG::default();
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
