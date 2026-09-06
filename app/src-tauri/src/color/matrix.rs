//! 色域转换矩阵
//!
//! 计算 BT.2020 ↔ BT.709 等色域之间的线性光转换矩阵。

/// CIE 色度坐标 (x, y)
#[derive(Clone, Copy, Debug)]
pub struct Chromaticity {
    pub x: f32,
    pub y: f32,
}

/// 色域 primaries + 白点
#[derive(Clone, Copy, Debug)]
pub struct ColorPrimaries {
    pub r: Chromaticity,
    pub g: Chromaticity,
    pub b: Chromaticity,
    pub white: Chromaticity,
}

/// D65 白点
pub const D65: Chromaticity = Chromaticity {
    x: 0.3127,
    y: 0.3290,
};

/// BT.709 (sRGB / HDTV)
pub const BT709: ColorPrimaries = ColorPrimaries {
    r: Chromaticity { x: 0.640, y: 0.330 },
    g: Chromaticity { x: 0.300, y: 0.600 },
    b: Chromaticity { x: 0.150, y: 0.060 },
    white: D65,
};

/// BT.2020 (UHDTV / HDR)
pub const BT2020: ColorPrimaries = ColorPrimaries {
    r: Chromaticity { x: 0.708, y: 0.292 },
    g: Chromaticity { x: 0.170, y: 0.797 },
    b: Chromaticity { x: 0.131, y: 0.046 },
    white: D65,
};

/// DCI-P3 (影院 / 部分 HDR 显示器)
pub const DCI_P3: ColorPrimaries = ColorPrimaries {
    r: Chromaticity { x: 0.680, y: 0.320 },
    g: Chromaticity { x: 0.265, y: 0.690 },
    b: Chromaticity { x: 0.150, y: 0.060 },
    white: D65,
};

/// 3x3 矩阵（行主序）
#[derive(Clone, Copy, Debug)]
pub struct Mat3 {
    pub m: [[f32; 3]; 3],
}

impl Mat3 {
    pub const IDENTITY: Mat3 = Mat3 {
        m: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
    };

    /// 矩阵乘法 self * other
    pub fn mul(&self, other: &Mat3) -> Mat3 {
        let mut r = [[0f32; 3]; 3];
        for i in 0..3 {
            for j in 0..3 {
                let mut s = 0.0;
                for k in 0..3 {
                    s += self.m[i][k] * other.m[k][j];
                }
                r[i][j] = s;
            }
        }
        Mat3 { m: r }
    }

    /// 应用到 RGB 向量
    #[inline]
    pub fn apply(&self, r: f32, g: f32, b: f32) -> (f32, f32, f32) {
        (
            self.m[0][0] * r + self.m[0][1] * g + self.m[0][2] * b,
            self.m[1][0] * r + self.m[1][1] * g + self.m[1][2] * b,
            self.m[2][0] * r + self.m[2][1] * g + self.m[2][2] * b,
        )
    }

    /// 3x3 矩阵求逆（高斯消元）
    pub fn inverse(&self) -> Mat3 {
        let a = &self.m;
        let det = a[0][0] * (a[1][1] * a[2][2] - a[1][2] * a[2][1])
            - a[0][1] * (a[1][0] * a[2][2] - a[1][2] * a[2][0])
            + a[0][2] * (a[1][0] * a[2][1] - a[1][1] * a[2][0]);
        if det.abs() < 1e-12 {
            return Mat3::IDENTITY;
        }
        let inv_det = 1.0 / det;
        Mat3 {
            m: [
                [
                    (a[1][1] * a[2][2] - a[1][2] * a[2][1]) * inv_det,
                    (a[0][2] * a[2][1] - a[0][1] * a[2][2]) * inv_det,
                    (a[0][1] * a[1][2] - a[0][2] * a[1][1]) * inv_det,
                ],
                [
                    (a[1][2] * a[2][0] - a[1][0] * a[2][2]) * inv_det,
                    (a[0][0] * a[2][2] - a[0][2] * a[2][0]) * inv_det,
                    (a[0][2] * a[1][0] - a[0][0] * a[1][2]) * inv_det,
                ],
                [
                    (a[1][0] * a[2][1] - a[1][1] * a[2][0]) * inv_det,
                    (a[0][1] * a[2][0] - a[0][0] * a[2][1]) * inv_det,
                    (a[0][0] * a[1][1] - a[0][1] * a[1][0]) * inv_det,
                ],
            ],
        }
    }
}

