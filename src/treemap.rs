//! Squarified treemap (Bruls, Huizing, van Wijk, 2000).
//!
//! Раскладывает значения по прямоугольнику так, чтобы ячейки были близки к квадратам:
//! тогда их легко сравнивать глазами и в них помещаются подписи.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    pub fn area(&self) -> f32 {
        self.w * self.h
    }

    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }

    pub fn inset(&self, d: f32) -> Rect {
        Rect::new(self.x + d, self.y + d, (self.w - 2.0 * d).max(0.0), (self.h - 2.0 * d).max(0.0))
    }
}

/// Раскладывает `values` (по убыванию, неотрицательные) в `bounds`.
/// Возвращает прямоугольники в том же порядке. Нулевые значения получают пустой прямоугольник.
pub fn squarify(values: &[f64], bounds: Rect) -> Vec<Rect> {
    let mut out = vec![Rect::new(bounds.x, bounds.y, 0.0, 0.0); values.len()];
    let total: f64 = values.iter().sum();
    if total <= 0.0 || bounds.w <= 0.0 || bounds.h <= 0.0 {
        return out;
    }
    let scale = (bounds.w as f64 * bounds.h as f64) / total;
    let areas: Vec<f64> = values.iter().map(|v| v * scale).collect();

    let mut free = (bounds.x as f64, bounds.y as f64, bounds.w as f64, bounds.h as f64);
    let mut start = 0;
    while start < areas.len() && areas[start] > 0.0 {
        let side = free.2.min(free.3);
        let mut end = start + 1;
        let mut row_sum = areas[start];
        let mut best = worst(&areas[start..end], row_sum, side);
        while end < areas.len() && areas[end] > 0.0 {
            let s = row_sum + areas[end];
            let w = worst(&areas[start..=end], s, side);
            if w > best {
                break;
            }
            best = w;
            row_sum = s;
            end += 1;
        }

        // Кладём ряд вдоль короткой стороны свободной области.
        let (fx, fy, fw, fh) = free;
        if fw >= fh {
            // Колонка слева.
            let col_w = if fh > 0.0 { row_sum / fh } else { 0.0 };
            let mut y = fy;
            for i in start..end {
                let h = if col_w > 0.0 { areas[i] / col_w } else { 0.0 };
                out[i] = Rect::new(fx as f32, y as f32, col_w as f32, h as f32);
                y += h;
            }
            free = (fx + col_w, fy, (fw - col_w).max(0.0), fh);
        } else {
            // Строка сверху.
            let row_h = if fw > 0.0 { row_sum / fw } else { 0.0 };
            let mut x = fx;
            for i in start..end {
                let w = if row_h > 0.0 { areas[i] / row_h } else { 0.0 };
                out[i] = Rect::new(x as f32, fy as f32, w as f32, row_h as f32);
                x += w;
            }
            free = (fx, fy + row_h, fw, (fh - row_h).max(0.0));
        }
        start = end;
    }
    out
}

/// Худшее соотношение сторон в ряду (чем ближе к 1, тем лучше).
fn worst(row: &[f64], sum: f64, side: f64) -> f64 {
    let (mut lo, mut hi) = (f64::MAX, 0.0f64);
    for &a in row {
        lo = lo.min(a);
        hi = hi.max(a);
    }
    let s2 = sum * sum;
    let w2 = side * side;
    (w2 * hi / s2).max(s2 / (w2 * lo))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classic_example_fills_area_without_overlap() {
        // Пример из статьи: 6x4 и значения 6,6,4,3,2,2,1.
        let vals = [6.0, 6.0, 4.0, 3.0, 2.0, 2.0, 1.0];
        let b = Rect::new(0.0, 0.0, 6.0, 4.0);
        let rs = squarify(&vals, b);
        let total: f32 = rs.iter().map(Rect::area).sum();
        assert!((total - 24.0).abs() < 1e-3);
        for (r, v) in rs.iter().zip(vals) {
            assert!((r.area() - v as f32).abs() < 1e-3);
            assert!(r.x >= -1e-4 && r.y >= -1e-4 && r.x + r.w <= 6.0 + 1e-3 && r.y + r.h <= 4.0 + 1e-3);
        }
        for i in 0..rs.len() {
            for j in i + 1..rs.len() {
                let (a, c) = (rs[i], rs[j]);
                let ox = (a.x + a.w).min(c.x + c.w) - a.x.max(c.x);
                let oy = (a.y + a.h).min(c.y + c.h) - a.y.max(c.y);
                assert!(ox <= 1e-3 || oy <= 1e-3, "ячейки {i} и {j} пересекаются");
            }
        }
        // Первые две ячейки из статьи — 3x2.
        assert!((rs[0].w - 3.0).abs() < 1e-3 && (rs[0].h - 2.0).abs() < 1e-3);
    }

    #[test]
    fn zeros_and_empty() {
        assert!(squarify(&[], Rect::new(0.0, 0.0, 10.0, 10.0)).is_empty());
        let rs = squarify(&[5.0, 0.0], Rect::new(0.0, 0.0, 10.0, 10.0));
        assert!((rs[0].area() - 100.0).abs() < 1e-3);
        assert_eq!(rs[1].area(), 0.0);
    }
}
