//! Плоское дерево узлов и статистика по типам файлов.

use std::collections::HashMap;

use crate::scan::RawNode;

pub type NodeId = u32;

#[derive(Debug)]
pub struct Node {
    pub name: Box<str>,
    pub parent: Option<NodeId>,
    /// Дети, отсортированные по размеру (крупные первыми).
    pub children: Vec<NodeId>,
    pub is_dir: bool,
    pub size: u64,
    pub alloc: u64,
    /// Сколько всего файлов внутри (для файла — 1).
    pub files: u64,
    /// Индекс в [`Tree::types`]; у папок `None`.
    pub kind: Option<u16>,
}

#[derive(Debug, Clone)]
pub struct TypeStat {
    /// Расширение в нижнем регистре, без точки; пустая строка — «без расширения».
    pub ext: Box<str>,
    pub size: u64,
    pub files: u64,
}

#[derive(Debug)]
pub struct Tree {
    pub nodes: Vec<Node>,
    /// Типы файлов, отсортированные по размеру.
    pub types: Vec<TypeStat>,
}

impl Tree {
    pub const ROOT: NodeId = 0;

    pub fn from_raw(raw: RawNode) -> Self {
        let mut tree = Tree { nodes: Vec::new(), types: Vec::new() };
        let mut ext_index: HashMap<Box<str>, u16> = HashMap::new();
        let mut type_acc: Vec<TypeStat> = Vec::new();
        tree.push(raw, None, &mut ext_index, &mut type_acc);

        // Переупорядочиваем типы по размеру и перенумеровываем ссылки на них.
        let mut order: Vec<u16> = (0..type_acc.len() as u16).collect();
        order.sort_by(|&a, &b| type_acc[b as usize].size.cmp(&type_acc[a as usize].size));
        let mut remap = vec![0u16; order.len()];
        for (new, &old) in order.iter().enumerate() {
            remap[old as usize] = new as u16;
        }
        for n in &mut tree.nodes {
            if let Some(k) = &mut n.kind {
                *k = remap[*k as usize];
            }
        }
        tree.types = order.into_iter().map(|i| type_acc[i as usize].clone()).collect();
        tree
    }

    fn push(
        &mut self,
        raw: RawNode,
        parent: Option<NodeId>,
        ext_index: &mut HashMap<Box<str>, u16>,
        types: &mut Vec<TypeStat>,
    ) -> NodeId {
        let id = self.nodes.len() as NodeId;
        let kind = if raw.is_dir {
            None
        } else {
            let ext = extension(&raw.name);
            let idx = *ext_index.entry(ext.clone()).or_insert_with(|| {
                types.push(TypeStat { ext, size: 0, files: 0 });
                (types.len() - 1) as u16
            });
            let t = &mut types[idx as usize];
            t.size += raw.size;
            t.files += 1;
            Some(idx)
        };
        self.nodes.push(Node {
            name: raw.name,
            parent,
            children: Vec::new(),
            is_dir: raw.is_dir,
            size: raw.size,
            alloc: raw.alloc,
            files: if raw.is_dir { 0 } else { 1 },
            kind,
        });

        let mut kids = raw.children;
        kids.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));
        let mut ids = Vec::with_capacity(kids.len());
        let mut files = 0;
        for k in kids {
            let cid = self.push(k, Some(id), ext_index, types);
            files += self.nodes[cid as usize].files;
            ids.push(cid);
        }
        let n = &mut self.nodes[id as usize];
        n.children = ids;
        if n.is_dir {
            n.files = files;
        }
        id
    }

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id as usize]
    }

    /// Путь от корня до узла включительно.
    pub fn ancestors(&self, id: NodeId) -> Vec<NodeId> {
        let mut v = vec![id];
        let mut cur = id;
        while let Some(p) = self.node(cur).parent {
            v.push(p);
            cur = p;
        }
        v.reverse();
        v
    }

    /// Полный путь к узлу на диске.
    pub fn path(&self, id: NodeId) -> std::path::PathBuf {
        let mut p = std::path::PathBuf::new();
        for a in self.ancestors(id) {
            p.push(&*self.node(a).name);
        }
        p
    }

    /// Доля узла от родителя, 0..=1.
    pub fn share_of_parent(&self, id: NodeId) -> f64 {
        match self.node(id).parent {
            Some(p) if self.node(p).size > 0 => self.node(id).size as f64 / self.node(p).size as f64,
            Some(_) => 0.0,
            None => 1.0,
        }
    }
}

