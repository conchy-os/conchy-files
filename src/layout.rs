//! Раскладка treemap для дерева: вложенные папки, заголовки, группировка мелочи.

use crate::model::{NodeId, Tree};
use crate::treemap::{Rect, squarify};

/// Ячейка карты в пикселях окна.
#[derive(Debug, Clone)]
pub struct Cell {
    pub rect: Rect,
    pub kind: CellKind,
    pub depth: u8,
}

#[derive(Debug, Clone)]
pub enum CellKind {
    Node(NodeId),
    /// «ещё N элементов · X»: мелочь, которой не хватило места.
    Others { parent: NodeId, count: usize, size: u64 },
}

pub const HEADER_H: f32 = 15.0;
const MIN_AREA: f32 = 3.0;
const MAX_DEPTH: u8 = 14;
const MAX_CELLS: usize = 30000;

/// Раскладывает детей `root` внутри `bounds`. Родители идут раньше детей,
/// поэтому при поиске под курсором нужно идти с конца.
pub fn layout(tree: &Tree, root: NodeId, bounds: Rect) -> Vec<Cell> {
    let mut out = Vec::new();
    place_children(tree, root, bounds, 0, &mut out);
    out
}

fn place_children(tree: &Tree, parent: NodeId, area: Rect, depth: u8, out: &mut Vec<Cell>) {
    if area.w < 3.0 || area.h < 3.0 || out.len() >= MAX_CELLS {
        return;
    }
    let node = tree.node(parent);
    let total = node.size.max(1) as f64;
    let px_per_byte = area.area() as f64 / total;

    // Крупные дети получают свою ячейку, остальное — общий блок «ещё N».
    let mut values = Vec::new();
    let mut kept = Vec::new();
    let mut rest_size = 0u64;
    let mut rest_count = 0usize;
    for &c in &node.children {
        let s = tree.node(c).size;
        if s == 0 {
            continue;
        }
        if (s as f64 * px_per_byte) as f32 >= MIN_AREA && kept.len() < 5000 {
            values.push(s as f64);
            kept.push(c);
        } else {
            rest_size += s;
            rest_count += 1;
        }
    }
    if rest_count == 1 && rest_size > 0 {
        // Один «остаток» нет смысла прятать.
        let c = *node.children.iter().find(|&&c| {
            let s = tree.node(c).size;
            s > 0 && !kept.contains(&c)
        }).unwrap();
        values.push(rest_size as f64);
        kept.push(c);
        rest_count = 0;
    } else if rest_count > 0 {
        values.push(rest_size as f64);
    }

    let rects = squarify(&values, area);
    for (i, r) in rects.into_iter().enumerate() {
        if r.w < 1.0 || r.h < 1.0 {
            continue;
        }
        if i >= kept.len() {
            out.push(Cell { rect: r, kind: CellKind::Others { parent, count: rest_count, size: rest_size }, depth });
            continue;
        }
        let id = kept[i];
        out.push(Cell { rect: r, kind: CellKind::Node(id), depth });
        let child = tree.node(id);
        if child.is_dir && !child.children.is_empty() && depth < MAX_DEPTH && r.w > 6.0 && r.h > 6.0 {
            let mut inner = r.inset(1.0);
            if r.h > 40.0 && r.w > 50.0 {
                inner.y += HEADER_H;
                inner.h = (inner.h - HEADER_H).max(0.0);
            }
            place_children(tree, id, inner, depth + 1, out);
        }
    }
}

/// Самая глубокая ячейка под точкой.
pub fn hit(cells: &[Cell], x: f32, y: f32) -> Option<&Cell> {
    cells.iter().rev().find(|c| c.rect.contains(x, y))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::RawNode;

    fn file(name: &str, size: u64) -> RawNode {
        RawNode { name: name.into(), size, alloc: size, ..Default::default() }
    }

    #[test]
    fn groups_small_children_and_nests_dirs() {
        let mut kids: Vec<RawNode> = (0..1000).map(|i| file(&format!("tiny{i}"), 1)).collect();
        kids.push(RawNode {
            name: "big".into(),
            is_dir: true,
            size: 50_000,
            alloc: 50_000,
            children: vec![file("a.bin", 30_000), file("b.bin", 20_000)],
        });
        let raw = RawNode { name: "/r".into(), is_dir: true, size: 51_000, alloc: 0, children: kids };
        let t = Tree::from_raw(raw);
        let cells = layout(&t, Tree::ROOT, Rect::new(0.0, 0.0, 400.0, 300.0));

        let others: Vec<_> = cells.iter().filter(|c| matches!(c.kind, CellKind::Others { .. })).collect();
        assert_eq!(others.len(), 1, "1000 крошечных файлов собраны в один блок");
        if let CellKind::Others { count, .. } = others[0].kind {
            assert_eq!(count, 1000);
        }
        let nested = cells.iter().filter(|c| c.depth == 1).count();
        assert_eq!(nested, 2, "внутри папки big видны оба файла");

        // Под точкой внутри big находится файл, а не сама папка.
        let c = hit(&cells, 200.0, 150.0).unwrap();
        assert_eq!(c.depth, 1);
    }
}
