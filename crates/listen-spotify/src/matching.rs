//! Finding the YouTube Music copy of a Spotify track: Spotify's audio is DRM-protected and can't be
//! played outside its own clients, so Listen plays the same recording from YouTube Music. The
//! candidates come from a YouTube Music song search; this scores them by title, artist and length.

use listen_core::model::Song;
use std::collections::HashSet;

/// Words that mark a different version of a song (unless the Spotify title has them too).
const VERSION_WORDS: &[&str] = &[
    "live",
    "remix",
    "cover",
    "karaoke",
    "instrumental",
    "acoustic",
    "sped",
    "slowed",
    "reverb",
    "nightcore",
    "8d",
    "demo",
    "edit",
    "mix",
    "version",
    "tribute",
];

/// Words that don't change the recording ("Remastered 2009").
fn is_noise(word: &str) -> bool {
    word.starts_with("remaster")
        || (word.len() == 4 && word.starts_with(['1', '2']) && word.parse::<u16>().is_ok())
}

/// Latin letters without their accents ("Beyoncé" and "Beyonce" are the same artist).
fn fold(c: char) -> char {
    match c {
        'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' | 'ā' => 'a',
        'ç' | 'č' | 'ć' => 'c',
        'é' | 'è' | 'ê' | 'ë' | 'ē' | 'ě' => 'e',
        'í' | 'ì' | 'î' | 'ï' | 'ī' => 'i',
        'ñ' | 'ń' | 'ň' => 'n',
        'ó' | 'ò' | 'ô' | 'ö' | 'õ' | 'ø' | 'ō' => 'o',
        'ú' | 'ù' | 'û' | 'ü' | 'ū' | 'ů' => 'u',
        'ý' | 'ÿ' => 'y',
        'š' | 'ś' => 's',
        'ž' | 'ź' | 'ż' => 'z',
        'ř' => 'r',
        c => c,
    }
}