/// Расширение файла: `photo.JPG` → `jpg`, `.bashrc` → ``, `a.tar.gz` → `gz`.
/// Числовые хвосты версий пропускаются, если перед ними есть настоящее расширение:
/// `libfoo.so.12.0` → `so`, но `data.046` → `046`.
pub fn extension(name: &str) -> Box<str> {
    fn last(s: &str) -> Option<&str> {
        match s.rfind('.') {
            Some(0) | None => None,
            Some(i) if i + 1 == s.len() => None,
            Some(i) => Some(&s[i + 1..]),
        }
    }
    let is_num = |e: &str| e.bytes().all(|b| b.is_ascii_digit());
    let Some(first) = last(name) else { return "".into() };
    let mut rest = name;
    let mut ext = first;
    while is_num(ext) {
        rest = &rest[..rest.len() - ext.len() - 1];
        match last(rest) {
            Some(e) => ext = e,
            // Настоящего расширения нет — оставляем числовое как есть.
            None => return first.to_ascii_lowercase().into(),
        }
    }
    ext.to_ascii_lowercase().into()
}

/// Человекочитаемый размер: `1536` → `1.5 КБ`.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["Б", "КБ", "МБ", "ГБ", "ТБ", "ПБ"];
    if bytes < 1024 {
        return format!("{bytes} Б");
    }
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if v >= 100.0 { format!("{v:.0} {}", UNITS[u]) } else { format!("{v:.1} {}", UNITS[u]) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, size: u64) -> RawNode {
        RawNode { name: name.into(), size, alloc: size, ..Default::default() }
    }

    #[test]
    fn builds_sorted_tree_and_types() {
        let raw = RawNode {
            name: "/r".into(),
            is_dir: true,
            size: 600,
            alloc: 600,
            children: vec![
                file("small.txt", 100),
                RawNode {
                    name: "dir".into(),
                    is_dir: true,
                    size: 500,
                    alloc: 500,
                    children: vec![file("a.MP4", 300), file("b.mp4", 200)],
                },
            ],
        };
        let t = Tree::from_raw(raw);
        let root = t.node(Tree::ROOT);
        assert_eq!(root.files, 3);
        assert_eq!(&*t.node(root.children[0]).name, "dir", "крупное первым");
        assert_eq!(&*t.types[0].ext, "mp4");
        assert_eq!(t.types[0].size, 500);
        assert_eq!(t.types[0].files, 2);
        let mp4 = t.node(t.node(root.children[0]).children[0]);
        assert_eq!(mp4.kind, Some(0), "индексы типов перенумерованы");
        assert_eq!(t.path(t.node(root.children[0]).children[1]), std::path::Path::new("/r/dir/b.mp4"));
        assert!((t.share_of_parent(root.children[0]) - 5.0 / 6.0).abs() < 1e-9);
    }

    #[test]
    fn extensions_and_sizes() {
        assert_eq!(&*extension("photo.JPG"), "jpg");
        assert_eq!(&*extension(".bashrc"), "");
        assert_eq!(&*extension("a.tar.gz"), "gz");
        assert_eq!(&*extension("Makefile"), "");
        assert_eq!(&*extension("libvulkan.so.1.4.363"), "so");
        assert_eq!(&*extension("libfoo.so.12"), "so");
        assert_eq!(&*extension("backup.7z"), "7z");
        assert_eq!(&*extension("2024"), "");
        assert_eq!(&*extension("data.046"), "046");
        assert_eq!(&*extension("archive.tar.001"), "tar");
        assert_eq!(human_size(512), "512 Б");
        assert_eq!(human_size(1536), "1.5 КБ");
        assert_eq!(human_size(250 * 1024 * 1024 * 1024), "250 ГБ");
    }
}
