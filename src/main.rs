#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use muda::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    env,
    fs::{self, File, OpenOptions},
    io::{self, Read},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};
use tray_icon::{
    menu::MenuId, Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent,
};

// 引入 build.rs 在构建期生成的淡蓝色与灰色 45° 旋转正方形图标数据
include!(concat!(env!("OUT_DIR"), "/icons.rs"));

/// 跨线程发送给主线程托盘的指令
enum UserEvent {
    UpdateTray {
        tooltip: Option<String>,
        items: Option<Vec<CustomMenuSpec>>,
        gray: Option<bool>,
    },
    ExitApp,
}

#[derive(Clone)]
struct CustomMenuSpec {
    id: String,
    label: String,
    enabled: bool,
    action: Option<Value>, // 绑定点击该菜单项时自动执行的命令 (同 /api/v1/exec 格式)
}

fn get_blue_icon() -> Icon {
    Icon::from_rgba(BLUE_ICON_32.to_vec(), 32, 32).unwrap()
}

fn get_gray_icon() -> Icon {
    Icon::from_rgba(GRAY_ICON_32.to_vec(), 32, 32).unwrap()
}

fn main() {
    // 1. 启动自清理：静默删除上次自更新残留的 .old 文件
    if let Ok(exe) = env::current_exe() {
        let _ = fs::remove_file(exe.with_extension("old"));
    }

    // 2. Windows 下自动注册 cicada:// 自定义 URL 协议，支持网页一键唤醒
    #[cfg(target_os = "windows")]
    register_custom_protocol();

    // 3. 解析命令行参数：如果由 cicada:// 协议唤醒，忽略协议 URL，默认绑定 127.0.0.1:4140
    let mut addr = "127.0.0.1:4140".to_string();
    for arg in env::args().skip(1) {
        if arg.starts_with("cicada://") {
            continue;
        }
        if arg.contains(':') && !arg.contains("://") {
            addr = arg;
        }
    }

    let ui_base = env::var("WEB_UI_URL").unwrap_or_else(|_| format!("http://{}", addr));
    let launch_url = format!("{}/", ui_base.trim_end_matches('/'));

    // 4. 构建主线程原生事件循环 (接管系统托盘与菜单)
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let menu_actions: Arc<Mutex<HashMap<MenuId, Value>>> = Arc::new(Mutex::new(HashMap::new()));
    let tray_menu = build_native_menu(&[], &mut menu_actions.lock().unwrap());

    // 初始状态默认为淡蓝色 45° 旋转正方形图标
    let mut current_is_gray = false;
    let tray_icon_res = TrayIconBuilder::new()
        .with_menu(Box::new(tray_menu))
        .with_menu_on_left_click(false)
        .with_tooltip("Cicada: AI Proxy Controller")
        .with_icon(get_blue_icon())
        .build();

    let mut tray_icon = match tray_icon_res {
        Ok(t) => Some(t),
        Err(e) => {
            eprintln!("Warning: Failed to create system tray icon: {:?}", e);
            None
        }
    };

    // 若托盘图标创建失败(如无头环境/会话0)，创建隐藏窗口以维持事件循环不退出
    let _dummy_window = if tray_icon.is_none() {
        tao::window::WindowBuilder::new()
            .with_visible(false)
            .build(&event_loop)
            .ok()
    } else {
        None
    };

    // 5. 在后台线程启动 tiny_http 服务 (通讯无需 token)
    let http_addr = addr.clone();
    let http_proxy = proxy.clone();
    thread::spawn(move || {
        run_http_server(&http_addr, http_proxy);
    });

    let tray_event_receiver = TrayIconEvent::receiver();
    let menu_event_receiver = MenuEvent::receiver();

    // 4. 运行主线程事件循环
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        while let Ok(tray_event) = tray_event_receiver.try_recv() {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = tray_event
            {
                open_browser(&launch_url);
            }
        }

        while let Ok(menu_event) = menu_event_receiver.try_recv() {
            let id_str = menu_event.id.as_ref();
            if id_str == "__open_web__" {
                open_browser(&launch_url);
            } else if id_str == "__exit_app__" {
                std::process::exit(0);
            } else if let Some(action_val) = menu_actions.lock().unwrap().get(&menu_event.id).cloned() {
                thread::spawn(move || {
                    let _ = execute_command_from_json(&action_val);
                });
            }
        }

        if let tao::event::Event::UserEvent(user_evt) = event {
            match user_evt {
                UserEvent::UpdateTray { tooltip, items, gray } => {
                    if let Some(tray) = &mut tray_icon {
                        // 动态切换淡蓝色 / 灰色图标
                        if let Some(to_gray) = gray {
                            if to_gray != current_is_gray {
                                current_is_gray = to_gray;
                                let icon = if to_gray {
                                    get_gray_icon()
                                } else {
                                    get_blue_icon()
                                };
                                let _ = tray.set_icon(Some(icon));
                            }
                        }
                        if let Some(tip) = tooltip {
                            let _ = tray.set_tooltip(Some(tip));
                        }
                        if let Some(menu_items) = items {
                            let new_menu = build_native_menu(&menu_items, &mut menu_actions.lock().unwrap());
                            tray.set_menu(Some(Box::new(new_menu)));
                        }
                    }
                }
                UserEvent::ExitApp => {
                    std::process::exit(0);
                }
            }
        }
    });
}

