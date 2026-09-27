//! **第二个传输**（规范-消息协议 §6.2）。
//!
//! 到这一版为止只有 stdio 一个传输。它有个结构性限制：**通道是父子进程私有的** ——
//! 服务一死（或者中控一收摊），那条会话就没了；**别的会话不可能接上同一个进程**。
//! 于是"多个会话共享一个长期服务"（`lifecycle: resident`）在 stdio 上**根本写不出来**。
//!
//! 所以这一版补第二个传输。它必须满足 §6.2 那四条（下面每条都有判据）：
//!
//! | §6.2 要求 | 这一版怎么满足 |
//! |---|---|
//! | 承载全部 **7 种**消息 | 它只搬**行**，一行是什么由 `harness-core` 的编解码定 —— **传输不认识协议** |
//! | 同一 `corr` 内顺序单调 | 单连接内是**有序的字节流**（TCP 本来就保证） |
//! | **双向** | 全双工：一个连接上两边都能写 |
//! | 能表达"连接断了" | 读写返回 `Err`/EOF；**对端没了是传输自己告诉你的**，不是等超时 |
//!
//! **为什么是回环 TCP 而不是命名管道**：Windows 命名管道要一大片 FFI
//! （`CreateNamedPipeW` / `ConnectNamedPipe` / `ReadFile` / `WriteFile` 加上
//! 重叠 I/O 那套），而这一层要证的是**协议与传输解耦**，不是"用上了管道"。
//! 回环 TCP 是纯 std、跨平台，而且**根本不经过网卡**（不触发防火墙弹窗）。
//! 命名管道 / Unix 套接字是同一个接口的另一个实现 —— 换掉它不该动上面任何一层，
//! 这正是这条不变量要保的东西。
//!
//! ⚠️ **口令不是密码学强度**：它挡的是"同一台机器上随手连上来的别的东西"，
//! 不是"已经能在你机器上读文件的人"（那人都能读会合文件，自然也读得到口令）。
//! 这条写清楚，免得以后有人把它当安全边界。

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};

/// 传输自报的名字。**会合文件里要写它** ——
/// 以后换实现时，老客户端看见不认识的名字该拒绝，而不是猜着乱连。
///
/// ⚠️ **2026-09-27：认不认它，不再用等号了。** 原来 `alive()` 写的是
/// `self.protocol == PROTOCOL`，而**等号就是"没有兼容处理"**：
/// 同一个 major 里加个可选字段（抬一下 minor），全世界的会合文件当场全失效。
/// 现在口径是 [`accepts_tag`]：**名字要对得上，major 要对得上**
/// （规范-消息协议 §9）。
pub const PROTOCOL: &str = "ndjson-tcp-1";

/// **从标签里取出"它是第几版"**：`ndjson-tcp-1.7` → `(1, 7)`。
///
/// 为什么不直接借 `harness_core::Version`：这个 crate 的产品代码**刻意不依赖 core**
/// （见 `Cargo.toml` 里那段说明：传输不该认识信封）。所以这里自带一个小解析，
/// 而**它与 core 一致这件事由 `tests/transport.rs` 逐例盯着** ——
/// 两份会漂的实现，得有人查。
///
/// 不认识的标签一律 `None`（名字不对、版本不是数字、段数不对）：**不猜**。
pub fn tag_version(tag: &str) -> Option<(u32, u32)> {
    let rest = tag.strip_prefix("ndjson-tcp-")?;
    let (major, minor) = match rest.split_once('.') {
        Some((a, b)) => (a, b),
        None => (rest, "0"),
    };
    let digits = |s: &str| -> Option<u32> {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        s.parse().ok()
    };
    Some((digits(major)?, digits(minor)?))
}

/// **这一版传输认它吗？** —— 同 major 就认（规范-消息协议 §9）。
///
/// 反过来不认（major 不同）是**故意的**：会合文件里那句"我是什么协议"
/// 要是对不上，硬连上去只会拿到一堆看不懂的字节。
pub fn accepts_tag(tag: &str) -> bool {
    match (tag_version(tag), tag_version(PROTOCOL)) {
        (Some(theirs), Some(mine)) => theirs.0 == mine.0,
        _ => false,
    }
}

