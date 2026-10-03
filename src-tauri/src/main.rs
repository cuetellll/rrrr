#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    fs,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};
use tauri::{
    image::Image,
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, RunEvent, State, WindowEvent, Wry,
};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};
use tauri_plugin_notification::NotificationExt;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

const PROXY_PORT: u16 = 12334;
const API_PORT: u16 = 12335;
/// v2.5: پورت SOCKS5 محلی Aether (مدل اتصال ۲)
const AETHER_PORT: u16 = 1819;

#[derive(Default)]
struct Core {
    child: Mutex<Option<Child>>,
    proxy_on: Mutex<bool>,
    /// v2.5: پروسه‌ی Aether (فقط وقتی مدل ۲ فعاله)
    aether: Mutex<Option<Child>>,
}

fn hide(cmd: &mut Command) {
    #[cfg(windows)]
    {
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let _ = cmd;
}

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

fn data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let d = app.path().app_data_dir().map_err(err)?;
    fs::create_dir_all(&d).map_err(err)?;
    Ok(d)
}

/// sing-box.exe رو کنار برنامه، پوشه bin یا resources پیدا می‌کنه
fn singbox_path(app: &AppHandle) -> Result<PathBuf, String> {
    let mut c: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            c.push(dir.join("sing-box.exe"));
            c.push(dir.join("bin").join("sing-box.exe"));
        }
    }
    if let Ok(r) = app.path().resource_dir() {
        c.push(r.join("sing-box.exe"));
    }
    c.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("bin").join("sing-box.exe"));
    c.into_iter()
        .find(|p| p.exists())
        .ok_or_else(|| "sing-box.exe not found (put it next to MahyarVPN.exe)".to_string())
}

fn tail(path: &PathBuf) -> String {
    let s = fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = s.lines().collect();
    let t = lines[lines.len().saturating_sub(12)..].join("\n");
    if t.trim().is_empty() { "sing-box exited unexpectedly".into() } else { t }
}

fn check_config(sb: &PathBuf, cfg: &PathBuf) -> Result<(), String> {
    let mut cmd = Command::new(sb);
    cmd.arg("check").arg("-c").arg(cfg).arg("--disable-color");
    hide(&mut cmd);
    let out = cmd.output().map_err(err)?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{}{}",
            String::from_utf8_lossy(&out.stderr),
            String::from_utf8_lossy(&out.stdout)
        ))
    }
}

fn spawn_core(dir: &PathBuf, sb: &PathBuf, cfg: &PathBuf, log: &PathBuf) -> Result<Child, String> {
    let f = fs::File::create(log).map_err(err)?;
    let f2 = f.try_clone().map_err(err)?;
    let mut cmd = Command::new(sb);
    cmd.arg("run")
        .arg("-c")
        .arg(cfg)
        .arg("-D")
        .arg(dir)
        .arg("--disable-color")
        .stdin(Stdio::null())
        .stdout(Stdio::from(f))
        .stderr(Stdio::from(f2));
    hide(&mut cmd);
    cmd.spawn().map_err(err)
}

fn kill_core(core: &Core) {
    if let Some(mut c) = core.child.lock().unwrap().take() {
        let _ = c.kill();
        let _ = c.wait();
    }
}

// ---------------- v2.5 · Aether (مدل اتصال ۲) ----------------
/// aether.exe رو کنار برنامه، پوشه‌ی aether/ یا resources پیدا می‌کنه
fn aether_path(app: &AppHandle) -> Result<PathBuf, String> {
    let mut c: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            c.push(dir.join("aether").join("aether.exe"));
            c.push(dir.join("aether.exe"));
        }
    }
    if let Ok(r) = app.path().resource_dir() {
        c.push(r.join("aether").join("aether.exe"));
        c.push(r.join("aether.exe"));
    }
    let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("bin");
    c.push(dev.join("aether").join("aether.exe"));
    c.push(dev.join("aether.exe"));
    c.into_iter()
        .find(|p| p.exists())
        .ok_or_else(|| "aether.exe not found (put it in the aether folder next to MahyarVPN.exe)".to_string())
}

fn kill_aether(core: &Core) {
    if let Some(mut c) = core.aether.lock().unwrap().take() {
        let _ = c.kill();
        let _ = c.wait();
    }
}

