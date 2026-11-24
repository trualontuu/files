//! Миниатюры и значки через IShellItemImageFactory. Пул потоков, свежие запросы — первыми.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use windows::core::{w, Interface, HSTRING};
use windows::Win32::Foundation::GENERIC_READ;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::*;
use windows::Win32::UI::Shell::*;

pub struct Job {
    pub id: u64,
    pub path: String,
    pub px: i32,
    /// true — значок программы (exe/lnk), false — только настоящая миниатюра
    pub icon: bool,
    pub key: String,
}

type Done = Arc<dyn Fn(u64, Option<Arc<Vec<u8>>>) + Send + Sync>;

#[derive(Clone)]
pub struct Pool {
    q: Arc<(Mutex<Vec<Job>>, Condvar)>,
    cache: Arc<Mutex<HashMap<String, Option<Arc<Vec<u8>>>>>>,
    done: Done,
}

impl Pool {
    pub fn new(threads: usize, done: impl Fn(u64, Option<Arc<Vec<u8>>>) + Send + Sync + 'static) -> Pool {
        let pool = Pool { q: Arc::new((Mutex::new(vec![]), Condvar::new())), cache: Arc::new(Mutex::new(HashMap::new())), done: Arc::new(done) };
        for _ in 0..threads {
            let p = pool.clone();
            std::thread::spawn(move || p.worker());
        }
        pool
    }

    pub fn push(&self, job: Job) {
        if let Some(hit) = self.cache.lock().unwrap().get(&job.key).cloned() {
            (self.done)(job.id, hit);
            return;
        }
        let (m, cv) = &*self.q;
        let mut q = m.lock().unwrap();
        // очередь не растёт бесконечно при быстрой прокрутке: самые старые отбрасываем
        if q.len() > 400 {
            let old: Vec<Job> = q.drain(..100).collect();
            drop(q);
            for j in old {
                (self.done)(j.id, None);
            }
            q = m.lock().unwrap();
        }
        q.push(job);
        cv.notify_one();
    }

    fn worker(&self) {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        loop {
            let job = {
                let (m, cv) = &*self.q;
                let mut q = m.lock().unwrap();
                while q.is_empty() {
                    q = cv.wait(q).unwrap();
                }
                q.pop().unwrap()
            };
            let r = make(&job.path, job.px, job.icon).map(Arc::new);
            {
                let mut c = self.cache.lock().unwrap();
                if c.len() > 4000 {
                    c.clear();
                }
                c.insert(job.key.clone(), r.clone());
            }
            (self.done)(job.id, r);
        }
    }
}

/// Картинки, которые декодируем сами через WIC (не зависим от системного кэша эскизов).
const WIC_EXT: &[&str] = &["jpg", "jpeg", "jfif", "png", "gif", "bmp", "webp", "tif", "tiff", "ico", "heic", "heif", "avif", "dng", "jxr", "wdp"];

fn make(path: &str, px: i32, icon: bool) -> Option<Vec<u8>> {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    if !icon && WIC_EXT.contains(&ext.as_str()) {
        if let Some(b) = wic(path, px as u32) {
            return Some(b);
        }
    }
    shell(path, px, icon)
}

thread_local! {
    static WIC: Option<IWICImagingFactory> = unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok() };
}

