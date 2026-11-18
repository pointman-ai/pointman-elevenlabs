//! `elevenlabs.live`'s parts that need no stream: where a stream may come from, where its audio is cut into chunks,
//! the words' times on the wall clock, the posts' lines and the lag figures. ElevenLabs' CLI has no realtime
//! speech to text, so a live stream is heard in chunks: ffmpeg turns it into 16 kHz mono PCM, each chunk of about
//! `every_seconds` is cut at its quietest moment near the end (not through a word), and each goes to batch
//! speech to text.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The PCM ffmpeg gives: 16-bit little-endian mono at 16 kHz, ElevenLabs' `pcm_s16le_16`.
pub const RATE: usize = 16_000;
/// How far back from a chunk's end its cut may move, to a quiet moment: at most this, and at most a fifth of it.
pub const CUT_WINDOW: Duration = Duration::from_secs(3);
/// The span whose loudness is compared when looking for the quietest moment.
const FRAME: usize = RATE / 50;

/// The URL schemes a stream may come from. A local file is a path instead (checked against the node's folders).
pub const SCHEMES: [&str; 6] = ["https", "http", "rtmp", "rtmps", "srt", "rtsp"];
/// What ffmpeg may open on its way to the stream, so a playlist can't point it at a local file or another of its
/// protocols (`concat:`, `subfile:`, `pipe:`).
pub const PROTOCOLS: &str = "http,https,tcp,tls,crypto,rtmp,rtmps,srt,rtsp,udp,hls,httpproxy";

/// Where a stream comes from, as the `url` param gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A page yt-dlp knows (YouTube, Twitch, Vimeo…): yt-dlp finds the stream's own address.
    Page(String),
    /// An address ffmpeg reads itself: an HLS playlist, RTMP, SRT, a media file over https.
    Stream(String),
    /// A file on this machine.
    File(String),
}

/// What `url` is, or why it can't be one.
pub fn source(url: &str) -> Result<Source, String> {
    let u = url.trim();
    if u.is_empty() {
        return Err("url is missing: a YouTube live URL, or anything yt-dlp or ffmpeg reads".into());
    }
    if u.starts_with('-') {
        return Err(format!("{u} isn't a URL"));
    }
    let Some((scheme, rest)) = u.split_once("://") else {
        if u.contains(':') && !u.starts_with('/') && !u.starts_with('~') {
            return Err(format!("{u} isn't a URL ffmpeg should open: give an https:// (or rtmp, srt) address"));
        }
        return Ok(Source::File(u.to_string()));
    };
    let scheme = scheme.to_ascii_lowercase();
    if !SCHEMES.contains(&scheme.as_str()) {
        return Err(format!("{scheme}:// isn't a stream's address: give an {} one", SCHEMES.join(", ")));
    }
    if rest.is_empty() {
        return Err(format!("{u} has no address"));
    }
    if scheme.starts_with("http") && !direct(u) {
        return Ok(Source::Page(u.to_string()));
    }
    Ok(Source::Stream(u.to_string()))
}

/// An http(s) address ffmpeg reads as it is: a playlist or a media file, by its path's ending.
fn direct(url: &str) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or(url).to_ascii_lowercase();
    [".m3u8", ".m3u", ".mpd", ".mp3", ".aac", ".m4a", ".wav", ".ogg", ".opus", ".flac", ".mp4", ".ts", ".mkv", ".webm"]
        .iter()
        .any(|e| path.ends_with(e))
}

/// A thread's number from `thr_71027`, `71027`, or a number.
pub fn thread_id(given: &serde_json::Value) -> Option<i64> {
    let n = match given {
        serde_json::Value::Number(n) => n.as_i64()?,
        serde_json::Value::String(s) => s.trim().trim_start_matches("thr_").parse().ok()?,
        _ => return None,
    };
    (n > 0).then_some(n)
}

/// The sample where a chunk of `samples` ends: its quietest moment in the last [`CUT_WINDOW`] (and at most a fifth
/// of it), so the cut falls in a pause rather than through a word. The end itself when there's nothing to choose.
pub fn quiet_cut(samples: &[i16]) -> usize {
    let n = samples.len();
    let window = (CUT_WINDOW.as_secs_f64() * RATE as f64) as usize;
    let window = window.min(n / 5);
    if window < FRAME * 2 {
        return n;
    }
    let from = n - window;
    let mut best = (u64::MAX, n);
    let mut at = from;
    while at + FRAME <= n {
        let energy: u64 = samples[at..at + FRAME].iter().map(|&s| (i64::from(s) * i64::from(s)) as u64).sum();
        // ties go to the later frame: the longer chunk
        if energy <= best.0 {
            best = (energy, at + FRAME / 2);
        }
        at += FRAME / 2;
    }
    best.1
}