/// رنگ‌های ANSI ترمینال رو از لاگ پاک می‌کنه
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(ch) = it.next() {
        if ch == '\u{1b}' {
            if it.peek() == Some(&'[') {
                it.next();
                while let Some(&c) = it.peek() {
                    it.next();
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(ch);
    }
    out
}

fn port_open(port: u16) -> bool {
    let a: std::net::SocketAddr = ([127, 0, 0, 1], port).into();
    std::net::TcpStream::connect_timeout(&a, Duration::from_millis(300)).is_ok()
}

/// لاگ Aether رو هم توی فایل می‌نویسه هم خط‌به‌خط برای UI می‌فرسته
fn pump<R: std::io::Read + Send + 'static>(app: AppHandle, r: R, log: std::sync::Arc<Mutex<fs::File>>) {
    thread::spawn(move || {
        use std::io::{BufRead, BufReader, Write};
        for line in BufReader::new(r).lines().map_while(Result::ok) {
            let l = strip_ansi(&line);
            if let Ok(mut f) = log.lock() {
                let _ = writeln!(f, "{l}");
            }
            let t = l.trim();
            if !t.is_empty() {
                let _ = app.emit("aether-log", t.chars().take(160).collect::<String>());
            }
        }
    });
}

/// Aether رو بدون هیچ سؤالی (همه‌ی گزینه‌ها با فلگ) بالا میاره و صبر می‌کنه تا
/// SOCKS5 روی 127.0.0.1:1819 باز بشه. Aether پورت رو فقط وقتی باز می‌کنه که تونل
/// واقعاً دیتا رد کرده باشه، پس باز شدن پورت یعنی اتصال سالمه.
#[tauri::command]
async fn start_aether(app: AppHandle, protocol: String, scan: String, h2: bool, timeout_secs: Option<u64>) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let core = app.state::<Core>();
        kill_aether(&core);
        // اگه از قبل (مثلاً بعد از کرش) یه Aether دیگه پورت رو گرفته، ببندش
        if port_open(AETHER_PORT) {
            let mut k = Command::new("taskkill");
            k.args(["/F", "/IM", "aether.exe"]).stdout(Stdio::null()).stderr(Stdio::null());
            hide(&mut k);
            let _ = k.status();
            thread::sleep(Duration::from_millis(600));
        }
        let exe = aether_path(&app)?;
        // هویت WARP و آخرین گیت‌وی سالم اینجا می‌مونن تا هر بار ثبت‌نام تازه نشه.
        // مثل Aether-GUI فقط cwd رو ثابت نگه می‌داریم و خود Aether اسم فایل هویت رو انتخاب می‌کنه
        // (WireGuard: aether.toml، MASQUE: aether-masque.toml). پوشه‌ی جدید = هویت تمیز،
        // چون نسخه‌ی قبلی هر دو پروتکل رو توی یه فایل می‌ریخت و خرابش می‌کرد.
        let dir = data_dir(&app)?.join("aether-data");
        fs::create_dir_all(&dir).map_err(err)?;
        let log_path = dir.join("aether.log");
        let log = std::sync::Arc::new(Mutex::new(fs::File::create(&log_path).map_err(err)?));

        let proto = match protocol.as_str() { "wg" | "gool" => protocol.clone(), _ => "masque".to_string() };
        let scan = match scan.as_str() { "turbo" | "thorough" | "stealth" | "ironclad" => scan.clone(), _ => "balanced".to_string() };
        let noize = if proto == "masque" { "firewall" } else { "balanced" };

        let bind = format!("127.0.0.1:{AETHER_PORT}");
        let mut cmd = Command::new(&exe);
        // همون فلگ‌هایی که Aether-GUI می‌فرسته (مثلاً: --wg --balanced -4 --quick-reconnect --noize balanced)
        cmd.current_dir(&dir)
            .arg(format!("--{proto}"))
            .arg(format!("--{scan}"))
            .args(["-4", "--quick-reconnect", "--noize", noize, "--bind", bind.as_str()])
            // همیشه ست میشه (0 یا 1) تا Aether سؤال «MASQUE transport» رو نپرسه
            .env("AETHER_MASQUE_HTTP2", if proto == "masque" && h2 { "1" } else { "0" })
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if proto == "masque" && h2 {
            cmd.arg("--h2");
        }
        hide(&mut cmd);
        let mut child = cmd.spawn().map_err(err)?;
        if let Some(o) = child.stdout.take() { pump(app.clone(), o, log.clone()); }
        if let Some(e) = child.stderr.take() { pump(app.clone(), e, log.clone()); }
        *core.aether.lock().unwrap() = Some(child);

        // اسکن گیت‌وی ممکنه طول بکشه (ironclad کندترینه)
        let limit = Duration::from_secs(timeout_secs.unwrap_or(if scan == "ironclad" { 300 } else { 150 }));
        let start = Instant::now();
        loop {
            let exited: Option<bool> = {
                let mut g = core.aether.lock().unwrap();
                let st = g.as_mut().map(|c| matches!(c.try_wait(), Ok(Some(_))));
                if st == Some(true) {
                    *g = None;
                }
                st
            };
            match exited {
                None => return Err("cancelled".into()), // کاربر وسط کار قطع کرد
                Some(true) => {
                    thread::sleep(Duration::from_millis(250)); // بذار لاگ آخر نوشته بشه
                    return Err(tail(&log_path).replace("sing-box", "aether"));
                }
                Some(false) => {}
            }
            if port_open(AETHER_PORT) {
                return Ok(());
            }
            if start.elapsed() > limit {
                kill_aether(&core);
                return Err("Aether: no working gateway found (timeout)".into());
            }
            thread::sleep(Duration::from_millis(400));
        }
    })
    .await
    .map_err(err)?
}

