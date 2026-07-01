// server lists live in /defaults/*.txt so non-rust contributors can PR new servers in one line
const RELAYS: &str = include_str!("../../../defaults/relays.txt");
const TRACKERS: &str = include_str!("../../../defaults/trackers.txt");

pub fn default_relays() -> Vec<String> {
    parse(RELAYS)
}

pub fn default_trackers() -> Vec<String> {
    parse(TRACKERS)
}

fn parse(raw: &str) -> Vec<String> {
    raw.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_comments_and_blanks() {
        let got = parse("# a comment\n\nwss://a\n  wss://b  \n# tail\n");
        assert_eq!(got, vec!["wss://a", "wss://b"]);
    }

    #[test]
    fn bundled_lists_are_non_empty() {
        assert!(!default_relays().is_empty());
        assert!(!default_trackers().is_empty());
    }
}