// ============================================================================
// 原生托盘菜单动态构建器 & 浏览器唤起
// ============================================================================

fn build_native_menu(
    custom_items: &[CustomMenuSpec],
    action_map: &mut HashMap<MenuId, Value>,
) -> Menu {
    let menu = Menu::new();
    action_map.clear();

    for spec in custom_items {
        let item = MenuItem::with_id(spec.id.clone(), &spec.label, spec.enabled, None);
        if let Some(act) = &spec.action {
            action_map.insert(item.id().clone(), act.clone());
        }
        let _ = menu.append(&item);
    }

    if !custom_items.is_empty() {
        let _ = menu.append(&PredefinedMenuItem::separator());
    }

    let open_item = MenuItem::with_id("__open_web__", "🌐 打开 Cicada 控制台", true, None);
    let exit_item = MenuItem::with_id("__exit_app__", "❌ 退出 Cicada", true, None);
    let _ = menu.append(&open_item);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&exit_item);

    menu
}

fn open_browser(url: &str) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let escaped = url.replace('&', "^&");
        let _ = Command::new("cmd")
            .args(["/C", "start", "", &escaped])
            .creation_flags(0x08000000)
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("open").arg(url).spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = Command::new("xdg-open").arg(url).spawn();
    }
}

// ============================================================================
// 后台 HTTP 服务线程 (包含 /fs, /exec, /tray, /exit 四个接口)
// ============================================================================

fn find_local_file(filename: &str) -> Option<PathBuf> {
    if let Ok(cwd) = env::current_dir() {
        let p = cwd.join(filename);
        if p.is_file() {
            return Some(p);
        }
        let p_dist = cwd.join("dist").join(filename);
        if p_dist.is_file() {
            return Some(p_dist);
        }
    }
    if let Ok(exe) = env::current_exe() {
        if let Some(parent) = exe.parent() {
            let p = parent.join(filename);
            if p.is_file() {
                return Some(p);
            }
            let p_dist = parent.join("dist").join(filename);
            if p_dist.is_file() {
                return Some(p_dist);
            }
        }
    }
    None
}

#[cfg(target_os = "windows")]
fn register_custom_protocol() {
    if let Ok(exe_path) = env::current_exe() {
        let exe_str = exe_path.to_string_lossy().to_string();
        let cmd_str = format!("\"{}\" \"%1\"", exe_str);
        use std::os::windows::process::CommandExt;

        let reg_add = |args: &[&str]| {
            let _ = Command::new("reg")
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(0x08000000)
                .status();
        };

        reg_add(&["add", "HKCU\\Software\\Classes\\cicada", "/ve", "/d", "URL:Cicada Protocol", "/f"]);
        reg_add(&["add", "HKCU\\Software\\Classes\\cicada", "/v", "URL Protocol", "/d", "", "/f"]);
        reg_add(&["add", "HKCU\\Software\\Classes\\cicada\\DefaultIcon", "/ve", "/d", &format!("\"{}\",0", exe_str), "/f"]);
        reg_add(&["add", "HKCU\\Software\\Classes\\cicada\\shell\\open\\command", "/ve", "/d", &cmd_str, "/f"]);
    }
}

