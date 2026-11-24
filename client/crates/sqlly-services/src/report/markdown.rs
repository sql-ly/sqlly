//! Markdown cells parsed into a small, safe tree every renderer walks.
//! Raw HTML is dropped (its text is not shown either), links keep only
//! safe targets, and images keep only embedded raster data, so a notebook
//! from someone else cannot smuggle script or tracking pixels into a report.

use markdown::mdast::{AlignKind, Node};

#[derive(Debug, Clone, PartialEq)]
pub enum MdBlock {
    Paragraph(Vec<Inline>),
    Heading {
        level: u8,
        content: Vec<Inline>,
    },
    List {
        ordered: bool,
        start: u32,
        items: Vec<ListItem>,
    },
    Quote(Vec<MdBlock>),
    Code {
        language: Option<String>,
        text: String,
    },
    Table {
        align: Vec<Align>,
        header: Vec<Vec<Inline>>,
        rows: Vec<Vec<Vec<Inline>>>,
    },
    Rule,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ListItem {
    pub checked: Option<bool>,
    pub blocks: Vec<MdBlock>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
    None,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Inline {
    Text(String),
    Strong(Vec<Inline>),
    Emphasis(Vec<Inline>),
    Strike(Vec<Inline>),
    Code(String),
    /// `url` is already checked by [`super::text::safe_link`].
    Link {
        url: String,
        content: Vec<Inline>,
    },
    /// An embedded raster image (`data:image/png;base64,…`).
    Image {
        src: String,
        alt: String,
    },
    Break,
}

/// What parsing removed, for the fidelity report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Removed {
    pub html: usize,
    pub links: usize,
    pub images: usize,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct MdDoc {
    pub blocks: Vec<MdBlock>,
    pub removed: Removed,
}

/// Largest Markdown source parsed; longer text is cut (a malicious cell
/// must not stall an export).
pub const MAX_SOURCE: usize = 512 * 1024;

pub fn parse(source: &str) -> MdDoc {
    let mut cut = source.len().min(MAX_SOURCE);
    while !source.is_char_boundary(cut) {
        cut -= 1;
    }
    let source = &source[..cut];
    let options = markdown::ParseOptions::gfm();
    let Ok(root) = markdown::to_mdast(source, &options) else {
        return MdDoc {
            blocks: vec![MdBlock::Paragraph(vec![Inline::Text(source.to_string())])],
            removed: Removed::default(),
        };
    };
    let mut definitions = std::collections::HashMap::new();
    collect_definitions(&root, &mut definitions);
    let mut cx = Cx {
        removed: Removed::default(),
        definitions,
        depth: 0,
    };
    let blocks = match &root {
        Node::Root(r) => cx.blocks(&r.children),
        _ => Vec::new(),
    };
    MdDoc {
        blocks,
        removed: cx.removed,
    }
}

fn collect_definitions(node: &Node, out: &mut std::collections::HashMap<String, String>) {
    if let Node::Definition(d) = node {
        out.entry(d.identifier.to_lowercase())
            .or_insert(d.url.clone());
    }
    if let Some(children) = node.children() {
        for child in children {
            collect_definitions(child, out);
        }
    }
}

struct Cx {
    removed: Removed,
    definitions: std::collections::HashMap<String, String>,
    depth: usize,
}

const MAX_DEPTH: usize = 32;

impl Cx {
    fn blocks(&mut self, nodes: &[Node]) -> Vec<MdBlock> {
        if self.depth > MAX_DEPTH {
            return Vec::new();
        }
        self.depth += 1;
        let mut out = Vec::new();
        for node in nodes {
            match node {
                Node::Paragraph(p) => {
                    let content = self.inlines(&p.children);
                    if !content.is_empty() {
                        out.push(MdBlock::Paragraph(content));
                    }
                }
                Node::Heading(h) => out.push(MdBlock::Heading {
                    level: h.depth.clamp(1, 6),
                    content: self.inlines(&h.children),
                }),
                Node::List(list) => {
                    let items = list
                        .children
                        .iter()
                        .filter_map(|item| match item {
                            Node::ListItem(li) => Some(ListItem {
                                checked: li.checked,
                                blocks: self.blocks(&li.children),
                            }),
                            _ => None,
                        })
                        .collect();
                    out.push(MdBlock::List {
                        ordered: list.ordered,
                        start: list.start.unwrap_or(1),
                        items,
                    });
                }
                Node::Blockquote(q) => out.push(MdBlock::Quote(self.blocks(&q.children))),
                Node::Code(code) => out.push(MdBlock::Code {
                    language: code.lang.clone().filter(|l| {
                        l.len() <= 32
                            && l.chars()
                                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+'))
                    }),
                    text: code.value.clone(),
                }),
                Node::ThematicBreak(_) => out.push(MdBlock::Rule),
                Node::Table(table) => {
                    let align = table
                        .align
                        .iter()
                        .map(|a| match a {
                            AlignKind::Left => Align::Left,
                            AlignKind::Center => Align::Center,
                            AlignKind::Right => Align::Right,
                            AlignKind::None => Align::None,
                        })
                        .collect();
                    let mut rows: Vec<Vec<Vec<Inline>>> = table
                        .children
                        .iter()
                        .filter_map(|row| match row {
                            Node::TableRow(r) => Some(
                                r.children
                                    .iter()
                                    .map(|cell| match cell {
                                        Node::TableCell(c) => self.inlines(&c.children),
                                        _ => Vec::new(),
                                    })
                                    .collect(),
                            ),
                            _ => None,
                        })
                        .collect();
                    let header = if rows.is_empty() {
                        Vec::new()
                    } else {
                        rows.remove(0)
                    };
                    out.push(MdBlock::Table {
                        align,
                        header,
                        rows,
                    });
                }
                Node::Html(_) => self.removed.html += 1,
                Node::Definition(_)
                | Node::FootnoteDefinition(_)
                | Node::Yaml(_)
                | Node::Toml(_) => {}
                Node::Math(m) => out.push(MdBlock::Code {
                    language: Some("math".into()),
                    text: m.value.clone(),
                }),
                other => {
                    let content = self.inlines(std::slice::from_ref(other));
                    if !content.is_empty() {
                        out.push(MdBlock::Paragraph(content));
                    }
                }
            }
        }
        self.depth -= 1;
        out
    }

    fn inlines(&mut self, nodes: &[Node]) -> Vec<Inline> {
        if self.depth > MAX_DEPTH {
            return Vec::new();
        }
        self.depth += 1;
        let mut out: Vec<Inline> = Vec::new();
        for node in nodes {
            match node {
                Node::Text(t) => push_text(&mut out, &t.value),
                Node::Strong(s) => out.push(Inline::Strong(self.inlines(&s.children))),
                Node::Emphasis(e) => out.push(Inline::Emphasis(self.inlines(&e.children))),
                Node::Delete(d) => out.push(Inline::Strike(self.inlines(&d.children))),
                Node::InlineCode(c) => out.push(Inline::Code(c.value.clone())),
                Node::InlineMath(m) => out.push(Inline::Code(m.value.clone())),
                Node::Break(_) => out.push(Inline::Break),
                Node::Link(link) => {
                    let content = self.inlines(&link.children);
                    self.link(&mut out, &link.url, content);
                }
                Node::LinkReference(r) => {
                    let content = self.inlines(&r.children);
                    match self.definitions.get(&r.identifier.to_lowercase()).cloned() {
                        Some(url) => self.link(&mut out, &url, content),
                        None => out.extend(content),
                    }
                }
                Node::Image(image) => self.image(&mut out, &image.url, &image.alt),
                Node::ImageReference(r) => {
                    let url = self.definitions.get(&r.identifier.to_lowercase()).cloned();
                    match url {
                        Some(url) => self.image(&mut out, &url, &r.alt),
                        None => push_text(&mut out, &r.alt),
                    }
                }
                Node::Html(_) => self.removed.html += 1,
                Node::FootnoteReference(f) => push_text(&mut out, &format!("[{}]", f.identifier)),
                other => {
                    if let Some(children) = other.children() {
                        let nested = self.inlines(children);
                        out.extend(nested);
                    }
                }
            }
        }
        self.depth -= 1;
        out
    }

    fn link(&mut self, out: &mut Vec<Inline>, url: &str, content: Vec<Inline>) {
        match super::text::safe_link(url) {
            Some(url) => out.push(Inline::Link { url, content }),
            None => {
                self.removed.links += 1;
                out.extend(content);
            }
        }
    }

    fn image(&mut self, out: &mut Vec<Inline>, url: &str, alt: &str) {
        match super::text::safe_image_data_url(url) {
            Some(src) => out.push(Inline::Image {
                src,
                alt: alt.to_string(),
            }),
            None => {
                self.removed.images += 1;
                push_text(out, alt);
            }
        }
    }
}

fn push_text(out: &mut Vec<Inline>, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(Inline::Text(last)) = out.last_mut() {
        last.push_str(text);
    } else {
        out.push(Inline::Text(text.to_string()));
    }
}

/// The plain text of inline content, for PDF bookmarks, sheet cells and
/// alt text.
pub fn plain(inlines: &[Inline]) -> String {
    let mut out = String::new();
    for inline in inlines {
        match inline {
            Inline::Text(t) | Inline::Code(t) => out.push_str(t),
            Inline::Strong(c) | Inline::Emphasis(c) | Inline::Strike(c) => out.push_str(&plain(c)),
            Inline::Link { content, .. } => out.push_str(&plain(content)),
            Inline::Image { alt, .. } => out.push_str(alt),
            Inline::Break => out.push(' '),
        }
    }
    out
}

/// The plain text of a whole document, paragraphs joined by blank lines.
pub fn plain_blocks(blocks: &[MdBlock]) -> String {
    let mut parts = Vec::new();
    for block in blocks {
        match block {
            MdBlock::Paragraph(c) | MdBlock::Heading { content: c, .. } => parts.push(plain(c)),
            MdBlock::List { items, .. } => {
                for item in items {
                    parts.push(format!("- {}", plain_blocks(&item.blocks)));
                }
            }
            MdBlock::Quote(inner) => parts.push(plain_blocks(inner)),
            MdBlock::Code { text, .. } => parts.push(text.clone()),
            MdBlock::Table { header, rows, .. } => {
                let line = |cells: &Vec<Vec<Inline>>| {
                    cells
                        .iter()
                        .map(|c| plain(c))
                        .collect::<Vec<_>>()
                        .join(" | ")
                };
                parts.push(line(header));
                for row in rows {
                    parts.push(line(row));
                }
            }
            MdBlock::Rule => {}
        }
    }
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structure_survives() {
        let doc = parse(
            "# Title\n\nSome **bold** and *it* and `code`.\n\n- a\n- [x] b\n\n| a | b |\n|--:|---|\n| 1 | 2 |\n\n```sql\nselect 1\n```\n",
        );
        assert!(matches!(doc.blocks[0], MdBlock::Heading { level: 1, .. }));
        assert!(
            matches!(&doc.blocks[1], MdBlock::Paragraph(p) if p.iter().any(|i| matches!(i, Inline::Strong(_))))
        );
        assert!(
            matches!(&doc.blocks[2], MdBlock::List { items, .. } if items[1].checked == Some(true))
        );
        assert!(
            matches!(&doc.blocks[3], MdBlock::Table { align, rows, .. } if align[0] == Align::Right && rows.len() == 1)
        );
        assert!(matches!(&doc.blocks[4], MdBlock::Code { language: Some(l), .. } if l == "sql"));
        assert_eq!(doc.removed, Removed::default());
    }

    #[test]
    fn raw_html_and_unsafe_targets_are_removed() {
        let doc = parse(
            "<script>alert(1)</script>\n\nHi <img src=x onerror=alert(1)> [x](javascript:alert(1)) ![p](https://t.example/p.png) [ok](https://sqlly.app)\n",
        );
        let text = plain_blocks(&doc.blocks);
        assert!(!text.contains("script"));
        assert!(!text.contains("onerror"));
        assert_eq!(doc.removed.html, 2);
        assert_eq!(doc.removed.links, 1);
        assert_eq!(doc.removed.images, 1);
        let has_ok_link = |blocks: &[MdBlock]| {
            blocks.iter().any(|b| matches!(b, MdBlock::Paragraph(p) if p.iter().any(|i| matches!(i, Inline::Link { url, .. } if url == "https://sqlly.app"))))
        };
        assert!(has_ok_link(&doc.blocks));
    }

    #[test]
    fn reference_links_are_checked_too() {
        let doc = parse("[x][evil]\n\n[evil]: javascript:alert(1)\n");
        assert_eq!(doc.removed.links, 1);
        assert_eq!(plain_blocks(&doc.blocks), "x");
    }

    #[test]
    fn deep_nesting_is_bounded() {
        let source = ">".repeat(5000) + " deep";
        let doc = parse(&source);
        assert!(doc.blocks.len() <= 1);
    }
}
