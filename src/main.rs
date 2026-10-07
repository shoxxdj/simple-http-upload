//! simple_http_upload — tiny file-drop server with a web interface.
//! Standard library only, zero dependencies.
//!
//! Upload: `PUT /<dir>/<name>` with the file as the raw request body
//! (what the web UI does, and what `curl -T file http://host:port/` does).

use std::env;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::exit;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

const INDEX: &str = include_str!("index.html");
const CHUNK: usize = 256 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_NAME: usize = 180;

const USAGE: &str = "\
simple_http_upload — file drop over a web interface (zero dependencies)

USAGE
    simple_http_upload [OPTIONS] <PORT>

OPTIONS
    -p, --port <PORT>         Port to listen on (or just pass it as first argument)
    -i, --interface <ADDR>    Interface to bind (default: 0.0.0.0)
    -d, --dir <DIR>           Directory to serve and store uploads in (default: .)
        --no-listing          Disable directory listing
    -h, --help                Show this help

From the command line: curl -T file http://host:port/
";

struct Config {
    root: PathBuf,
    listing: bool,
}

// ───────────────────────── Console & server-side progress bar ─────────────────────────

struct Active {
    id: u64,
    name: String,
    total: u64,
    done: Arc<AtomicU64>,
    started: Instant,
}

struct ConsoleState {
    active: Vec<Active>,
    drawn: usize,
}

struct Console {
    state: Mutex<ConsoleState>,
    tty: bool,
    next_id: AtomicU64,
}

fn out(s: &str) {
    let mut o = io::stdout().lock();
    let _ = o.write_all(s.as_bytes());
    let _ = o.flush();
}

impl Console {
    fn new() -> Self {
        Console {
            state: Mutex::new(ConsoleState { active: Vec::new(), drawn: 0 }),
            tty: io::stdout().is_terminal(),
            next_id: AtomicU64::new(1),
        }
    }

    fn lock(&self) -> MutexGuard<'_, ConsoleState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Erases the previously drawn block of bars.
    fn clear(&self, s: &mut ConsoleState) {
        if self.tty && s.drawn > 0 {
            out(&format!("\x1b[{}A\r\x1b[J", s.drawn));
        }
        s.drawn = 0;
    }

    /// Draws one bar per running upload (one line each).
    fn draw(&self, s: &mut ConsoleState) {
        if !self.tty || s.active.is_empty() {
            return;
        }
        let mut buf = String::new();
        for a in &s.active {
            buf.push_str(&render(a));
            buf.push('\n');
        }
        s.drawn = s.active.len();
        out(&buf);
    }

    fn refresh(&self) {
        let mut s = self.lock();
        if s.active.is_empty() {
            return;
        }
        self.clear(&mut s);
        self.draw(&mut s);
    }

    fn start(&self, name: &str, peer: IpAddr, total: u64) -> (u64, Arc<AtomicU64>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let done = Arc::new(AtomicU64::new(0));
        let mut s = self.lock();
        self.clear(&mut s);
        if !self.tty {
            out(&format!("↑ {name} ({}) ← {peer}\n", fmt_size(total)));
        }
        s.active.push(Active {
            id,
            name: name.to_string(),
            total,
            done: done.clone(),
            started: Instant::now(),
        });
        self.draw(&mut s);
        (id, done)
    }

    fn finish(&self, id: u64, msg: &str) {
        let mut s = self.lock();
        self.clear(&mut s);
        s.active.retain(|a| a.id != id);
        out(&format!("{msg}\n"));
        self.draw(&mut s);
    }
}

fn render(a: &Active) -> String {
    const W: usize = 20;
    let done = a.done.load(Ordering::Relaxed).min(a.total);
    let frac = if a.total == 0 { 1.0 } else { done as f64 / a.total as f64 };
    let filled = ((frac * W as f64).round() as usize).min(W);
    let bar = "█".repeat(filled) + &"░".repeat(W - filled);
    let speed = done as f64 / a.started.elapsed().as_secs_f64().max(0.001);
    format!(
        "  {:<22} {} {:>3}% {:>10}/s {:>9}",
        short(&a.name, 22),
        bar,
        (frac * 100.0) as u32,
        fmt_size(speed as u64),
        fmt_size(a.total),
    )
}