fn run_http_server(addr: &str, proxy: EventLoopProxy<UserEvent>) {
    let mut server = None;
    for _ in 0..20 {
        if let Ok(s) = Server::http(addr) {
            server = Some(s);
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    let server = match server {
        Some(s) => s,
        None => {
            eprintln!("Cicada instance already running or failed to bind {}", addr);
            std::process::exit(0);
        }
    };

    println!("======================================================");
    println!("Cicada listening on: http://{}", addr);
    println!("Web UI / Studio:     http://{}/", addr);
    println!("======================================================");

    for mut req in server.incoming_requests() {
        if req.method() == &Method::Options {
            let _ = req.respond(with_cors(Response::empty(204)));
            continue;
        }

        let raw_url = req.url().to_string();
        let (path, query) = raw_url.split_once('?').unwrap_or((&raw_url, ""));

        if path == "/ping" {
            let _ = reply_json(req, 200, json!({ "status": "ok", "name": "cicada", "version": "0.2.0" }));
            continue;
        }

        if path == "/cicada.exe" || path == "/zero-daemon.exe" {
            if let Some(exe_path) = find_local_file("cicada.exe")
                .or_else(|| find_local_file("zero-daemon.exe"))
                .or_else(|| env::current_exe().ok())
            {
                if let Ok(file) = File::open(&exe_path) {
                    let mut res = Response::from_file(file);
                    res.add_header(hdr("Content-Type", "application/octet-stream"));
                    res.add_header(hdr("Content-Disposition", "attachment; filename=\"cicada.exe\""));
                    let _ = req.respond(with_cors(res));
                    continue;
                }
            }
            let _ = req.respond(with_cors(Response::empty(404)));
            continue;
        }

        if path == "/" || path == "/ui" || path == "/index.html" {
            if let Some(index_path) = find_local_file("index.html") {
                if let Ok(file) = File::open(&index_path) {
                    let mut res = Response::from_file(file);
                    res.add_header(hdr("Content-Type", "text/html; charset=utf-8"));
                    let _ = req.respond(with_cors(res));
                    continue;
                }
            }
            let _ = reply_json(req, 200, json!({ "status": "ok", "name": "cicada", "version": "0.2.0" }));
            continue;
        }

        if path == "/favicon.ico" {
            if let Some(ico_path) = find_local_file("logo.png")
                .or_else(|| find_local_file("favicon.ico"))
                .or_else(|| find_local_file("app.ico"))
                .or_else(|| find_local_file("icon.png"))
            {
                if let Ok(file) = File::open(&ico_path) {
                    let mut res = Response::from_file(file);
                    let ct = if ico_path.extension().and_then(|e| e.to_str()) == Some("png") {
                        "image/png"
                    } else {
                        "image/x-icon"
                    };
                    res.add_header(hdr("Content-Type", ct));
                    let _ = req.respond(with_cors(res));
                    continue;
                }
            }
            let _ = req.respond(with_cors(Response::empty(404)));
            continue;
        }

        if path == "/logo.png" || path == "/icon.png" {
            if let Some(img_path) = find_local_file("logo.png")
                .or_else(|| find_local_file("icon.png"))
                .or_else(|| find_local_file("favicon.ico"))
                .or_else(|| find_local_file("app.ico"))
            {
                if let Ok(file) = File::open(&img_path) {
                    let mut res = Response::from_file(file);
                    let ct = if img_path.extension().and_then(|e| e.to_str()) == Some("ico") {
                        "image/x-icon"
                    } else {
                        "image/png"
                    };
                    res.add_header(hdr("Content-Type", ct));
                    let _ = req.respond(with_cors(res));
                    continue;
                }
            }
            let _ = req.respond(with_cors(Response::empty(404)));
            continue;
        }

        // 守护端与网页之间通讯无需 token，所有本地 API 请求直接通过
        let method = req.method().clone();
        let result = match (&method, path) {
            (Method::Get, "/api/v1/fs") => handle_fs_get(query),
            (Method::Put, "/api/v1/fs") => handle_fs_put(query, &mut req),
            (Method::Post, "/api/v1/exec") => handle_exec(&mut req),
            (Method::Post, "/api/v1/tray") | (Method::Post, "/api/v1/tray/state") | (Method::Post, "/api/v1/tray/color") => {
                handle_tray_update(query, &mut req, &proxy)
            }
            (Method::Get, "/api/v1/tray") => {
                handle_tray_update_get(query, &proxy)
            }
            (Method::Post, "/api/v1/exit") => handle_exit(&mut req, &proxy),
            _ => Err((404, "Endpoint not found".into())),
        };

        match result {
            Ok(ResponsePayload::Json(val)) => {
                let _ = reply_json(req, 200, val);
            }
            Ok(ResponsePayload::File(file)) => {
                let mut res = Response::from_file(file);
                res.add_header(hdr("Content-Type", "application/octet-stream"));
                let _ = req.respond(with_cors(res));
            }
            Err((code, msg)) => {
                let _ = reply_json(req, code, json!({ "error": msg }));
            }
        }
    }
}

enum ResponsePayload {
    Json(Value),
    File(File),
}

fn handle_fs_get(query: &str) -> Result<ResponsePayload, (u16, String)> {
    let rel_path = get_query_param(query, "path").ok_or((400, "Missing ?path=".into()))?;
    let safe_path = sanitize_path(&rel_path)?;

    if get_query_param(query, "ls").as_deref() == Some("1") {
        let mut entries = Vec::new();
        for entry in fs::read_dir(&safe_path).map_err(|e| (500, e.to_string()))? {
            if let Ok(e) = entry {
                entries.push(e.file_name().to_string_lossy().into_owned());
            }
        }
        return Ok(ResponsePayload::Json(json!({ "files": entries })));
    }

    let file = File::open(&safe_path).map_err(|e| (404, e.to_string()))?;
    Ok(ResponsePayload::File(file))
}

fn handle_fs_put(query: &str, req: &mut Request) -> Result<ResponsePayload, (u16, String)> {
    let rel_path = get_query_param(query, "path").ok_or((400, "Missing ?path=".into()))?;
    let safe_path = sanitize_path(&rel_path)?;

    if let Some(parent) = safe_path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    if get_query_param(query, "backup").as_deref() == Some("1") && safe_path.exists() {
        let _ = fs::copy(&safe_path, safe_path.with_extension("bak"));
    }

    let mut file = File::create(&safe_path).map_err(|e| (500, e.to_string()))?;
    let bytes_written = io::copy(&mut req.as_reader(), &mut file).map_err(|e| (400, e.to_string()))?;

    #[cfg(unix)]
    if get_query_param(query, "executable").as_deref() == Some("1") {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&safe_path, fs::Permissions::from_mode(0o755));
    }

    Ok(ResponsePayload::Json(json!({ "status": "saved", "bytes": bytes_written })))
}

fn handle_exec(req: &mut Request) -> Result<ResponsePayload, (u16, String)> {
    let body = read_json_body(req)?;
    let res = execute_command_from_json(&body)?;
    Ok(ResponsePayload::Json(res))
}

fn execute_command_from_json(body: &Value) -> Result<Value, (u16, String)> {
    let bin = body["bin"].as_str().ok_or((400, "Missing 'bin' field".into()))?;
    let detached = body["detached"].as_bool().unwrap_or(false);
    let log_file = body["log_file"].as_str().unwrap_or("core_runtime.log");

    let args: Vec<String> = body["args"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();

    let cwd = env::current_dir().map_err(|e| (500, e.to_string()))?;

    let exe_path = if !bin.contains(['/', '\\']) {
        let candidate = cwd.join(bin);
        let candidate_exe = cwd.join(format!("{}.exe", bin));
        if candidate.is_file() {
            candidate
        } else if candidate_exe.is_file() {
            candidate_exe
        } else {
            PathBuf::from(bin)
        }
    } else {
        PathBuf::from(bin)
    };

    let mut cmd = Command::new(exe_path);
    cmd.args(&args).current_dir(&cwd);

    if detached {
        let safe_log = sanitize_path(log_file)?;
        let out_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&safe_log)
            .map_err(|e| (500, e.to_string()))?;
        let err_file = out_file.try_clone().map_err(|e| (500, e.to_string()))?;
        cmd.stdin(Stdio::null()).stdout(Stdio::from(out_file)).stderr(Stdio::from(err_file));

        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x00000008 | 0x08000000);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }

        let child = cmd.spawn().map_err(|e| (500, format!("Spawn failed: {}", e)))?;
        Ok(json!({ "status": "detached", "pid": child.id() }))
    } else {
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000);
        }
        let output = cmd.output().map_err(|e| (500, format!("Exec failed: {}", e)))?;
        Ok(json!({
            "exit_code": output.status.code().unwrap_or(-1),
            "stdout": String::from_utf8_lossy(&output.stdout),
            "stderr": String::from_utf8_lossy(&output.stderr)
        }))
    }
}