/// 一条连接。**只搬行**：每一行对上面那层是一个 NDJSON 消息。
#[derive(Debug)]
pub struct Conn {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl Conn {
    /// 连上去，并交出口令。口令不对时服务会回一句 `{"error":...}` 再断开 ——
    /// 所以这里的读会拿到那一行，而不是一个说不清的"连不上"。
    pub fn connect(addr: &str, token: &str) -> std::io::Result<Self> {
        let stream = TcpStream::connect(addr)?;
        stream.set_nodelay(true).ok();
        let mut writer = stream.try_clone()?;
        writer.write_all(token.as_bytes())?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        let mut reader = BufReader::new(stream);
        // **握手必须一问一答**：服务一定会先回一句 ——
        // 要么 `{"ok":...}`，要么 `{"error":...}`。
        //
        // （第一版写成"偷看一眼缓冲区"，那样在服务不说话时会**永远阻塞**，
        //   连 `alive()` 都动不了。显式的 ack 把这件事变成确定的。）
        let mut ack = String::new();
        let n = reader.read_line(&mut ack)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "对端没回话就断了（它是这个协议的服务吗？）",
            ));
        }
        if ack.contains("\"error\"") {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("对端拒绝了这个口令：{}", ack.trim()),
            ));
        }
        Ok(Self { reader, writer })
    }

    pub fn read_line(&mut self) -> std::io::Result<Option<String>> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line)?;
        if n == 0 {
            return Ok(None);
        }
        Ok(Some(line.trim_end_matches(['\r', '\n']).to_string()))
    }

    pub fn write_line(&mut self, line: &str) -> std::io::Result<()> {
        self.writer.write_all(line.as_bytes())?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()
    }

    pub fn try_clone(&self) -> std::io::Result<Self> {
        Ok(Self {
            reader: BufReader::new(self.reader.get_ref().try_clone()?),
            writer: self.writer.try_clone()?,
        })
    }
}

/// **它就是一条能写的通道** —— 这样上面那层（`codec::write_line`、`emit`）
/// 一个字都不用改就能走套接字。传输可替换，说的就是这件事。
impl std::io::Write for Conn {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.writer.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

/// 服务那一侧：听在一个回环端口上，谁带着口令来就跟谁说话。
pub struct Listener {
    listener: TcpListener,
    token: String,
    addr: SocketAddr,
    /// ack 里要带上"我是谁" —— 对面靠它确认自己连对了对象
    instance: String,
}

impl Listener {
    pub fn bind(instance: &str) -> std::io::Result<Self> {
        // **只绑回环**：不经网卡，也不给外面开一个口子
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        Ok(Self {
            listener,
            token: random_token(),
            addr,
            instance: instance.to_string(),
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    /// 接受一条连接。**握手不通过就直接断**，但先回一句说清为什么 ——
    /// 让对面看到"口令不对"，而不是一个说不清的 EOF（报错即手册）。
    pub fn accept(&self) -> std::io::Result<Conn> {
        let (stream, _) = self.listener.accept()?;
        stream.set_nodelay(true).ok();
        let mut writer = stream.try_clone()?;
        let mut reader = BufReader::new(stream);
        let mut first = String::new();
        reader.read_line(&mut first)?;
        if first.trim() != self.token {
            // 中文别塞进字节串（`b"..."` 只收 ASCII）—— 拼成 String 再写
            let msg = "{\"error\":\"bad-token\",\"why\":\"这个通道要口令\",\
                       \"remedy\":\"口令在会合文件里（服务启动时写的）\"}\n";
            let _ = writer.write_all(msg.as_bytes());
            let _ = writer.flush();
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "口令不对",
            ));
        }
        // **先答一声，再说别的**：对面在等这一行才能确定"接上了"
        let ack = format!(
            "{{\"ok\":true,\"protocol\":\"{PROTOCOL}\",\"instance\":\"{}\"}}\n",
            self.instance
        );
        writer.write_all(ack.as_bytes())?;
        writer.flush()?;
        Ok(Conn {
            reader,
            writer,
        })
    }

    /// 换个线程用的克隆（`TcpListener` 本身不是 `Clone`）。
    pub fn try_clone(&self) -> std::io::Result<Self> {
        Ok(Self {
            listener: self.listener.try_clone()?,
            token: self.token.clone(),
            addr: self.addr,
            instance: self.instance.clone(),
        })
    }
}

/// **会合文件**：一个常驻服务把"我在哪、口令是什么、我是几号进程"写在磁盘上，
/// 让**别的会话**找得到它。
///
/// ⚠️ **文件不是真相。** 进程崩了文件还在（没人来删），所以接上之前必须
/// **真的连一次**（`probe`）。这条和"磁盘指纹"、"锚点要重新解析"是同一条纪律：
/// **写下来的东西只能说'曾经如此'，要当前事实就得自己去问。**
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Rendezvous {
    pub instance: String,
    pub kind: String,
    pub protocol: String,
    pub addr: String,
    pub token: String,
    pub pid: u32,
    pub started_ms: i64,
}

impl Rendezvous {
    pub fn new(instance: &str, kind: &str, addr: SocketAddr, token: &str) -> Self {
        Self {
            instance: instance.to_string(),
            kind: kind.to_string(),
            protocol: PROTOCOL.to_string(),
            addr: addr.to_string(),
            token: token.to_string(),
            pid: std::process::id(),
            started_ms: now_ms(),
        }
    }