fn short(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n - 1).collect::<String>() + "…"
    }
}

fn fmt_size(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{n} B") } else { format!("{v:.1} {}", U[i]) }
}

// ───────────────────────── HTTP ─────────────────────────

struct Request {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

fn read_line_limited(r: &mut impl BufRead, line: &mut String) -> io::Result<()> {
    let n = Read::take(&mut *r, 8192).read_line(line)?;
    if n == 0 || !line.ends_with('\n') {
        return Err(bad("invalid or too long line"));
    }
    Ok(())
}

fn read_head(r: &mut impl BufRead) -> io::Result<Request> {
    let mut line = String::new();
    read_line_limited(r, &mut line)?;
    let mut parts = line.split_whitespace();
    let (Some(m), Some(t), Some(v)) = (parts.next(), parts.next(), parts.next()) else {
        return Err(bad("invalid request"));
    };
    if !v.starts_with("HTTP/1.") {
        return Err(bad("unsupported HTTP version"));
    }
    let (method, target) = (m.to_string(), t.to_string());

    let mut headers = Vec::new();
    loop {
        line.clear();
        read_line_limited(r, &mut line)?;
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        if headers.len() >= 64 {
            return Err(bad("too many headers"));
        }
        if let Some((k, v)) = l.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    Ok(Request { method, target, headers })
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        301 => "Moved Permanently",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        411 => "Length Required",
        _ => "Internal Server Error",
    }
}

/// `extra`: additional headers, each terminated by \r\n.
fn send(w: &mut impl Write, status: u16, ctype: &str, extra: &str, body: &[u8], head_only: bool) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\nX-Content-Type-Options: nosniff\r\nCache-Control: no-store\r\n{extra}\r\n",
        reason(status),
        body.len()
    );
    w.write_all(head.as_bytes())?;
    if !head_only {
        w.write_all(body)?;
    }
    w.flush()
}

fn text(w: &mut impl Write, status: u16, msg: &str, head_only: bool) -> io::Result<()> {
    send(w, status, "text/plain; charset=utf-8", "", msg.as_bytes(), head_only)
}

fn json_err(w: &mut impl Write, status: u16, msg: &str) -> io::Result<()> {
    let body = format!("{{\"ok\":false,\"error\":{}}}", json_str(msg));
    send(w, status, "application/json", "", body.as_bytes(), false)
}

fn json_str(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                let _ = write!(o, "\\u{:04x}", c as u32);
            }
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

// ───────────────────────── Paths & encoding ─────────────────────────

fn pct_encode(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => o.push(b as char),
            _ => {
                let _ = write!(o, "%{b:02X}");
            }
        }
    }
    o
}

fn pct_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut o = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            o.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            o.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(o).ok()
}

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            c => o.push(c),
        }
    }
    o
}

/// Splits and decodes a URL path. Rejects anything that could escape the served directory.
fn parse_path(raw: &str) -> Option<Vec<String>> {
    if !raw.starts_with('/') {
        return None;
    }
    let mut segs = Vec::new();
    for s in raw.split('/').filter(|s| !s.is_empty()) {
        let d = pct_decode(s)?;
        if d == "." || d == ".." || d.contains(['/', '\\', '\0']) {
            return None;
        }
        segs.push(d);
    }
    Some(segs)
}

fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= MAX_NAME
        && n != "."
        && n != ".."
        && !n.chars().any(|c| c.is_control() || c == '/' || c == '\\')
}

fn mime(p: &Path) -> &'static str {
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        // Text and HTML are served as plain text: an uploaded file never runs in the browser.
        "txt" | "md" | "log" | "csv" | "json" | "xml" | "html" | "htm" | "svg" | "js" | "css" | "rs"
        | "py" | "toml" | "yaml" | "yml" | "sh" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

// ───────────────────────── GET / HEAD ─────────────────────────

