//! 一个只监听本机的极简 HTTP 服务 —— 让界面上的路径可以点开。
//!
//! # 为什么需要它
//!
//! 界面是 `file://` 打开的静态页面，而**浏览器不允许页面直接打开本地文件夹**
//! （导航到 `file://` 目录会被拦，也拿不到资源管理器）。所以"点路径 → 跳过去看"
//! 这件事必须有个本地进程代劳。
//!
//! # 设计取舍
//!
//! 1. **只用 `std`，不引框架。** 需求只有一个动作（打开/定位文件夹），
//!    为此引一个 HTTP 库会显著放大二进制与攻击面。
//!
//! 2. **只做只读动作。** 服务永远不删、不改、不移动任何东西 ——
//!    它能做的只有"打开资源管理器"和"在父目录中定位"。即使被滥用，
//!    危害上限也只是"弹出一个文件夹窗口"。
//!
//! 3. **三层限制，缺一不可**（这是个能被网页调用的本地端口，必须当成
//!    不可信输入来防）：
//!    - **随机令牌**：32 字节随机十六进制。页面带 `?t=` 才受理。
//!      恶意网页读不到本地 HTML，所以拿不到令牌。
//!    - **路径必须在扫描根内**：规范化后校验前缀，挡住 `..` 逃逸。
//!      即使令牌泄露，也无法用它去翻系统目录。
//!    - **校验 Host 头**：只接受 `127.0.0.1:端口` / `localhost:端口`，
//!      防 DNS rebinding（把域名解析到 127.0.0.1 来绕过同源限制）。
//!
//! 4. **空闲自动退出。** 用户关掉浏览器之后不该留一个孤儿进程 ——
//!    超过 [`IDLE_TIMEOUT`] 没有请求就自己结束。
//!
//! 5. **绑定 127.0.0.1，绝不绑 0.0.0.0。** 局域网里别的机器连不上。

use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 无请求多久后自动退出。
///
/// 10 分钟：足够用户慢慢看完一份清单、来回点几十次路径；
/// 又不至于在他关掉浏览器后长期留一个进程。
///
/// 可用环境变量 `DD_SERVE_IDLE_SECS` 覆盖 —— 既方便测试这段逻辑，
/// 也方便用户按自己的节奏调整（设为 0 表示不自动退出）。
pub fn idle_timeout() -> Option<Duration> {
    if let Ok(v) = std::env::var("DD_SERVE_IDLE_SECS") {
        if let Ok(secs) = v.trim().parse::<u64>() {
            return if secs == 0 {
                None // 不自动退出
            } else {
                Some(Duration::from_secs(secs))
            };
        }
    }
    Some(Duration::from_secs(10 * 60))
}

/// 默认空闲时长，用于提示文案。
pub const DEFAULT_IDLE_MINUTES: u64 = 10;

/// 请求头的字节上限。HTTP 头本来就该很短；给足余量防畸形请求。
const MAX_HEAD_BYTES: usize = 16 * 1024;

/// 同时处理的连接上限。正常使用只需要 1~2 个；给点余量即可，
/// 避免被大量并发连接拖住。
const MAX_INFLIGHT: usize = 8;

pub struct Server {
    listener: TcpListener,
    /// 随机令牌；页面带上它才受理
    token: String,
    /// 允许操作的路径范围（扫描根）
    root: PathBuf,
    /// 当前在处理的连接数
    inflight: Arc<AtomicUsize>,
}

impl Server {
    /// 绑定本机随机端口并生成令牌。
    ///
    /// **必须在生成 HTML 之前调用** —— 页面里要写进端口和令牌。
    pub fn bind(root: &Path) -> std::io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        // 非阻塞：主循环要能定期醒来检查空闲超时
        listener.set_nonblocking(true)?;

