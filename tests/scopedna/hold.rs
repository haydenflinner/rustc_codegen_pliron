// The clone-entry arg of an inlined callee is kept as a block param for
// the scoped `noalias` fact only where the fact can be used
// (`phisimp::scoped_hold`). Held, the `&ItemBody::Text` constant below was
// no longer visible to `constload`, and the derived `PartialEq` compared
// against its memory byte by byte: `append_text` went from 90 to 173
// instructions (pulldown-cmark `Tree::append_text`, +9.5% executed).
// test.sh `scopedna` requires the same instruction count with the knob on.
use std::num::NonZeroUsize;
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum ItemBody {
    Paragraph, Text, SoftBreak, HardBreak,
    MaybeEmphasis(usize, bool, bool), MaybeSmartQuote(u8, bool, bool), MaybeCode(usize, bool),
    MaybeHtml, MaybeLinkOpen, MaybeLinkClose(bool), MaybeImage,
    Emphasis, Strong, Strikethrough, Code(usize), Link(usize), Image(usize), FootnoteReference(usize),
    TaskListMarker(bool), Rule, Heading(u32), FencedCodeBlock(usize), IndentCodeBlock, Html, OwnedHtml(usize),
    BlockQuote, List(bool, u8, u64), ListItem(usize), SynthesizeText(usize), SynthesizeChar(char),
    FootnoteDefinition(usize), Table(usize), TableHead, TableRow, TableCell, Root,
}
#[derive(Debug, Clone, Copy)]
pub struct Item { pub start: usize, pub end: usize, pub body: ItemBody }
#[derive(Debug, Eq, PartialEq, Clone, Copy)]
pub struct TreeIndex(NonZeroUsize);
impl TreeIndex { fn new(i: usize) -> Self { TreeIndex(NonZeroUsize::new(i).unwrap()) } pub fn get(self) -> usize { self.0.get() } }
#[derive(Debug, Clone, Copy)]
pub struct Node<T> { pub child: Option<TreeIndex>, pub next: Option<TreeIndex>, pub item: T }
pub struct Tree<T> { nodes: Vec<Node<T>>, spine: Vec<TreeIndex>, cur: Option<TreeIndex> }
impl<T> std::ops::Index<TreeIndex> for Tree<T> { type Output = Node<T>; fn index(&self, ix: TreeIndex) -> &Self::Output { self.nodes.index(ix.get()) } }
impl<T> std::ops::IndexMut<TreeIndex> for Tree<T> { fn index_mut(&mut self, ix: TreeIndex) -> &mut Node<T> { self.nodes.index_mut(ix.get()) } }
impl<T: Default> Tree<T> {
    pub fn with_capacity(cap: usize) -> Tree<T> {
        let mut nodes = Vec::with_capacity(cap);
        nodes.push(Node { child: None, next: None, item: <T as Default>::default() });
        Tree { nodes, spine: Vec::new(), cur: None }
    }
    pub fn cur(&self) -> Option<TreeIndex> { self.cur }
    pub fn append(&mut self, item: T) -> TreeIndex {
        let ix = self.create_node(item);
        let this = Some(ix);
        if let Some(ix) = self.cur { self[ix].next = this; }
        else if let Some(&parent) = self.spine.last() { self[parent].child = this; }
        self.cur = this;
        ix
    }
    pub fn create_node(&mut self, item: T) -> TreeIndex {
        let this = self.nodes.len();
        self.nodes.push(Node { child: None, next: None, item });
        TreeIndex::new(this)
    }
}
impl Default for Item { fn default() -> Self { Item { start: 0, end: 0, body: ItemBody::Root } } }
impl Tree<Item> {
    #[inline(never)]
    pub fn append_text(&mut self, start: usize, end: usize) {
        if end > start {
            if let Some(ix) = self.cur() {
                if ItemBody::Text == self[ix].item.body && self[ix].item.end == start {
                    self[ix].item.end = end;
                    return;
                }
            }
            self.append(Item { start, end, body: ItemBody::Text });
        }
    }
}
fn main() {
    let mut t: Tree<Item> = Tree::with_capacity(8);
    for i in 0..std::hint::black_box(100usize) { t.append_text(i * 3, i * 3 + 2 + (i & 1)); }
    println!("{} {:?}", t.nodes.len(), t.nodes.last().map(|n| n.item));
}