fn serve(w: &mut TcpStream, cfg: &Config, raw: &str, segs: &[String], head_only: bool) -> io::Result<()> {
    let mut p = cfg.root.clone();
    p.extend(segs);
    let Ok(p) = fs::canonicalize(&p) else {
        return text(w, 404, "Not found", head_only);
    };
    if !p.starts_with(&cfg.root) {
        return text(w, 403, "Forbidden", head_only);
    }
    let Ok(meta) = fs::metadata(&p) else {
        return text(w, 404, "Not found", head_only);
    };

    if meta.is_dir() {
        if !raw.ends_with('/') {
            return send(w, 301, "text/plain", &format!("Location: {raw}/\r\n"), b"", head_only);
        }
        let page = render_page(cfg, segs, &p);
        return send(w, 200, "text/html; charset=utf-8", "", page.as_bytes(), head_only);
    }

    let Ok(mut f) = File::open(&p) else {
        return text(w, 403, "Forbidden", head_only);
    };
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nX-Content-Type-Options: nosniff\r\n\r\n",
        mime(&p),
        meta.len()
    );
    w.write_all(head.as_bytes())?;
    if !head_only {
        io::copy(&mut f, w)?;
    }
    Ok(())
}

fn render_page(cfg: &Config, segs: &[String], dir: &Path) -> String {
    let mut path = String::from("/");
    let mut crumbs = String::from("<a href=\"/\">root</a>");
    let mut href = String::from("/");
    for s in segs {
        path.push_str(s);
        path.push('/');
        href.push_str(&pct_encode(s));
        href.push('/');
        let _ = write!(crumbs, "<span>/</span><a href=\"{href}\">{}</a>", esc(s));
    }
    let list = if cfg.listing { list_html(dir) } else { String::new() };

    // Replace in reverse page order, one occurrence at a time, so that a file name
    // containing "{{CRUMBS}}" cannot disturb the template.
    INDEX
        .replacen("{{LIST}}", &list, 1)
        .replacen("{{CRUMBS}}", &crumbs, 1)
        .replacen("{{PATH}}", &esc(&path), 1)
}

fn list_html(dir: &Path) -> String {
    let mut dirs: Vec<String> = Vec::new();
    let mut files: Vec<(String, u64)> = Vec::new();
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') && name.ends_with(".part") {
                continue; // upload in progress
            }
            let Ok(m) = fs::metadata(e.path()) else { continue };
            if m.is_dir() {
                dirs.push(name);
            } else {
                files.push((name, m.len()));
            }
        }
    }
    dirs.sort_by_key(|n| n.to_lowercase());
    files.sort_by_key(|(n, _)| n.to_lowercase());

    let count = dirs.len() + files.len();
    let mut o = format!("<section id=\"files\"><h2>Contents <span>{count}</span></h2>");
    if count == 0 {
        o.push_str("<p class=\"empty\">Nothing here yet.</p></section>");
        return o;
    }
    o.push_str("<ul>");
    for d in &dirs {
        let _ = write!(o, "<li><a class=\"dir\" href=\"{}/\">{}/</a><span></span></li>", pct_encode(d), esc(d));
    }
    for (n, sz) in &files {
        let _ = write!(o, "<li><a href=\"{}\">{}</a><span>{}</span></li>", pct_encode(n), esc(n), fmt_size(*sz));
    }
    o.push_str("</ul></section>");
    o
}

// ───────────────────────── PUT (streaming upload) ─────────────────────────