        Ok(Self {
            listener,
            token: new_token(),
            root: root.to_path_buf(),
            inflight: Arc::new(AtomicUsize::new(0)),
        })
    }

    pub fn port(&self) -> u16 {
        self.listener
            .local_addr()
            .map(|a| a.port())
            .unwrap_or(0)
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port())
    }

    /// 进入服务循环，直到空闲超时。返回退出的原因，供调用方提示用户。
    pub fn run(self) -> String {
        let mut last_hit = Instant::now();
        let idle = idle_timeout();

        loop {
            match self.listener.accept() {
                Ok((stream, _addr)) => {
                    last_hit = Instant::now();
                    self.handle(stream);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if let Some(d) = idle {
                        if last_hit.elapsed() >= d {
                            return format!("已空闲 {} 秒，服务自动停止。", d.as_secs());
                        }
                    }
                    std::thread::sleep(Duration::from_millis(120));
                }
                Err(e) => {
                    // 单次 accept 失败不该让服务整体挂掉
                    eprintln!("  ⚠ 连接异常（已忽略）: {e}");
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
    }

    fn handle(&self, stream: TcpStream) {
        // 并发上限：超了就直接关掉这个连接，不做任何事
        if self.inflight.load(Ordering::Relaxed) >= MAX_INFLIGHT {
            let _ = respond(&stream, 503, "busy");
            return;
        }
        self.inflight.fetch_add(1, Ordering::Relaxed);

        let token = self.token.clone();
        let root = self.root.clone();
        let inflight = Arc::clone(&self.inflight);

        // 每连接一个线程：请求极少，用不着线程池
        std::thread::spawn(move || {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
            if let Err(msg) = serve_one(stream, &token, &root) {
                eprintln!("  ⚠ {msg}");
            }
            inflight.fetch_sub(1, Ordering::Relaxed);
        });
    }
}

/// 处理一个连接。返回 Err 只用于日志，不影响服务继续运行。
fn serve_one(stream: TcpStream, token: &str, root: &Path) -> Result<(), String> {
    // ---- 1. 读请求头 ----
    //
    // **逐行读到空行为止**，不能用 `read_to_string` —— 客户端（浏览器）
    // 发完头之后不会关连接，一直读下去会卡到超时，每次点击都白等 5 秒。
    let mut reader = BufReader::new(
        stream
            .try_clone()
            .map_err(|e| format!("clone 失败: {e}"))?,
    );

    let mut consumed = 0usize;
    let mut request_line = String::new();
    consumed += reader
        .read_line(&mut request_line)
        .map_err(|e| format!("读请求行失败: {e}"))?;

    let mut host = String::new();
    loop {
        if consumed > MAX_HEAD_BYTES {
            let _ = respond(&stream, 400, "header too large");
            return Ok(());
        }
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .map_err(|e| format!("读请求头失败: {e}"))?;
        if n == 0 {
            break; // 对端关了
        }
        consumed += n;
        if line == "\r\n" || line == "\n" {
            break; // 头结束
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.eq_ignore_ascii_case("host") {
                host = v.trim().to_ascii_lowercase();
            }
        }
    }

    let request_line = request_line.trim_end();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");

    if method != "GET" {
        let _ = respond(&stream, 405, "only GET");
        return Ok(());
    }

    // ---- 2. 校验 Host 头（防 DNS rebinding） ----
    //
    // 恶意网页可以把某个域名解析到 127.0.0.1，让浏览器认为请求是"同源"的。
    // 但那种请求的 Host 会是攻击者的域名。所以只接受本机形式。
    if !(host.starts_with("127.0.0.1:") || host.starts_with("localhost:")) {
        let _ = respond(&stream, 403, "bad host");
        return Ok(());
    }

    // ---- 3. 解析 query ----
    let Some(qpos) = target.find('?') else {
        let _ = respond(&stream, 404, "no query");
        return Ok(());
    };
    let path = &target[..qpos];
    let query = &target[qpos + 1..];

    let mut token_got = String::new();
    let mut action = String::new();
    let mut target_path = String::new();
    for kv in query.split('&') {
        let (k, v) = match kv.split_once('=') {
            Some((k, v)) => (k, v),
            None => continue,
        };
        let v = url_decode(v);
        match k {
            "t" => token_got = v,
            "a" => action = v,
            "p" => target_path = v,
            _ => {}
        }
    }

    // ---- 4. 令牌必须匹配 ----
    if !constant_eq(&token_got, token) {
        let _ = respond(&stream, 403, "bad token");
        return Ok(());
    }

    if path != "/act" {
        let _ = respond(&stream, 404, "unknown path");
        return Ok(());
    }

    // 健康检查放在路径校验**之前** —— 它本来就不针对任何路径。
    // 前端用它判断服务是否可达（见 PIXEL_GIF 处的说明）。
    if action == "ping" {
        let _ = respond_image(&stream, PIXEL_GIF);
        return Ok(());
    }

    // ---- 5. 路径必须真实存在、且落在扫描根内 ----
    let Some(real) = resolve_within(root, &target_path) else {
        let _ = respond(&stream, 403, "out of scope");
        return Ok(());
    };

    // ---- 6. 执行（只有这两个动作，都是只读的） ----
    //
    // 用 spawn 不等它：资源管理器的退出码不可靠（成功也常返回 1），
    // 等它还会拖住响应。但 **spawn 本身失败必须暴露** —— 否则界面上
    // 点了没反应，用户无从判断是服务没收到还是打开失败。
    match action.as_str() {
        "open" | "reveal" => {
            let p = real.to_string_lossy().to_string();
            let spawned = if action == "reveal" {
                std::process::Command::new("explorer.exe")
                    .arg(format!("/select,{p}"))
                    .spawn()
            } else {
                std::process::Command::new("explorer.exe").arg(&p).spawn()
            };

            match spawned {
                Ok(_) => {
                    let _ = respond(&stream, 204, "");
                }
                Err(e) => {
                    eprintln!("  ⚠ 无法启动资源管理器: {e}");
                    let _ = respond(&stream, 500, "cannot spawn explorer");
                }
            }
        }
        _ => {
            let _ = respond(&stream, 400, "unknown action");
        }
    }

    Ok(())
}

/// 1×1 全透明 GIF（43 字节）。
///
/// 用 GIF 而不是 PNG：更小，且这串字节是长期稳定的经典值，不需要现算 CRC。
const PIXEL_GIF: &[u8] = &[
    0x47, 0x49, 0x46, 0x38, 0x39, 0x61, // "GIF89a"
    0x01, 0x00, 0x01, 0x00, 0x80, 0x00, 0x00, // 逻辑屏幕：1×1
    0x00, 0x00, 0x00, 0xFF, 0xFF, 0xFF, // 调色板
    0x21, 0xF9, 0x04, 0x01, 0x00, 0x00, 0x00, 0x00, // 图形控制扩展
    0x2C, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, // 图像描述符
    0x02, 0x02, 0x44, 0x01, 0x00, // 图像数据
    0x3B, // 结束符
];

fn respond_image(s: &TcpStream, data: &[u8]) -> std::io::Result<()> {
    let mut w = s;
    let head = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: image/gif\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\
         \r\n",
        data.len()
    );
    w.write_all(head.as_bytes())?;
    w.write_all(data)?;
    w.flush()
}

/// 把用户给的路径解析成真实路径，并确认它没跑出 `root` 之外。
///
/// 关键点：**先 canonicalize 再比前缀**。直接比较字符串的话，
/// `C:\root\..\Windows` 这种能通过检查。
fn resolve_within(root: &Path, raw: &str) -> Option<PathBuf> {
    if raw.is_empty() {
        return None;
    }

    // 浏览器传来的可能带引号（用户从别处复制时容易带上）
    let cleaned = raw.trim().trim_matches('"');
    let candidate = PathBuf::from(cleaned);

    // 必须存在 —— 这一条同时挡掉了"猜路径探测"和大部分畸形输入
    let real = candidate.canonicalize().ok()?;

    // 根也要规范化，否则盘符根或含 8.3 短名的路径比不出来
    let root_real = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());

    let r = strip_verbatim(&root_real);
    let t = strip_verbatim(&real);
    if !path_starts_with(&t, &r) {
        return None;
    }
    Some(t)
}

