use crate::Tendril;

// todo: should this be grapheme aware?

pub fn to_pascal_case(text: impl Iterator<Item = char>) -> Tendril {
    let mut res = Tendril::new();
    to_pascal_case_with(text, &mut res);
    res
}

pub fn to_pascal_case_with(text: impl Iterator<Item = char>, buf: &mut Tendril) {
    let mut at_word_start = true;
    for c in text {
        // we don't count _ as a word char here so case conversions work well
        if !c.is_alphanumeric() {
            at_word_start = true;
            continue;
        }
        if at_word_start {
            at_word_start = false;
            buf.extend(c.to_uppercase());
        } else {
            buf.push(c)
        }
    }
}

pub fn to_upper_case_with(text: impl Iterator<Item = char>, buf: &mut Tendril) {
    for c in text {
        for c in c.to_uppercase() {
            buf.push(c)
        }
    }
}

pub fn to_lower_case_with(text: impl Iterator<Item = char>, buf: &mut Tendril) {
    for c in text {
        for c in c.to_lowercase() {
            buf.push(c)
        }
    }
}

pub fn to_camel_case(text: impl Iterator<Item = char>) -> Tendril {
    let mut res = Tendril::new();
    to_camel_case_with(text, &mut res);
    res
}
pub fn to_camel_case_with(text: impl Iterator<Item = char>, buf: &mut Tendril) {
    let mut first = true;
    let mut at_word_start = false;

    for c in text {
        // we don't count _ as a word char here so case conversions work well
        if !c.is_alphanumeric() {
            at_word_start = true;
            continue;
        }

        if first {
            buf.extend(c.to_lowercase());
            first = false;
        } else if at_word_start {
            at_word_start = false;
            buf.extend(c.to_uppercase());
        } else {
            buf.extend(c.to_lowercase());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camel_case_capitalizes_word_starts() {
        let camel = |text: &str| to_camel_case(text.chars()).to_string();
        assert_eq!(camel("otto_botto_the_dog"), "ottoBottoTheDog");
        assert_eq!(camel("OTTO_BOTTO"), "ottoBotto");
        assert_eq!(camel("OttO_boTTO"), "ottoBotto");
        assert_eq!(camel("Ott0_b0TT0"), "ott0B0tt0");
        assert_eq!(camel("O"), "o");
        assert_eq!(camel(""), "");
    }
}