// ---------------- System proxy (Windows registry) ----------------
#[cfg(windows)]
mod sysproxy {
    use std::ffi::c_void;
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};

    #[link(name = "wininet")]
    extern "system" {
        fn InternetSetOptionW(h: *mut c_void, opt: u32, buf: *mut c_void, len: u32) -> i32;
    }

    const KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Internet Settings";

    fn refresh() {
        unsafe {
            InternetSetOptionW(std::ptr::null_mut(), 39, std::ptr::null_mut(), 0); // SETTINGS_CHANGED
            InternetSetOptionW(std::ptr::null_mut(), 37, std::ptr::null_mut(), 0); // REFRESH
        }
    }

    pub fn set(enable: bool, port: u16) -> Result<(), String> {
        let (key, _) = RegKey::predef(HKEY_CURRENT_USER)
            .create_subkey(KEY)
            .map_err(|e| e.to_string())?;
        if enable {
            key.set_value("ProxyServer", &format!("127.0.0.1:{port}")).map_err(|e| e.to_string())?;
            key.set_value(
                "ProxyOverride",
                &"localhost;127.*;10.*;172.16.*;172.17.*;172.18.*;172.19.*;172.2*;172.30.*;172.31.*;192.168.*;<local>".to_string(),
            )
            .map_err(|e| e.to_string())?;
            key.set_value("ProxyEnable", &1u32).map_err(|e| e.to_string())?;
        } else {
            key.set_value("ProxyEnable", &0u32).map_err(|e| e.to_string())?;
        }
        refresh();
        Ok(())
    }

    /// اگه دفعه قبل برنامه کرش کرده و پروکسی روشن مونده، خاموشش کن
    pub fn cleanup_stale(port: u16) {
        if let Ok(key) = RegKey::predef(HKEY_CURRENT_USER).open_subkey(KEY) {
            let on: u32 = key.get_value("ProxyEnable").unwrap_or(0);
            let srv: String = key.get_value("ProxyServer").unwrap_or_default();
            if on == 1 && srv == format!("127.0.0.1:{port}") {
                let _ = set(false, port);
            }
        }
    }
}

#[cfg(not(windows))]
mod sysproxy {
    pub fn set(_e: bool, _p: u16) -> Result<(), String> { Ok(()) }
    pub fn cleanup_stale(_p: u16) {}
}

fn set_proxy(core: &Core, enable: bool, port: u16) -> Result<(), String> {
    let mut on = core.proxy_on.lock().unwrap();
    if !enable && !*on {
        return Ok(());
    }
    sysproxy::set(enable, port)?;
    *on = enable;
    Ok(())
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{:02X}", b),
        })
        .collect()
}