/// 解析是否变灰：
/// 1. `gray: true / false`
/// 2. `color: "gray" / "blue"`
/// 3. `state: "gray" / "inactive" / "stopped" / "disabled"` vs `"blue" / "active" / "running"`
fn parse_gray_flag(body: &Value, query: &str) -> Option<bool> {
    if let Some(g) = body["gray"].as_bool() {
        return Some(g);
    }
    if let Some(c) = body["color"].as_str() {
        return Some(c.eq_ignore_ascii_case("gray") || c.eq_ignore_ascii_case("grey"));
    }
    if let Some(s) = body["state"].as_str() {
        let s_lower = s.to_ascii_lowercase();
        return Some(matches!(s_lower.as_str(), "gray" | "grey" | "inactive" | "stopped" | "disabled" | "off"));
    }
    if let Some(s) = body["status"].as_str() {
        let s_lower = s.to_ascii_lowercase();
        return Some(matches!(s_lower.as_str(), "gray" | "grey" | "inactive" | "stopped" | "disabled" | "off"));
    }
    if let Some(q_gray) = get_query_param(query, "gray") {
        return Some(q_gray == "1" || q_gray.eq_ignore_ascii_case("true"));
    }
    if let Some(q_color) = get_query_param(query, "color") {
        return Some(q_color.eq_ignore_ascii_case("gray") || q_color.eq_ignore_ascii_case("grey"));
    }
    None
}

