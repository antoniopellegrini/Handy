//! App-level long-form handling for Cohere GGUF models.
//!
//! Cohere Transcribe was trained on clips no longer than 35 seconds. The
//! encoder accepts much longer input, but the decoder can silently stop early.
//! Split long recordings into overlapping windows and reconcile the repeated
//! text at each join.

const SAMPLE_RATE: usize = 16_000;
const WINDOW_SAMPLES: usize = 35 * SAMPLE_RATE;
const OVERLAP_SAMPLES: usize = SAMPLE_RATE;
const SPLIT_SEARCH_SAMPLES: usize = WINDOW_SAMPLES / 4;
const SPLIT_SEARCH_SAMPLES_WIDE: usize = WINDOW_SAMPLES / 2;

const PAUSE_FRAME_SAMPLES: usize = SAMPLE_RATE / 50;
const PAUSE_FRAME_STEP: usize = PAUSE_FRAME_SAMPLES / 2;
const PAUSE_RATIO: f32 = 0.30;

const PREVIOUS_TOKEN_SEARCH: usize = 24;
const CURRENT_TOKEN_SEARCH: usize = 12;

pub(super) fn requires_chunking(sample_count: usize) -> bool {
    sample_count > WINDOW_SAMPLES
}

/// Split 16 kHz mono PCM into model-sized windows. Consecutive windows share
/// one second of audio. A low-energy frame is preferred as the boundary, but
/// the fixed 35-second boundary remains safe because of that overlap.
pub(super) fn audio_windows(pcm: &[f32]) -> Vec<&[f32]> {
    if pcm.len() <= WINDOW_SAMPLES {
        return vec![pcm];
    }

    let mut windows = Vec::new();
    let mut offset = 0;

    while offset < pcm.len() {
        let remaining = &pcm[offset..];
        let take = if remaining.len() <= WINDOW_SAMPLES {
            remaining.len()
        } else {
            pick_split(remaining, WINDOW_SAMPLES, SPLIT_SEARCH_SAMPLES)
                .or_else(|| pick_split(remaining, WINDOW_SAMPLES, SPLIT_SEARCH_SAMPLES_WIDE))
                .unwrap_or(WINDOW_SAMPLES)
        };

        let end = offset + take;
        windows.push(&pcm[offset..end]);
        if end == pcm.len() {
            break;
        }

        // The widest boundary search cannot choose a window shorter than half
        // the target size, so this subtraction always makes forward progress.
        offset += take.saturating_sub(OVERLAP_SAMPLES);
    }

    windows
}

/// Find the quietest 20 ms frame before `target`, provided it is substantially
/// quieter than the searched region. Uniform speech has a minimum too, so the
/// ratio check prevents treating an arbitrary frame as a pause.
fn pick_split(pcm: &[f32], target: usize, search: usize) -> Option<usize> {
    if target >= pcm.len() || search < PAUSE_FRAME_SAMPLES * 2 || target < PAUSE_FRAME_SAMPLES * 2 {
        return None;
    }

    let lower_bound = PAUSE_FRAME_SAMPLES.max(target.saturating_sub(search));
    let mut amplitude_sum = 0.0_f64;
    let mut frame_count = 0;
    let mut quietest = None;

    for at in (lower_bound..=target - PAUSE_FRAME_SAMPLES).step_by(PAUSE_FRAME_STEP) {
        let amplitude = pcm[at..at + PAUSE_FRAME_SAMPLES]
            .iter()
            .map(|sample| sample.abs())
            .sum::<f32>()
            / PAUSE_FRAME_SAMPLES as f32;

        amplitude_sum += f64::from(amplitude);
        frame_count += 1;
        if quietest.is_none_or(|(_, best_amplitude)| amplitude < best_amplitude) {
            quietest = Some((at + PAUSE_FRAME_SAMPLES / 2, amplitude));
        }
    }

    let (split, quietest_amplitude) = quietest?;
    let mean_amplitude = (amplitude_sum / f64::from(frame_count)) as f32;
    (quietest_amplitude <= mean_amplitude * PAUSE_RATIO).then_some(split)
}

#[derive(Debug)]
struct TextToken<'a> {
    text: &'a str,
    end: usize,
    is_content: bool,
}

/// Append a window transcript and remove the exact text emitted for the audio
/// overlap. Failure is deliberately lossless: if the join is ambiguous, the
/// whole new transcript is appended and a few words may repeat.
pub(super) fn merge_transcript(accumulated: &mut String, current: &str) -> bool {
    let current = current.trim();
    if current.is_empty() {
        return true;
    }
    if accumulated.is_empty() {
        accumulated.push_str(current);
        return true;
    }

    let previous_tokens = tokenize(accumulated);
    let current_tokens = tokenize(current);
    let current_skip = text_seam(&previous_tokens, &current_tokens);
    let matched = current_skip.is_some();
    let remainder = current_skip
        .map(|offset| current[offset..].trim_start())
        .unwrap_or(current);

    append_text(accumulated, remainder);
    matched
}