// ---------------- Commands ----------------
#[tauri::command]
async fn start_core(app: AppHandle, config: String, system_proxy: bool, port: u16) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let core = app.state::<Core>();
        kill_core(&core);
        let _ = set_proxy(&core, false, port);
        let dir = data_dir(&app)?;
        let cfg = dir.join("config.json");
        let log = dir.join("core.log");
        fs::write(&cfg, config).map_err(err)?;
        let sb = singbox_path(&app)?;
        check_config(&sb, &cfg)?;
        let mut child = spawn_core(&dir, &sb, &cfg, &log)?;
        thread::sleep(Duration::from_millis(1500));
        if let Ok(Some(_)) = child.try_wait() {
            return Err(tail(&log));
        }
        *core.child.lock().unwrap() = Some(child);
        if system_proxy {
            if let Err(e) = set_proxy(&core, true, port) {
                kill_core(&core);
                return Err(e);
            }
        }
        Ok(())
    })
    .await
    .map_err(err)?
}

#[tauri::command]
async fn stop_core(app: AppHandle) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let core = app.state::<Core>();
        kill_core(&core);
        kill_aether(&core);
        set_proxy(&core, false, PROXY_PORT)
    })
    .await
    .map_err(err)?
}

#[tauri::command]
fn core_running(core: State<'_, Core>) -> bool {
    let sb = {
        let mut g = core.child.lock().unwrap();
        match g.as_mut() {
            Some(c) => matches!(c.try_wait(), Ok(None)),
            None => false,
        }
    };
    // v2.5: اگه مدل ۲ فعاله، Aether هم باید زنده باشه
    let mut a = core.aether.lock().unwrap();
    let ae = match a.as_mut() {
        Some(c) => matches!(c.try_wait(), Ok(None)),
        None => true,
    };
    sb && ae
}

/// تست پینگ واقعی: یه sing-box موقت با همه سرورها بالا میاد و از طریق
/// Clash API برای هر سرور یه درخواست HTTP واقعی زده میشه
#[tauri::command]
async fn test_delays(
    app: AppHandle,
    config: String,
    count: usize,
    url: String,
    timeout: u32,
    concurrency: Option<usize>,
) -> Result<Vec<i64>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .map_err(err)?
            .port();
        let mut v: serde_json::Value = serde_json::from_str(&config).map_err(err)?;
        v["experimental"] = serde_json::json!({
            "clash_api": { "external_controller": format!("127.0.0.1:{port}") }
        });
        let dir = data_dir(&app)?;
        let cfg = dir.join("test.json");
        let log = dir.join("test.log");
        fs::write(&cfg, v.to_string()).map_err(err)?;
        let sb = singbox_path(&app)?;
        check_config(&sb, &cfg)?;
        let mut child = spawn_core(&dir, &sb, &cfg, &log)?;

        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_millis(timeout as u64 + 3000))
            .build();
        let base = format!("http://127.0.0.1:{port}");
        let start = Instant::now();
        let mut ready = false;
        while start.elapsed() < Duration::from_secs(8) {
            if let Ok(Some(_)) = child.try_wait() {
                break;
            }
            if agent.get(&format!("{base}/version")).call().is_ok() {
                ready = true;
                break;
            }
            thread::sleep(Duration::from_millis(150));
        }
        if !ready {
            let _ = child.kill();
            let _ = child.wait();
            return Err(tail(&log));
        }

        let enc = urlencode(&url);
        let par = concurrency.unwrap_or(16).clamp(1, 64);
        let mut results = vec![-1i64; count];
        let mut i0 = 0;
        while i0 < count {
            let end = (i0 + par).min(count);
            thread::scope(|s| {
                let handles: Vec<_> = (i0..end)
                    .map(|i| {
                        let agent = &agent;
                        let base = &base;
                        let enc = &enc;
                        s.spawn(move || -> i64 {
                            let u = format!("{base}/proxies/n{i}/delay?timeout={timeout}&url={enc}");
                            match agent.get(&u).call() {
                                Ok(resp) => resp
                                    .into_string()
                                    .ok()
                                    .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                                    .and_then(|j| j["delay"].as_i64())
                                    .filter(|d| *d > 0)
                                    .unwrap_or(-1),
                                Err(_) => -1,
                            }
                        })
                    })
                    .collect();
                for (k, h) in handles.into_iter().enumerate() {
                    results[i0 + k] = h.join().unwrap_or(-1);
                }
            });
            i0 = end;
            let _ = app.emit("delay-progress", serde_json::json!({ "done": end, "total": count }));
        }
        let _ = child.kill();
        let _ = child.wait();
        Ok(results)
    })
    .await
    .map_err(err)?
}

#[derive(serde::Deserialize)]
struct Target {
    host: String,
    port: u16,
}

