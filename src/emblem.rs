//! The codemine emblem for the web UI, composed into an SVG from the data in
//! the cilki crate: the recolored icon next to the block-letter word grid.
//! The crate's library only carries the emblem data — its renderer lives in
//! its binary — so the few lines of geometry are mirrored here.

use std::sync::LazyLock;

use cilki::emblem::{CODEMINE, Emblem};

/// The emblem SVG, rendered once on first use.
pub static SVG: LazyLock<String> = LazyLock::new(|| render(&CODEMINE));

fn render(emblem: &Emblem) -> String {
    let side = emblem.rect_side_px;
    let step = side + emblem.rect_gap_px;
    let margin = emblem.margin_px;
    let icon_width = emblem.icon_width.unwrap_or(0);
    let columns = emblem.word[0].chars().count();

    // The block letters; an empty column pulls the following letters a
    // little closer, matching the crate's own renderer.
    let mut rects = String::new();
    let mut adjust = 0;
    for c in 0..columns {
        let mut empty = true;
        for (r, row) in emblem.word.iter().enumerate() {
            if row.chars().nth(c).unwrap_or(' ') != ' ' {
                empty = false;
                let x = margin + icon_width + c * step - adjust;
                let y = margin + r * step;
                rects.push_str(&format!(
                    r#"<rect x="{x}" y="{y}" width="{side}" height="{side}" rx="1" fill="{}"/>"#,
                    emblem.color
                ));
            }
        }
        if empty {
            adjust += 3;
        }
    }

    let width = margin * 2 + icon_width + (columns - 1) * step + side - adjust;
    let height = margin * 2 + (emblem.word.len() - 1) * step + side;

    // The icon ships as a standalone SVG document drawn in black; drop its
    // XML declaration and nest it, recolored, beside the letters.
    let icon = emblem.icon[emblem.icon.find("<svg").unwrap_or(0)..]
        .replace("#000000", emblem.color);
    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}"><g transform="translate({},{})">{icon}</g>{rects}</svg>"#,
        margin / 2,
        margin / 2,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emblem_renders_icon_and_letters() {
        let svg = &*SVG;
        assert!(svg.starts_with("<svg xmlns="), "{svg}");
        // The nested icon survived and everything took the project color.
        assert_eq!(svg.matches("<svg").count(), 2, "{svg}");
        assert!(!svg.contains("<?xml"), "{svg}");
        assert!(!svg.contains("#000000"), "{svg}");
        assert!(svg.contains(CODEMINE.color), "{svg}");
        // One rect per filled cell in the word grid, plus any in the icon.
        let cells: usize = CODEMINE
            .word
            .iter()
            .map(|row| row.chars().filter(|c| *c != ' ').count())
            .sum();
        assert!(svg.matches("<rect").count() >= cells, "{svg}");
    }
}
