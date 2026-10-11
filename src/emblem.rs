//! The codemine emblem for the web UI, composed into an SVG by the cilki
//! crate from the icon and the block-letter word grid it carries.

use std::sync::LazyLock;

use cilki::emblem::CODEMINE;

/// The emblem SVG, rendered once on first use.
pub static SVG: LazyLock<String> = LazyLock::new(|| CODEMINE.to_svg_string(None));

/// Just the icon, recolored, for the favicon.
pub static ICON: LazyLock<String> =
    LazyLock::new(|| CODEMINE.icon_svg().unwrap_or_default());

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
