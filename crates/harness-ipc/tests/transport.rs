//! **第二个传输的验收**：规范 §6.2 那四条，一条一条问。

use harness_ipc::{Conn, Listener, Rendezvous, PROTOCOL};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static N: AtomicU64 = AtomicU64::new(0);

fn tmp() -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("ipc-test-{}-{}-{n}", std::process::id(), harness_ipc::now_ms()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// §6.2 第三条：**双向**。一个连接上两边都能写。
#[test]
fn a_connection_is_bidirectional() {
    let l = Listener::bind("echo:1").unwrap();
    let addr = l.addr().to_string();
    let token = l.token().to_string();
    let h = std::thread::spawn(move || {
        let mut c = l.accept().unwrap();
        // 服务先说自己是谁（presence 那一行）
        c.write_line("{\"type\":\"presence\",\"from\":\"echo:1\"}").unwrap();
        // 然后收一条、回一条
        let got = c.read_line().unwrap().unwrap();
        assert!(got.contains("request"));
        c.write_line("{\"type\":\"result\"}").unwrap();
    });
    let mut c = Conn::connect(&addr, &token).unwrap();
    let hello = c.read_line().unwrap().unwrap();
    assert!(hello.contains("presence"), "{hello}");
    c.write_line("{\"type\":\"request\"}").unwrap();
    let back = c.read_line().unwrap().unwrap();
    assert!(back.contains("result"));
    h.join().unwrap();
}

/// §6.2 第二条：同一连接内**顺序单调** —— 发出去什么顺序，对面就按什么顺序收到。
#[test]
fn lines_arrive_in_order() {
    let l = Listener::bind("x:1").unwrap();
    let addr = l.addr().to_string();
    let token = l.token().to_string();
    let h = std::thread::spawn(move || {
        let mut c = l.accept().unwrap();
        let mut got = Vec::new();
        for _ in 0..50 {
            got.push(c.read_line().unwrap().unwrap());
        }
        got
    });
    let mut c = Conn::connect(&addr, &token).unwrap();
    for i in 0..50 {
        c.write_line(&format!("{{\"n\":{i}}}")).unwrap();
    }
    let got = h.join().unwrap();
    for (i, line) in got.iter().enumerate() {
        assert!(line.contains(&format!("\"n\":{i}")), "第 {i} 条乱了：{line}");
    }
}

/// §6.2 第四条：**对端没了是传输自己说的**，不是等超时。
#[test]
fn a_dead_peer_is_reported_as_eof() {
    let l = Listener::bind("x:1").unwrap();
    let addr = l.addr().to_string();
    let token = l.token().to_string();
    let h = std::thread::spawn(move || {
        let _c = l.accept().unwrap();
        // 连接一丢就退出
    });
    let mut c = Conn::connect(&addr, &token).unwrap();
    h.join().unwrap();
    // 对面没了 ⇒ 读到的是 EOF（`Ok(None)`），不是"再等等"
    let got = c.read_line().unwrap();
    assert!(got.is_none(), "对端没了应当是 EOF，而不是 {got:?}");
}

#[test]
fn a_wrong_token_is_refused_with_a_reason() {
    let l = Listener::bind("x:1").unwrap();
    let addr = l.addr().to_string();
    std::thread::spawn(move || {
        let _ = l.accept(); // 口令不对 ⇒ accept 自己返回 Err
    });
    let e = Conn::connect(&addr, "不是口令").unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(e.to_string().contains("口令"), "{e}");
}

/// **会合文件**：写进去、读回来，字段一个不少（别的会话就靠它找过来）。
#[test]
fn the_rendezvous_file_round_trips() {
    let d = tmp();
    let l = Listener::bind("mem:1").unwrap();
    let rv = Rendezvous::new("mem:1", "mem", l.addr(), l.token());
    let path = rv.write(&d).unwrap();
    assert!(path.exists());
    let back = Rendezvous::read(&d, "mem:1").unwrap();
    assert_eq!(back, rv);
    assert_eq!(back.protocol, PROTOCOL);
    assert_eq!(back.pid, std::process::id());
    // **实例名里有冒号也要能存成文件名**（`mem:1` → `mem_1.json`）
    assert!(path.file_name().unwrap().to_str().unwrap().contains("mem_1"));
    Rendezvous::remove(&d, "mem:1").unwrap();
    assert!(Rendezvous::read(&d, "mem:1").is_none());
    // 删不存在的不算错（收摊时可能已经删过一遍）
    Rendezvous::remove(&d, "mem:1").unwrap();
}

/// **文件不是真相**：端口没人听的时候，`alive()` 必须说"不活"，
/// 哪怕文件还在那儿躺着（进程崩了不会来删文件）。
#[test]
fn a_stale_rendezvous_file_is_not_alive() {
    let d = tmp();
    // 造一个没人听的地址：绑一个端口再把它放掉
    let l = Listener::bind("ghost:1").unwrap();
    let addr = l.addr();
    let rv = Rendezvous::new("ghost:1", "ghost", addr, l.token());
    // 先证明活着
    let h = std::thread::spawn(move || {
        let _ = l.accept();
    });
    rv.write(&d).unwrap();
    assert!(rv.alive(), "有人听的时候该说活着");
    h.join().unwrap();
    // 现在没人听了，文件还在
    assert!(Rendezvous::read(&d, "ghost:1").is_some(), "文件还在");
    assert!(!rv.alive(), "没人听 ⇒ 不许说活着");
}

#[test]
fn a_rendezvous_with_a_foreign_protocol_is_not_alive() {
    let l = Listener::bind("x:1").unwrap();
    let mut rv = Rendezvous::new("x:1", "x", l.addr(), l.token());
    rv.protocol = "named-pipe-9".into();
    // 协议名不认识就别连（以后换实现时，老客户端该拒绝而不是乱连）
    assert!(!rv.alive());
}

/// **传输的版本闸门：同 major 就认**（2026-09-27，规范-消息协议 §9）。
///
/// 这条以前是等号（`self.protocol == PROTOCOL`），而**等号就是"没有兼容处理"**：
/// 同一个 major 里加一个可选字段（抬一下 minor），
/// **所有还活着的常驻服务的会合文件当场全失效** —— 明明什么都没变。
#[test]
fn a_same_major_tag_connects_even_when_the_minor_is_newer() {
    let l = Listener::bind("mem:1").unwrap();
    let mut rv = Rendezvous::new("mem:1", "mem", l.addr(), l.token());
    // 造一个"更新的 minor"：同一个 major，所以该连上。
    rv.protocol = "ndjson-tcp-1.7".into();
    let h = std::thread::spawn(move || {
        let _ = l.accept();
    });
    assert!(rv.alive(), "同一个 major 该连上（原来的等号会把它判成死的）");
    h.join().unwrap();
    // 而 major 对不上就不连 —— 硬连上去只会拿到一堆看不懂的字节。
    rv.protocol = "ndjson-tcp-2.0".into();
    assert!(!rv.alive(), "major 对不上不许连");
    // 名字就不是这个协议的，更不许连。
    rv.protocol = "named-pipe-1".into();
    assert!(!rv.alive());
}

/// **那份小解析和 `harness-core` 的版本口径必须一致。**
///
/// 为什么要有这一条：`harness-ipc` 的产品代码刻意不依赖 core
/// （传输不该认识信封），于是"版本怎么读"这件事**有两份实现** ——
/// 而两份手写的东西一定会漂。它们没法共用一行代码，那就至少**共用一组例子**：
/// 下面每一行，两边的答案必须一样。
#[test]
fn the_transport_tag_reads_versions_exactly_like_core_does() {
    let cases = [
        ("ndjson-tcp-1", "1"),
        ("ndjson-tcp-1.0", "1.0"),
        ("ndjson-tcp-1.7", "1.7"),
        ("ndjson-tcp-2.0", "2.0"),
        ("ndjson-tcp-10.20", "10.20"),
    ];
    for (tag, ver) in cases {
        let theirs = harness_core::Version::parse(ver).unwrap();
        assert_eq!(
            harness_ipc::tag_version(tag),
            Some((theirs.major, theirs.minor)),
            "`{tag}` 两边读出来不一样"
        );
    }
    // 读不出来的那些也要一致：两边都不许猜。
    for bad in ["ndjson-tcp", "ndjson-tcp-", "ndjson-tcp-x", "ndjson-tcp-1.", "ndjson-tcp-1.0.2", "named-pipe-1"] {
        assert!(harness_ipc::tag_version(bad).is_none(), "`{bad}` 不该读出来");
        assert!(harness_core::Version::parse(bad).is_none(), "`{bad}` 两边都不该认");
    }
    // 自己报的那一版，也得和 core 常量是同一版。
    let mine = harness_ipc::tag_version(PROTOCOL).unwrap();
    assert_eq!(
        mine,
        (
            harness_core::version::protocol_version().major,
            harness_core::version::protocol_version().minor
        ),
        "传输自报的版本和 `PROTOCOL_VERSION` 不是同一版"
    );
}

/// 口令**每次都不一样**（不然"口令"这两个字就没意义了）。
#[test]
fn tokens_differ_between_instances() {
    let a = harness_ipc::random_token();
    let b = harness_ipc::random_token();
    assert_ne!(a, b);
    assert_eq!(a.len(), 64);
}

/// 连接是**字节流**：写进去的行里带中文/转义也能原样出来。
#[test]
fn lines_survive_utf8() {
    let l = Listener::bind("x:1").unwrap();
    let addr = l.addr().to_string();
    let token = l.token().to_string();
    let h = std::thread::spawn(move || {
        let mut c = l.accept().unwrap();
        c.read_line().unwrap().unwrap()
    });
    let mut c = Conn::connect(&addr, &token).unwrap();
    let line = r#"{"text":"一行中文，带 \"引号\" 和 \\ 反斜杠"}"#;
    c.write_line(line).unwrap();
    assert_eq!(h.join().unwrap(), line);
}

/// 一个监听器能**同时**接两个会话 —— 这正是 resident 要的形状
/// （两个中控共用一个服务进程，而不是各起一个）。
#[test]
fn one_listener_serves_two_sessions() {
    let l = Listener::bind("mem:1").unwrap();
    let addr = l.addr().to_string();
    let token = l.token().to_string();
    let l2 = l.try_clone().unwrap();
    let a = std::thread::spawn(move || {
        let mut c = l.accept().unwrap();
        let got = c.read_line().unwrap().unwrap();
        c.write_line(&format!("{{\"saw\":\"{got}\"}}")).unwrap();
    });
    let b = std::thread::spawn(move || {
        let mut c = l2.accept().unwrap();
        let got = c.read_line().unwrap().unwrap();
        c.write_line(&format!("{{\"saw\":\"{got}\"}}")).unwrap();
    });
    let mut c1 = Conn::connect(&addr, &token).unwrap();
    let mut c2 = Conn::connect(&addr, &token).unwrap();
    c1.write_line("会话一").unwrap();
    c2.write_line("会话二").unwrap();
    assert!(c1.read_line().unwrap().unwrap().contains("会话一"));
    assert!(c2.read_line().unwrap().unwrap().contains("会话二"));
    a.join().unwrap();
    b.join().unwrap();
    let _ = std::io::stdout().flush();
}
