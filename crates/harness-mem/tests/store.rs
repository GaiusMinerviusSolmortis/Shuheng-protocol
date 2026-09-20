//! 碎片库的验收 —— 每一条都是"这一层会不会骗人"的问题。

use harness_mem::store::{NewFrag, Query, Store, MAX_TEXT_BYTES};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static N: AtomicU64 = AtomicU64::new(0);

fn tmp() -> PathBuf {
    let n = N.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!(
        "mem-store-test-{}-{}-{n}",
        std::process::id(),
        harness_core::now_ms()
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn new<'a>(anchor: &'a str, text: &'a str) -> NewFrag<'a> {
    NewFrag {
        anchor,
        text,
        epistemic: "decision",
        tags: vec![],
        agent: "agent:dada",
        session: "s-1",
        now_ms: 1_000_000,
        ttl_ms: None,
    }
}

#[test]
fn a_fragment_lands_and_comes_back_with_its_provenance() {
    let s = Store::open(&tmp()).unwrap();
    let out = s.deposit(new("/p", "我们决定用方案 X")).unwrap();
    assert!(out.stored);
    assert_eq!(out.id, "f-1");
    let r = s.recall("/p", &Query::default()).unwrap();
    assert_eq!(r.items.len(), 1);
    assert_eq!(r.items[0]["text"], "我们决定用方案 X");
    // **谁写的、哪次会话** —— §16.10 说这两样是溯源的全部
    assert_eq!(r.items[0]["agent"], "agent:dada");
    assert_eq!(r.items[0]["session"], "s-1");
    assert!(!r.unchanged);
}

/// **幂等**：同一条话写两遍，第二遍要认出来，而不是攒出第二条。
#[test]
fn the_same_words_twice_is_recognized_not_duplicated() {
    let s = Store::open(&tmp()).unwrap();
    s.deposit(new("/p", "一样的话")).unwrap();
    let again = s.deposit(new("/p", "一样的话")).unwrap();
    assert!(!again.stored, "第二条不该被写进去");
    assert_eq!(again.id, "f-1");
    assert_eq!(s.recall("/p", &Query::default()).unwrap().items.len(), 1);
}

#[test]
fn an_unknown_epistemic_is_refused_with_the_closed_list() {
    let s = Store::open(&tmp()).unwrap();
    let mut n = new("/p", "x");
    n.epistemic = "我觉得";
    let e = s.deposit(n).unwrap_err();
    assert!(e.remedy.contains("fact"), "{}", e.remedy);
    assert!(e.remedy.contains("opinion"), "{}", e.remedy);
}

#[test]
fn too_long_text_is_refused_and_points_elsewhere() {
    let s = Store::open(&tmp()).unwrap();
    let long = "啊".repeat(MAX_TEXT_BYTES);
    let e = s.deposit(new("/p", &long)).unwrap_err();
    assert!(e.remedy.contains("pointer"), "{}", e.remedy);
}

#[test]
fn a_missing_anchor_is_refused() {
    let s = Store::open(&tmp()).unwrap();
    let e = s.deposit(new("   ", "没说要挂哪")).unwrap_err();
    assert!(e.why.contains("没有锚点"), "{}", e.why);
}

/// **ETag 那条**：指纹一样就该答"没变"，而且**不能靠条数**。
#[test]
fn if_none_match_says_unchanged_and_a_new_fragment_moves_it() {
    let s = Store::open(&tmp()).unwrap();
    s.deposit(new("/p", "一条")).unwrap();
    let first = s.recall("/p", &Query::default()).unwrap();
    let q = Query {
        if_none_match: Some(first.etag.clone()),
        ..Default::default()
    };
    let same = s.recall("/p", &q).unwrap();
    assert!(same.unchanged);
    assert!(same.items.is_empty(), "说了没变就别再把内容倒一遍");
    s.deposit(new("/p", "两条")).unwrap();
    assert!(!s.recall("/p", &q).unwrap().unchanged);
}

/// **pin 变一次，指纹也要变** —— 否则"条数和内容都没变"会被误答成没变。
#[test]
fn pinning_moves_the_anchor_etag() {
    let s = Store::open(&tmp()).unwrap();
    s.deposit(new("/p", "一条")).unwrap();
    let before = s.recall("/p", &Query::default()).unwrap().etag;
    s.set_pin("/p", "f-1", true).unwrap();
    let after = s.recall("/p", &Query::default()).unwrap().etag;
    assert_ne!(before, after, "pin 是状态的一部分，指纹必须跟着动");
}

/// **确定性层**：被钉住的永远先给，而且**不受 `limit` 的挤**。
#[test]
fn pinned_fragments_come_first_and_are_not_crowded_out() {
    let s = Store::open(&tmp()).unwrap();
    s.deposit(new("/p", "钉住的")).unwrap();
    for i in 0..5 {
        s.deposit(new("/p", &format!("普通的 {i}"))).unwrap();
    }
    s.set_pin("/p", "f-1", true).unwrap();
    let q = Query {
        limit: Some(1),
        ..Default::default()
    };
    let r = s.recall("/p", &q).unwrap();
    assert_eq!(r.pinned.len(), 1, "钉住的那条必须在 pinned 里");
    assert_eq!(r.items.len(), 1, "limit 只管非钉住的那一堆");
}

/// **TTL 是"标出来"，不是"删掉"**：默认不给看，但要数出少了几条。
#[test]
fn an_expired_fragment_is_skipped_but_counted() {
    let s = Store::open(&tmp()).unwrap();
    let mut n = new("/p", "会过期的");
    // 1 毫秒的寿命 + 固定的写入时刻 ⇒ 读到它的时候早就过期了
    n.ttl_ms = Some(1);
    s.deposit(n).unwrap();
    s.deposit(new("/p", "不过期的")).unwrap();
    let r = s.recall("/p", &Query::default()).unwrap();
    assert_eq!(r.items.len(), 1);
    assert_eq!(r.expired_skipped, 1, "少了几条必须看得见（§16.6 降级必须显式）");
    let all = Query {
        include_expired: true,
        ..Default::default()
    };
    assert_eq!(s.recall("/p", &all).unwrap().items.len(), 2);
}

/// **负数不是"不过期"** —— 那是写错了，得报出来（两个意思混在一起以后没人分得清）。
#[test]
fn a_negative_ttl_is_refused_rather_than_read_as_never() {
    let s = Store::open(&tmp()).unwrap();
    let mut n = new("/p", "寿命写错了");
    n.ttl_ms = Some(-1);
    let e = s.deposit(n).unwrap_err();
    assert!(e.why.contains("-1"), "{}", e.why);
    assert!(e.remedy.contains("0"), "{}", e.remedy);
}

/// **忘记是打墓碑**：默认看不见它，但它还在账上，而且墓碑上写着为什么。
#[test]
fn forgetting_is_a_tombstone_not_a_delete() {
    let s = Store::open(&tmp()).unwrap();
    s.deposit(new("/p", "一条错的")).unwrap();
    s.forget("/p", "f-1", "当时判断错了").unwrap();
    assert!(s.recall("/p", &Query::default()).unwrap().items.is_empty());
    let with = Query {
        include_forgotten: true,
        ..Default::default()
    };
    assert_eq!(s.recall("/p", &with).unwrap().items.len(), 1);
    // 忘了的不许再钉
    assert!(s.set_pin("/p", "f-1", true).is_err());
}

#[test]
fn forget_without_a_reason_is_refused() {
    let s = Store::open(&tmp()).unwrap();
    s.deposit(new("/p", "一条")).unwrap();
    let e = s.forget("/p", "f-1", "  ").unwrap_err();
    assert!(e.why.contains("为什么"), "{}", e.why);
}

/// **写的人不一样，但记忆挂在同一个锚点上**（§16.10 那个开发/攻防的场景）。
#[test]
fn two_agents_share_one_anchor_and_provenance_survives() {
    let s = Store::open(&tmp()).unwrap();
    s.deposit(new("/p", "开发者留下的约定")).unwrap();
    let mut n = new("/p", "攻防者发现的弱点");
    n.agent = "agent:sec";
    n.epistemic = "experience";
    n.session = "s-2";
    s.deposit(n).unwrap();
    let r = s.recall("/p", &Query::default()).unwrap();
    assert_eq!(r.items.len(), 2);
    let agents: Vec<&str> = r.items.iter().map(|i| i["agent"].as_str().unwrap()).collect();
    assert!(agents.contains(&"agent:sec"));
    assert!(agents.contains(&"agent:dada"));
}

/// 召回是**字面匹配**：这个测试把"它能做什么、不能做什么"钉死。
#[test]
fn recall_matches_literally_and_says_so_by_behavior() {
    let s = Store::open(&tmp()).unwrap();
    let mut n = new("/p", "终端里跑命令要用 proc.exec");
    n.tags = vec!["term".into()];
    s.deposit(n).unwrap();
    s.deposit(new("/p", "跟这个话题无关的另一条")).unwrap();
    assert_eq!(
        s.recall(&"/p".to_string(), &Query { text: Some("proc.exec".into()), ..Default::default() })
            .unwrap()
            .items
            .len(),
        1
    );
    // tag 也命中
    assert_eq!(
        s.recall("/p", &Query { text: Some("term".into()), ..Default::default() })
            .unwrap()
            .items
            .len(),
        1
    );
    // **同义改写命中不了** —— 这不是 bug，是这一版的边界（语义匹配是以后的事）
    assert_eq!(
        s.recall("/p", &Query { text: Some("执行命令".into()), ..Default::default() })
            .unwrap()
            .items
            .len(),
        0
    );
}

#[test]
fn scanning_the_store_returns_every_anchor() {
    let s = Store::open(&tmp()).unwrap();
    s.deposit(new("/p/a", "x")).unwrap();
    s.deposit(new("/p/b", "y")).unwrap();
    s.deposit(new("/p/b", "z")).unwrap();
    let all = s.summaries(None).unwrap();
    assert_eq!(all.len(), 2);
    let b = all.iter().find(|a| a.anchor == "/p/b").unwrap();
    assert_eq!(b.alive, 2);
    let a = all.iter().find(|a| a.anchor == "/p/a").unwrap();
    assert_eq!(a.alive, 1);
}

/// **重启之后账还在** —— 这一层要是靠内存，跨会话就是空话。
#[test]
fn the_ledger_survives_a_restart() {
    let d = tmp();
    {
        let s = Store::open(&d).unwrap();
        s.deposit(new("/p", "重启前写的")).unwrap();
    }
    let s2 = Store::open(&d).unwrap();
    let r = s2.recall("/p", &Query::default()).unwrap();
    assert_eq!(r.items.len(), 1);
    assert_eq!(r.items[0]["text"], "重启前写的");
    // **序号也要接着涨**，不能重头来（撞 id 那条纪律）
    let next = s2.deposit(new("/p", "重启后写的")).unwrap();
    assert_eq!(next.id, "f-2");
}