fn handle_tray_update(
    query: &str,
    req: &mut Request,
    proxy: &EventLoopProxy<UserEvent>,
) -> Result<ResponsePayload, (u16, String)> {
    let body = read_json_body(req)?;
    let tooltip = body["tooltip"].as_str().map(String::from);
    let gray = parse_gray_flag(&body, query);

    let items = if let Some(arr) = body["items"].as_array() {
        let mut list = Vec::new();
        for item in arr {
            let id = item["id"].as_str().unwrap_or("custom").to_string();
            let label = item["label"].as_str().unwrap_or("Menu Item").to_string();
            let enabled = item["enabled"].as_bool().unwrap_or(true);
            let action = if item["action"].is_object() {
                Some(item["action"].clone())
            } else {
                None
            };
            list.push(CustomMenuSpec { id, label, enabled, action });
        }
        Some(list)
    } else {
        None
    };

    proxy
        .send_event(UserEvent::UpdateTray { tooltip, items, gray })
        .map_err(|_| (500, "Failed to notify tray thread".into()))?;

    Ok(ResponsePayload::Json(json!({
        "status": "tray_updated",
        "gray": gray
    })))
}

fn handle_tray_update_get(
    query: &str,
    proxy: &EventLoopProxy<UserEvent>,
) -> Result<ResponsePayload, (u16, String)> {
    let tooltip = get_query_param(query, "tooltip");
    let gray = parse_gray_flag(&json!({}), query);

    proxy
        .send_event(UserEvent::UpdateTray { tooltip, items: None, gray })
        .map_err(|_| (500, "Failed to notify tray thread".into()))?;

    Ok(ResponsePayload::Json(json!({
        "status": "tray_updated",
        "gray": gray
    })))
}