/// Lower case, without accents, "(feat. …)" / "- Remastered 2011" style suffixes and punctuation.
pub fn normalize(s: &str) -> String {
    let lower: String = s.to_lowercase().chars().map(fold).collect();
    // " - Remastered 2011", " - Radio Edit": Spotify's version suffix.
    let base = lower.split(" - ").next().unwrap_or(&lower);
    let mut out = String::with_capacity(base.len());
    let mut depth = 0usize;
    let mut bracket = String::new();
    for c in base.chars() {
        match c {
            '(' | '[' => {
                depth += 1;
                bracket.clear();
            }
            ')' | ']' if depth > 0 => {
                depth -= 1;
                // Keep what tells versions apart ("(Live)"), drop featured artists and remasters.
                let inner = bracket.trim_start();
                if !(inner.starts_with("feat") || inner.starts_with("with ") || inner.contains("remaster")) {
                    out.push(' ');
                    out.push_str(&bracket);
                }
                bracket.clear();
            }
            c if depth > 0 => bracket.push(c),
            c => out.push(c),
        }
    }
    let cleaned: String = out.chars().map(|c| if c.is_alphanumeric() { c } else { ' ' }).collect();
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn words(s: &str) -> HashSet<String> {
    normalize(s).split(' ').filter(|w| !w.is_empty() && !is_noise(w)).map(str::to_string).collect()
}

/// The single artists of an artist line ("A & B", "A, B", "A feat. B", "A x B").
fn artist_names(line: &str) -> Vec<String> {
    let mut names = vec![line.to_lowercase()];
    for sep in [",", " & ", " and ", " x ", " feat. ", " feat ", " ft. ", " with ", " / "] {
        names = names.iter().flat_map(|n| n.split(sep).map(str::to_string).collect::<Vec<_>>()).collect();
    }
    names.iter().map(|n| normalize(n)).filter(|n| !n.is_empty()).collect()
}

/// Share of words the two titles have in common (0..1).
fn title_similarity(a: &str, b: &str) -> f64 {
    let (a, b) = (words(a), words(b));
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    a.intersection(&b).count() as f64 / a.union(&b).count() as f64
}

/// What to search YouTube Music for.
pub fn query(song: &Song) -> String {
    let artist = song.artists.first().map(|a| a.name.as_str()).unwrap_or_default();
    let title = song.title.split(" - ").next().unwrap_or(&song.title);
    format!("{artist} {title}").trim().to_string()
}

/// How well `candidate` (a YouTube Music song) matches `wanted` (the Spotify track), about 0..1.
pub fn score(wanted: &Song, candidate: &Song) -> f64 {
    let mut score = 0.5 * title_similarity(&wanted.title, &candidate.title);

    let candidate_artists = artist_names(&candidate.artist_line());
    let artist_hit = wanted.artists.iter().any(|a| {
        let name = normalize(&a.name);
        !name.is_empty() && candidate_artists.contains(&name)
    });
    // Another artist with the same title is a cover or a tribute.
    score += if artist_hit { 0.3 } else { -0.3 };

    let (w, c) = (wanted.duration.as_secs_f64(), candidate.duration.as_secs_f64());
    if w > 0.0 && c > 0.0 {
        let diff = (w - c).abs();
        score += 0.2 * (1.0 - diff / 20.0).max(0.0);
        if diff > 60.0 {
            score -= 0.2;
        }
    }

    let (have, got) = (words(&wanted.title), words(&candidate.title));
    let extra = VERSION_WORDS.iter().filter(|w| got.contains(**w) && !have.contains(**w)).count();
    score - 0.25 * extra as f64
}

/// Lowest score taken as the same recording.
pub const MIN_SCORE: f64 = 0.45;

/// The best candidate, if any is good enough.
pub fn best<'a>(wanted: &Song, candidates: &'a [Song]) -> Option<&'a Song> {
    candidates
        .iter()
        .map(|c| (score(wanted, c), c))
        .filter(|(s, _)| *s >= MIN_SCORE)
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, c)| c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use listen_core::model::{ArtistRef, GainInfo, ServerId, SourceKind};
    use std::time::Duration;

    fn song(title: &str, artist: &str, secs: u64) -> Song {
        Song {
            id: title.into(),
            server_id: ServerId("s".into()),
            source: SourceKind::YouTubeMusic,
            title: title.into(),
            artists: vec![ArtistRef { id: None, name: artist.into() }],
            album_artists: vec![],
            album: None,
            album_id: None,
            image: None,
            duration: Duration::from_secs(secs),
            track_number: None,
            disc_number: None,
            year: None,
            genres: vec![],
            codec: None,
            bit_rate: None,
            sample_rate: None,
            bit_depth: None,
            bpm: None,
            favorite: false,
            rating: None,
            play_count: None,
            gain: GainInfo::default(),
            explicit: false,
            path: None,
            size: None,
            suffix: None,
            channels: None,
            created: None,
            comment: None,
            mbz: Default::default(),
            extra: Default::default(),
        }
    }

    #[test]
    fn normalizes_titles() {
        assert_eq!(normalize("Here Comes The Sun - Remastered 2009"), "here comes the sun");
        assert_eq!(normalize("Stay (feat. Justin Bieber)"), "stay");
        assert_eq!(normalize("Song (Live at Wembley)"), "song live at wembley");
        assert_eq!(normalize("Don't Stop Me Now"), "don t stop me now");
        assert_eq!(normalize("Beyoncé"), "beyonce");
    }

    #[test]
    fn query_is_artist_and_title() {
        assert_eq!(
            query(&song("Here Comes The Sun - Remastered 2009", "The Beatles", 185)),
            "The Beatles Here Comes The Sun"
        );
    }

    #[test]
    fn picks_the_studio_recording() {
        let wanted = song("Bohemian Rhapsody - Remastered 2011", "Queen", 354);
        let candidates = vec![
            song("Bohemian Rhapsody (Live Aid)", "Queen", 360),
            song("Bohemian Rhapsody (Karaoke Version)", "Sing King", 355),
            song("Bohemian Rhapsody", "Queen", 355),
        ];
        assert_eq!(best(&wanted, &candidates).unwrap().title, "Bohemian Rhapsody");
    }

    #[test]
    fn rejects_a_different_song() {
        let wanted = song("Yesterday", "The Beatles", 125);
        let candidates =
            vec![song("Let It Be", "The Beatles", 243), song("Yesterday Once More", "Carpenters", 238)];
        assert!(best(&wanted, &candidates).is_none());
    }

    #[test]
    fn a_tribute_act_is_not_the_artist() {
        let wanted = song("Here Comes The Sun - Remastered 2009", "The Beatles", 185);
        let candidates = vec![
            song("Here Comes The Sun", "Yesterday - A Tribute To The Beatles", 187),
            song("Here Comes The Sun (Remastered 2009)", "The Beatles", 186),
        ];
        assert_eq!(best(&wanted, &candidates).unwrap().artist_line(), "The Beatles");
        assert!(best(&wanted, &candidates[..1]).is_none());
        // Several artists on the YouTube side.
        let stay = song("Stay (with Justin Bieber)", "The Kid LAROI", 141);
        assert!(best(&stay, &[song("STAY", "The Kid LAROI & Justin Bieber", 142)]).is_some());
    }

    #[test]
    fn length_breaks_a_tie() {
        let wanted = song("Intro", "The xx", 128);
        let candidates = vec![song("Intro", "The xx", 300), song("Intro", "The xx", 127)];
        assert_eq!(best(&wanted, &candidates).unwrap().duration, Duration::from_secs(127));
    }
}
