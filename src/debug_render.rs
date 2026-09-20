use std::collections::BTreeMap;

use crate::Contact;

// Size of the ASCII grid we draw the live touch surface onto.
const GRID_W: usize = 80;
const GRID_H: usize = 20;

/// Draw the current set of contacts as a bordered ASCII grid, redrawn in
/// place each call (via the \x1B[H "cursor home" escape) rather than
/// scrolling the terminal.
pub fn render_grid(
    contacts: &BTreeMap<u16, Contact>,
    x_min: i32,
    x_max: i32,
    y_min: i32,
    y_max: i32,
) {
    // Only draw fingers that are actually touching -- a link collection can
    // exist in `contacts` with tip == 0 (an unused/hovering slot).
    let active: Vec<&Contact> = contacts.values().filter(|c| c.tip != 0).collect();

    let mut grid = vec![vec!['.'; GRID_W]; GRID_H];

    // Guard against a degenerate (zero-width) range with .max(1), so we
    // never divide by zero if x_min/x_max were never actually filled in
    // (e.g. this device's descriptor didn't expose X/Y the way we expect).
    let x_span = (x_max - x_min).max(1) as f32;
    let y_span = (y_max - y_min).max(1) as f32;

    for c in &active {
        // Normalize this contact's raw x/y into 0.0..=1.0 using the
        // device's real reported range, then scale that fraction onto our
        // fixed grid dimensions.
        let nx = ((c.x as i32 - x_min) as f32 / x_span).clamp(0.0, 1.0);
        let ny = ((c.y as i32 - y_min) as f32 / y_span).clamp(0.0, 1.0);
        let col = (nx * (GRID_W - 1) as f32).round() as usize;
        let row = (ny * (GRID_H - 1) as f32).round() as usize;

        // Label each finger with a single hex digit derived from its
        // contact id, so multiple simultaneous fingers are visually
        // distinguishable. Falls back to 'O' only if id somehow exceeds a
        // single hex digit (shouldn't normally happen).
        let ch = std::char::from_digit(c.id & 0xF, 16).unwrap_or('O');
        grid[row][col] = ch.to_ascii_uppercase();
    }

    // Build the whole frame as one string and print it in a single call --
    // much less flicker-prone than printing line by line. "\x1B[H" resets
    // the cursor to the top-left WITHOUT clearing (unlike "\x1B[2J"), so
    // each frame overwrites the previous one in place.
    let mut out = String::from("\x1B[H");
    out.push('+');
    out.push_str(&"-".repeat(GRID_W));
    out.push_str("+\n");
    for row in &grid {
        out.push('|');
        out.extend(row.iter());
        out.push_str("|\n");
    }
    out.push('+');
    out.push_str(&"-".repeat(GRID_W));
    out.push_str("+\n");

    out.push_str(&format!("contacts: {}          \n", active.len()));
    for c in &active {
        out.push_str(&format!(
            "  id={} x={:5} y={:5}                    \n",
            c.id, c.x, c.y
        ));
    }
    // Trailing padding lines: if the PREVIOUS frame had more contact lines
    // than this one, without this the old lines would linger on screen
    // since we're overwriting in place rather than clearing first.
    out.push_str("                                                            \n");
    out.push_str("                                                            \n");

    print!("{out}");
    use std::io::Write;
    let _ = std::io::stdout().flush(); // stdout is line-buffered by default; force this frame out immediately
}
