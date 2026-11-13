//! Small helpers for Korean text shown to people: a duration in Korean
//! units, and the particle that fits a word the program fills in (a number,
//! a commit, a name).
//!
//! A particle depends on whether the word, as read aloud, ends in a final
//! consonant (받침): "기록 3은", "기록 2는". Words read in Korean (Hangul)
//! are decided by their last syllable, digits by their Korean reading
//! (영 일 이 삼 사 오 육 칠 팔 구), Latin letters by their usual Korean
//! reading (엘 엠 엔 알 end in one; 에이 비 씨 ... do not). Anything else
//! (symbols, an empty word) counts as ending in a vowel, the common case
//! for punctuation-ended words like ")".

use std::time::Duration;

/// A duration in Korean units: "5분", "1시간 30분", "30초", "0초".
pub fn duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    let (hours, minutes, seconds) = (secs / 3600, secs / 60 % 60, secs % 60);
    let mut parts = Vec::new();
    if hours > 0 {
        parts.push(format!("{hours}시간"));
    }
    if minutes > 0 {
        parts.push(format!("{minutes}분"));
    }
    if seconds > 0 || parts.is_empty() {
        parts.push(format!("{seconds}초"));
    }
    parts.join(" ")
}

/// Whether `word` read aloud ends in a final consonant.
fn ends_in_consonant(word: &str) -> bool {
    match word.chars().last() {
        Some(last @ '가'..='힣') => !(last as u32 - '가' as u32).is_multiple_of(28),
        // 영 일 삼 육 칠 팔.
        Some('0' | '1' | '3' | '6' | '7' | '8') => true,
        // 엘 엠 엔 알.
        Some('l' | 'L' | 'm' | 'M' | 'n' | 'N' | 'r' | 'R') => true,
        _ => false,
    }
}

/// `word` with the topic particle: "기록 3은", "기록 2는".
pub fn topic(word: &str) -> String {
    let particle = if ends_in_consonant(word) {
        "은"
    } else {
        "는"
    };
    format!("{word}{particle}")
}

/// `word` with the object particle: "커밋 3a1을", "리뷰 42를".
pub fn object(word: &str) -> String {
    let particle = if ends_in_consonant(word) {
        "을"
    } else {
        "를"
    };
    format!("{word}{particle}")
}

/// `word` with the subject particle: "커밋 abc0이", "커밋 abc2가".
pub fn subject(word: &str) -> String {
    let particle = if ends_in_consonant(word) {
        "이"
    } else {
        "가"
    };
    format!("{word}{particle}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_in_korean_units() {
        let cases = [
            (0, "0초"),
            (1, "1초"),
            (30, "30초"),
            (60, "1분"),
            (300, "5분"),
            (90, "1분 30초"),
            (3600, "1시간"),
            (5400, "1시간 30분"),
            (3661, "1시간 1분 1초"),
        ];
        for (secs, expected) in cases {
            assert_eq!(duration(Duration::from_secs(secs)), expected, "{secs}");
        }
    }

    #[test]
    fn particles_follow_how_the_word_is_read() {
        // Digits as read in Korean: 영 일 이 삼 사 오 육 칠 팔 구.
        let topics: Vec<String> = (0..10).map(|n| topic(&n.to_string())).collect();
        assert_eq!(
            topics,
            [
                "0은", "1은", "2는", "3은", "4는", "5는", "6은", "7은", "8은", "9는"
            ]
        );
        assert_eq!(topic("12"), "12는");
        assert_eq!(topic("10"), "10은");
        assert_eq!(object("42"), "42를");
        assert_eq!(object("7"), "7을");
        assert_eq!(subject("abc0"), "abc0이");
        assert_eq!(subject("abc2"), "abc2가");
        // Hex letters: 에이 비 씨 디 이 에프 end in a vowel.
        assert_eq!(object("1fea22d"), "1fea22d를");
        assert_eq!(object("9611ff"), "9611ff를");
        // Hangul by the last syllable.
        assert_eq!(topic("기록"), "기록은");
        assert_eq!(topic("파일"), "파일은");
        assert_eq!(topic("메모"), "메모는");
        assert_eq!(object("pull request"), "pull request를");
        assert_eq!(object(""), "를");
    }
}