/// Windows 的 canonicalize 会给出 `\\?\C:\...` 形式，比较前要去掉。
fn strip_verbatim(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    PathBuf::from(s)
}

/// 路径前缀判断，Windows 下大小写不敏感。
///
/// 必须按"路径分量"比较，不能按字符串 —— 否则 `C:\root2` 会被当成
/// `C:\root` 的子路径。
fn path_starts_with(p: &Path, base: &Path) -> bool {
    let pc: Vec<String> = p
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
        .collect();
    let bc: Vec<String> = base
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
        .collect();
    if bc.len() > pc.len() {
        return false;
    }
    bc.iter().zip(pc.iter()).all(|(b, x)| b == x)
}

/// 定长比较，不因首个不同字节就返回（减少时序信息）。
fn constant_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.bytes().zip(b.bytes()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// 32 字节随机令牌，十六进制。
///
/// 用系统熵源；取不到就退回"时间 + 地址"混合（仍是不可预测的，
/// 只是不如前者强）。
fn new_token() -> String {
    let mut buf = [0u8; 32];
    let ok = read_system_random(&mut buf);
    if !ok {
        // 退路：时间戳 + 本次进程里若干地址的混合
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let seed = now
            ^ (buf.as_ptr() as u128)
            ^ (&buf as *const _ as u128).rotate_left(17);
        for (i, b) in buf.iter_mut().enumerate() {
            *b = ((seed >> ((i % 16) * 8)) as u8) ^ (i as u8).wrapping_mul(31);
        }
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(windows)]
fn read_system_random(buf: &mut [u8]) -> bool {
    // BCryptGenRandom(BCRYPT_USE_SYSTEM_PREFERRED_RNG)
    const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x0000_0002;
    #[link(name = "bcrypt")]
    extern "system" {
        fn BCryptGenRandom(
            algo: *mut core::ffi::c_void,
            buffer: *mut u8,
            len: u32,
            flags: u32,
        ) -> i32;
    }
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            buf.as_mut_ptr(),
            buf.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    status == 0
}

#[cfg(not(windows))]
fn read_system_random(buf: &mut [u8]) -> bool {
    // 非 Windows 平台本项目暂不使用；返回 false 让调用方走退路。
    let _ = buf;
    false
}

/// 百分号解码。前端用 `encodeURIComponent`，所以不会出现裸 `+`，
/// 这里也就不把 `+` 当空格（路径里可能有真的 `+`）。
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            match (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h * 16 + l);
                    i += 3;
                    continue;
                }
                _ => {}
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn respond(mut s: &TcpStream, code: u16, body: &str) -> std::io::Result<()> {
    let reason = match code {
        204 => "No Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "OK",
    };
    // 刻意不加 `Access-Control-Allow-Origin`：前端用 <img> 信标发请求，
    // 不需要读响应；不开放 CORS 就少一扇门。
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\
         \r\n",
        body.len()
    );
    s.write_all(head.as_bytes())?;
    if !body.is_empty() {
        s.write_all(body.as_bytes())?;
    }
    s.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------- 路径范围

    #[test]
    fn path_prefix_must_respect_component_boundary() {
        // 按字符串比前缀会出错：`C:\root2` 以 `C:\root` 开头，
        // 但它不是 root 的子路径。必须按路径分量比。
        assert!(path_starts_with(
            Path::new(r"C:\root\sub\file"),
            Path::new(r"C:\root")
        ));
        assert!(path_starts_with(
            Path::new(r"C:\root"),
            Path::new(r"C:\root")
        ));
        assert!(
            !path_starts_with(Path::new(r"C:\root2\file"), Path::new(r"C:\root")),
            "名称前缀相近的兄弟目录不该被当成子路径"
        );
        assert!(
            !path_starts_with(Path::new(r"C:\other"), Path::new(r"C:\root")),
            "完全不同的一支"
        );
    }

    #[test]
    fn path_prefix_is_case_insensitive_on_windows() {
        // Windows 路径大小写不敏感，用户从别处复制的路径大小写常不一致
        assert!(path_starts_with(
            Path::new(r"c:\ROOT\Sub"),
            Path::new(r"C:\root")
        ));
    }

    #[test]
    fn resolve_rejects_parent_traversal() {
        // 用 .. 逃出扫描根 —— 这是最典型的越权尝试
        let root = std::env::temp_dir();
        let escaped = format!(r"{}\..\Windows", root.display());
        assert!(
            resolve_within(&root, &escaped).is_none(),
            "`..` 逃逸必须被拒绝"
        );
    }

    #[test]
    fn resolve_rejects_outside_paths() {
        let root = std::env::temp_dir();
        assert!(resolve_within(&root, r"C:\Windows").is_none());
        assert!(resolve_within(&root, r"C:\Windows\System32").is_none());
    }

    #[test]
    fn resolve_rejects_nonexistent() {
        // 不存在的路径一律拒绝：既挡住"猜路径探测"，也避免把
        // 畸形输入交给资源管理器
        let root = std::env::temp_dir();
        assert!(resolve_within(&root, r"C:\no-such-path-diskdoctor-xyz").is_none());
    }

    #[test]
    fn resolve_accepts_inside_path_and_strips_quotes() {
        let root = std::env::temp_dir();
        let inside = std::env::temp_dir();
        let got = resolve_within(&root, &inside.to_string_lossy());
        assert!(got.is_some(), "扫描根自身应当被接受");

        // 用户从别处复制路径时常带上引号
        let quoted = format!("\"{}\"", inside.display());
        assert!(
            resolve_within(&root, &quoted).is_some(),
            "两侧引号应被清理"
        );
    }

    #[test]
    fn resolve_rejects_empty() {
        let root = std::env::temp_dir();
        assert!(resolve_within(&root, "").is_none());
        assert!(resolve_within(&root, "   ").is_none());
    }

    // ---------------------------------------------------------- URL 解码

    #[test]
    fn url_decode_handles_utf8_and_specials() {
        // 前端用 encodeURIComponent，中文会变成 UTF-8 的百分号序列
        assert_eq!(url_decode("%E7%A3%81%E7%9B%98"), "磁盘");
        assert_eq!(url_decode("C%3A%5CUsers"), r"C:\Users");
        assert_eq!(url_decode("a%20b"), "a b");
        // 前端会把 + 编码成 %2B，所以裸 + 不该被当成空格
        assert_eq!(url_decode("a+b"), "a+b");
    }

    #[test]
    fn url_decode_keeps_malformed_input() {
        // 畸形的百分号序列不能吞字符，也不能 panic
        assert_eq!(url_decode("100%"), "100%");
        assert_eq!(url_decode("%ZZ"), "%ZZ");
        assert_eq!(url_decode(""),
                   "");
    }

    // ---------------------------------------------------------- 令牌比较

    #[test]
    fn token_comparison_is_exact() {
        assert!(constant_eq("abc123", "abc123"));
        assert!(!constant_eq("abc123", "abc124"));
        assert!(!constant_eq("abc", "abcd"));
        assert!(!constant_eq("", "x"));
        assert!(constant_eq("", ""));
    }

    #[test]
    fn token_is_long_and_hex() {
        let t = new_token();
        assert_eq!(t.len(), 64, "32 字节应编码成 64 位十六进制");
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
        // 两次生成不该相同
        assert_ne!(t, new_token());
    }

    // ---------------------------------------------------------- 空闲超时

    #[test]
    fn idle_timeout_defaults_and_overrides() {
        // 默认 10 分钟
        std::env::remove_var("DD_SERVE_IDLE_SECS");
        assert_eq!(idle_timeout(), Some(Duration::from_secs(600)));

        // 0 表示"不自动退出"
        std::env::set_var("DD_SERVE_IDLE_SECS", "0");
        assert_eq!(idle_timeout(), None);

        std::env::set_var("DD_SERVE_IDLE_SECS", "5");
        assert_eq!(idle_timeout(), Some(Duration::from_secs(5)));

        // 乱七八糟的值就退回默认，不能因此崩掉
        std::env::set_var("DD_SERVE_IDLE_SECS", "abc");
        assert_eq!(idle_timeout(), Some(Duration::from_secs(600)));

        std::env::remove_var("DD_SERVE_IDLE_SECS");
    }
}