/// 色度坐标 → XYZ（归一化使 Y=1）
fn chrom_to_xyz(c: Chromaticity) -> [f32; 3] {
    if c.y < 1e-10 {
        return [0.0, 0.0, 0.0];
    }
    [c.x / c.y, 1.0, (1.0 - c.x - c.y) / c.y]
}

/// 由 primaries 计算 RGB→XYZ 矩阵（线性光）
pub fn rgb_to_xyz_matrix(p: ColorPrimaries) -> Mat3 {
    let xr = chrom_to_xyz(p.r);
    let xg = chrom_to_xyz(p.g);
    let xb = chrom_to_xyz(p.b);
    let xw = chrom_to_xyz(p.white);

    // 矩阵 [xr xg xb; yr yg yb; zr zg zb]
    let mat = Mat3 {
        m: [
            [xr[0], xg[0], xb[0]],
            [xr[1], xg[1], xb[1]],
            [xr[2], xg[2], xb[2]],
        ],
    };
    let inv = mat.inverse();
    // S = inv * [xw; yw; zw]
    let s = inv.apply(xw[0], xw[1], xw[2]);
    // RGB→XYZ = [xr*s0, xg*s1, xb*s2; ...]
    Mat3 {
        m: [
            [xr[0] * s.0, xg[0] * s.1, xb[0] * s.2],
            [xr[1] * s.0, xg[1] * s.1, xb[1] * s.2],
            [xr[2] * s.0, xg[2] * s.1, xb[2] * s.2],
        ],
    }
}

/// 计算 src→dst 色域转换矩阵（线性光）
pub fn gamut_matrix(src: ColorPrimaries, dst: ColorPrimaries) -> Mat3 {
    let src_to_xyz = rgb_to_xyz_matrix(src);
    let dst_to_xyz = rgb_to_xyz_matrix(dst);
    // dst←src = (XYZ←dst)^-1 * (src→XYZ)
    dst_to_xyz.inverse().mul(&src_to_xyz)
}

/// 预计算：BT.2020 → BT.709
pub fn bt2020_to_bt709() -> Mat3 {
    gamut_matrix(BT2020, BT709)
}

/// 预计算：DCI-P3(D65) → BT.709
///
/// P3 超出 709 的部分产生负分量——scRGB 允许负值，DWM 高级合成会
/// 按显示器原生色域正确重现（比裁剪更准）。
pub fn p3_to_bt709() -> Mat3 {
    gamut_matrix(DCI_P3, BT709)
}

/// 预计算：BT.709 → BT.2020
pub fn bt709_to_bt2020() -> Mat3 {
    gamut_matrix(BT709, BT2020)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bt2020_to_709_whitepoint_preserved() {
        let m = bt2020_to_bt709();
        // D65 白 (1,1,1) 应映射到接近 (1,1,1)
        let (r, g, b) = m.apply(1.0, 1.0, 1.0);
        assert!((r - 1.0).abs() < 1e-3, "R={}", r);
        assert!((g - 1.0).abs() < 1e-3, "G={}", g);
        assert!((b - 1.0).abs() < 1e-3, "B={}", b);
    }

    #[test]
    fn matrix_inverse_roundtrip() {
        let m = rgb_to_xyz_matrix(BT2020);
        let inv = m.inverse();
        let id = m.mul(&inv);
        for i in 0..3 {
            for j in 0..3 {
                let expected = if i == j { 1.0 } else { 0.0 };
                assert!(
                    (id.m[i][j] - expected).abs() < 1e-4,
                    "[{}][{}]={}",
                    i,
                    j,
                    id.m[i][j]
                );
            }
        }
    }
}