fn wic(path: &str, px: u32) -> Option<Vec<u8>> {
    WIC.with(|f| unsafe {
        let f = f.as_ref()?;
        let dec = f.CreateDecoderFromFilename(&HSTRING::from(path), None, GENERIC_READ, WICDecodeMetadataCacheOnDemand).ok()?;
        let frame = dec.GetFrame(0).ok()?;
        let (mut w, mut h) = (0u32, 0u32);
        frame.GetSize(&mut w, &mut h).ok()?;
        if w == 0 || h == 0 {
            return None;
        }
        // поворот по EXIF (фото с телефона)
        let orient = frame
            .GetMetadataQueryReader()
            .ok()
            .and_then(|q| {
                let mut v = windows::Win32::System::Com::StructuredStorage::PROPVARIANT::default();
                q.GetMetadataByName(w!("/app1/ifd/{ushort=274}"), &mut v).ok()?;
                u16::try_from(&v).ok()
            })
            .unwrap_or(1);
        let k = (px as f64 / w.max(h) as f64).min(1.0);
        let (tw, th) = (((w as f64 * k).round() as u32).max(1), ((h as f64 * k).round() as u32).max(1));
        let mut src: IWICBitmapSource = frame.cast().ok()?;
        if k < 1.0 {
            let sc = f.CreateBitmapScaler().ok()?;
            sc.Initialize(&src, tw, th, WICBitmapInterpolationModeHighQualityCubic).ok()?;
            src = sc.cast().ok()?;
        }
        let tr = match orient {
            2 => Some(WICBitmapTransformFlipHorizontal),
            3 => Some(WICBitmapTransformRotate180),
            4 => Some(WICBitmapTransformFlipVertical),
            5 => Some(WICBitmapTransformOptions(WICBitmapTransformRotate90.0 | WICBitmapTransformFlipHorizontal.0)),
            6 => Some(WICBitmapTransformRotate90),
            7 => Some(WICBitmapTransformOptions(WICBitmapTransformRotate270.0 | WICBitmapTransformFlipHorizontal.0)),
            8 => Some(WICBitmapTransformRotate270),
            _ => None,
        };
        if let Some(tr) = tr {
            let fr = f.CreateBitmapFlipRotator().ok()?;
            fr.Initialize(&src, tr).ok()?;
            src = fr.cast().ok()?;
        }
        let cv = f.CreateFormatConverter().ok()?;
        cv.Initialize(&src, &GUID_WICPixelFormat32bppRGBA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom).ok()?;
        let (mut ow, mut oh) = (0u32, 0u32);
        cv.GetSize(&mut ow, &mut oh).ok()?;
        let mut buf = vec![0u8; (ow * oh * 4) as usize];
        cv.CopyPixels(std::ptr::null(), ow * 4, &mut buf).ok()?;
        encode(&buf, ow, oh)
    })
}

fn encode(px: &[u8], w: u32, h: u32) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(px.len() / 3);
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut wr = enc.write_header().ok()?;
        wr.write_image_data(px).ok()?;
    }
    Some(out)
}

fn shell(path: &str, px: i32, icon: bool) -> Option<Vec<u8>> {
    unsafe {
        let f: IShellItemImageFactory = match SHCreateItemFromParsingName(&HSTRING::from(path), None) {
            Ok(f) => f,
            Err(e) => { dbg(&format!("item {path}: {e:?}")); return None; }
        };
        let flags = if icon { SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK } else { SIIGBF_THUMBNAILONLY | SIIGBF_RESIZETOFIT };
        let hbm = match f.GetImage(SIZE { cx: px, cy: px }, flags) {
            Ok(h) => h,
            Err(e) => { dbg(&format!("image {path} {px}: {e:?}")); return None; }
        };
        let r = to_png(hbm);
        let _ = DeleteObject(hbm.into());
        r
    }
}

unsafe fn to_png(hbm: HBITMAP) -> Option<Vec<u8>> {
    unsafe {
        let mut bm = BITMAP::default();
        if GetObjectW(hbm.into(), std::mem::size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as _)) == 0 {
            return None;
        }
        let (w, h) = (bm.bmWidth, bm.bmHeight.abs());
        if w <= 0 || h <= 0 {
            return None;
        }
        let mut bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut px = vec![0u8; (w * h * 4) as usize];
        let dc = GetDC(None);
        let n = GetDIBits(dc, hbm, 0, h as u32, Some(px.as_mut_ptr() as _), &mut bi, DIB_RGB_COLORS);
        ReleaseDC(None, dc);
        if n == 0 {
            return None;
        }
        // у JPEG-миниатюр альфа нулевая — значит картинка непрозрачная
        let no_alpha = px.chunks_exact(4).all(|c| c[3] == 0);
        for c in px.chunks_exact_mut(4) {
            let (b, g, r, a) = (c[0], c[1], c[2], c[3]);
            if no_alpha {
                c[0] = r;
                c[1] = g;
                c[2] = b;
                c[3] = 255;
            } else if a == 0 {
                c.copy_from_slice(&[0, 0, 0, 0]);
            } else {
                // из premultiplied в обычную альфу
                let un = |v: u8| ((v as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8;
                c[0] = un(r);
                c[1] = un(g);
                c[2] = un(b);
            }
        }
        encode(&px, w as u32, h as u32)
    }
}

fn dbg(s: &str) {
    if std::env::var("FILES_TEST").is_ok() {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(std::env::temp_dir().join("files-thumbdbg.txt")) {
            let _ = writeln!(f, "{s}");
        }
    }
}