fn text_seam(previous: &[TextToken<'_>], current: &[TextToken<'_>]) -> Option<usize> {
    if previous.is_empty() || current.is_empty() {
        return None;
    }

    let previous_begin = previous.len().saturating_sub(PREVIOUS_TOKEN_SEARCH);
    let current_end = current.len().min(CURRENT_TOKEN_SEARCH);
    let mut best = None;

    for previous_at in previous_begin..previous.len() {
        for current_at in 0..current_end {
            let mut length = 0;
            while previous_at + length < previous.len()
                && current_at + length < current_end
                && previous[previous_at + length].text == current[current_at + length].text
            {
                length += 1;
            }

            let reaches_previous_end = previous_at + length == previous.len();
            let strong_enough =
                length >= 2 || (length == 1 && current_at == 0 && current[current_at].is_content);
            if !reaches_previous_end || !strong_enough {
                continue;
            }

            let current_match_end = current_at + length;
            if current_match_end == current.len() {
                continue;
            }

            if best.is_none_or(|(best_length, _)| length > best_length) {
                best = Some((length, current[current_match_end - 1].end));
            }
        }
    }

    best.map(|(_, byte_offset)| byte_offset)
}

fn tokenize(text: &str) -> Vec<TextToken<'_>> {
    let mut tokens = Vec::new();
    let mut chars = text.char_indices().peekable();

    while let Some((start, ch)) = chars.next() {
        if ch.is_whitespace() {
            continue;
        }

        let mut end = start + ch.len_utf8();
        let is_content = ch.is_alphanumeric();
        if is_content && !is_cjk(ch) {
            while let Some(&(next_at, next)) = chars.peek() {
                if is_cjk(next) || !(next.is_alphanumeric() || next == '\'') {
                    break;
                }
                chars.next();
                end = next_at + next.len_utf8();
            }
        }

        tokens.push(TextToken {
            text: &text[start..end],
            end,
            is_content,
        });
    }

    tokens
}

fn append_text(accumulated: &mut String, text: &str) {
    if text.is_empty() {
        return;
    }

    let previous = accumulated.chars().next_back();
    let next = text.chars().next();
    if matches!((previous, next), (Some(left), Some(right)) if needs_space(left, right)) {
        accumulated.push(' ');
    }
    accumulated.push_str(text);
}

fn needs_space(previous: char, next: char) -> bool {
    if previous.is_whitespace() || next.is_whitespace() || is_cjk(previous) || is_cjk(next) {
        return false;
    }
    !matches!(
        next,
        ',' | '.' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '%'
    ) && !matches!(previous, '(' | '[' | '{' | '/' | '#' | '@')
}

fn is_cjk(ch: char) -> bool {
    matches!(
        ch as u32,
        0x2E80..=0x2FFF
            | 0x3040..=0x30FF
            | 0x31F0..=0x31FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xAC00..=0xD7AF
            | 0xF900..=0xFAFF
            | 0x20000..=0x2FA1F
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_audio_stays_in_one_window() {
        let pcm = vec![0.0; WINDOW_SAMPLES];
        let windows = audio_windows(&pcm);

        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].len(), WINDOW_SAMPLES);
    }

    #[test]
    fn long_audio_uses_bounded_overlapping_windows() {
        let pcm = vec![0.5; WINDOW_SAMPLES * 2];
        let windows = audio_windows(&pcm);

        assert_eq!(windows.len(), 3);
        assert!(windows.iter().all(|window| window.len() <= WINDOW_SAMPLES));
        assert_eq!(
            windows[0].as_ptr_range().end,
            windows[1][OVERLAP_SAMPLES..].as_ptr()
        );
    }

    #[test]
    fn window_boundary_prefers_a_pause() {
        let mut pcm = vec![0.5; WINDOW_SAMPLES + SAMPLE_RATE];
        let pause_start = WINDOW_SAMPLES - 2 * SAMPLE_RATE;
        pcm[pause_start..pause_start + SAMPLE_RATE].fill(0.0);

        let windows = audio_windows(&pcm);

        assert_eq!(windows.len(), 2);
        assert!((pause_start..pause_start + SAMPLE_RATE).contains(&windows[0].len()));
    }

    #[test]
    fn overlap_is_removed_at_a_clean_text_seam() {
        let mut text = "one two three four five".to_string();

        assert!(merge_transcript(&mut text, "four five six seven"));
        assert_eq!(text, "one two three four five six seven");
    }

    #[test]
    fn ambiguous_text_is_appended_instead_of_dropped() {
        let mut text = "one two three nine".to_string();

        assert!(!merge_transcript(&mut text, "two three four"));
        assert_eq!(text, "one two three nine two three four");
    }

    #[test]
    fn repeated_speech_after_the_overlap_is_preserved() {
        let mut text = "start repeat this phrase".to_string();

        assert!(merge_transcript(
            &mut text,
            "repeat this phrase repeat this phrase finish"
        ));
        assert_eq!(text, "start repeat this phrase repeat this phrase finish");
    }

    #[test]
    fn a_fully_consumed_window_is_not_treated_as_overlap() {
        let mut text = "one two three".to_string();

        assert!(!merge_transcript(&mut text, "two three"));
        assert_eq!(text, "one two three two three");
    }

    #[test]
    fn unspaced_cjk_text_is_joined_without_inserting_spaces() {
        let mut text = "\u{4eca}\u{5929}\u{5929}\u{6c14}\u{5f88}\u{597d}".to_string();

        assert!(merge_transcript(
            &mut text,
            "\u{5929}\u{6c14}\u{5f88}\u{597d}\u{6211}\u{4eec}\u{51fa}\u{53d1}"
        ));
        assert_eq!(
            text,
            "\u{4eca}\u{5929}\u{5929}\u{6c14}\u{5f88}\u{597d}\u{6211}\u{4eec}\u{51fa}\u{53d1}"
        );
    }
}