/// مرحله‌ی اول اسکن هوشمند: اتصال TCP خام به هزاران سرور به‌صورت موازی.
/// خروجی: زمان اتصال (ms) یا -1. سرورهای مرده/فیلترشده همین‌جا حذف میشن
/// تا تست واقعی (که سنگین‌تره) فقط روی امیدوارکننده‌ها اجرا بشه.
#[tauri::command]
async fn tcp_ping(app: AppHandle, targets: Vec<Target>, timeout: u32, concurrency: usize) -> Result<Vec<i64>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
        use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
        let n = targets.len();
        if n == 0 {
            return Ok(Vec::new());
        }
        let results: Vec<AtomicI64> = (0..n).map(|_| AtomicI64::new(-1)).collect();
        let next = AtomicUsize::new(0);
        let done = AtomicUsize::new(0);
        let to = Duration::from_millis(timeout.max(200) as u64);
        let workers = concurrency.clamp(1, 256).min(n);
        let step = (n / 60).max(10);
        thread::scope(|s| {
            for _ in 0..workers {
                s.spawn(|| loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= n {
                        break;
                    }
                    let t = &targets[i];
                    let addr: Option<SocketAddr> = (t.host.as_str(), t.port).to_socket_addrs().ok().and_then(|it| {
                        let v: Vec<SocketAddr> = it.collect();
                        v.iter().find(|a| a.is_ipv4()).or_else(|| v.first()).cloned()
                    });
                    let ms = addr
                        .and_then(|a| {
                            let st = Instant::now();
                            TcpStream::connect_timeout(&a, to).ok().map(|_| (st.elapsed().as_millis() as i64).max(1))
                        })
                        .unwrap_or(-1);
                    results[i].store(ms, Ordering::Relaxed);
                    let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                    if d % step == 0 || d == n {
                        let _ = app.emit("scan-progress", serde_json::json!({ "phase": "tcp", "done": d, "total": n }));
                    }
                });
            }
        });
        Ok(results.into_iter().map(|a| a.into_inner()).collect())
    })
    .await
    .map_err(err)?
}

/// بدنه‌ی پاسخ تا ۴۰ مگ (into_string سقف ۱۰ مگ داره؛ ساب‌های عمومی ممکنه بزرگ‌تر باشن)
fn read_body(r: ureq::Response) -> std::io::Result<String> {
    use std::io::Read;
    let mut buf = String::new();
    r.into_reader().take(40 * 1024 * 1024).read_to_string(&mut buf)?;
    Ok(buf)
}

/// دریافت ساب‌اسکریپشن؛ آدرس‌ها به ترتیب امتحان میشن (اصلی، بعد میرور)
#[tauri::command]
async fn fetch_text(urls: Vec<String>) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(25)).build();
        let mut errs = Vec::new();
        for u in urls {
            // #fragment (اسم ساب) فرستاده نمیشه
            let u = u.split('#').next().unwrap_or("").to_string();
            match agent.get(&u).set("User-Agent", "MahyarVPN/1.0").call() {
                Ok(r) => match read_body(r) {
                    Ok(s) if !s.trim().is_empty() => return Ok(s),
                    Ok(_) => errs.push(format!("{u}: empty")),
                    Err(e) => errs.push(format!("{u}: {e}")),
                },
                Err(e) => errs.push(format!("{u}: {e}")),
            }
        }
        Err(errs.join("\n"))
    })
    .await
    .map_err(err)?
}

/// v2.4: باز کردن صفحه‌ی ریلیز توی مرورگر پیش‌فرض (فقط لینک‌های github.com)
#[tauri::command]
fn open_url(url: String) -> Result<(), String> {
    if !url.starts_with("https://github.com/") {
        return Err("blocked url".into());
    }
    Command::new("explorer.exe").arg(&url).spawn().map(|_| ()).map_err(err)
}

/// ترافیک کل (آپلود، دانلود) از Clash API هسته‌ی اصلی
#[tauri::command]
async fn core_stats() -> Result<(u64, u64), String> {
    tauri::async_runtime::spawn_blocking(|| {
        let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(2)).build();
        let s = agent
            .get(&format!("http://127.0.0.1:{API_PORT}/connections"))
            .call()
            .map_err(err)?
            .into_string()
            .map_err(err)?;
        let v: serde_json::Value = serde_json::from_str(&s).map_err(err)?;
        Ok((
            v["uploadTotal"].as_u64().unwrap_or(0),
            v["downloadTotal"].as_u64().unwrap_or(0),
        ))
    })
    .await
    .map_err(err)?
}