    fn path(dir: &std::path::Path, instance: &str) -> std::path::PathBuf {
        dir.join(format!("{}.json", safe_name(instance)))
    }

    /// **原子写**：先写 `.tmp` 再改名 —— 读的人要么看到上一版，要么看到这一版，
    /// 不会看到半截（同会话日志那份 meta.json）。
    pub fn write(&self, dir: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
        std::fs::create_dir_all(dir)?;
        let path = Self::path(dir, &self.instance);
        let tmp = path.with_extension("json.tmp");
        let body = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, &path)?;
        Ok(path)
    }

    pub fn read(dir: &std::path::Path, instance: &str) -> Option<Self> {
        let text = std::fs::read_to_string(Self::path(dir, instance)).ok()?;
        let v: serde_json::Value = serde_json::from_str(&text).ok()?;
        Some(Self {
            instance: v.get("instance")?.as_str()?.to_string(),
            kind: v.get("kind").and_then(|k| k.as_str()).unwrap_or("").to_string(),
            protocol: v.get("protocol")?.as_str()?.to_string(),
            addr: v.get("addr")?.as_str()?.to_string(),
            token: v.get("token")?.as_str()?.to_string(),
            pid: v.get("pid").and_then(|p| p.as_u64()).unwrap_or(0) as u32,
            started_ms: v.get("started_ms").and_then(|t| t.as_i64()).unwrap_or(0),
        })
    }

    pub fn remove(dir: &std::path::Path, instance: &str) -> std::io::Result<()> {
        match std::fs::remove_file(Self::path(dir, instance)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// **它现在还活着吗？** —— 真连一次，问一句 `ping`，看它回不回。
    ///
    /// 为什么不看进程号：进程号会被系统复用；为什么不只看文件：
    /// 崩了的进程不会来删文件。**能连上并答话，才叫活着。**
    pub fn alive(&self) -> bool {
        accepts_tag(&self.protocol)
            // 连上 + 口令通过 + 拿到 ack，三件事一次做完才算活着。
            // **不发任何协议消息**：探活不该在别人的协议流里留下东西。
            && Conn::connect(&self.addr, &self.token).is_ok()
    }
}

/// 实例名 → 能做文件名的片段（`mem:1` → `mem_1`）。
///
/// 谁要用：会合文件、**常驻服务的日志文件**。两个名字都从这儿出 ——
/// 免得"想找它的日志"的人得先猜命名规则。
pub fn safe_name(instance: &str) -> String {
    instance
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// **常驻服务的日志**放在会合目录里（不是继承中控的管道）——
/// 理由见 `harness-ctrl` 的 `spawn_resident`：它活得比中控久，
/// 而"管道另一头没人了"既会让日志读不出来、也会让它每写一行撞一次 EPIPE。
pub fn log_path_for(dir: &std::path::Path, instance: &str) -> std::path::PathBuf {
    dir.join(format!("{}.log", safe_name(instance)))
}

/// 一条够用的口令（**不是密码学强度**，见文件头那段）。
///
/// 来源是 `RandomState` —— std 里唯一一个"每个实例都不一样、由 OS 播种"的东西。
/// 取四份独立播种的哈希，拼成 128 位十六进制。
pub fn random_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut out = String::with_capacity(64);
    for i in 0..4u64 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(i);
        h.write_u128(nanos);
        h.write_u32(std::process::id());
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 一个"够久"的等待：等一个还没起来的常驻服务把会合文件写出来。
///
/// 为什么不用固定 sleep：它要**轮询事实**（文件出现 + 真的连得上），
/// 而不是赌一个时长。`grace_ms` 是上限，到了就**如实说没等到**。
pub fn wait_ready(dir: &std::path::Path, instance: &str, grace_ms: u64) -> Option<Rendezvous> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(grace_ms);
    loop {
        if let Some(rv) = Rendezvous::read(dir, instance) {
            if rv.alive() {
                return Some(rv);
            }
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}
