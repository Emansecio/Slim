//! Welcome / empty-state identity (DESIGN-SLIM-TUI §1.2, §15 empty state).
//!
//! The wordmark is a braille dot-matrix rendering of "SLIM" assembled from
//! per-letter bitmaps, so it stays crisp in every terminal font and degrades
//! to plain text on narrow viewports or `NO_COLOR`. The ambient pulse is the
//! single glyph allowed to animate while idle (spec §2: one glyph, ≤ 2 fps,
//! text and alignment stable); it freezes under reduced motion.

/// Letters used by the Slim wordmark, in order.
const WORD: &[&[&str]] = &[
    &[".###", "#...", "#...", ".##.", "...#", "...#", "###."], // S
    &["#....", "#....", "#....", "#....", "#....", "#....", "####."], // L
    &["###", ".#.", ".#.", ".#.", ".#.", ".#.", "###"], // I
    &["#...#", "##.##", "#.#.#", "#.#.#", "#...#", "#...#", "#...#"], // M
];

/// Ambient pulse glyphs (braille, 6 frames ≈ clockwise arc).
pub const PULSE: [char; 6] = ['\u{25dc}', '\u{25e0}', '\u{25dd}', '\u{25de}', '\u{25e1}', '\u{25df}'];

/// ASCII fallback pulse for terminals without braille coverage.
pub const PULSE_ASCII: [char; 4] = ['|', '/', '-', '\\'];

/// Braille dot value for (column, row) inside one cell, rows 0..=3.
fn dot_bit(col: usize, row: usize) -> u16 {
    const BITS: [[u16; 4]; 2] = [
        [0x01, 0x02, 0x04, 0x40], // left column
        [0x08, 0x10, 0x20, 0x80], // right column
    ];
    BITS[col][row]
}

/// Dot-matrix "SLIM" as braille lines (4 lines, equal display width).
/// Deterministic: same output every call, unit-tested.
pub fn wordmark() -> Vec<String> {
    const GAP: usize = 1;
    let rows = WORD[0].len(); // all letters share the 7-row grid
    let total_cols: usize = WORD.iter().map(|l| l[0].len()).sum::<usize>()
        + GAP * (WORD.len() - 1);
    let cols = total_cols + (total_cols % 2); // even: pairs feed braille cells

    let mut grid = vec![vec![false; cols]; rows];
    let mut offset = 0usize;
    for letter in WORD {
        for (y, line) in letter.iter().enumerate() {
            for (x, ch) in line.chars().enumerate() {
                if ch == '#' {
                    grid[y][offset + x] = true;
                }
            }
        }
        offset += letter[0].len() + GAP;
    }

    // Each braille cell stacks two adjacent dot rows: grid row `top` maps to
    // the cell's upper dots, `top + 1` to the lower dots (positions 1/2).
    let mut out = Vec::with_capacity((rows + 1) / 2);
    for pair in 0..(rows + 1) / 2 {
        let top = pair * 2;
        let bottom = top + 1; // may be past the grid → blank lower half
        let mut line = String::with_capacity(cols / 2);
        for cell in 0..cols / 2 {
            let mut bits = 0u16;
            for col in 0..2 {
                if grid[top][cell * 2 + col] {
                    bits |= dot_bit(col, 0);
                }
                if bottom < rows && grid[bottom][cell * 2 + col] {
                    bits |= dot_bit(col, 1);
                }
            }
            line.push(char::from_u32(0x2800 + bits as u32).unwrap_or('\u{2800}'));
        }
        out.push(line);
    }
    out
}

/// Pulse glyph for the given ambient frame (any u64; cycles).
pub fn pulse(frame: u64, ascii_fallback: bool) -> char {
    if ascii_fallback {
        PULSE_ASCII[(frame % PULSE_ASCII.len() as u64) as usize]
    } else {
        PULSE[(frame % PULSE.len() as u64) as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::{pulse, wordmark, PULSE, PULSE_ASCII};

    #[test]
    fn wordmark_is_four_uniform_braille_lines() {
        let lines = wordmark();
        assert_eq!(lines.len(), 4);
        let width = lines[0].chars().count();
        assert!(width > 0);
        for line in &lines {
            assert_eq!(line.chars().count(), width, "uniform width");
            for ch in line.chars() {
                let code = ch as u32;
                assert!(
                    ch == ' ' || (0x2800..=0x28FF).contains(&code),
                    "only braille or space, got {ch:?}"
                );
            }
        }
        // Some dots must exist on every line (letters are connected shapes).
        for line in &lines {
            assert!(line.chars().any(|ch| ch != '\u{2800}' && ch != ' '));
        }
    }

    #[test]
    fn wordmark_is_deterministic() {
        assert_eq!(wordmark(), wordmark());
    }

    #[test]
    fn pulse_cycles_deterministically() {
        assert_eq!(pulse(0, false), PULSE[0]);
        assert_eq!(pulse(6, false), PULSE[0]);
        assert_eq!(pulse(1, false), PULSE[1]);
        assert_eq!(pulse(0, true), PULSE_ASCII[0]);
        assert_eq!(pulse(4, true), PULSE_ASCII[0]);
    }
}