/// IP و کشور خروجی، از داخل خود تونل (چک واقعی اتصال)
#[tauri::command]
async fn get_ip(port: u16) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let proxy = ureq::Proxy::new(format!("http://127.0.0.1:{port}")).map_err(err)?;
        let agent = ureq::AgentBuilder::new()
            .proxy(proxy)
            .timeout(Duration::from_secs(8))
            .build();
        agent
            .get("http://ip-api.com/json/?fields=status,query,country,countryCode")
            .call()
            .map_err(err)?
            .into_string()
            .map_err(err)
    })
    .await
    .map_err(err)?
}

#[tauri::command]
fn is_admin() -> bool {
    #[cfg(windows)]
    {
        let mut c = Command::new("net");
        c.arg("session").stdout(Stdio::null()).stderr(Stdio::null());
        hide(&mut c);
        c.status().map(|s| s.success()).unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        true
    }
}

#[tauri::command]
fn relaunch_admin(app: AppHandle) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(err)?;
    let core = app.state::<Core>();
    kill_core(&core);
    kill_aether(&core);
    let _ = set_proxy(&core, false, PROXY_PORT);
    let script = format!(
        "Start-Process -FilePath '{}' -ArgumentList '--elevated' -Verb RunAs",
        exe.display().to_string().replace('\'', "''")
    );
    let mut c = Command::new("powershell");
    c.args(["-NoProfile", "-WindowStyle", "Hidden", "-Command"]).arg(script);
    hide(&mut c);
    let st = c.status().map_err(err)?;
    if st.success() {
        app.exit(0);
        Ok(())
    } else {
        Err("UAC was cancelled".into())
    }
}

// ---------------- System tray ----------------
/// آیتم‌های منوی راست‌کلیک tray که متنشون از سمت UI عوض میشه
struct TrayMenu {
    status: MenuItem<Wry>,
    toggle: MenuItem<Wry>,
    show: MenuItem<Wry>,
    quit: MenuItem<Wry>,
    icon_idle: Image<'static>,
    icon_on: Image<'static>,
}

fn show_main_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