/// When each sample reached this machine: a few marks of (sample, time), read between them.
#[derive(Debug, Default, Clone)]
pub struct Arrivals {
    marks: Vec<(u64, f64)>,
}

impl Arrivals {
    /// `sample` (counted from the stream's start) had arrived by `at` (Unix seconds).
    pub fn mark(&mut self, sample: u64, at: f64) {
        if self.marks.last().is_some_and(|&(s, _)| s >= sample) {
            return;
        }
        self.marks.push((sample, at));
    }

    /// When `sample` arrived: the first mark at or after it. ffmpeg hands audio over in bursts, so a sample's
    /// arrival is the moment the burst that carried it was read.
    pub fn at(&self, sample: u64) -> Option<f64> {
        let i = self.marks.partition_point(|&(s, _)| s < sample);
        self.marks.get(i).or(self.marks.last()).map(|&(_, t)| t)
    }

    /// Forget marks before `sample`, keeping the one that covers it.
    pub fn forget_before(&mut self, sample: u64) {
        let i = self.marks.partition_point(|&(s, _)| s < sample);
        self.marks.drain(..i);
    }
}

/// A word as ElevenLabs gives it: its text and its start and end in the chunk, in seconds.
#[derive(Debug, Clone, PartialEq)]
pub struct Word {
    pub text: String,
    pub start: f64,
    pub end: f64,
}

/// The words in a speech-to-text answer (not the spacing or audio events), and its whole text.
pub fn words_of(answer: &serde_json::Value) -> (Vec<Word>, String) {
    let words = answer["words"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|w| w["type"].as_str().unwrap_or("word") == "word")
        .filter_map(|w| {
            Some(Word {
                text: w["text"].as_str()?.trim().to_string(),
                start: w["start"].as_f64()?,
                end: w["end"].as_f64().or_else(|| w["start"].as_f64())?,
            })
        })
        .filter(|w| !w.text.is_empty())
        .collect::<Vec<_>>();
    let text = answer["text"].as_str().map(str::trim).filter(|t| !t.is_empty()).map(String::from);
    let text = text.unwrap_or_else(|| words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" "));
    (words, text)
}

/// "13:52:07" for a time (UTC).
pub fn clock(unix: f64) -> String {
    let s = unix.max(0.0) as i64 % 86_400;
    format!("{:02}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60)
}

/// "2026-11-18T13:52:07.250Z".
pub fn iso(unix: f64) -> String {
    let ms = (unix.max(0.0) * 1000.0).round() as i64;
    format!("{}T{}.{:03}Z", crate::words::day(ms / 1000), clock((ms / 1000) as f64), ms % 1000)
}

pub fn unix(t: SystemTime) -> f64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// A post's line: `[13:52:07 UTC] words`. An `@` is kept from waking anyone (a zero-width space after it), and a
/// line never starts as a slash command.
pub fn line(at: f64, text: &str) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ").replace('@', "@\u{200b}");
    format!("[{} UTC] {text}", clock(at))
}

/// Lags in seconds: their median and worst.
#[derive(Debug, Default, Clone)]
pub struct Lags(Vec<f64>);

