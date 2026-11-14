//! Анализ места для нижней панели: сводка по любой папке уже просканированного
//! дерева и точечное обновление дерева после операций с файлами.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::path::{Path, PathBuf};

use crate::model::{NodeId, Tree};
use crate::scan::{Progress, RawNode, ScanOptions, scan};

/// Сколько места занимает один тип файлов внутри папки.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeShare {
    /// Индекс в [`Tree::types`] — по нему же красится treemap.
    pub kind: u16,
    pub size: u64,
    pub files: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    pub size: u64,
    pub alloc: u64,
    pub files: u64,
    pub dirs: u64,
    /// Типы по убыванию размера.
    pub types: Vec<TypeShare>,
    /// Самые крупные файлы во всём поддереве, по убыванию.
    pub largest: Vec<NodeId>,
}

/// Обходит поддерево `root` один раз и собирает сводку.
pub fn summarize(tree: &Tree, root: NodeId, top: usize) -> Summary {
    let r = tree.node(root);
    let mut s = Summary { size: r.size, alloc: r.alloc, files: r.files, ..Default::default() };
    let mut types: HashMap<u16, (u64, u64)> = HashMap::new();
    let mut heap: BinaryHeap<Reverse<(u64, NodeId)>> = BinaryHeap::new();
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        let n = tree.node(id);
        if n.is_dir {
            if id != root {
                s.dirs += 1;
            }
            stack.extend_from_slice(&n.children);
            continue;
        }
        if let Some(k) = n.kind {
            let t = types.entry(k).or_default();
            t.0 += n.size;
            t.1 += 1;
        }
        if top > 0 {
            heap.push(Reverse((n.size, id)));
            if heap.len() > top {
                heap.pop();
            }
        }
    }
    s.types = types.into_iter().map(|(kind, (size, files))| TypeShare { kind, size, files }).collect();
    s.types.sort_by(|a, b| b.size.cmp(&a.size).then(a.kind.cmp(&b.kind)));
    let mut largest: Vec<(u64, NodeId)> = heap.into_iter().map(|Reverse(x)| x).collect();
    largest.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    s.largest = largest.into_iter().map(|(_, id)| id).collect();
    s
}

/// Свежие данные с диска для изменившихся путей. Если в списке есть и папка,
/// и что-то внутри неё, перечитывается только папка. Долго — вызывать в фоне.
pub fn rescan(touched: &[PathBuf]) -> Vec<(PathBuf, Option<RawNode>)> {
    touched
        .iter()
        .filter(|p| !touched.iter().any(|q| q != *p && p.starts_with(q)))
        .map(|p| {
            let raw = scan(p, &ScanOptions::default(), &Progress::default()).ok().map(|mut r| {
                r.name = p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned()).into();
                r
            });
            (p.clone(), raw)
        })
        .collect()
}

/// Подменяет в дереве изменившиеся узлы. Пути вне дерева пропускаются.
/// Возвращает `true`, если дерево изменилось.
pub fn patch(tree: &mut Tree, refresh: Vec<(PathBuf, Option<RawNode>)>) -> bool {
    let root = PathBuf::from(&*tree.node(Tree::ROOT).name);
    let mut changed = false;
    for (path, raw) in refresh {
        if path == root || !path.starts_with(&root) {
            continue;
        }
        if let Some(id) = tree.find(&path) {
            tree.remove(id);
            changed = true;
        }
        if let (Some(raw), Some(parent)) = (raw, path.parent().and_then(|p| tree.find(p))) {
            tree.insert(parent, raw);
            changed = true;
        }
    }
    changed
}

/// Корень дерева на диске.
pub fn root_path(tree: &Tree) -> &Path {
    Path::new(&*tree.node(Tree::ROOT).name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn file(name: &str, size: u64) -> RawNode {
        RawNode { name: name.into(), size, alloc: size, ..Default::default() }
    }

    #[test]
    fn summary_of_subtree() {
        let raw = RawNode {
            name: "/r".into(),
            is_dir: true,
            children: vec![
                file("a.mp4", 500),
                RawNode {
                    name: "d".into(),
                    is_dir: true,
                    children: vec![file("b.mp4", 300), file("c.txt", 50), RawNode { name: "e".into(), is_dir: true, children: vec![file("f.txt", 10)], ..Default::default() }],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let t = Tree::from_raw(raw);
        let d = t.find(Path::new("/r/d")).unwrap();
        let s = summarize(&t, d, 2);
        assert_eq!(s.dirs, 1);
        assert_eq!(s.types.len(), 2);
        assert_eq!(&*t.types[s.types[0].kind as usize].ext, "mp4");
        assert_eq!(s.types[0].size, 300);
        assert_eq!(s.types[1].files, 2, "txt: c и f");
        let names: Vec<&str> = s.largest.iter().map(|&id| &*t.node(id).name).collect();
        assert_eq!(names, ["b.mp4", "c.txt"], "топ-2 по размеру");
    }

    #[test]
    fn patch_after_file_operations() {
        let d = std::env::temp_dir().join(format!("conchy-analysis-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("a")).unwrap();
        fs::write(d.join("a/x.bin"), vec![0u8; 1000]).unwrap();
        fs::write(d.join("y.bin"), vec![0u8; 10]).unwrap();
        let mut t = Tree::from_raw(scan(&d, &ScanOptions::default(), &Progress::default()).unwrap());
        assert_eq!(t.node(Tree::ROOT).size, 1010);

        // Перенесли x.bin из a в корень, удалили y.bin.
        fs::rename(d.join("a/x.bin"), d.join("x.bin")).unwrap();
        fs::remove_file(d.join("y.bin")).unwrap();
        let touched = vec![d.join("a/x.bin"), d.join("x.bin"), d.join("y.bin"), PathBuf::from("/elsewhere")];
        assert!(patch(&mut t, rescan(&touched)));
        assert!(t.find(&d.join("a/x.bin")).is_none());
        assert!(t.find(&d.join("y.bin")).is_none());
        assert_eq!(t.node(t.find(&d.join("x.bin")).unwrap()).size, 1000);
        assert_eq!(t.node(Tree::ROOT).size, 1000);
        assert_eq!(t.node(t.find(&d.join("a")).unwrap()).size, 0);
        assert_eq!(root_path(&t), d);
        fs::remove_dir_all(&d).unwrap();
    }
}
