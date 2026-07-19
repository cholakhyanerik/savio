//! SHA-256 и разбор файла контрольных сумм.
//!
//! Своя реализация, а не крейт: алгоритм фиксирован стандартом FIPS 180-4 и
//! не меняется, кода на него — сотня строк, а набор зависимостей проекта
//! намеренно минимален. Проверяется на официальных векторах в тестах ниже.
//!
//! Считаем на лету, по мере загрузки: файл размером в сотню мегабайт незачем
//! перечитывать с диска ради хеша.

/// Константы раундов: дробные части кубических корней первых 64 простых чисел.
const K: [u32; 64] = [
    0x428a_2f98, 0x7137_4491, 0xb5c0_fbcf, 0xe9b5_dba5, 0x3956_c25b, 0x59f1_11f1, 0x923f_82a4,
    0xab1c_5ed5, 0xd807_aa98, 0x1283_5b01, 0x2431_85be, 0x550c_7dc3, 0x72be_5d74, 0x80de_b1fe,
    0x9bdc_06a7, 0xc19b_f174, 0xe49b_69c1, 0xefbe_4786, 0x0fc1_9dc6, 0x240c_a1cc, 0x2de9_2c6f,
    0x4a74_84aa, 0x5cb0_a9dc, 0x76f9_88da, 0x983e_5152, 0xa831_c66d, 0xb003_27c8, 0xbf59_7fc7,
    0xc6e0_0bf3, 0xd5a7_9147, 0x06ca_6351, 0x1429_2967, 0x27b7_0a85, 0x2e1b_2138, 0x4d2c_6dfc,
    0x5338_0d13, 0x650a_7354, 0x766a_0abb, 0x81c2_c92e, 0x9272_2c85, 0xa2bf_e8a1, 0xa81a_664b,
    0xc24b_8b70, 0xc76c_51a3, 0xd192_e819, 0xd699_0624, 0xf40e_3585, 0x106a_a070, 0x19a4_c116,
    0x1e37_6c08, 0x2748_774c, 0x34b0_bcb5, 0x391c_0cb3, 0x4ed8_aa4a, 0x5b9c_ca4f, 0x682e_6ff3,
    0x748f_82ee, 0x78a5_636f, 0x84c8_7814, 0x8cc7_0208, 0x90be_fffa, 0xa450_6ceb, 0xbef9_a3f7,
    0xc671_78f2,
];

/// Начальное состояние: дробные части квадратных корней первых восьми простых.
const INIT: [u32; 8] = [
    0x6a09_e667, 0xbb67_ae85, 0x3c6e_f372, 0xa54f_f53a, 0x510e_527f, 0x9b05_688c, 0x1f83_d9ab,
    0x5be0_cd19,
];