impl Lags {
    pub fn add(&mut self, lag: f64) {
        if lag.is_finite() {
            self.0.push(lag.max(0.0));
        }
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn median(&self) -> Option<f64> {
        let mut v = self.0.clone();
        if v.is_empty() {
            return None;
        }
        v.sort_by(|a, b| a.total_cmp(b));
        let m = v.len() / 2;
        Some(if v.len().is_multiple_of(2) { (v[m - 1] + v[m]) / 2.0 } else { v[m] })
    }

    pub fn worst(&self) -> Option<f64> {
        self.0.iter().copied().reduce(f64::max)
    }

    pub fn json(&self) -> serde_json::Value {
        let r = |x: Option<f64>| x.map(|x| (x * 10.0).round() / 10.0);
        serde_json::json!({"median_s": r(self.median()), "worst_s": r(self.worst()), "count": self.len()})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sources_are_pages_streams_or_files() {
        assert_eq!(
            source("https://www.youtube.com/watch?v=abc").unwrap(),
            Source::Page("https://www.youtube.com/watch?v=abc".into())
        );
        assert_eq!(source("https://youtu.be/abc").unwrap(), Source::Page("https://youtu.be/abc".into()));
        assert_eq!(
            source("https://cdn.x/live/index.m3u8?t=1").unwrap(),
            Source::Stream("https://cdn.x/live/index.m3u8?t=1".into())
        );
        assert_eq!(source("rtmp://host/app/key").unwrap(), Source::Stream("rtmp://host/app/key".into()));
        assert_eq!(source("/Users/x/talk.mp4").unwrap(), Source::File("/Users/x/talk.mp4".into()));
        assert!(source("file:///etc/passwd").is_err());
        assert!(source("concat:/a|/b").is_err());
        assert!(source("subfile,,start,0,end,0,,:/etc/passwd").is_err());
        assert!(source("-i").is_err());
        assert!(source(" ").is_err());
    }

    #[test]
    fn threads_by_any_name() {
        assert_eq!(thread_id(&json!("thr_71027")), Some(71027));
        assert_eq!(thread_id(&json!("71027")), Some(71027));
        assert_eq!(thread_id(&json!(71027)), Some(71027));
        assert_eq!(thread_id(&json!("post_5")), None);
        assert_eq!(thread_id(&json!(0)), None);
    }

    #[test]
    fn a_chunk_is_cut_in_its_pause() {
        // 15 s of "speech" with a quiet 200 ms at 13.5 s
        let mut s: Vec<i16> = (0..15 * RATE).map(|i| if i.is_multiple_of(2) { 8000 } else { -8000 }).collect();
        let quiet = (13.5 * RATE as f64) as usize;
        for x in &mut s[quiet..quiet + RATE / 5] {
            *x = 10;
        }
        let cut = quiet_cut(&s);
        assert!(cut > quiet && cut < quiet + RATE / 5, "cut at {cut}, the pause is {quiet}..");
        // all silence: the end
        assert_eq!(quiet_cut(&vec![0; 15 * RATE]), 15 * RATE - FRAME / 2);
        // too short to choose
        assert_eq!(quiet_cut(&[1, 2, 3]), 3);
    }

    #[test]
    fn arrivals_say_when_a_sample_came() {
        let mut a = Arrivals::default();
        a.mark(1600, 100.0);
        a.mark(3200, 100.1);
        a.mark(3200, 100.2);
        a.mark(48_000, 103.0);
        assert_eq!(a.at(0), Some(100.0));
        assert_eq!(a.at(1601), Some(100.1));
        assert_eq!(a.at(40_000), Some(103.0));
        assert_eq!(a.at(90_000), Some(103.0), "past the last: the last");
        a.forget_before(3000);
        assert_eq!(a.at(0), Some(100.1));
    }

    #[test]
    fn words_and_lines() {
        let got = json!({"text": " Hello @james. ", "words": [
            {"text": "Hello", "start": 0.5, "end": 0.9, "type": "word"},
            {"text": " ", "start": 0.9, "end": 1.0, "type": "spacing"},
            {"text": "(laughs)", "start": 1.0, "end": 1.2, "type": "audio_event"},
            {"text": "@james.", "start": 1.2, "end": 1.6, "type": "word"}]});
        let (w, text) = words_of(&got);
        assert_eq!(w.len(), 2);
        assert_eq!(w[1], Word { text: "@james.".into(), start: 1.2, end: 1.6 });
        assert_eq!(text, "Hello @james.");
        assert_eq!(line(1_763_473_927.4, &text), "[13:52:07 UTC] Hello @\u{200b}james.");
        assert_eq!(iso(1_763_473_927.25), "2025-11-18T13:52:07.250Z");
    }

    #[test]
    fn lags_median_and_worst() {
        let mut l = Lags::default();
        assert_eq!(l.median(), None);
        for x in [3.0, 9.0, 4.0, 5.0] {
            l.add(x);
        }
        assert_eq!(l.median(), Some(4.5));
        assert_eq!(l.worst(), Some(9.0));
        assert_eq!(l.json()["count"], 4);
    }
}
