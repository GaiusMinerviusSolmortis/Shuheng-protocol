//! **§6.2 第一条的判据：这个传输承不承载全部 7 种消息。**
//!
//! 为什么值得单独写一条：传输本身**不认识消息种类**（它只搬行）——
//! 所以"承载 7 种"这件事在结构上必然成立，但那句话当时**没人验过**。
//! 这一条用**真的信封 + 真的编解码器 + 真的套接字**把它跑一遍：
//! 编码 → 过套接字 → 解码 → **逐字段比**。
//!
//! （往 socket 里塞 `{"x":1}` 那种"看起来过了"的测试没有意义 ——
//!   要证的是**这七种东西放在这条通道上不会丢东西**。）

use harness_core::codec;
use harness_core::envelope::{Authority, Envelope, MessageType};
use harness_ipc::{Conn, Listener};
use serde_json::json;

/// 造一条**每种字段都填满**的信封。空字段是掩盖问题的常见手段 ——
/// 所以这里连 `corr` 都不留空（它在规范里是可选的，但真实调用里都有）。
fn envelope(kind: MessageType, i: usize) -> Envelope {
    Envelope {
        id: format!("m-{i}"),
        ts: 1_700_000_000_000 + i as i64,
        kind,
        from: "agent:达达".into(),
        to: if kind == MessageType::Presence {
            "ctrl".into()
        } else {
            "term:1".into()
        },
        corr: Some(format!("c-{i}")),
        sid: "s-1".into(),
        authority: Authority::User,
        // 里面塞点**不该被弄坏的东西**：中文、转义、嵌套、数组
        payload: json!({
            "tool": "run",
            "args": {"cmd": "echo \"带引号和 \\ 反斜杠\"", "n": i},
            "text": "一行中文",
            "nested": {"a": [1, 2, {"b": null}]},
        }),
    }
}

#[test]
fn all_seven_message_kinds_survive_the_second_transport() {
    let l = Listener::bind("probe:1").unwrap();
    let addr = l.addr().to_string();
    let token = l.token().to_string();

    let h = std::thread::spawn(move || {
        let mut c = l.accept().unwrap();
        let mut got = Vec::new();
        for _ in 0..MessageType::ALL.len() {
            let line = c.read_line().unwrap().expect("对面不该提前关");
            got.push(codec::decode(&line).expect("解出来还得是合法的信封"));
        }
        got
    });

    let mut c = Conn::connect(&addr, &token).unwrap();
    let sent: Vec<Envelope> = MessageType::ALL
        .iter()
        .enumerate()
        .map(|(i, k)| envelope(*k, i))
        .collect();
    for e in &sent {
        c.write_line(&codec::encode(e).unwrap()).unwrap();
    }

    let got = h.join().unwrap();
    assert_eq!(sent.len(), 7, "规范里就是 7 种，少一种这条判据就废了");
    for (i, (a, b)) in sent.iter().zip(got.iter()).enumerate() {
        assert_eq!(a.kind, b.kind, "第 {i} 条的种类变了");
        assert_eq!(a, b, "第 {i} 条（{:?}）过了一趟套接字就变了", a.kind);
    }
    // 名字上也点一遍，免得将来有人改 `ALL` 的顺序还以为这条在测别的东西
    let kinds: Vec<MessageType> = got.iter().map(|e| e.kind).collect();
    assert_eq!(
        kinds,
        vec![
            MessageType::Request,
            MessageType::Chunk,
            MessageType::Result,
            MessageType::Cancel,
            MessageType::Event,
            MessageType::Inject,
            MessageType::Presence,
        ]
    );
}

/// 反着再来一遍：**对面（服务那侧）写，我们（调用方）读**。
/// 单向过一遍就宣称"双向都行"是不够的 —— §6.2 第三条要求的是双向。
#[test]
fn the_other_direction_carries_them_too() {
    let l = Listener::bind("probe:2").unwrap();
    let addr = l.addr().to_string();
    let token = l.token().to_string();

    let h = std::thread::spawn(move || {
        let mut c = l.accept().unwrap();
        for (i, k) in MessageType::ALL.iter().enumerate() {
            c.write_line(&codec::encode(&envelope(*k, i)).unwrap()).unwrap();
        }
    });

    let mut c = Conn::connect(&addr, &token).unwrap();
    let mut got = Vec::new();
    for _ in 0..MessageType::ALL.len() {
        let line = c.read_line().unwrap().unwrap();
        got.push(codec::decode(&line).unwrap());
    }
    h.join().unwrap();
    for (i, k) in MessageType::ALL.iter().enumerate() {
        assert_eq!(got[i].kind, *k, "第 {i} 条反着走就不对了");
    }
}