fn upload(
    r: &mut impl BufRead,
    w: &mut TcpStream,
    req: &Request,
    cfg: &Config,
    con: &Console,
    peer: IpAddr,
    raw: &str,
    segs: &[String],
) -> io::Result<()> {
    let Some(name) = segs.last().filter(|_| !raw.ends_with('/')) else {
        return json_err(w, 400, "Missing file name");
    };
    if !valid_name(name) {
        return json_err(w, 400, "Invalid file name");
    }
    if req.header("transfer-encoding").is_some() {
        return json_err(w, 411, "Content-Length required (chunked transfer is not supported)");
    }
    let Some(total) = req.header("content-length").and_then(|v| v.parse::<u64>().ok()) else {
        return json_err(w, 411, "Content-Length required");
    };

    let mut d = cfg.root.clone();
    d.extend(&segs[..segs.len() - 1]);
    let Ok(dir) = fs::canonicalize(&d) else {
        return json_err(w, 404, "Directory not found");
    };
    if !dir.starts_with(&cfg.root) || !dir.is_dir() {
        return json_err(w, 403, "Directory forbidden");
    }

    if req.header("expect").is_some_and(|v| v.eq_ignore_ascii_case("100-continue")) {
        w.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
        w.flush()?;
    }

    let started = Instant::now();
    let (id, done) = con.start(name, peer, total);
    match receive(r, &dir, name, total, id, &done) {
        Ok(final_path) => {
            let final_name = final_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let secs = started.elapsed().as_secs_f64().max(0.001);
            con.finish(
                id,
                &format!(
                    "✔ {final_name} ({}) in {secs:.1}s, {}/s ← {peer}",
                    fmt_size(total),
                    fmt_size((total as f64 / secs) as u64)
                ),
            );
            let body = format!("{{\"ok\":true,\"name\":{},\"size\":{total}}}", json_str(&final_name));
            send(w, 201, "application/json", "", body.as_bytes(), false)
        }
        Err(e) => {
            let why = match e.kind() {
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => "timed out".to_string(),
                _ => e.to_string(),
            };
            con.finish(id, &format!("✘ {name} : {why} ← {peer}"));
            json_err(w, 500, &why)
        }
    }
}

/// Writes the body to a temporary `.name.<id>.part` file, then renames it without overwriting.
fn receive(r: &mut impl Read, dir: &Path, name: &str, total: u64, id: u64, done: &AtomicU64) -> io::Result<PathBuf> {
    let part = dir.join(format!(".{name}.{id}.part"));
    let mut f = OpenOptions::new().write(true).create_new(true).open(&part)?;
    let res = copy_body(r, &mut f, total, done);
    drop(f);
    if let Err(e) = res {
        let _ = fs::remove_file(&part);
        return Err(e);
    }
    let target = unique_path(dir, name);
    if let Err(e) = fs::rename(&part, &target) {
        let _ = fs::remove_file(&part);
        return Err(e);
    }
    Ok(target)
}

fn copy_body(r: &mut impl Read, f: &mut File, total: u64, done: &AtomicU64) -> io::Result<()> {
    let mut buf = vec![0u8; CHUNK];
    let mut left = total;
    while left > 0 {
        let want = left.min(buf.len() as u64) as usize;
        let n = match r.read(&mut buf[..want]) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "connection interrupted")),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        f.write_all(&buf[..n])?;
        left -= n as u64;
        done.fetch_add(n as u64, Ordering::Relaxed);
    }
    Ok(())
}

/// `name.ext` → `name (1).ext` → `name (2).ext`… so an existing file is never overwritten.
fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let p = Path::new(name);
    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    (1..)
        .map(|i| dir.join(format!("{stem} ({i}){ext}")))
        .find(|c| !c.exists())
        .unwrap()
}

// ───────────────────────── Connections ─────────────────────────

fn handle(stream: TcpStream, cfg: &Config, con: &Console) -> io::Result<()> {
    let peer = stream.peer_addr()?.ip();
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    let mut w = stream.try_clone()?;
    let mut r = BufReader::with_capacity(CHUNK, stream);

    let Ok(req) = read_head(&mut r) else {
        return text(&mut w, 400, "Invalid request", false);
    };
    let target = req.target.clone();
    let raw = target.split_once('?').map_or(target.as_str(), |(p, _)| p);
    let Some(segs) = parse_path(raw) else {
        return text(&mut w, 400, "Invalid path", false);
    };

    match req.method.as_str() {
        "GET" => serve(&mut w, cfg, raw, &segs, false),
        "HEAD" => serve(&mut w, cfg, raw, &segs, true),
        "PUT" => upload(&mut r, &mut w, &req, cfg, con, peer, raw, &segs),
        _ => send(&mut w, 405, "text/plain; charset=utf-8", "Allow: GET, HEAD, PUT\r\n", b"Method not allowed", false),
    }
}

