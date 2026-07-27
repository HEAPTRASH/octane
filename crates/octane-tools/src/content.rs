//! Content heuristics shared by the tools that read files.

/// Bytes inspected when deciding whether a file is binary.
pub const BINARY_SNIFF_BYTES: usize = 8_192;

/// A NUL byte in the first block means binary.
///
/// The same heuristic git and ripgrep use. Cheap, and wrong only for files a
/// coding agent will not meet. Getting it right matters in both directions:
/// a false positive hides a readable file, and a false negative dumps a binary
/// into the context window.
pub fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(BINARY_SNIFF_BYTES).any(|byte| *byte == 0)
}

/// Recognize images by extension so an error can name the format.
///
/// "This is a PNG" tells the model something actionable; "this is binary" does
/// not. SVG is deliberately absent — it is text and genuinely readable.
pub fn image_kind(path: &camino::Utf8Path) -> Option<&'static str> {
    let extension = path.extension()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "png" => "PNG",
        "jpg" | "jpeg" => "JPEG",
        "gif" => "GIF",
        "webp" => "WebP",
        "bmp" => "BMP",
        "ico" => "ICO",
        "tiff" | "tif" => "TIFF",
        _ => return None,
    })
}

/// Cut a line to `max` characters, marking that it was cut.
///
/// Counted in characters, not bytes, so the cut never lands mid-codepoint. The
/// callers pick their own cap: a matching line in a search can be shorter than a
/// line of a file the model asked to read.
pub fn clip_line(line: &str, max: usize) -> String {
    if line.chars().count() <= max {
        return line.to_string();
    }
    let cut: String = line.chars().take(max).collect();
    format!("{cut}… [line truncated]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8Path;

    #[test]
    fn text_is_not_binary() {
        assert!(!looks_binary(b"fn main() {}\n"));
        assert!(!looks_binary("héllo wörld".as_bytes()));
        assert!(!looks_binary(b""));
    }

    #[test]
    fn a_nul_byte_means_binary() {
        assert!(looks_binary(&[0x7f, 0x45, 0x4c, 0x46, 0x00]));
    }

    #[test]
    fn only_the_first_block_is_inspected() {
        // A NUL far past the sniff window is not enough to condemn the file.
        let mut bytes = vec![b'a'; BINARY_SNIFF_BYTES + 100];
        bytes.push(0x00);
        assert!(!looks_binary(&bytes));
    }

    #[test]
    fn images_are_named_but_svg_is_not() {
        assert_eq!(image_kind(Utf8Path::new("a.png")), Some("PNG"));
        assert_eq!(image_kind(Utf8Path::new("a.JPEG")), Some("JPEG"));
        assert_eq!(image_kind(Utf8Path::new("a.svg")), None);
        assert_eq!(image_kind(Utf8Path::new("a.rs")), None);
        assert_eq!(image_kind(Utf8Path::new("noext")), None);
    }
}
