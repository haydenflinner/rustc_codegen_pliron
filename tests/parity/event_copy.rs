#![crate_type="lib"]
use std::num::NonZeroUsize;
#[derive(Clone, Copy)]
pub enum Body { Text, Code(u32), Link(usize), Heading(u32), Html, Other(u8) }
#[derive(Clone, Copy)]
pub struct Item { pub start: usize, pub end: usize, pub body: Body }
#[derive(Clone, Copy)]
pub struct Node { pub child: Option<NonZeroUsize>, pub next: Option<NonZeroUsize>, pub item: Item }
pub struct Tree { nodes: Vec<Node>, cur: Option<NonZeroUsize>, spine: Vec<usize> }
pub enum Tag<'a> { Para, Heading(u32), Link(u8, &'a str, &'a str), Code(&'a str) }
pub enum Event<'a> { Start(Tag<'a>), End(Tag<'a>), Text(&'a str), Code(&'a str), Html(&'a str), Rule, Break(bool) }
#[inline(never)]
fn item_to_event<'a>(item: Item, text: &'a str) -> Event<'a> {
    match item.body {
        Body::Text => Event::Text(&text[item.start..item.end]),
        Body::Code(_) => Event::Code(&text[item.start..item.end]),
        Body::Link(n) => Event::Start(Tag::Link(n as u8, &text[item.start..item.end], text)),
        Body::Heading(n) => Event::Start(Tag::Heading(n)),
        Body::Html => Event::Html(text),
        Body::Other(b) => Event::Break(b != 0),
    }
}
#[no_mangle]
pub fn next_ev<'a>(t: &mut Tree, text: &'a str) -> Option<Event<'a>> {
    let cur = t.cur?;
    let node = t.nodes[cur.get()];
    let item = node.item;
    let event = item_to_event(item, text);
    if let Event::Start(..) = event {
        t.spine.push(cur.get());
        t.cur = node.child;
    } else {
        t.cur = node.next;
    }
    Some(event)
}