// ───────────────────────── CLI ─────────────────────────

fn die(msg: &str) -> ! {
    eprintln!("error: {msg}\n\n{USAGE}");
    exit(2)
}

fn parse_port(v: &str) -> u16 {
    v.parse().unwrap_or_else(|_| die(&format!("invalid port: {v}")))
}

fn parse_iface(v: &str) -> IpAddr {
    let v = v.trim_matches(['[', ']']);
    if v == "localhost" {
        return IpAddr::V4(Ipv4Addr::LOCALHOST);
    }
    v.parse().unwrap_or_else(|_| die(&format!("invalid interface: {v}")))
}

fn parse_args() -> (u16, IpAddr, bool, PathBuf) {
    let mut port = None;
    let mut dir = PathBuf::from(".");
    let mut iface = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
    let mut listing = true;
    let mut it = env::args().skip(1);

    while let Some(a) = it.next() {
        let (key, inline) = match a.split_once('=') {
            Some((k, v)) if k.starts_with("--") => (k.to_string(), Some(v.to_string())),
            _ => (a.clone(), None),
        };
        match key.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                exit(0);
            }
            "--no-listing" => listing = false,
            "-p" | "--port" => {
                let v = inline.or_else(|| it.next()).unwrap_or_else(|| die("--port expects a value"));
                port = Some(parse_port(&v));
            }
            "-i" | "--interface" => {
                let v = inline.or_else(|| it.next()).unwrap_or_else(|| die("--interface expects a value"));
                iface = parse_iface(&v);
            }
            "-d" | "--dir" => {
                let v = inline.or_else(|| it.next()).unwrap_or_else(|| die("--dir expects a value"));
                dir = PathBuf::from(v);
            }
            s if !s.starts_with('-') => port = Some(parse_port(s)),
            _ => die(&format!("unknown option: {a}")),
        }
    }
    let Some(port) = port else { die("missing port") };
    (port, iface, listing, dir)
}

fn url(ip: IpAddr, port: u16) -> String {
    match ip {
        IpAddr::V6(_) => format!("http://[{ip}]:{port}"),
        IpAddr::V4(_) => format!("http://{ip}:{port}"),
    }
}

/// Local outbound address (no packet is actually sent).
fn lan_ip() -> Option<IpAddr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    s.local_addr().ok().map(|a| a.ip())
}

fn main() {
    let (port, iface, listing, dir) = parse_args();

    let root = fs::canonicalize(&dir)
        .ok()
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| die(&format!("not a directory: {}", dir.display())));
    let listener = TcpListener::bind(SocketAddr::new(iface, port)).unwrap_or_else(|e| {
        eprintln!("error: cannot listen on {iface}:{port} : {e}");
        exit(1)
    });
    let port = listener.local_addr().map_or(port, |a| a.port());

    out(&format!("simple_http_upload · {}\n", root.display()));
    if iface.is_unspecified() {
        out(&format!("  → {}\n", url(IpAddr::V4(Ipv4Addr::LOCALHOST), port)));
        if let Some(ip) = lan_ip() {
            out(&format!("  → {}\n", url(ip, port)));
        }
        out("  ⚠ no authentication: anyone on the network can upload files\n");
    } else {
        out(&format!("  → {}\n", url(iface, port)));
    }
    out(&format!("  listing {} · Ctrl-C to quit\n\n", if listing { "enabled" } else { "disabled" }));

    let cfg = Arc::new(Config { root, listing });
    let con = Arc::new(Console::new());

    if con.tty {
        let c = con.clone();
        thread::spawn(move || loop {
            thread::sleep(Duration::from_millis(100));
            c.refresh();
        });
    }

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let (cfg, con) = (cfg.clone(), con.clone());
        let _ = thread::Builder::new().spawn(move || {
            let _ = handle(stream, &cfg, &con);
        });
    }
}