/// یه نقطه‌ی رنگی گوشه‌ی پایین آیکون می‌کشه (سبز = وصل)
fn badge(base: &Image<'_>, rgb: [u8; 3]) -> Image<'static> {
    let (w, h) = (base.width(), base.height());
    let mut px = base.rgba().to_vec();
    let r = (w.min(h) as f32) * 0.24;
    let (cx, cy) = (w as f32 - r - 1.0, h as f32 - r - 1.0);
    for y in 0..h {
        for x in 0..w {
            let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
            let i = ((y * w + x) * 4) as usize;
            if d <= r {
                px[i] = rgb[0]; px[i + 1] = rgb[1]; px[i + 2] = rgb[2]; px[i + 3] = 255;
            } else if d <= r + 1.5 {
                px[i] = 4; px[i + 1] = 6; px[i + 2] = 15; px[i + 3] = 255; // حاشیه‌ی تیره دور نقطه
            }
        }
    }
    Image::new_owned(px, w, h)
}

fn setup_tray(app: &tauri::App) -> tauri::Result<()> {
    let h = app.handle();
    let status = MenuItem::with_id(h, "status", "○ MahyarVPN", false, None::<&str>)?;
    let toggle = MenuItem::with_id(h, "toggle", "Connect", true, None::<&str>)?;
    let show = MenuItem::with_id(h, "show", "Open MahyarVPN", true, None::<&str>)?;
    let quit = MenuItem::with_id(h, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(
        h,
        &[
            &status,
            &PredefinedMenuItem::separator(h)?,
            &toggle,
            &show,
            &PredefinedMenuItem::separator(h)?,
            &quit,
        ],
    )?;

    let base = app
        .default_window_icon()
        .cloned()
        .ok_or_else(|| tauri::Error::AssetNotFound("window icon".into()))?;
    let icon_idle = Image::new_owned(base.rgba().to_vec(), base.width(), base.height());
    let icon_on = badge(&base, [16, 245, 168]);

    TrayIconBuilder::with_id("main")
        .icon(icon_idle.clone())
        .tooltip("MahyarVPN")
        .menu(&menu)
        .menu_on_left_click(false) // چپ‌کلیک = باز کردن پنجره، راست‌کلیک = منو
        .on_menu_event(|app, e| match e.id.as_ref() {
            "toggle" => {
                let _ = app.emit("tray-toggle", ());
            }
            "show" => show_main_window(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, e| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
                show_main_window(tray.app_handle());
            }
        })
        .build(app)?;

    app.manage(TrayMenu { status, toggle, show, quit, icon_idle, icon_on });
    Ok(())
}

#[tauri::command]
fn set_tray(
    app: AppHandle,
    tray: State<'_, TrayMenu>,
    connected: bool,
    status: String,
    toggle: String,
    toggle_enabled: bool,
    show: String,
    quit: String,
    tooltip: String,
) -> Result<(), String> {
    tray.status.set_text(status).map_err(err)?;
    tray.toggle.set_text(toggle).map_err(err)?;
    tray.toggle.set_enabled(toggle_enabled).map_err(err)?;
    tray.show.set_text(show).map_err(err)?;
    tray.quit.set_text(quit).map_err(err)?;
    if let Some(t) = app.tray_by_id("main") {
        let _ = t.set_tooltip(Some(tooltip));
        let _ = t.set_icon(Some(if connected { tray.icon_on.clone() } else { tray.icon_idle.clone() }));
    }
    Ok(())
}

#[tauri::command]
fn hide_main(app: AppHandle) -> Result<(), String> {
    match app.get_webview_window("main") {
        Some(w) => w.hide().map_err(err),
        None => Err("no window".into()),
    }
}

#[tauri::command]
fn show_main(app: AppHandle) {
    show_main_window(&app);
}

// ---------------- Autostart & notifications ----------------
#[tauri::command]
fn get_autostart(app: AppHandle) -> bool {
    app.autolaunch().is_enabled().unwrap_or(false)
}

#[tauri::command]
fn set_autostart(app: AppHandle, on: bool) -> Result<(), String> {
    let al = app.autolaunch();
    if on { al.enable().map_err(err) } else { al.disable().map_err(err) }
}

/// فقط وقتی پنجره مخفیه (کنار ساعت) اعلان نشون میده
#[tauri::command]
fn notify(app: AppHandle, title: String, body: String) {
    let hidden = app
        .get_webview_window("main")
        .map(|w| !w.is_visible().unwrap_or(true))
        .unwrap_or(true);
    if hidden {
        let _ = app.notification().builder().title(title).body(body).show();
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let autostarted = args.iter().any(|a| a == "--autostart");
    // نسخه‌ی ادمینِ تازه باید صبر کنه تا نسخه‌ی قبلی کامل بسته بشه، وگرنه
    // قفل single-instance هنوز دست اونه و این یکی بی‌صدا بسته میشه
    if args.iter().any(|a| a == "--elevated") {
        thread::sleep(Duration::from_millis(1500));
    }

    tauri::Builder::default()
        // باید اولین پلاگین باشه: اجرای دوباره = آوردن همون پنجره‌ی قبلی جلو
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| show_main_window(app)))
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, Some(vec!["--autostart"])))
        .plugin(tauri_plugin_notification::init())
        .manage(Core::default())
        .setup(move |app| {
            sysproxy::cleanup_stale(PROXY_PORT);
            setup_tray(app)?;
            // پنجره مخفی ساخته میشه؛ اگه با ویندوز اجرا شده همون کنار ساعت می‌مونه
            if !autostarted {
                show_main_window(app.handle());
            }
            Ok(())
        })
        // دکمه‌ی X (یا Alt+F4) برنامه رو نمی‌بنده، میره کنار ساعت؛ خروج کامل از منوی tray
        .on_window_event(|w, e| {
            if let WindowEvent::CloseRequested { api, .. } = e {
                api.prevent_close();
                let _ = w.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            start_core,
            stop_core,
            start_aether,
            core_running,
            test_delays,
            tcp_ping,
            fetch_text,
            open_url,
            is_admin,
            relaunch_admin,
            core_stats,
            get_ip,
            set_tray,
            hide_main,
            show_main,
            get_autostart,
            set_autostart,
            notify
        ])
        .build(tauri::generate_context!())
        .expect("failed to start MahyarVPN")
        .run(|app, event| {
            if let RunEvent::Exit = event {
                let core = app.state::<Core>();
                kill_core(&core);
                kill_aether(&core);
                let _ = set_proxy(&core, false, PROXY_PORT);
            }
        });
}
