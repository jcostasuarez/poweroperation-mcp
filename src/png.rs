//! BMP (24/32 bits, sin compresión) a PNG, para las miniaturas de Graphics Builder y `po_runtime_screenshot`: los
//! clientes MCP muestran PNG pero no BMP. Deflate propio (LZ77 + Huffman fijo) con filtro adaptativo por fila, sin
//! otra dependencia: las pantallas SCADA tienen grandes zonas de un color y comprimen mucho (antes iban "stored",
//! ~2 MB a 900×675).

/// Convierte un BMP de 24 o 32 bits a PNG RGB. `None` si el BMP tiene otro formato.
pub fn bmp_to_png(bmp: &[u8]) -> Option<Vec<u8>> {
    let u16_at = |o: usize| bmp.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let u32_at = |o: usize| bmp.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    if bmp.get(..2)? != b"BM" {
        return None;
    }
    let data_off = u32_at(10)? as usize;
    let width = u32_at(18)? as i32;
    let height = u32_at(22)? as i32;
    let bpp = u16_at(28)? as usize;
    let compression = u32_at(30)?;
    // 0 = BI_RGB; 3 = BI_BITFIELDS (32 bits BGRA con las máscaras estándar).
    if width <= 0 || height == 0 || !(bpp == 24 || bpp == 32) || !(compression == 0 || compression == 3) {
        return None;
    }
    let (w, h) = (width as usize, height.unsigned_abs() as usize);
    let stride = (w * bpp / 8).div_ceil(4) * 4;
    let mut rgb = Vec::with_capacity(h * w * 3);
    for y in 0..h {
        // Positivo = filas de abajo hacia arriba.
        let src_row = if height > 0 { h - 1 - y } else { y };
        let row = bmp.get(data_off + src_row * stride..data_off + src_row * stride + w * bpp / 8)?;
        for px in row.chunks_exact(bpp / 8) {
            rgb.extend_from_slice(&[px[2], px[1], px[0]]);
        }
    }
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(w as u32).to_be_bytes());
    ihdr.extend_from_slice(&(h as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8 bits, RGB, deflate, filtro adaptativo, sin entrelazado
    chunk(&mut png, b"IHDR", &ihdr);
    chunk(&mut png, b"IDAT", &zlib(&filter_rows(&rgb, w * 3, 3)));
    chunk(&mut png, b"IEND", &[]);
    Some(png)
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// Filtra cada fila (`row_len` bytes, `bpp` bytes por píxel) con el filtro PNG que deja la menor suma de valores
/// absolutos (la heurística que recomienda la especificación). Cada fila sale con su byte de filtro adelante.
fn filter_rows(data: &[u8], row_len: usize, bpp: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / row_len.max(1));
    let zero = vec![0u8; row_len];
    let mut cand: [Vec<u8>; 5] = Default::default();
    for (y, row) in data.chunks_exact(row_len).enumerate() {
        let up = if y == 0 { &zero[..] } else { &data[(y - 1) * row_len..y * row_len] };
        for (f, c) in cand.iter_mut().enumerate() {
            c.clear();
            for i in 0..row_len {
                let a = if i >= bpp { row[i - bpp] } else { 0 };
                let b = up[i];
                let cc = if i >= bpp { up[i - bpp] } else { 0 };
                let pred = match f {
                    0 => 0,
                    1 => a,
                    2 => b,
                    3 => ((a as u16 + b as u16) / 2) as u8,
                    _ => paeth(a, b, cc),
                };
                c.push(row[i].wrapping_sub(pred));
            }
        }
        let cost = |c: &Vec<u8>| c.iter().map(|&x| (x as i8).unsigned_abs() as u32).sum::<u32>();
        let best = (0..5).min_by_key(|&f| cost(&cand[f])).unwrap_or(0);
        out.push(best as u8);
        out.extend_from_slice(&cand[best]);
    }
    out
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = a as i16 + b as i16 - c as i16;
    let (pa, pb, pc) = ((p - a as i16).abs(), (p - b as i16).abs(), (p - c as i16).abs());
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// Bits de deflate: se escriben del menos significativo al más significativo.
struct Bits {
    out: Vec<u8>,
    acc: u64,
    n: u32,
}

impl Bits {
    fn put(&mut self, v: u32, len: u32) {
        self.acc |= (v as u64) << self.n;
        self.n += len;
        while self.n >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
    }

    /// Código Huffman: va con el bit más significativo primero.
    fn code(&mut self, code: u32, len: u32) {
        self.put(code.reverse_bits() >> (32 - len), len);
    }

    fn finish(mut self) -> Vec<u8> {
        if self.n > 0 {
            self.out.push(self.acc as u8);
        }
        self.out
    }
}

const LEN_BASE: [u16; 29] = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const LEN_EXTRA: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DIST_BASE: [u16; 30] =
    [1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577];
const DIST_EXTRA: [u8; 30] = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];

/// Símbolo 0..=287 con el código Huffman fijo de deflate (RFC 1951 §3.2.6).
fn fixed_symbol(bits: &mut Bits, sym: u16) {
    let s = sym as u32;
    match sym {
        0..=143 => bits.code(0x30 + s, 8),
        144..=255 => bits.code(0x190 + s - 144, 9),
        256..=279 => bits.code(s - 256, 7),
        _ => bits.code(0xC0 + s - 280, 8),
    }
}

const WINDOW: usize = 32_768;
const MIN_MATCH: usize = 3;
const MAX_MATCH: usize = 258;
/// Candidatos que se prueban por posición: más es más lento y apenas comprime más.
const MAX_CHAIN: usize = 64;
const HASH_BITS: u32 = 15;

fn hash3(d: &[u8], i: usize) -> usize {
    ((d[i] as usize) << 10 ^ (d[i + 1] as usize) << 5 ^ d[i + 2] as usize) & ((1 << HASH_BITS) - 1)
}

/// Un solo bloque deflate con Huffman fijo; coincidencias LZ77 por cadenas de hash (greedy).
fn deflate(data: &[u8]) -> Vec<u8> {
    let mut bits = Bits { out: Vec::with_capacity(data.len() / 8 + 16), acc: 0, n: 0 };
    bits.put(1, 1); // BFINAL
    bits.put(1, 2); // BTYPE = 01 (Huffman fijo)
    let mut head = vec![usize::MAX; 1 << HASH_BITS];
    let mut prev = vec![usize::MAX; WINDOW];
    let insert = |i: usize, head: &mut Vec<usize>, prev: &mut Vec<usize>| {
        if i + MIN_MATCH <= data.len() {
            let h = hash3(data, i);
            prev[i % WINDOW] = head[h];
            head[h] = i;
        }
    };
    let mut i = 0;
    while i < data.len() {
        let (mut best_len, mut best_dist) = (0, 0);
        if i + MIN_MATCH <= data.len() {
            let max = MAX_MATCH.min(data.len() - i);
            let mut cand = head[hash3(data, i)];
            let mut chain = 0;
            // `cand < i` siempre; la distancia se limita a la ventana (las entradas viejas de `prev` quedan fuera).
            while cand != usize::MAX && i - cand < WINDOW && chain < MAX_CHAIN {
                if data[cand + best_len.min(max - 1)] == data[i + best_len.min(max - 1)] {
                    let len = data[cand..cand + max].iter().zip(&data[i..i + max]).take_while(|(a, b)| a == b).count();
                    if len > best_len {
                        (best_len, best_dist) = (len, i - cand);
                        if len == max {
                            break;
                        }
                    }
                }
                let next = prev[cand % WINDOW];
                if next == usize::MAX || next >= cand {
                    break;
                }
                cand = next;
                chain += 1;
            }
        }
        if best_len >= MIN_MATCH {
            let li = LEN_BASE.partition_point(|&b| b as usize <= best_len) - 1;
            fixed_symbol(&mut bits, 257 + li as u16);
            bits.put((best_len - LEN_BASE[li] as usize) as u32, LEN_EXTRA[li] as u32);
            let di = DIST_BASE.partition_point(|&b| b as usize <= best_dist) - 1;
            bits.code(di as u32, 5);
            bits.put((best_dist - DIST_BASE[di] as usize) as u32, DIST_EXTRA[di] as u32);
            for k in i..i + best_len {
                insert(k, &mut head, &mut prev);
            }
            i += best_len;
        } else {
            fixed_symbol(&mut bits, data[i] as u16);
            insert(i, &mut head, &mut prev);
            i += 1;
        }
    }
    fixed_symbol(&mut bits, 256); // fin de bloque
    bits.finish()
}

/// Flujo zlib (cabecera, deflate, Adler-32).
fn zlib(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x9C];
    out.extend(deflate(data));
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut s) = (1u32, 0u32);
    for &x in data {
        a = (a + x as u32) % 65_521;
        s = (s + a) % 65_521;
    }
    (s << 16) | a
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { 0xEDB8_8320 ^ (crc >> 1) } else { crc >> 1 };
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Inflate mínimo (bloques stored y Huffman fijo), independiente del compresor, para verificarlo.
    fn inflate(z: &[u8]) -> Vec<u8> {
        assert_eq!((z[0] as u16 * 256 + z[1] as u16) % 31, 0, "cabecera zlib");
        let data = &z[2..];
        let mut pos = 0usize; // en bits
        let bit = |pos: &mut usize| {
            let b = (data[*pos / 8] >> (*pos % 8)) & 1;
            *pos += 1;
            b as u32
        };
        let mut out: Vec<u8> = Vec::new();
        loop {
            let last = bit(&mut pos);
            let kind = bit(&mut pos) | bit(&mut pos) << 1;
            match kind {
                0 => {
                    pos = pos.div_ceil(8) * 8;
                    let p = pos / 8;
                    let len = u16::from_le_bytes([data[p], data[p + 1]]) as usize;
                    out.extend_from_slice(&data[p + 4..p + 4 + len]);
                    pos += (4 + len) * 8;
                }
                1 => loop {
                    let mut code = 0u32;
                    for _ in 0..7 {
                        code = code << 1 | bit(&mut pos);
                    }
                    let sym = if code <= 23 {
                        256 + code
                    } else {
                        code = code << 1 | bit(&mut pos);
                        match code {
                            0x30..=0xBF => code - 0x30,
                            0xC0..=0xC7 => 280 + code - 0xC0,
                            _ => {
                                code = code << 1 | bit(&mut pos);
                                144 + code - 0x190
                            }
                        }
                    };
                    let extra = |pos: &mut usize, n: u8| (0..n).fold(0u32, |v, k| v | bit(pos) << k);
                    match sym {
                        0..=255 => out.push(sym as u8),
                        256 => break,
                        _ => {
                            let li = (sym - 257) as usize;
                            let len = LEN_BASE[li] as usize + extra(&mut pos, LEN_EXTRA[li]) as usize;
                            let mut di = 0u32;
                            for _ in 0..5 {
                                di = di << 1 | bit(&mut pos);
                            }
                            let dist = DIST_BASE[di as usize] as usize + extra(&mut pos, DIST_EXTRA[di as usize]) as usize;
                            for _ in 0..len {
                                out.push(out[out.len() - dist]);
                            }
                        }
                    }
                },
                _ => panic!("bloque deflate tipo {kind} no esperado"),
            }
            if last == 1 {
                break;
            }
        }
        let tail = &z[z.len() - 4..];
        assert_eq!(u32::from_be_bytes([tail[0], tail[1], tail[2], tail[3]]), adler32(&out), "Adler-32");
        out
    }

    /// Datos IDAT descomprimidos y sin filtrar → píxeles RGB.
    fn unfilter(raw: &[u8], row_len: usize, bpp: usize) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        for (y, r) in raw.chunks_exact(row_len + 1).enumerate() {
            let start = out.len();
            for i in 0..row_len {
                let a = if i >= bpp { out[start + i - bpp] } else { 0 };
                let b = if y > 0 { out[start + i - row_len] } else { 0 };
                let c = if y > 0 && i >= bpp { out[start + i - row_len - bpp] } else { 0 };
                let pred = match r[0] {
                    0 => 0,
                    1 => a,
                    2 => b,
                    3 => ((a as u16 + b as u16) / 2) as u8,
                    4 => paeth(a, b, c),
                    f => panic!("filtro {f}"),
                };
                out.push(r[1 + i].wrapping_add(pred));
            }
        }
        out
    }

    fn idat(png: &[u8]) -> &[u8] {
        let at = png.windows(4).position(|w| w == b"IDAT").unwrap();
        let len = u32::from_be_bytes(png[at - 4..at].try_into().unwrap()) as usize;
        &png[at + 4..at + 4 + len]
    }

    /// BMP de w×h a 32 bits de arriba abajo con los píxeles BGRA de `px(x, y)`.
    fn bmp32(w: usize, h: usize, px: impl Fn(usize, usize) -> [u8; 3]) -> Vec<u8> {
        let mut b = vec![0u8; 54];
        b[..2].copy_from_slice(b"BM");
        b[10] = 54;
        b[14] = 40;
        b[18..22].copy_from_slice(&(w as i32).to_le_bytes());
        b[22..26].copy_from_slice(&(-(h as i32)).to_le_bytes());
        b[26] = 1;
        b[28] = 32;
        for y in 0..h {
            for x in 0..w {
                let [r, g, bl] = px(x, y);
                b.extend_from_slice(&[bl, g, r, 0]);
            }
        }
        b
    }

    /// BMP de 2×2 a 24 bits (abajo hacia arriba): rojo, verde / azul, blanco.
    fn bmp_2x2() -> Vec<u8> {
        let mut b = vec![0u8; 54];
        b[..2].copy_from_slice(b"BM");
        b[10] = 54;
        b[14] = 40;
        b[18] = 2;
        b[22] = 2;
        b[26] = 1;
        b[28] = 24;
        // Fila de abajo (azul, blanco) y luego la de arriba (rojo, verde); BGR + 2 bytes de relleno.
        b.extend_from_slice(&[255, 0, 0, 255, 255, 255, 0, 0]);
        b.extend_from_slice(&[0, 0, 255, 0, 255, 0, 0, 0]);
        b
    }

    #[test]
    fn converts_bmp() {
        let png = bmp_to_png(&bmp_2x2()).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        let rgb = unfilter(&inflate(idat(&png)), 6, 3);
        assert_eq!(rgb, [255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255]);
        assert_eq!(crc32(b"IEND"), 0xAE42_6082);
    }

    #[test]
    fn deflate_round_trips() {
        let mut noise = Vec::new();
        let mut x = 12345u32;
        for _ in 0..70_000 {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            noise.push((x >> 16) as u8);
        }
        let mut text = Vec::new();
        for i in 0..5000 {
            text.extend_from_slice(format!("Feeder{}\\MMXU1\\TotW;", i % 37).as_bytes());
        }
        let cases: [&[u8]; 6] = [b"", b"a", b"abcabcabcabcabcabc", &[0u8; 100_000], &noise, &text];
        for data in cases {
            assert_eq!(inflate(&zlib(data)), data, "largo {}", data.len());
        }
        // Distancias más allá de la ventana: el bloque de 40 KB se repite (solo lo alcanzan coincidencias < 32 KB).
        let mut far = noise[..40_000].to_vec();
        far.extend_from_slice(&noise[..40_000]);
        assert_eq!(inflate(&zlib(&far)), far);
    }

    #[test]
    fn screenshots_compress() {
        // Pantalla tipo SCADA: fondo gris, un rectángulo de color, líneas y un degradé (no es solo un color).
        let (w, h) = (300, 200);
        let px = |x: usize, y: usize| {
            if (40..120).contains(&x) && (30..90).contains(&y) {
                [0, 176, 0]
            } else if y % 50 == 0 || x % 75 == 0 {
                [0, 0, 0]
            } else if y > 150 {
                [x as u8, 100, 200]
            } else {
                [192, 192, 192]
            }
        };
        let png = bmp_to_png(&bmp32(w, h, px)).unwrap();
        let rgb = unfilter(&inflate(idat(&png)), w * 3, 3);
        let expected: Vec<u8> = (0..h).flat_map(|y| (0..w).flat_map(move |x| px(x, y))).collect();
        assert_eq!(rgb, expected);
        assert!(png.len() * 30 < w * h * 3, "PNG de {} bytes para {} sin comprimir", png.len(), w * h * 3);
    }

    #[test]
    fn rejects_other_formats() {
        assert!(bmp_to_png(b"\x89PNG....").is_none());
        let mut b = bmp_2x2();
        b[28] = 8;
        assert!(bmp_to_png(&b).is_none());
    }
}
