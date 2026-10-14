//! Text normalization shared by every crate that matches names: library
//! search, the DJ's composer and work matching, room and favorite names.

/// Lowercase, fold common Latin diacritics to ASCII, and collapse every run of
/// non-alphanumerics to one space: `"Dvořák: Symphony No. 9, Op.95"` →
/// `"dvorak symphony no 9 op 95"`.
#[must_use]
pub fn normalize(s: &str) -> String {
    fn push(out: &mut String, text: &str, gap: &mut bool) {
        if *gap && !out.is_empty() {
            out.push(' ');
        }
        *gap = false;
        out.push_str(text);
    }
    let mut out = String::with_capacity(s.len());
    let mut gap = false;
    for ch in s.chars().flat_map(char::to_lowercase) {
        if let Some(ascii) = fold(ch) {
            push(&mut out, ascii, &mut gap);
        } else if ch == '♭' || ch == '♯' {
            gap = true;
            push(&mut out, if ch == '♭' { "flat" } else { "sharp" }, &mut gap);
            gap = true;
        } else if ch.is_alphanumeric() {
            let mut buf = [0u8; 4];
            push(&mut out, ch.encode_utf8(&mut buf), &mut gap);
        } else {
            gap = true;
        }
    }
    out
}

fn fold(c: char) -> Option<&'static str> {
    Some(match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => "a",
        'æ' => "ae",
        'ç' | 'ć' | 'č' => "c",
        'ď' | 'đ' | 'ð' => "d",
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ė' | 'ę' | 'ě' => "e",
        'ì' | 'í' | 'î' | 'ï' | 'ī' | 'į' | 'ı' => "i",
        'ł' | 'ľ' | 'ĺ' => "l",
        'ñ' | 'ń' | 'ň' => "n",
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ő' => "o",
        'œ' => "oe",
        'ř' | 'ŕ' => "r",
        'ś' | 'š' | 'ş' | 'ș' => "s",
        'ß' => "ss",
        'ť' | 'ţ' | 'ț' => "t",
        'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ů' | 'ű' | 'ų' => "u",
        'ý' | 'ÿ' => "y",
        'ź' | 'ż' | 'ž' => "z",
        'þ' => "th",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_diacritics_case_and_punctuation() {
        assert_eq!(
            normalize("Dvořák: Symphony No. 9, Op.95"),
            "dvorak symphony no 9 op 95"
        );
        assert_eq!(normalize("Ada\u{2019}s  Studio"), "ada s studio");
        assert_eq!(normalize("Prélude in D♭"), "prelude in d flat");
        assert_eq!(normalize("  "), "");
    }
}