/// Потоковый счётчик SHA-256: `update` можно звать сколько угодно раз.
pub struct Sha256 {
    state: [u32; 8],
    /// Недобранный до 64 байт остаток предыдущего вызова `update`.
    block: [u8; 64],
    filled: usize,
    total_bytes: u64,
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256 {
    pub fn new() -> Self {
        Self {
            state: INIT,
            block: [0; 64],
            filled: 0,
            total_bytes: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.total_bytes = self.total_bytes.wrapping_add(data.len() as u64);

        // Сначала добираем хвост от прошлого вызова до полного блока.
        if self.filled > 0 {
            let need = (64 - self.filled).min(data.len());
            self.block[self.filled..self.filled + need].copy_from_slice(&data[..need]);
            self.filled += need;
            data = &data[need..];
            if self.filled == 64 {
                let block = self.block;
                self.compress(&block);
                self.filled = 0;
            }
        }

        // Целые блоки жмём прямо из входного среза, без копирования.
        while let Some((block, rest)) = data.split_first_chunk::<64>() {
            self.compress(block);
            data = rest;
        }

        if !data.is_empty() {
            self.block[..data.len()].copy_from_slice(data);
            self.filled = data.len();
        }
    }

    /// Дополняет сообщение по стандарту и отдаёт 32 байта хеша.
    pub fn finish(mut self) -> [u8; 32] {
        let bit_len = self.total_bytes.wrapping_mul(8);

        // Обязательная единица, затем нули, затем длина в битах big-endian.
        self.update(&[0x80]);
        // `update` увеличил счётчик, но на дополнение он не распространяется —
        // длину мы уже сняли выше, поэтому здесь просто добиваем нулями.
        while self.filled != 56 {
            self.update(&[0x00]);
        }
        self.update(&bit_len.to_be_bytes());
        debug_assert_eq!(self.filled, 0, "после дополнения блок обязан быть пуст");

        let mut out = [0u8; 32];
        for (chunk, word) in out.chunks_exact_mut(4).zip(self.state.iter()) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for (i, chunk) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }

        for (slot, value) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }
}

/// Шестнадцатеричная запись хеша в нижнем регистре — как в файлах сумм.
pub fn hex(digest: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Ищет сумму для файла в содержимом `SHA2-256SUMS`.
///
/// Формат GNU coreutils: сумма, два пробела, имя файла. Встречается и вариант
/// с ` *` (binary mode), поэтому разделитель разбираем терпимо — иначе проверка
/// молча не найдёт строку и мы посчитаем, что суммы для файла просто нет.
pub fn find_sum<'a>(sums: &'a str, file_name: &str) -> Option<&'a str> {
    sums.lines().find_map(|line| {
        let (sum, name) = line.split_once(' ')?;
        let name = name.trim_start_matches([' ', '*']).trim();
        (name == file_name && sum.len() == 64 && sum.bytes().all(|b| b.is_ascii_hexdigit()))
            .then_some(sum)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(data: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(data);
        hex(&h.finish())
    }

    /// Официальные векторы FIPS 180-4 / NIST.
    #[test]
    fn matches_known_vectors() {
        assert_eq!(
            digest(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            digest(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            digest(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    /// Ровно 55, 56 и 64 байта — границы, на которых ломается дополнение:
    /// при 56 длина уже не влезает в текущий блок и нужен ещё один.
    #[test]
    fn padding_boundaries_are_correct() {
        assert_eq!(
            digest(&[b'a'; 55]),
            "9f4390f8d30c2dd92ec9f095b65e2b9ae9b0a925a5258e241c9f1e910f734318"
        );
        assert_eq!(
            digest(&[b'a'; 56]),
            "b35439a4ac6f0948b6d6f9e3c6af0f5f590ce20f1bde7090ef7970686ec6738a"
        );
        assert_eq!(
            digest(&[b'a'; 64]),
            "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb"
        );
    }

    /// Хеш не должен зависеть от того, какими кусками пришли данные:
    /// из сети они приходят чанками произвольного размера.
    #[test]
    fn streaming_matches_single_shot() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        let one_shot = digest(&data);

        for chunk in [1usize, 7, 63, 64, 65, 128] {
            let mut h = Sha256::new();
            for part in data.chunks(chunk) {
                h.update(part);
            }
            assert_eq!(hex(&h.finish()), one_shot, "разбивка по {chunk} байт");
        }
    }

    #[test]
    fn sums_file_is_parsed() {
        let sums = concat!(
            "0000000000000000000000000000000000000000000000000000000000000001  yt-dlp\n",
            "0000000000000000000000000000000000000000000000000000000000000002  yt-dlp.exe\n",
            "0000000000000000000000000000000000000000000000000000000000000003 *yt-dlp_macos\n",
        );

        assert_eq!(find_sum(sums, "yt-dlp.exe").unwrap(), "0".repeat(63) + "2");
        // Имя не должно совпадать по префиксу: `yt-dlp` и `yt-dlp.exe` — разные файлы.
        assert_eq!(find_sum(sums, "yt-dlp").unwrap(), "0".repeat(63) + "1");
        // Вариант с `*` (binary mode) тоже обязан разбираться.
        assert_eq!(find_sum(sums, "yt-dlp_macos").unwrap(), "0".repeat(63) + "3");
        assert_eq!(find_sum(sums, "нет-такого"), None);
        assert_eq!(find_sum("мусор без сумм", "yt-dlp"), None);
    }
}
