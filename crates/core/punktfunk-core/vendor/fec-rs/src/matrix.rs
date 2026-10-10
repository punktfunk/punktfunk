use crate::galois;

#[derive(PartialEq, Debug, Clone)]
pub struct Matrix {
    pub row_count: usize,
    pub col_count: usize,
    pub data: Vec<u8>,
}

impl Matrix {
    pub fn new(rows: usize, cols: usize) -> Self {
        Self {
            row_count: rows,
            col_count: cols,
            data: vec![0u8; rows * cols],
        }
    }

    pub fn identity(size: usize) -> Self {
        let mut m = Self::new(size, size);
        for i in 0..size {
            m.data[i * size + i] = 1;
        }
        m
    }

    pub fn vandermonde(rows: usize, cols: usize) -> Self {
        let mut m = Self::new(rows, cols);
        for r in 0..rows {
            let r_a = r as u8;
            for c in 0..cols {
                m.data[r * cols + c] = galois::exp(r_a, c);
            }
        }
        m
    }

    #[inline]
    pub fn get(&self, r: usize, c: usize) -> u8 {
        self.data[r * self.col_count + c]
    }

    #[inline]
    pub fn set(&mut self, r: usize, c: usize, val: u8) {
        self.data[r * self.col_count + c] = val;
    }

    pub fn get_row(&self, row: usize) -> &[u8] {
        let start = row * self.col_count;
        &self.data[start..start + self.col_count]
    }

    pub fn sub_matrix(&self, rmin: usize, cmin: usize, rmax: usize, cmax: usize) -> Self {
        let new_rows = rmax - rmin;
        let new_cols = cmax - cmin;
        let mut m = Self::new(new_rows, new_cols);
        for r in rmin..rmax {
            for c in cmin..cmax {
                m.data[(r - rmin) * new_cols + (c - cmin)] = self.get(r, c);
            }
        }
        m
    }

    pub fn multiply(&self, rhs: &Matrix) -> Self {
        assert_eq!(
            self.col_count, rhs.row_count,
            "Matrix dimensions incompatible for multiply"
        );
        let mut result = Self::new(self.row_count, rhs.col_count);
        for r in 0..self.row_count {
            for c in 0..rhs.col_count {
                let mut val = 0u8;
                for i in 0..self.col_count {
                    val = galois::add(val, galois::mul(self.get(r, i), rhs.get(i, c)));
                }
                result.set(r, c, val);
            }
        }
        result
    }

    pub fn augment(&self, rhs: &Matrix) -> Self {
        assert_eq!(
            self.row_count, rhs.row_count,
            "Matrix row counts must match for augment"
        );
        let new_cols = self.col_count + rhs.col_count;
        let mut m = Self::new(self.row_count, new_cols);
        for r in 0..self.row_count {
            for c in 0..self.col_count {
                m.set(r, c, self.get(r, c));
            }
            for c in 0..rhs.col_count {
                m.set(r, self.col_count + c, rhs.get(r, c));
            }
        }
        m
    }

    fn swap_rows(&mut self, r1: usize, r2: usize) {
        if r1 == r2 {
            return;
        }
        let s1 = r1 * self.col_count;
        let s2 = r2 * self.col_count;
        for i in 0..self.col_count {
            self.data.swap(s1 + i, s2 + i);
        }
    }

    fn gaussian_elim(&mut self) -> Result<(), &'static str> {
        for r in 0..self.row_count {
            // Pivot search
            if self.get(r, r) == 0 {
                for r_below in r + 1..self.row_count {
                    if self.get(r_below, r) != 0 {
                        self.swap_rows(r, r_below);
                        break;
                    }
                }
            }
            if self.get(r, r) == 0 {
                return Err("Singular matrix");
            }
            // Scale to 1
            if self.get(r, r) != 1 {
                let scale = galois::div(1, self.get(r, r));
                for c in 0..self.col_count {
                    let val = galois::mul(scale, self.get(r, c));
                    self.set(r, c, val);
                }
            }
            // Eliminate below
            for r_below in r + 1..self.row_count {
                if self.get(r_below, r) != 0 {
                    let scale = self.get(r_below, r);
                    for c in 0..self.col_count {
                        let val =
                            galois::add(self.get(r_below, c), galois::mul(scale, self.get(r, c)));
                        self.set(r_below, c, val);
                    }
                }
            }
        }

        // Back substitution
        for d in 0..self.row_count {
            for r_above in 0..d {
                if self.get(r_above, d) != 0 {
                    let scale = self.get(r_above, d);
                    for c in 0..self.col_count {
                        let val =
                            galois::add(self.get(r_above, c), galois::mul(scale, self.get(d, c)));
                        self.set(r_above, c, val);
                    }
                }
            }
        }
        Ok(())
    }

    pub fn invert(&self) -> Result<Self, &'static str> {
        assert!(
            self.row_count == self.col_count,
            "Cannot invert non-square matrix"
        );
        let mut work = self.augment(&Self::identity(self.row_count));
        work.gaussian_elim()?;
        Ok(work.sub_matrix(0, self.row_count, self.col_count, self.col_count * 2))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mat(data: Vec<Vec<u8>>) -> Matrix {
        let rows = data.len();
        let cols = data[0].len();
        let flat: Vec<u8> = data.into_iter().flatten().collect();
        Matrix {
            row_count: rows,
            col_count: cols,
            data: flat,
        }
    }

    #[test]
    fn test_identity() {
        let m = Matrix::identity(3);
        let expected = mat(vec![vec![1, 0, 0], vec![0, 1, 0], vec![0, 0, 1]]);
        assert_eq!(m, expected);
    }

    #[test]
    fn test_multiply() {
        let m1 = mat(vec![vec![1, 2], vec![3, 4]]);
        let m2 = mat(vec![vec![5, 6], vec![7, 8]]);
        let result = m1.multiply(&m2);
        let expected = mat(vec![vec![11, 22], vec![19, 42]]);
        assert_eq!(result, expected);
    }

    #[test]
    fn test_invert() {
        let m = mat(vec![
            vec![56, 23, 98],
            vec![3, 100, 200],
            vec![45, 201, 123],
        ]);
        let inv = m.invert().unwrap();
        let expected = mat(vec![
            vec![175, 133, 33],
            vec![130, 13, 245],
            vec![112, 35, 126],
        ]);
        assert_eq!(inv, expected);
    }

    #[test]
    fn test_invert_identity() {
        let m = Matrix::identity(4);
        let inv = m.invert().unwrap();
        assert_eq!(inv, m);
    }

    #[test]
    fn test_multiply_identity() {
        let m = mat(vec![
            vec![56, 23, 98],
            vec![3, 100, 200],
            vec![45, 201, 123],
        ]);
        let id = Matrix::identity(3);
        assert_eq!(m.multiply(&id), m);
        assert_eq!(id.multiply(&m), m);
    }

    #[test]
    fn test_invert_times_original_is_identity() {
        let m = mat(vec![
            vec![56, 23, 98],
            vec![3, 100, 200],
            vec![45, 201, 123],
        ]);
        let inv = m.invert().unwrap();
        let product = m.multiply(&inv);
        assert_eq!(product, Matrix::identity(3));
    }
}