fn handle_exit(
    req: &mut Request,
    proxy: &EventLoopProxy<UserEvent>,
) -> Result<ResponsePayload, (u16, String)> {
    let body = read_json_body(req).unwrap_or_else(|_| json!({}));

    if let Some(new_bin_rel) = body["upgrade_from"].as_str() {
        let new_bin_path = sanitize_path(new_bin_rel)?;
        if !new_bin_path.exists() {
            return Err((400, "Upgrade binary file does not exist".into()));
        }

        let current_exe = env::current_exe().map_err(|e| (500, e.to_string()))?;
        let old_exe = current_exe.with_extension("old");
        let _ = fs::remove_file(&old_exe);

        fs::rename(&current_exe, &old_exe).map_err(|e| (500, format!("Rename self failed: {}", e)))?;
        if let Err(e) = fs::rename(&new_bin_path, &current_exe) {
            let _ = fs::rename(&old_exe, &current_exe);
            return Err((500, format!("Replace binary failed: {}", e)));
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&current_exe, fs::Permissions::from_mode(0o755));
        }

        let original_args: Vec<String> = env::args().skip(1).collect();
        let p = proxy.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(250));
            let mut cmd = Command::new(&current_exe);
            cmd.args(&original_args);
            let _ = cmd.spawn();
            let _ = p.send_event(UserEvent::ExitApp);
        });

        return Ok(ResponsePayload::Json(json!({ "status": "upgrading_and_restarting" })));
    }

    let p = proxy.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(200));
        let _ = p.send_event(UserEvent::ExitApp);
    });

    Ok(ResponsePayload::Json(json!({ "status": "exiting" })))
}

fn sanitize_path(rel: &str) -> Result<PathBuf, (u16, String)> {
    let path = Path::new(rel);
    for comp in path.components() {
        match comp {
            Component::Normal(_) | Component::CurDir => continue,
            _ => return Err((403, "Path traversal or forbidden path prefix detected".into())),
        }
    }
    let cwd = env::current_dir().map_err(|e| (500, e.to_string()))?;
    Ok(cwd.join(path))
}

fn read_json_body(req: &mut Request) -> Result<Value, (u16, String)> {
    let mut content = String::new();
    req.as_reader().read_to_string(&mut content).map_err(|e| (400, e.to_string()))?;
    if content.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&content).map_err(|e| (400, format!("Invalid JSON: {}", e)))
}

fn url_decode(s: &str) -> String {
    let mut bytes = Vec::with_capacity(s.len());
    let mut chars = s.as_bytes().iter().copied();
    while let Some(b) = chars.next() {
        if b == b'%' {
            if let (Some(h1), Some(h2)) = (chars.next(), chars.next()) {
                let hex_slice = [h1, h2];
                if let Ok(hex_str) = std::str::from_utf8(&hex_slice) {
                    if let Ok(val) = u8::from_str_radix(hex_str, 16) {
                        bytes.push(val);
                        continue;
                    }
                }
            }
        } else if b == b'+' {
            bytes.push(b' ');
            continue;
        }
        bytes.push(b);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn get_query_param(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key {
                return Some(url_decode(v));
            }
        }
    }
    None
}



fn hdr(name: &'static str, val: &'static str) -> Header {
    Header::from_bytes(name.as_bytes(), val.as_bytes()).unwrap()
}

fn with_cors<R: Read>(mut res: Response<R>) -> Response<R> {
    res.add_header(hdr("Access-Control-Allow-Origin", "*"));
    res.add_header(hdr("Access-Control-Allow-Methods", "GET, POST, PUT, OPTIONS"));
    res.add_header(hdr("Access-Control-Allow-Headers", "Authorization, Content-Type"));
    res.add_header(hdr("Access-Control-Max-Age", "86400"));
    res.add_header(hdr("Access-Control-Allow-Private-Network", "true"));
    res
}

fn reply_json(req: Request, status: u16, body: Value) -> std::io::Result<()> {
    let data = body.to_string().into_bytes();
    let mut res = Response::from_data(data).with_status_code(StatusCode(status));
    res.add_header(hdr("Content-Type", "application/json; charset=utf-8"));
    req.respond(with_cors(res))
}


